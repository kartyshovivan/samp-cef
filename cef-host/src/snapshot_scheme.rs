//! `sampcef://snapshot/<entity snapshot string>`: renders of game models (skins, vehicles,
//! objects) made by client.dll inside the game on request.
//!
//! The request is answered asynchronously: `open` forwards it to the game as
//! `Event::SnapshotRequest` and keeps CEF's callback; `complete` (on `Command::SnapshotResult`)
//! stores the PNG and resumes the request. An empty result becomes a 404 so the page can fall
//! back to a prerendered image.

use std::collections::HashMap;
use std::os::raw::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cef::types::string::CefString;
use cef_sys::{
    cef_callback_t, cef_request_t, cef_resource_handler_t, cef_resource_read_callback_t,
    cef_resource_skip_callback_t, cef_response_t, cef_scheme_handler_factory_t, cef_string_t,
};
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;

use crate::assets_scheme::{ASSET_SCHEME, SNAPSHOT_HOST, Wrapper};

#[derive(Default)]
struct State {
    done: bool,
    body: Vec<u8>,
    offset: usize,
    /// `cef_callback_t*` (add_ref'ed) to resume the request once the PNG arrives.
    callback: Option<usize>,
}

#[derive(Default)]
struct SnapshotHandler {
    state: Arc<Mutex<State>>,
}

static PENDING: Mutex<Option<HashMap<u32, Arc<Mutex<State>>>>> = Mutex::new(None);
static NEXT_REQ: AtomicU32 = AtomicU32::new(1);

pub fn register() {
    let scheme = CefString::new(ASSET_SCHEME);
    let domain = CefString::new(SNAPSHOT_HOST);

    let mut object: cef_scheme_handler_factory_t = unsafe { std::mem::zeroed() };
    object.create = Some(factory_create);
    let factory = Wrapper::new(object, ()).into_cef();

    let result = unsafe {
        cef_sys::cef_register_scheme_handler_factory(
            scheme.as_cef_string(),
            domain.as_cef_string(),
            factory,
        )
    };

    if result == 0 {
        log::error!("failed to register snapshot scheme handler factory");
    }
}

/// The game answered request `req` (empty `png` = couldn't render).
pub fn complete(req: u32, png: Vec<u8>) {
    let state = PENDING.lock().as_mut().and_then(|map| map.remove(&req));

    let Some(state) = state else {
        return;
    };

    let callback = {
        let mut state = state.lock();
        state.body = png;
        state.offset = 0;
        state.done = true;
        state.callback.take()
    };

    if let Some(callback) = callback {
        let callback = callback as *mut cef_callback_t;

        unsafe {
            if let Some(cont) = (*callback).cont {
                cont(callback);
            }
            release_callback(callback);
        }
    }
}

unsafe fn release_callback(callback: *mut cef_callback_t) {
    unsafe {
        if let Some(release) = (*callback).base.release {
            release(&mut (*callback).base);
        }
    }
}

/// `sampcef://snapshot/<spec>[.png]` -> `<spec>`.
fn spec_from_url(url: &str) -> Option<String> {
    let prefix = format!("{}://{}/", ASSET_SCHEME, SNAPSHOT_HOST);
    let rest = url.strip_prefix(&prefix)?;
    let rest = rest.split(['?', '#']).next()?;
    let decoded = percent_decode_str(rest).decode_utf8().ok()?;
    let spec = decoded.trim_end_matches(".png").trim_matches('/');

    (!spec.is_empty() && spec.len() < 256).then(|| spec.to_string())
}

fn request_url(request: *mut cef_request_t) -> String {
    unsafe {
        (*request)
            .get_url
            .map(|get_url| CefString::from(get_url(request)).to_string())
            .unwrap_or_default()
    }
}

extern "system" fn factory_create(
    _this: *mut cef_scheme_handler_factory_t, _browser: *mut cef_sys::cef_browser_t,
    _frame: *mut cef_sys::cef_frame_t, _scheme_name: *const cef_string_t,
    _request: *mut cef_request_t,
) -> *mut cef_resource_handler_t {
    let mut object: cef_resource_handler_t = unsafe { std::mem::zeroed() };
    object.open = Some(resource_open);
    object.get_response_headers = Some(resource_get_response_headers);
    object.skip = Some(resource_skip);
    object.read = Some(resource_read);
    object.cancel = Some(resource_cancel);

    Wrapper::new(object, SnapshotHandler::default()).into_cef()
}

