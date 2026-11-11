//! Browsers living in the host process and their CEF handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cef::ProcessId;
use cef::browser::{Browser, BrowserHost, ContextMenuParams, Frame, MenuModel};
use cef::client::Client;
use cef::handlers::audio::AudioHandler;
use cef::handlers::context_menu::ContextMenuHandler;
use cef::handlers::lifespan::LifespanHandler;
use cef::handlers::load::LoadHandler;
use cef::handlers::render::{DirtyRects, PaintElement, RenderHandler};
use cef::process_message::ProcessMessage;
use cef::types::list::{List, ValueType};
use cef::types::string::CefString;
use cef_ipc::frame::Rect;
use cef_ipc::{Event, FrameBuffer, KeyEvent, Value};
use cef_sys::{cef_audio_parameters_t, cef_key_event_t, cef_mouse_event_t, cef_rect_t};
use parking_lot::Mutex;

pub struct HostBrowser {
    id: u32,
    audio: bool,
    size: Mutex<(u32, u32)>,
    hidden: AtomicBool,
    browser: Mutex<Option<Browser>>,
    frame: Mutex<Option<FrameBuffer>>,
    frame_generation: AtomicU32,
    /// Channel count per audio stream, needed to flatten planar packets.
    audio_channels: Mutex<HashMap<i32, i32>>,
}

static BROWSERS: Mutex<Option<HashMap<u32, Arc<HostBrowser>>>> = Mutex::new(None);

fn with_browsers<R>(f: impl FnOnce(&mut HashMap<u32, Arc<HostBrowser>>) -> R) -> R {
    let mut guard = BROWSERS.lock();
    f(guard.get_or_insert_with(HashMap::new))
}

pub fn get(id: u32) -> Option<Arc<HostBrowser>> {
    with_browsers(|map| map.get(&id).cloned())
}

pub fn count() -> usize {
    with_browsers(|map| map.len())
}

pub fn all() -> Vec<Arc<HostBrowser>> {
    with_browsers(|map| map.values().cloned().collect())
}

impl HostBrowser {
    pub fn browser(&self) -> Option<Browser> {
        self.browser.lock().clone()
    }

    pub fn host(&self) -> Option<BrowserHost> {
        self.browser().map(|b| b.host())
    }

    pub fn resize(&self, width: u32, height: u32) {
        *self.size.lock() = (width.max(1), height.max(1));

        if let Some(host) = self.host() {
            host.was_resized();
        }
    }

    pub fn set_hidden(&self, hidden: bool) {
        self.hidden.store(hidden, Ordering::SeqCst);

        if let Some(host) = self.host() {
            host.was_hidden(hidden);

            if !hidden {
                host.invalidate(PaintElement::View);
            }
        }
    }

    pub fn close(&self, force: bool) {
        if let Some(host) = self.host() {
            host.close_browser(force);
        } else {
            // Not created yet: forget it, `on_after_created` will close it.
            with_browsers(|map| map.remove(&self.id));
        }
    }

