use std::ffi::c_void;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::browser::manager::{ExternalClient, Manager};
use crate::static_cell::StaticCell;

use client_api::gta::entity::CEntity;
use client_api::gta::menu_manager::CMenuManager;
use client_api::gta::rw::{self, rpworld::*, rwplcore::*};
use client_api::samp::objects::Object;

use retour::GenericDetour;

static RENDER: StaticCell<Render> = StaticCell::new();

const REFERENCE_FRAMES: u64 = 10;

const DRAWING_EVENT: usize = 0x58FAE0;
const SHUTDOWN_RW_EVENT: usize = 0x53BB80;
/// CGame::Process: runs once per frame before the scene camera starts updating, so offscreen
/// RenderWare rendering (model snapshots) can use its own camera there.
const GAME_PROCESS: usize = 0x53BEE0;

type DrawingEventFn = extern "C" fn();
type ShutdownRwEventFn = extern "C" fn();
type GameProcessFn = extern "C" fn();

struct FrameCounter {
    start_at: Instant,
    frames: u64,
    last_fps: u64,
}

struct Render {
    manager: Arc<Mutex<Manager>>,
    centity_render: GenericDetour<extern "thiscall" fn(obj: *mut CEntity)>,
    drawing_event: GenericDetour<DrawingEventFn>,
    shutdown_event: GenericDetour<ShutdownRwEventFn>,
    game_process: GenericDetour<GameProcessFn>,
    counter: FrameCounter,
}

impl Render {
    fn get<'a>() -> Option<&'a mut Render> {
        unsafe { RENDER.get_mut() }
    }

    fn calc_frames(&mut self) -> Option<u64> {
        let counter = &mut self.counter;

        counter.frames += 1;

        if counter.frames == REFERENCE_FRAMES {
            let elapsed = counter.start_at.elapsed().as_millis() as u64;

            let fps = (REFERENCE_FRAMES * 1000) / elapsed;

            counter.last_fps = fps;
            counter.frames = 0;
            counter.start_at = Instant::now();

            return Some(fps);
        }

        None
    }
}

pub fn initialize(manager: Arc<Mutex<Manager>>) {
    log::trace!("hooking CEntity::render");

    let centity_render = unsafe {
        let render_func: extern "thiscall" fn(*mut CEntity) = std::mem::transmute(0x00534310);
        let centity_render = GenericDetour::new(render_func, centity_render).unwrap();

        centity_render.enable().unwrap();
        centity_render
    };

    let drawing_event = unsafe {
        let func: DrawingEventFn = std::mem::transmute(DRAWING_EVENT);
        let hook = GenericDetour::new(func, drawing_event).unwrap();

        hook.enable().unwrap();
        hook
    };

    let shutdown_event = unsafe {
        let func: ShutdownRwEventFn = std::mem::transmute(SHUTDOWN_RW_EVENT);
        let hook = GenericDetour::new(func, shutdown_event).unwrap();

        hook.enable().unwrap();
        hook
    };

    let game_process = unsafe {
        let func: GameProcessFn = std::mem::transmute(GAME_PROCESS);
        let hook = GenericDetour::new(func, game_process).unwrap();

        hook.enable().unwrap();
        hook
    };

    log::trace!("hooking ok ...");

    let counter = FrameCounter {
        start_at: Instant::now(),
        frames: 0,
        last_fps: 0,
    };

    let render = Render {
        manager,
        centity_render,
        drawing_event,
        shutdown_event,
        game_process,
        counter,
    };

    unsafe {
        RENDER.set(render);
    }
}

pub fn uninitialize() {
    unsafe {
        RENDER.take();
    }
}

fn on_render() {
    crate::app::mainloop();

    // Browsers go under SA:MP's own interface (chat, dialogs, scoreboard) and under overlays
    // that other plugins draw in Present (e.g. ORP's voice chat): they are drawn here, with
    // the game's HUD. `cef/draw_over_samp` restores the old order (from the SA:MP death window
    // hook, on top of SA:MP's interface).
    if !draw_over_samp() {
        render();
        draw_samp_chat();
    }
}

