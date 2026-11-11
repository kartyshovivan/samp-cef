use crate::app::{Event, ExternalBrowser};
use crate::audio::{Audio, BrowserAudioSettings};
use crate::browser::client::WebClient;
use crate::external::{BrowserReadyCallback, CallbackList};

use cef::types::list::List;
use cef_sys::{cef_event_flags_t, cef_key_event_t};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use client_api::gta::rw::rwcore::RwTexture;
use client_api::gta::rw::rwplcore::RwSurfaceProperties;

use crossbeam_channel::Sender;

/// Paces game-driven BeginFrames: every `n`-th game frame, with `n` chosen so the UI stays at
/// or just under 60 fps. A whole divisor keeps the cadence even (144 fps game -> every 3rd
/// frame = 48 fps), which looks smoother than alternating 2/3-frame gaps.
struct FramePacer {
    last: Option<std::time::Instant>,
    avg_dt: f32,
    counter: u32,
}

impl FramePacer {
    const TARGET_DT: f32 = 1.0 / 60.0;

    fn new() -> Self {
        Self { last: None, avg_dt: Self::TARGET_DT, counter: 0 }
    }

    /// Called once per game frame. Returns `true` if CEF should get a BeginFrame now.
    fn tick(&mut self) -> bool {
        let now = std::time::Instant::now();

        if let Some(last) = self.last.replace(now) {
            let dt = now.duration_since(last).as_secs_f32().clamp(0.001, 0.25);
            self.avg_dt += (dt - self.avg_dt) * 0.05;
        }

        // 10% slack so a 120 fps game reliably maps to every 2nd frame, not every 3rd.
        let divisor = ((Self::TARGET_DT / self.avg_dt) - 0.1).ceil().max(1.0) as u32;

        self.counter += 1;
        if self.counter >= divisor {
            self.counter = 0;
            true
        } else {
            false
        }
    }
}

const MIN_BROWSER_FPS: u64 = 30;
const MAX_BROWSER_FPS: u64 = 60;

#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub enum MouseKey {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Default)]
struct Mouse {
    x: i32,
    y: i32,
    keys: HashMap<MouseKey, bool>,
}

#[derive(Clone)]
pub struct ExternalClient {
    pub browser: Arc<WebClient>,
    pub object_ids: Vec<i32>,
    pub texture: String,
    pub scale: i32,
    pub origin_texture: *mut RwTexture,
    pub origin_surface_props: RwSurfaceProperties,
    pub audio_settings: BrowserAudioSettings,
    pub prev_replacement: *mut RwTexture,
}

pub struct Manager {
    clients: HashMap<u32, Arc<WebClient>>,
    audio: Arc<Audio>,
    ready_callbacks: HashMap<u32, Vec<BrowserReadyCallback>>,
    clients_on_txd: Vec<ExternalClient>,
    focused: Option<u32>,
    focused_queue: VecDeque<u32>,
    input_corrupted: bool,
    do_not_draw: bool,
    event_tx: Sender<Event>,
    mouse: Mouse,
    view_width: usize,
    view_height: usize,
    prev_fps: u64,
    /// Frame rate last applied to each browser. `BrowserHost::GetWindowlessFrameRate`
    /// is UI-thread only (returns 0 elsewhere), so we track it ourselves.
    applied_fps: HashMap<u32, i32>,
    /// Latest mouse move not yet sent: (browser id, x, y, modifiers). Moves are coalesced to
    /// one per game frame; high-rate mice otherwise flood the page with events (each one
    /// runs JS such as drag-scroll handlers) and the UI drops frames.
    pending_mouse_move: Option<(u32, i32, i32, u32)>,
    frame_pacer: FramePacer,
    cef_running: bool,
}