    pub fn mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        if let Some(host) = self.host() {
            host.send_mouse_move(cef_mouse_event_t { x, y, modifiers });
        }
    }

    pub fn mouse_click(&self, x: i32, y: i32, modifiers: u32, button: u8, down: bool) {
        use cef_sys::cef_mouse_button_type_t::*;

        let button = match button {
            cef_ipc::proto::MOUSE_MIDDLE => MBT_MIDDLE,
            cef_ipc::proto::MOUSE_RIGHT => MBT_RIGHT,
            _ => MBT_LEFT,
        };

        if let Some(host) = self.host() {
            host.send_mouse_click(button, cef_mouse_event_t { x, y, modifiers }, down);
        }
    }

    pub fn mouse_wheel(&self, x: i32, y: i32, delta: i32) {
        if let Some(host) = self.host() {
            host.send_mouse_wheel(x, y, delta);
        }
    }

    pub fn key(&self, e: KeyEvent) {
        let event = cef_key_event_t {
            #[cfg(not(target_arch = "x86"))]
            size: std::mem::size_of::<cef_key_event_t>(),
            type_: e.kind as _,
            modifiers: e.modifiers,
            windows_key_code: e.windows_key_code,
            native_key_code: e.native_key_code,
            is_system_key: e.is_system_key,
            character: e.character as _,
            unmodified_character: e.unmodified_character as _,
            focus_on_editable_field: e.focus_on_editable_field,
        };

        if let Some(host) = self.host() {
            host.send_keyboard_event(event);
        }
    }

    pub fn trigger_event(&self, name: &str, args: &Value) {
        let Some(frame) = self.browser().map(|b| b.main_frame()) else {
            return;
        };

        let msg = ProcessMessage::create("trigger_event");
        let list = msg.argument_list();
        list.set_string(0, &CefString::new(name));

        let payload = List::new();
        if let Value::List(items) = args {
            write_list(&payload, items);
        }
        list.set_list(1, payload);

        frame.send_process_message(ProcessId::Renderer, msg);
    }

    pub fn load_url(&self, url: &str) {
        if let Some(browser) = self.browser() {
            let url = crate::assets_scheme::resolve_browser_url(url);
            browser.main_frame().load_url(&url);
        }
    }

    pub fn set_frame_rate(&self, fps: i32) {
        if let Some(host) = self.host() {
            host.set_windowless_frame_rate(fps);
        }
    }

    pub fn set_audio_muted(&self, muted: bool) {
        if let Some(host) = self.host() {
            host.set_audio_muted(muted);
        }
    }

    /// Game-driven frame (external begin frame mode).
    pub fn begin_frame(&self) {
        if let Some(host) = self.host() {
            host.send_external_begin_frame();
        }
    }

    pub fn invalidate(&self) {
        if let Some(host) = self.host() {
            host.invalidate(PaintElement::View);
        }
    }

    pub fn toggle_dev_tools(&self, enabled: bool) {
        use winapi::um::winuser::*;

        let Some(host) = self.host() else {
            return;
        };

        if !enabled {
            host.close_dev_tools();
            return;
        }

        let caption = CefString::new(&format!("Dev Tools for {} browser", self.id));
        let mut window_info = unsafe { std::mem::zeroed::<cef_sys::cef_window_info_t>() };
        window_info.style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS | WS_VISIBLE;

        // CEF 89 (32-bit host): plain x/y/width/height.
        #[cfg(target_arch = "x86")]
        {
            window_info.x = CW_USEDEFAULT;
            window_info.y = CW_USEDEFAULT;
            window_info.width = CW_USEDEFAULT;
            window_info.height = CW_USEDEFAULT;
        }

        // CEF 156 (64-bit host): sized struct with `bounds`.
        #[cfg(not(target_arch = "x86"))]
        {
            window_info.size = std::mem::size_of::<cef_sys::cef_window_info_t>();
            window_info.bounds = cef_sys::cef_rect_t {
                x: CW_USEDEFAULT,
                y: CW_USEDEFAULT,
                width: CW_USEDEFAULT,
                height: CW_USEDEFAULT,
            };
        }

        window_info.window_name = caption.to_cef_string();

        let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_browser_settings_t>() };
        settings.size = std::mem::size_of::<cef_sys::cef_browser_settings_t>();

        host.open_dev_tools(&window_info, &settings);
    }

    /// Makes sure the shared frame buffer can hold `len` bytes, recreating it if needed.
    fn ensure_frame_buffer(&self, slot: &mut Option<FrameBuffer>, len: usize) -> bool {
        if slot.as_ref().is_some_and(|fb| fb.capacity() >= len) {
            return true;
        }

        let generation = self.frame_generation.fetch_add(1, Ordering::SeqCst);
        let name = format!("orp_cef_{}_{}_{}", std::process::id(), self.id, generation);
        // Round up to 1 MiB so small growth doesn't recreate the section every time.
        let capacity = len.div_ceil(1 << 20) << 20;

        match FrameBuffer::create(&name, capacity) {
            Ok(fb) => {
                log::info!("browser {}: frame buffer {} ({} bytes)", self.id, name, capacity);
                *slot = Some(fb);
                crate::ipc::send(Event::FrameBuffer { id: self.id, name, capacity: capacity as u32 });
                true
            }
            Err(err) => {
                log::error!("browser {}: failed to create frame buffer: {}", self.id, err);
                false
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create(
    id: u32, url: &str, width: u32, height: u32, hidden: bool, audio: bool, external_frames: bool,
    parent: usize,
) {
    let host_browser = Arc::new(HostBrowser {
        id,
        audio,
        size: Mutex::new((width.max(1), height.max(1))),
        hidden: AtomicBool::new(hidden),
        browser: Mutex::new(None),
        frame: Mutex::new(None),
        frame_generation: AtomicU32::new(0),
        audio_channels: Mutex::new(HashMap::new()),
    });

    if let Some(previous) = with_browsers(|map| map.insert(id, host_browser.clone())) {
        previous.close(true);
    }

    let mut window_info = unsafe { std::mem::zeroed::<cef_sys::cef_window_info_t>() };
    window_info.parent_window = parent as _;
    window_info.windowless_rendering_enabled = 1;

    // CEF 156: sized struct, and off-screen rendering needs the Alloy browser style.
    #[cfg(not(target_arch = "x86"))]
    {
        window_info.size = std::mem::size_of::<cef_sys::cef_window_info_t>();
        window_info.runtime_style = cef_sys::cef_runtime_style_t::CEF_RUNTIME_STYLE_ALLOY;
    }

    window_info.external_begin_frame_enabled = external_frames as i32;

    let url = CefString::new(&crate::assets_scheme::resolve_browser_url(url));

    let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_browser_settings_t>() };
    settings.size = std::mem::size_of::<cef_sys::cef_browser_settings_t>();
    settings.windowless_frame_rate = 60;
    settings.javascript_access_clipboard = cef_sys::cef_state_t::STATE_ENABLED;
    settings.javascript_dom_paste = cef_sys::cef_state_t::STATE_ENABLED;
    settings.remote_fonts = cef_sys::cef_state_t::STATE_ENABLED;
    settings.webgl = cef_sys::cef_state_t::STATE_ENABLED;
    settings.javascript = cef_sys::cef_state_t::STATE_ENABLED;

    let client = HostClient(host_browser);
    let result = BrowserHost::create_browser(&window_info, Some(client), &url, &settings);

    log::info!("create_browser({}) => {}", id, result);
}

#[derive(Clone)]
struct HostClient(Arc<HostBrowser>);

impl HostClient {
    /// `true` if this client is still the registered browser for its id.
    fn is_current(&self) -> bool {
        get(self.0.id).is_some_and(|b| Arc::ptr_eq(&b, &self.0))
    }
}

impl LifespanHandler for HostClient {
    fn on_after_created(&self, browser: Browser) {
        *self.0.browser.lock() = Some(browser.clone());

        if !self.is_current() {
            // Closed or replaced before CEF finished creating it.
            browser.host().close_browser(true);
            return;
        }

        if self.0.hidden.load(Ordering::SeqCst) {
            browser.host().was_hidden(true);
        }

        crate::ipc::send(Event::AfterCreated { id: self.0.id });
    }

    fn on_before_close(&self, _: Browser) {
        log::info!("browser {} closing", self.0.id);

        if let Some(browser) = self.0.browser.lock().take() {
            browser.host().close_dev_tools();
        }

        let removed = with_browsers(|map| {
            if map.get(&self.0.id).is_some_and(|b| Arc::ptr_eq(b, &self.0)) {
                map.remove(&self.0.id);
                true
            } else {
                false
            }
        });

        if removed {
            crate::ipc::send(Event::BeforeClose { id: self.0.id });
        }

        self.0.frame.lock().take();
    }
}

impl LoadHandler for HostClient {
    fn on_load_end(&self, _: Browser, frame: Frame, status_code: i32) {
        if frame.is_main() {
            crate::ipc::send(Event::LoadEnd { id: self.0.id, status: status_code });
        }
    }
}

impl ContextMenuHandler for HostClient {
    fn on_before_context_menu(&self, _: Browser, _: Frame, _: ContextMenuParams, model: MenuModel) {
        model.clear();
    }
}

impl RenderHandler for HostClient {
    fn view_rect(&self, _: Browser, rect: &mut cef_rect_t) {
        let (width, height) = *self.0.size.lock();
        *rect = cef_rect_t { x: 0, y: 0, width: width as i32, height: height as i32 };
    }

    fn on_popup_show(&self, _: Browser, show: bool) {
        crate::ipc::send(Event::PopupShow { id: self.0.id, show });
    }

    fn on_popup_size(&self, _: Browser, rect: &cef_rect_t) {
        crate::ipc::send(Event::PopupSize {
            id: self.0.id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
    }

    fn on_paint(
        &self, _: Browser, paint_type: PaintElement, dirty_rects: DirtyRects, buffer: &[u8],
        width: usize, height: usize,
    ) {
        match paint_type {
            PaintElement::Popup => {
                crate::ipc::send(Event::PopupPaint {
                    id: self.0.id,
                    width: width as u32,
                    height: height as u32,
                    pixels: buffer.to_vec(),
                });
            }

            PaintElement::View => {
                let len = width * height * 4;

                if len == 0 || buffer.len() < len {
                    return;
                }

                let mut slot = self.0.frame.lock();

                if !self.0.ensure_frame_buffer(&mut slot, len) {
                    return;
                }

                let rects: Vec<Rect> = dirty_rects
                    .as_slice()
                    .iter()
                    .map(|r| Rect { x: r.x, y: r.y, width: r.width, height: r.height })
                    .collect();

                if let Some(fb) = slot.as_ref() {
                    let started = std::time::Instant::now();
                    let changed = fb
                        .write_paint_diff(buffer, width as u32, height as u32, &rects)
                        .unwrap_or(0);

                    crate::stats::paint(started.elapsed().as_micros() as u64, changed);
                }
            }
        }
    }
}

impl AudioHandler for HostClient {
    fn get_audio_parameters(&self, _: Browser, _params: &mut cef_audio_parameters_t) -> bool {
        true
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn on_audio_stream_packet(
        &self, _: Browser, stream_id: i32, data: *mut *const f32, frames: i32, pts: i64,
    ) {
        let channels = self.0.audio_channels.lock().get(&stream_id).copied().unwrap_or(0);

        if channels <= 0 || frames <= 0 || data.is_null() {
            return;
        }

        let frames_usize = frames as usize;
        let mut flat = Vec::with_capacity(channels as usize * frames_usize);

        for ch in 0..channels as usize {
            let plane = unsafe { *data.add(ch) };

            if plane.is_null() {
                flat.extend(std::iter::repeat_n(0.0, frames_usize));
            } else {
                flat.extend_from_slice(unsafe { std::slice::from_raw_parts(plane, frames_usize) });
            }
        }

        crate::ipc::send(Event::AudioPacket { id: self.0.id, stream: stream_id, frames, pts, data: flat });
    }

    fn on_audio_stream_started(
        &self, _: Browser, stream_id: i32, channels: i32, _channel_layout: i32, sample_rate: i32,
        frames_per_buffer: i32,
    ) {
        self.0.audio_channels.lock().insert(stream_id, channels);

        crate::ipc::send(Event::AudioStarted {
            id: self.0.id,
            stream: stream_id,
            channels,
            sample_rate,
            frames_per_buffer,
        });
    }

    fn on_audio_stream_stopped(&self, _: Browser, stream_id: i32) {
        self.0.audio_channels.lock().remove(&stream_id);
        crate::ipc::send(Event::AudioStopped { id: self.0.id, stream: stream_id });
    }

    fn on_audio_stream_error(&self, _: Browser, error: String) {
        log::warn!("browser {}: audio stream error: {}", self.0.id, error);
    }
}

impl Client for HostClient {
    type LifespanHandler = Self;
    type RenderHandler = Self;
    type ContextMenuHandler = Self;
    type LoadHandler = Self;
    type AudioHandler = Self;

    fn lifespan_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn render_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn context_menu_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn load_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn audio_handler(&self) -> Option<Self> {
        self.0.audio.then(|| self.clone())
    }

    fn on_process_message(
        &self, _: Browser, _: Frame, _: ProcessId, msg: ProcessMessage,
    ) -> bool {
        let name = msg.name().to_string();
        let args = read_list(&msg.argument_list(), 0);

        crate::ipc::send(Event::ProcessMessage { id: self.0.id, name, args });
        true
    }
}

fn read_list(list: &List, depth: u32) -> Vec<Value> {
    (0..list.len())
        .map(|idx| match list.get_type(idx) {
            ValueType::Bool => Value::Bool(list.bool(idx)),
            ValueType::Integer => Value::Int(list.integer(idx)),
            ValueType::Double => Value::Double(list.double(idx)),
            ValueType::String => Value::String(list.string(idx).to_string()),
            ValueType::List if depth < 16 => list
                .list(idx)
                .map(|inner| Value::List(read_list(&inner, depth + 1)))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        })
        .collect()
}

fn write_list(list: &List, values: &[Value]) {
    for (idx, value) in values.iter().enumerate() {
        match value {
            Value::Null => list.set_null(idx),
            Value::Bool(v) => list.set_bool(idx, *v),
            Value::Int(v) => list.set_integer(idx, *v),
            Value::Double(v) => list.set_double(idx, *v),
            Value::String(v) => list.set_string(idx, &CefString::new(v)),
            Value::List(items) => {
                let inner = List::new();
                write_list(&inner, items);
                list.set_list(idx, inner);
            }
        }
    }
}