// ---- SA:MP chat with the HUD (0.3.7-R3) ------------------------------------------------------
//
// SA:MP draws its chat late (with the death window and the chat input), after textdraws, so
// the chat covered textdraw interfaces. With the browsers drawn at the HUD stage the chat is
// drawn right after them instead: over the browsers, under textdraws.

const CHAT_DRAW_R3: usize = 0x67680; // CChat::Draw
const CHAT_PTR_R3: usize = 0x26E8C8; // CChat*

type ChatDrawFn = extern "thiscall" fn(chat: *mut c_void);

static CHAT_DRAW: StaticCell<GenericDetour<ChatDrawFn>> = StaticCell::new();
static CHAT_FROM_HUD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called once SA:MP is loaded.
pub fn hook_samp_chat() {
    if draw_over_samp()
        || client_api::samp::version::version() != client_api::samp::version::Version::V037R3
        || unsafe { CHAT_DRAW.get_mut() }.is_some()
    {
        return;
    }

    let base = client_api::samp::handle();
    if base.is_null() {
        return;
    }

    unsafe {
        let func: ChatDrawFn = std::mem::transmute(base.add(CHAT_DRAW_R3));
        match GenericDetour::new(func, chat_draw) {
            Ok(hook) if hook.enable().is_ok() => {
                CHAT_DRAW.set(hook);
                log::info!("SA:MP chat drawn with the HUD");
            }
            _ => log::warn!("couldn't hook SA:MP chat drawing"),
        }
    }
}

extern "thiscall" fn chat_draw(chat: *mut c_void) {
    // SA:MP's own (late) call is skipped; ours from the HUD stage goes through.
    if CHAT_FROM_HUD.load(std::sync::atomic::Ordering::Relaxed)
        && let Some(hook) = unsafe { CHAT_DRAW.get_mut() }
    {
        hook.call(chat);
    }
}