impl Manager {
    pub fn new(event_tx: Sender<Event>, audio: Arc<Audio>) -> Manager {
        // init cef
        // crate::browser::cef::initialize(event_tx.clone());

        let mut keys = HashMap::new();

        keys.insert(MouseKey::Left, false);
        keys.insert(MouseKey::Middle, false);
        keys.insert(MouseKey::Right, false);

        let mouse = Mouse { x: 0, y: 0, keys };

        Manager {
            clients: HashMap::new(),
            ready_callbacks: HashMap::new(),
            clients_on_txd: Vec::new(),
            view_height: 0,
            view_width: 0,
            prev_fps: 60,
            applied_fps: HashMap::new(),
            pending_mouse_move: None,
            frame_pacer: FramePacer::new(),
            input_corrupted: false,
            do_not_draw: false,
            cef_running: false,
            focused: None,
            focused_queue: VecDeque::new(),
            audio,
            mouse,
            event_tx,
        }
    }

    pub fn create_browser(&mut self, id: u32, cbs: CallbackList, url: &str) {
        let render_mode = crate::utils::current_render_mode();
        log::trace!(
            "manager::create_browser({}, {:?}) render_mode: {:?}",
            id,
            url,
            render_mode
        );

        self.close_browser(id, true);

        let client = WebClient::new(id, cbs, self.event_tx.clone());

        log::trace!("crate::browser::cef::create_browser");

        crate::browser::cef::create_browser(client.clone(), url);
        self.append_client(id, client);
    }

    pub fn create_browser_on_texture(&mut self, ext: &ExternalBrowser, cbs: CallbackList) {
        log::trace!("manager::create_browser_on_texture({:?})", ext);

        self.close_browser(ext.id, true);

        let client = WebClient::new_extern(ext.id, cbs, self.event_tx.clone(), self.audio.clone());

        log::trace!("crate::browser::cef::create_browser");

        crate::browser::cef::create_browser(client.clone(), &ext.url);
        self.append_client(ext.id, client.clone());

        let ext_client = ExternalClient {
            browser: client,
            texture: ext.texture.clone(),
            object_ids: Vec::new(),
            scale: ext.scale,
            origin_texture: std::ptr::null_mut(),
            origin_surface_props: RwSurfaceProperties {
                ambient: 0.0,
                diffuse: 0.0,
                specular: 0.0,
            },
            audio_settings: BrowserAudioSettings {
                max_distance: crate::audio::MAX_DISTANCE,
                reference_distance: crate::audio::REFRENCE_DISTANCE,
            },
            prev_replacement: std::ptr::null_mut(),
        };

        self.clients_on_txd.push(ext_client);
    }