extern "system" fn resource_open(
    this: *mut cef_resource_handler_t, request: *mut cef_request_t, handle_request: *mut c_int,
    callback: *mut cef_callback_t,
) -> c_int {
    if request.is_null() || handle_request.is_null() {
        return 0;
    }

    let obj: &mut Wrapper<cef_resource_handler_t, SnapshotHandler> = Wrapper::unwrap(this);
    let url = request_url(request);

    let Some(spec) = spec_from_url(&url) else {
        // Bad URL: answer 404 right away.
        obj.interface.state.lock().done = true;
        unsafe { *handle_request = 1 };
        return 1;
    };

    let req = NEXT_REQ.fetch_add(1, Ordering::Relaxed);

    if !callback.is_null() {
        unsafe {
            if let Some(add_ref) = (*callback).base.add_ref {
                add_ref(&mut (*callback).base);
            }
        }
        obj.interface.state.lock().callback = Some(callback as usize);
    }

    PENDING
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(req, obj.interface.state.clone());

    log::trace!("snapshot request {}: {}", req, spec);
    crate::ipc::send(cef_ipc::Event::SnapshotRequest { req, spec });

    // Answer later from `complete`, through the callback.
    unsafe { *handle_request = 0 };
    1
}

extern "system" fn resource_get_response_headers(
    this: *mut cef_resource_handler_t, response: *mut cef_response_t, response_length: *mut i64,
    _redirect_url: *mut cef_string_t,
) {
    let obj: &mut Wrapper<cef_resource_handler_t, SnapshotHandler> = Wrapper::unwrap(this);
    let state = obj.interface.state.lock();
    let found = !state.body.is_empty();

    let (status, text) = if found { (200, "OK") } else { (404, "Not Found") };
    let status_text = CefString::new(text);
    let mime = CefString::new(if found { "image/png" } else { "text/plain" });

    unsafe {
        if let Some(set_status) = (*response).set_status {
            set_status(response, status);
        }
        if let Some(set_status_text) = (*response).set_status_text {
            set_status_text(response, status_text.as_cef_string());
        }
        if let Some(set_mime_type) = (*response).set_mime_type {
            set_mime_type(response, mime.as_cef_string());
        }
        if let Some(set_header_by_name) = (*response).set_header_by_name {
            // Same spec = same picture: let Chromium cache it.
            let name = CefString::new("Cache-Control");
            let value = CefString::new("max-age=86400");
            set_header_by_name(response, name.as_cef_string(), value.as_cef_string(), 1);
        }
        if !response_length.is_null() {
            *response_length = state.body.len() as i64;
        }
    }
}

extern "system" fn resource_skip(
    this: *mut cef_resource_handler_t, bytes_to_skip: i64, bytes_skipped: *mut i64,
    _callback: *mut cef_resource_skip_callback_t,
) -> c_int {
    let obj: &mut Wrapper<cef_resource_handler_t, SnapshotHandler> = Wrapper::unwrap(this);
    let mut state = obj.interface.state.lock();
    let remaining = state.body.len().saturating_sub(state.offset);
    let skipped = remaining.min(bytes_to_skip.max(0) as usize);
    state.offset += skipped;

    if !bytes_skipped.is_null() {
        unsafe { *bytes_skipped = skipped as i64 };
    }

    (skipped > 0) as c_int
}

extern "system" fn resource_read(
    this: *mut cef_resource_handler_t, data_out: *mut c_void, bytes_to_read: c_int,
    bytes_read: *mut c_int, _callback: *mut cef_resource_read_callback_t,
) -> c_int {
    let obj: &mut Wrapper<cef_resource_handler_t, SnapshotHandler> = Wrapper::unwrap(this);
    let mut state = obj.interface.state.lock();
    let remaining = state.body.len().saturating_sub(state.offset);
    let count = remaining.min(bytes_to_read.max(0) as usize);

    if count > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(state.body.as_ptr().add(state.offset), data_out.cast::<u8>(), count);
        }
        state.offset += count;
    }

    if !bytes_read.is_null() {
        unsafe { *bytes_read = count as c_int };
    }

    (count > 0) as c_int
}

extern "system" fn resource_cancel(this: *mut cef_resource_handler_t) {
    let obj: &mut Wrapper<cef_resource_handler_t, SnapshotHandler> = Wrapper::unwrap(this);
    let callback = obj.interface.state.lock().callback.take();

    // The PENDING entry is dropped when (if) the game answers.
    if let Some(callback) = callback {
        unsafe { release_callback(callback as *mut cef_callback_t) };
    }
}

#[cfg(test)]
mod tests {
    use super::spec_from_url;

    #[test]
    fn parses_snapshot_urls() {
        assert_eq!(
            spec_from_url("sampcef://snapshot/es_15889_0.0_180.0_0.0_0.1_1.0_0.2_4_5_6.png").as_deref(),
            Some("es_15889_0.0_180.0_0.0_0.1_1.0_0.2_4_5_6")
        );
        assert_eq!(
            spec_from_url("sampcef://snapshot/veh_411_0_0_0_0_1_0?x=1").as_deref(),
            Some("veh_411_0_0_0_0_1_0")
        );
        assert_eq!(spec_from_url("sampcef://snapshot/"), None);
        assert_eq!(spec_from_url("sampcef://assets/a.png"), None);
    }
}
