use std::collections::HashSet;
use std::ffi::CString;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use cef::types::list::{List, ValueType};
use cef::types::string::CefString;
use cef_ipc::{Command, FrameBuffer};
use cef_sys::cef_rect_t;

use client_api::utils::handle_result;

use crate::app::Event;
use crate::audio::Audio;
use crate::browser::host;
use crate::browser::view::View;
use crate::external::{CallbackList, EXTERNAL_BREAK};

struct Popup {
    buffer: Vec<u8>,
    rect: cef_rect_t,
    show: bool,
    /// The popup was just hidden: the area under it must be restored from the frame.
    was_shown: bool,
}

impl Default for Popup {
    fn default() -> Self {
        Popup {
            buffer: Vec::new(),
            rect: cef_rect_t { x: 0, y: 0, width: 0, height: 0 },
            show: false,
            was_shown: false,
        }
    }
}

/// A browser as seen from the game. CEF itself runs in cef_host.exe; this side owns the
/// texture, forwards input and commands, and uploads frames from shared memory.
pub struct WebClient {
    id: u32, // static
    is_extern: bool,
    hidden: AtomicBool,
    prev_hidden_flag: AtomicBool,
    closing: AtomicBool,
    listen_keys: AtomicBool,
    created: AtomicBool,
    loading: AtomicBool,
    pub view: Mutex<View>,
    frame: Mutex<Option<FrameBuffer>>,
    /// Frame-buffer `seq` that was last uploaded into the texture.
    uploaded_seq: AtomicU32,
    /// Re-upload the whole frame next time (texture was recreated).
    force_full: AtomicBool,
    popup: Mutex<Popup>,
    audio: Option<Arc<Audio>>, // static
    /// Channel count per audio stream, needed to rebuild planar pointers.
    audio_channels: Mutex<std::collections::HashMap<i32, i32>>,
    event_tx: Sender<Event>,
    callbacks: CallbackList,
    object_list: Mutex<HashSet<i32>>,
}

impl WebClient {
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(id: u32, cbs: CallbackList, event_tx: Sender<Event>) -> Arc<WebClient> {
        let rect = crate::utils::client_rect();

        log::trace!("crate::utils::client_rect: {:?}", rect);

        let mut view = View::new();
        view.make_display(rect[0], rect[1]);

        Self::build(id, cbs, event_tx, view, false, None)
    }

    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new_extern(
        id: u32, cbs: CallbackList, event_tx: Sender<Event>, audio: Arc<Audio>,
    ) -> Arc<WebClient> {
        Self::build(id, cbs, event_tx, View::new(), true, Some(audio))
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn build(
        id: u32, callbacks: CallbackList, event_tx: Sender<Event>, view: View, is_extern: bool,
        audio: Option<Arc<Audio>>,
    ) -> Arc<WebClient> {
        let client = Arc::new(WebClient {
            id,
            is_extern,
            hidden: AtomicBool::new(false),
            prev_hidden_flag: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            listen_keys: AtomicBool::new(false),
            created: AtomicBool::new(false),
            loading: AtomicBool::new(true),
            view: Mutex::new(view),
            frame: Mutex::new(None),
            uploaded_seq: AtomicU32::new(0),
            force_full: AtomicBool::new(false),
            popup: Mutex::new(Popup::default()),
            audio,
            audio_channels: Mutex::new(std::collections::HashMap::new()),
            event_tx,
            callbacks,
            object_list: Mutex::new(HashSet::new()),
        });

        host::register(&client);
        client
    }

    /// Asks the host to create the CEF browser for this client.
    pub fn create_remote(&self, url: &str) {
        let rect = self.view.lock().rect();

        host::send(Command::CreateBrowser {
            id: self.id,
            url: url.to_string(),
            width: rect.width as u32,
            height: rect.height as u32,
            hidden: self.hidden.load(Ordering::SeqCst),
            audio: self.is_extern,
            external_frames: crate::browser::host::external_frames(),
        });
    }

    // ---- events from the host -------------------------------------------------------

    pub fn on_after_created(&self) {
        log::trace!("browser {} created in host", self.id);
        self.created.store(true, Ordering::SeqCst);

        // Re-apply state that may have changed while CEF was creating the browser.
        self.hide(self.hidden.load(Ordering::SeqCst));
        self.notify_was_resized();
    }

    pub fn on_load_end(&self) {
        self.loading.store(false, Ordering::SeqCst);
    }

    pub fn on_before_close(&self) {
        log::trace!("browser {} closed in host", self.id);
        self.created.store(false, Ordering::SeqCst);
        self.frame.lock().take();
    }

    pub fn set_frame_buffer(&self, fb: FrameBuffer) {
        log::trace!("browser {}: frame buffer {}", self.id, fb.name());
        *self.frame.lock() = Some(fb);
        self.uploaded_seq.store(0, Ordering::SeqCst);
    }

    pub fn on_popup_show(&self, show: bool) {
        let mut popup = self.popup.lock();
        popup.show = show;

        if !show {
            popup.buffer.clear();
            popup.was_shown = true;
        }
    }

    pub fn on_popup_size(&self, rect: cef_rect_t) {
        let mut popup = self.popup.lock();
        popup.rect = rect;
        popup
            .buffer
            .resize(rect.width.max(0) as usize * rect.height.max(0) as usize * 4, 0);
    }

    pub fn on_popup_paint(&self, pixels: &[u8]) {
        let mut popup = self.popup.lock();

        if popup.buffer.len() == pixels.len() {
            popup.buffer.copy_from_slice(pixels);
        }
    }

    pub fn on_audio_stream_started(
        &self, stream_id: i32, channels: i32, sample_rate: i32, frames_per_buffer: i32,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };

        self.audio_channels.lock().insert(stream_id, channels);
        audio.create_stream(self.id, stream_id, channels, sample_rate, frames_per_buffer);

        for &object_id in self.object_list.lock().iter() {
            audio.add_source(self.id, object_id);
        }
    }