    #[inline]
    pub fn browser_append_to_object(&mut self, id: u32, object_id: i32) {
        self.audio.add_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                cl.object_ids.push(object_id);
                cl.browser.add_object(object_id);

                if cl.object_ids.len() == 1 {
                    cl.browser.hide(false);
                }
            });
    }

    #[inline]
    pub fn browser_remove_from_object(&mut self, id: u32, object_id: i32) {
        self.audio.remove_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                let mut idx = 0;

                while idx < cl.object_ids.len() {
                    if cl.object_ids[idx] == object_id {
                        cl.object_ids.remove(idx);
                        cl.browser.remove_object(object_id);
                    } else {
                        idx += 1;
                    }
                }

                if cl.object_ids.is_empty() {
                    cl.browser.hide(true);
                }
            });
    }

    #[inline]
    fn append_client(&mut self, id: u32, client: Arc<WebClient>) {
        if let Some(client) = self.clients.insert(id, client) {
            self.internal_close(client, true);
        }
    }

    #[inline]
    pub fn draw(&mut self) {
        crate::browser::stats::game_frame();

        // Follow the game window size (fullscreen <-> windowed, window resize). Nothing else
        // called `resize`, so after switching to a window the page kept its fullscreen size,
        // was drawn stretched and mouse coordinates no longer matched it.
        let [width, height] = crate::utils::client_rect();
        if width > 0 && height > 0 && (width != self.view_width || height != self.view_height) {
            self.resize(width, height);
        }

        self.flush_mouse_move();

        if crate::browser::host::external_frames() && self.frame_pacer.tick() {
            for client in self.clients.values().filter(|c| c.is_created() && !c.is_hidden()) {
                client.send_begin_frame();
            }
        }

        for client in self.clients.values() {
            client.update_view();
        }

        if self.do_not_draw {
            return;
        }

        if let Some(&focus) = self.focused.as_ref() {
            for client in self.clients.values().filter(|client| client.id() != focus) {
                client.draw();
            }

            if let Some(focused) = self.clients.get(&focus) {
                focused.draw();
            }
        } else {
            for client in self.clients.values() {
                client.draw();
            }
        }
    }

    #[inline]
    pub fn on_lost_device(&mut self) {
        log::trace!("manager::on_lost_device");

        for browser in self.clients.values() {
            browser.on_lost_device();

            if browser.is_extern() {
                self.clients_on_txd
                    .iter_mut()
                    .filter(|client| client.browser.id() == browser.id())
                    .for_each(|client| client.origin_texture = std::ptr::null_mut());
            }
        }
    }

    #[inline]
    pub fn on_reset_device(&self) {
        log::trace!("manager::on_reset_device");

        for client in self.clients.values() {
            client.on_reset_device();
        }
    }

    #[inline]
    pub fn resize(&mut self, width: usize, height: usize) {
        log::trace!(
            "manager::resize(width: {}, height: {}) current values: {} {}",
            width,
            height,
            self.view_width,
            self.view_height
        );

        if width == self.view_width && height == self.view_height {
            return;
        }

        self.view_width = width;
        self.view_height = height;

        for browser in self.clients.values() {
            if !browser.is_extern() {
                browser.resize(width, height);
            }
        }
    }

    #[inline]
    pub fn send_mouse_move_event(&mut self, x: i32, y: i32) {
        if self.input_corrupted {
            return;
        }

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            self.mouse.x = x;
            self.mouse.y = y;

            let keys = &self.mouse.keys;
            let mut modifiers = 0;

            if keys.get(&MouseKey::Left).cloned().unwrap_or(false) {
                modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON as u32;
            }

            if keys.get(&MouseKey::Middle).cloned().unwrap_or(false) {
                modifiers |= cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON as u32;
            }

            if keys.get(&MouseKey::Right).cloned().unwrap_or(false) {
                modifiers |= cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON as u32;
            }

            self.pending_mouse_move = Some((client.id(), x, y, modifiers));
        }
    }

    /// Sends the coalesced mouse move, if any. Called once per frame and before clicks and
    /// wheel events so their order relative to the move is preserved.
    fn flush_mouse_move(&mut self) {
        if let Some((id, x, y, modifiers)) = self.pending_mouse_move.take()
            && let Some(client) = self.clients.get(&id)
        {
            client.send_mouse_move(x, y, modifiers);
        }
    }

    #[inline]
    pub fn send_mouse_click_event(&mut self, button: MouseKey, is_down: bool) {
        if self.input_corrupted {
            return;
        }

        self.flush_mouse_move();

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            self.mouse.keys.insert(button, is_down);

            let key = match button {
                MouseKey::Left => cef_ipc::proto::MOUSE_LEFT,
                MouseKey::Middle => cef_ipc::proto::MOUSE_MIDDLE,
                MouseKey::Right => cef_ipc::proto::MOUSE_RIGHT,
            };

            client.send_mouse_click(self.mouse.x, self.mouse.y, key, is_down);
        }
    }

    #[inline]
    pub fn send_mouse_wheel(&mut self, delta: i32) {
        if self.input_corrupted {
            return;
        }

        self.flush_mouse_move();

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            client.send_mouse_wheel(self.mouse.x, self.mouse.y, delta);
        }
    }

    #[inline]
    pub fn send_keyboard_event(&self, event: cef_key_event_t) {
        if self.input_corrupted {
            return;
        }

        // отправлять события клавиш ТОЛЬКО сфокусированному браузеру. иначе можно и другим запросившим
        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            client.send_key(&event);
        } else {
            for client in self.clients.values().filter(|client| client.always_listen_keys()) {
                client.send_key(&event);
            }
        }
    }

    pub fn trigger_event(&self, event_name: &str, list: List) {
        let args = crate::browser::host::list_to_value(&list);

        for client in self.clients.values().filter(|client| client.is_created()) {
            client.trigger_event(event_name, &args);
        }
    }

    pub fn close_browser(&mut self, id: u32, force_close: bool) {
        if let Some(client) = self.clients.remove(&id) {
            self.internal_close(client, force_close);
        }

        self.cleanup_focus_after_close(id);
    }

    #[inline]
    pub fn hide_browser(&self, id: u32, hide: bool) {
        if let Some(browser) = self.clients.get(&id) {
            browser.hide(hide);
        }
    }

    pub fn browser_focus(&mut self, id: u32, focus: bool) {
        if self.clients.contains_key(&id) {
            if focus {
                if let Some(&cur_id) = self.focused.as_ref() {
                    if cur_id != id {
                        self.focused_queue.push_back(id);
                    }
                } else {
                    self.focused = Some(id);
                }
            } else if self
                .focused
                .as_ref()
                .filter(|focused| **focused == id)
                .is_some()
            {
                self.focused = self.focused_queue.pop_front();
            } else {
                self.focused_queue
                    .iter()
                    .position(|&queue| queue == id)
                    .map(|idx| self.focused_queue.remove(idx));
            }
        }
    }

    #[inline(always)]
    pub fn is_input_blocked(&self) -> bool {
        self.focused.is_some()
    }

    #[inline(always)]
    pub fn is_input_available(&self, browser: u32) -> bool {
        if self.input_corrupted {
            return false;
        }

        if self.is_input_blocked() {
            self.focused.as_ref().filter(|&&id| id == browser).is_some()
        } else {
            true
        }
    }

    #[inline(always)]
    pub fn set_corrupted(&mut self, corrupted: bool) {
        self.input_corrupted = corrupted;
    }

    #[inline(always)]
    pub fn is_input_corrupted(&self) -> bool {
        self.input_corrupted
    }

    #[inline(always)]
    pub fn do_not_draw(&mut self, donot: bool) {
        if self.do_not_draw != donot {
            self.do_not_draw = donot;
            self.temporary_hide(donot);
        }
    }

    #[inline(always)]
    pub fn browser_exists(&self, browser_id: u32) -> bool {
        self.clients.contains_key(&browser_id)
    }

    #[inline(always)]
    pub fn browser_ready(&self, browser_id: u32) -> bool {
        self.clients
            .get(&browser_id)
            .map(|client| client.is_ready())
            .unwrap_or(false)
    }

    pub fn toggle_dev_tools(&self, browser_id: u32, enabled: bool) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.toggle_dev_tools(enabled)
        }
    }

    pub fn always_listen_keys(&self, browser_id: u32, listen: bool) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.set_always_listen_keys(listen)
        }
    }

    pub fn set_audio_settings(&mut self, browser_id: u32, audio_settings: BrowserAudioSettings) {
        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == browser_id)
            .for_each(|cl| {
                cl.audio_settings = audio_settings;
            });
    }

    pub fn load_url(&self, browser_id: u32, url: &str) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.load_url(url)
        }
    }

    pub fn call_browser_ready(&self, browser_id: u32) {
        if let Some(callbacks) = self.ready_callbacks.get(&browser_id) {
            callbacks.iter().for_each(|cb| cb(browser_id))
        }
    }

    pub fn add_browser_ready(&mut self, browser_id: u32, callback: BrowserReadyCallback) {
        if self.browser_ready(browser_id) {
            callback(browser_id);
            return;
        }

        self.ready_callbacks
            .entry(browser_id)
            .or_default()
            .push(callback);
    }

    #[inline(always)]
    pub fn external_browsers(&mut self) -> &mut [ExternalClient] {
        &mut self.clients_on_txd
    }

    #[inline(always)]
    pub fn update_fps(&mut self, fps: u64) {
        // CEF can't present faster than the game uploads the texture, but never drop the
        // UI below 30 and never above CEF's own 60 fps cap. Small jitter is ignored so we
        // don't hammer `set_windowless_frame_rate` every second.
        let target = fps.clamp(MIN_BROWSER_FPS, MAX_BROWSER_FPS);

        if self.prev_fps.abs_diff(target) >= 5 || target == MAX_BROWSER_FPS {
            self.prev_fps = target;
        }

        let target = self.prev_fps as i32;
        let applied = &mut self.applied_fps;

        applied.retain(|id, _| self.clients.contains_key(id));

        for (&id, client) in self.clients.iter() {
            if applied.get(&id) == Some(&target) {
                continue;
            }

            if client.is_created() {
                client.set_frame_rate(target);
                applied.insert(id, target);
            }
        }
    }

    pub fn close_all_browsers(&mut self) {
        self.clients_on_txd.clear();
        let audio = self.audio.clone();

        self.clients
            .drain()
            .for_each(|(_, client)| Self::internal_close_client(client, &audio, true));

        self.focused = None;
        self.focused_queue.clear();
    }

    pub fn initialize_cef(&mut self) {
        log::trace!("manager::intialize_cef() cef_running: {}", self.cef_running);

        if self.cef_running {
            return;
        }

        log::trace!("PRE cef::initalize()");
        crate::browser::cef::initialize(self.event_tx.clone());
        log::trace!("POST cef::initalize()");

        self.cef_running = true;
    }

    pub fn shutdown_cef(&mut self) {
        log::trace!("manager::shutdown_cef() cef_running: {}", self.cef_running);

        if !self.cef_running {
            return;
        }

        crate::browser::cef::shutdown();
        self.cef_running = false;
    }

    pub fn remove_views(&mut self) {
        log::trace!("remove views");

        for client in self.clients.values() {
            client.remove_view();
        }

        log::trace!("remove views done");
    }

    #[inline]
    fn temporary_hide(&self, hide: bool) {
        for client in self.clients.values() {
            if hide {
                client.internal_hide(true, true);
            } else {
                client.restore_hide_status();
            }
        }
    }

    fn internal_close(&mut self, client: Arc<WebClient>, force_close: bool) {
        if client.is_extern()
            && let Some(idx) = self
                .clients_on_txd
                .iter()
                .position(|cl| cl.browser.id() == client.id())
        {
            self.clients_on_txd.remove(idx);
        }

        Self::internal_close_client(client, &self.audio, force_close);
    }

    fn internal_close_client(client: Arc<WebClient>, audio: &Arc<Audio>, force_close: bool) {
        log::trace!("internal_close_client");

        client.close(force_close);

        log::trace!("remove_view");

        client.remove_view();

        log::trace!("remove_all_streams");

        audio.remove_all_streams(client.id());

        log::trace!("internal_close_client end");
    }

    fn cleanup_focus_after_close(&mut self, closed_id: u32) {
        self.focused_queue
            .retain(|&id| id != closed_id && self.clients.contains_key(&id));

        let focused_stale = self
            .focused
            .as_ref()
            .filter(|id| !self.clients.contains_key(id))
            .is_some();

        if self.focused == Some(closed_id) || focused_stale {
            self.focused = None;

            while let Some(next_id) = self.focused_queue.pop_front() {
                if self.clients.contains_key(&next_id) {
                    self.focused = Some(next_id);
                    break;
                }
            }
        }

        if self.clients.is_empty() {
            self.focused = None;
            self.focused_queue.clear();
        }
    }
}