fn draw_samp_chat() {
    let Some(hook) = (unsafe { CHAT_DRAW.get_mut() }) else {
        return;
    };

    unsafe {
        let chat = *(client_api::samp::handle().add(CHAT_PTR_R3) as *const *mut c_void);
        if chat.is_null() {
            return;
        }

        CHAT_FROM_HUD.store(true, std::sync::atomic::Ordering::Relaxed);
        hook.call(chat);
        CHAT_FROM_HUD.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `cef/draw_over_samp`: draw browsers on top of SA:MP's interface, as before.
pub fn draw_over_samp() -> bool {
    static OVER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OVER.get_or_init(|| {
        let over = crate::utils::cef_dir().join("draw_over_samp").exists();
        log::info!("browsers drawn {}", if over { "over SA:MP interface" } else { "with the game HUD" });
        over
    })
}

pub fn render() {
    if let Some(render) = Render::get() {
        let fps = render.calc_frames();

        {
            let mut manager = render.manager.lock();

            if let Some(&fps) = fps.as_ref() {
                manager.update_fps(fps);
            }

            manager.do_not_draw(CMenuManager::is_menu_active());
            manager.draw();
        }
    }
}

fn on_destroy() {
    log::trace!("rwshutdown ...");

    if let Some(render) = Render::get() {
        let mut manager = render.manager.lock();
        manager.remove_views();
    }
}

struct RenderState {
    client: *mut ExternalClient,
    before: bool,
}

extern "C" fn drawing_event() {
    if let Some(render) = Render::get() {
        render.drawing_event.call();
    }

    on_render();
}

extern "C" fn game_process() {
    if let Some(render) = Render::get() {
        render.game_process.call();
    }

    // Between frames: the scene camera isn't updating, snapshots can render offscreen.
    crate::snapshot::process_frame();
}

extern "C" fn shutdown_event() {
    on_destroy();

    if let Some(render) = Render::get() {
        render.shutdown_event.call();
    }
}

extern "thiscall" fn centity_render(obj: *mut CEntity) {
    if let Some(render) = Render::get() {
        let mut manager = render.manager.lock();
        let _entity = unsafe { &mut *obj };

        let browsers = manager.external_browsers();

        for browser in browsers {
            let browser_ptr = browser as *mut _; // должно быть safe
            for &object_id in &browser.object_ids {
                if let Some(object) = Object::get(object_id)
                    && let Some(obj_entity) = object.entity()
                    && obj == obj_entity as *mut _ as *mut CEntity
                {
                    let rwobject = obj_entity._base._base.rw_entity as *mut RwObject;

                    if !rwobject.is_null() {
                        let mut before_state = RenderState {
                            client: browser_ptr,
                            before: true,
                        };

                        let before_ptr = &mut before_state as *mut _ as *mut c_void;

                        replace_texture(rwobject, before_ptr);

                        render.centity_render.call(obj);

                        let mut after_state = RenderState {
                            client: browser_ptr,
                            before: false,
                        };

                        let after_ptr = &mut after_state as *mut _ as *mut c_void;

                        replace_texture(rwobject, after_ptr);

                        return;
                    }
                }
            }
        }

        render.centity_render.call(obj);
    }
}

fn replace_texture(rwobject: *mut RwObject, render_state: *mut c_void) {
    unsafe {
        if (*rwobject).obj_type == rpCLUMP {
            rw::rpclump_for_all_atomics(rwobject as *mut _, Some(atomic_callback), render_state);
        } else {
            atomic_callback(rwobject as *mut _, render_state);
        }
    }
}

extern "C" fn atomic_callback(atomic: *mut RpAtomic, data: *mut c_void) -> *mut RpAtomic {
    unsafe {
        if !atomic.is_null() && !(*atomic).geometry.is_null() {
            let render = &mut *(data as *mut RenderState);

            if render.before {
                before_entity_render(
                    (*(*atomic).geometry).matList.as_mut_slice(),
                    &mut *render.client,
                );
            } else {
                after_entity_render(
                    (*(*atomic).geometry).matList.as_mut_slice(),
                    &mut *render.client,
                );
            }
        }
    }

    atomic
}

unsafe fn before_entity_render(materials: &mut [*mut RpMaterial], client: &mut ExternalClient) {
    for material in materials {
        let material = *material;
        unsafe {
            if material.is_null() {
                continue;
            }

            let texture = (*material).texture;

            if texture.is_null() {
                continue;
            }

            if !(*texture).name().contains(&client.texture) {
                continue;
            }

            let mut view = client.browser.view.lock();

            if view.rwtexture().is_none() && !(*texture).raster.is_null() {
                let raster = &mut *(*texture).raster;
                let width = (raster.width * client.scale) as usize;
                let height = (raster.height * client.scale) as usize;

                view.make_active();

                drop(view);

                client.browser.resize(width, height);
                client.browser.restore_hide_status();

                view = client.browser.view.lock();
            }

            if let Some(replace) = view.rwtexture() {
                client.origin_surface_props = (*material).surface_props.clone();

                (*material).surface_props.ambient = 16.0;
                (*material).surface_props.diffuse = 0.0;
                (*material).surface_props.specular = 0.0;

                client.origin_texture = (*material).texture;
                client.prev_replacement = replace.as_ptr();
                (*material).texture = replace.as_ptr();

                break; // replaced. do not replace another
            }
        }
    }
}

unsafe fn after_entity_render(materials: &mut [*mut RpMaterial], client: &mut ExternalClient) {
    for material in materials {
        let material = *material;
        unsafe {
            if material.is_null() {
                continue;
            }

            let texture = (*material).texture;

            if texture.is_null() || texture != client.prev_replacement {
                continue;
            }

            (*material).texture = client.origin_texture;
            (*material).surface_props = client.origin_surface_props.clone();

            break; //
        }
    }
}