    pub fn on_audio_stream_packet(&self, stream_id: i32, frames: i32, pts: i64, data: &[f32]) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };

        let channels = self.audio_channels.lock().get(&stream_id).copied().unwrap_or(0);
        let frames_usize = frames.max(0) as usize;

        if channels <= 0 || data.len() < channels as usize * frames_usize {
            return;
        }

        let mut planes: Vec<*const f32> = (0..channels as usize)
            .map(|ch| data[ch * frames_usize..].as_ptr())
            .collect();

        unsafe {
            audio.append_pcm(self.id, stream_id, planes.as_mut_ptr(), frames, pts as u64);
        }
    }

    pub fn on_audio_stream_stopped(&self, stream_id: i32) {
        self.audio_channels.lock().remove(&stream_id);

        if let Some(audio) = self.audio.as_ref() {
            audio.remove_stream(self.id, stream_id);
        }
    }

    /// Messages from the page (renderer -> host -> here).
    pub fn on_process_message(&self, name: &str, args: &List) {
        log::trace!(
            "WebClient::on_process_message. browser_id: {}, message: {:?}",
            self.id,
            name
        );

        let flag = |args: &List| match args.get_type(0) {
            ValueType::Integer => args.integer(0) == 1,
            ValueType::Bool => args.bool(0),
            _ => false,
        };

        match name {
            "set_focus" => {
                handle_result(self.event_tx.send(Event::FocusBrowser(self.id, flag(args))));
            }

            "hide" => {
                handle_result(self.event_tx.send(Event::HideBrowser(self.id, flag(args))));
            }

            "emit_event" => {
                if args.get_type(0) != ValueType::String {
                    return;
                }

                let event_name = args.string(0).to_string();

                if event_name == crate::autologin::REQUEST_EVENT {
                    let list = List::new();

                    if let Some(password) = crate::autologin::read_autologin_password() {
                        list.set_bool(0, true);
                        list.set_string(1, &CefString::new(&password));
                    } else {
                        list.set_bool(0, false);
                    }

                    handle_result(
                        self.event_tx
                            .send(Event::EmitEvent(crate::autologin::DATA_EVENT.to_string(), list)),
                    );

                    return;
                }

                {
                    let callbacks = self.callbacks.lock();

                    if let Some(cb) = callbacks.get(&event_name) {
                        let name = CString::new(event_name.clone()).unwrap_or_default();
                        let result = cb(name.as_ptr(), args.clone().into_cef());

                        // событие обработано плагином, нет смысла отправлять дальше серверу
                        if result == EXTERNAL_BREAK {
                            return;
                        }
                    }
                }

                let mut arguments = String::new();

                for idx in 1..args.len() {
                    let arg = match args.get_type(idx) {
                        ValueType::String => args.string(idx).to_string(),
                        ValueType::Bool => (if args.bool(idx) { 1 } else { 0 }).to_string(),
                        ValueType::Double => args.double(idx).to_string(),
                        ValueType::Integer => args.integer(idx).to_string(),
                        _ => "CEF_NULL".to_string(),
                    };

                    arguments.push_str(&arg);

                    if idx != args.len() - 1 {
                        arguments.push(' ');
                    }
                }

                handle_result(self.event_tx.send(Event::EmitEventOnServer(event_name, arguments)));
            }

            _ => (),
        }
    }

    // ---- game side ------------------------------------------------------------------

    #[inline]
    pub fn draw(&self) {
        let mut texture = self.view.lock();
        texture.draw();
    }

    #[inline]
    pub fn on_lost_device(&self) {
        self.internal_hide(true, true); // hide browser but do not save value

        let mut texture = self.view.lock();
        texture.on_lost_device();
        texture.make_inactive();
    }

    #[inline]
    pub fn on_reset_device(&self) {
        if !self.is_extern() {
            let mut view = self.view.lock();
            let rect = crate::utils::client_rect();

            view.make_active();
            view.make_display(rect[0], rect[1]);
        }

        self.notify_was_resized();
        self.force_full.store(true, Ordering::SeqCst);
        host::send(Command::Invalidate { id: self.id });

        self.restore_hide_status();
    }

    #[inline]
    pub fn resize(&self, width: usize, height: usize) {
        self.view.lock().resize(self.is_extern(), width, height);
        self.notify_was_resized();
        self.force_full.store(true, Ordering::SeqCst);
    }

    fn notify_was_resized(&self) {
        let rect = self.view.lock().rect();

        host::send(Command::Resize {
            id: self.id,
            width: rect.width as u32,
            height: rect.height as u32,
        });
    }

    /// Uploads whatever the host painted since the last call into the texture.
    #[inline]
    pub fn update_view(&self) {
        if self.hidden.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return;
        }

        let mut texture = self.view.lock();
        let size = texture.rect();

        {
            let frame = self.frame.lock();

            if let Some(fb) = frame.as_ref() {
                let seq = fb.seq();
                let force_full = self.force_full.load(Ordering::SeqCst);

                // Never wait for the host on the game thread: if it is writing right now,
                // pick the frame up on the next game frame.
                if (seq != self.uploaded_seq.load(Ordering::SeqCst) || force_full)
                    && let Some(mut guard) = fb.try_lock()
                {
                    let (width, height) = (guard.width(), guard.height());

                    // Only upload a frame that matches the texture; otherwise keep the
                    // pending rects, CEF repaints at the new size after `Resize`.
                    if width == size.width as u32 && height == size.height as u32 && width > 0 {
                        let started = std::time::Instant::now();

                        let rects: Vec<cef_rect_t> = if force_full {
                            vec![cef_rect_t { x: 0, y: 0, width: width as i32, height: height as i32 }]
                        } else {
                            guard
                                .rects()
                                .iter()
                                .map(|r| cef_rect_t { x: r.x, y: r.y, width: r.width, height: r.height })
                                .collect()
                        };

                        texture.update_texture(guard.pixels(), width as usize, &rects);
                        guard.clear_rects();

                        self.uploaded_seq.store(seq, Ordering::SeqCst);
                        self.force_full.store(false, Ordering::SeqCst);

                        let px: i64 = rects.iter().map(|r| r.width as i64 * r.height as i64).sum();
                        crate::browser::stats::upload(
                            started.elapsed().as_micros() as u64,
                            px as u64,
                            seq,
                        );

                        // The popup was drawn over this frame; restore what's under it.
                        let mut popup = self.popup.lock();
                        if popup.was_shown && !popup.show {
                            popup.was_shown = false;
                            let r = popup.rect;
                            texture.update_texture(guard.pixels(), width as usize, &[r]);
                        }
                    }
                }
            }
        }

        let popup = self.popup.lock();

        if popup.show
            && !popup.buffer.is_empty()
            && popup.rect.x + popup.rect.width < size.width
            && popup.rect.y + popup.rect.height < size.height
        {
            texture.update_popup(&popup.buffer, &popup.rect);
        }
    }

    pub fn hide(&self, hide: bool) {
        self.internal_hide(hide, false);
    }

    pub fn internal_hide(&self, hide: bool, update_prev: bool) {
        if update_prev {
            let cur = self.hidden.load(Ordering::SeqCst);
            self.prev_hidden_flag.store(cur, Ordering::SeqCst);
        }

        self.hidden.store(hide, Ordering::SeqCst);

        if hide {
            self.view.lock().clear();
        } else {
            // Texture was cleared while hidden: take the whole frame again.
            self.force_full.store(true, Ordering::SeqCst);
        }

        host::send(Command::Hide { id: self.id, hidden: hide });

        if !self.is_extern() {
            self.set_audio_muted(hide);
        }
    }

    pub fn restore_hide_status(&self) {
        self.internal_hide(self.prev_hidden_flag.load(Ordering::SeqCst), false);
    }

    pub fn set_audio_muted(&self, muted: bool) {
        host::send(Command::AudioMuted { id: self.id, muted });
    }

    pub fn add_object(&self, object_id: i32) {
        self.object_list.lock().insert(object_id);
    }

    pub fn remove_object(&self, object_id: i32) {
        self.object_list.lock().remove(&object_id);
    }

    pub fn remove_view(&self) {
        self.view.lock().make_inactive();
    }

    pub fn toggle_dev_tools(&self, enabled: bool) {
        host::send(Command::DevTools { id: self.id, enabled });
    }

    pub fn load_url(&self, url: &str) {
        self.loading.store(true, Ordering::SeqCst);
        host::send(Command::LoadUrl { id: self.id, url: url.to_string() });
    }

    pub fn send_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        host::send(Command::MouseMove { id: self.id, x, y, modifiers });
    }

    pub fn send_mouse_click(&self, x: i32, y: i32, button: u8, down: bool) {
        host::send(Command::MouseClick { id: self.id, x, y, modifiers: 0, button, down });
    }

    pub fn send_mouse_wheel(&self, x: i32, y: i32, delta: i32) {
        host::send(Command::MouseWheel { id: self.id, x, y, delta });
    }

    pub fn send_key(&self, event: &cef_sys::cef_key_event_t) {
        host::send(Command::Key {
            id: self.id,
            event: cef_ipc::KeyEvent {
                kind: event.type_,
                modifiers: event.modifiers,
                windows_key_code: event.windows_key_code,
                native_key_code: event.native_key_code,
                is_system_key: event.is_system_key,
                character: event.character,
                unmodified_character: event.unmodified_character,
                focus_on_editable_field: event.focus_on_editable_field,
            },
        });
    }

    pub fn trigger_event(&self, name: &str, args: &cef_ipc::Value) {
        host::send(Command::TriggerEvent { id: self.id, name: name.to_string(), args: args.clone() });
    }

    /// Game-driven frame (only used with `cef/external_frames`).
    pub fn send_begin_frame(&self) {
        host::send(Command::BeginFrame { id: self.id });
    }

    pub fn set_frame_rate(&self, fps: i32) {
        host::send(Command::FrameRate { id: self.id, fps });
    }

    /// Created in the host and finished loading its main frame.
    pub fn is_ready(&self) -> bool {
        self.created.load(Ordering::SeqCst) && !self.loading.load(Ordering::SeqCst)
    }

    pub fn is_created(&self) -> bool {
        self.created.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn is_hidden(&self) -> bool {
        self.hidden.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst)
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn is_extern(&self) -> bool {
        self.is_extern
    }

    pub fn close(&self, force_close: bool) {
        self.closing.store(true, Ordering::SeqCst);
        host::unregister(self);
        host::send(Command::CloseBrowser { id: self.id, force: force_close });
    }

    pub fn always_listen_keys(&self) -> bool {
        self.listen_keys.load(Ordering::SeqCst)
    }

    pub fn set_always_listen_keys(&self, listen: bool) {
        self.listen_keys.store(listen, Ordering::SeqCst);
    }
}
