//! Entity snapshots: renders of game models for the UI (`sampcef://snapshot/<spec>`).
//!
//! Spec format (from the server):
//!   `es_<model>_<rotX>_<rotY>_<rotZ>_<offsetX>_<offsetY>_<offsetZ>[_<color1>_<color2>_<vinyl>]`
//! * prefix `es` (or legacy `veh`/`obj`/`skin`) is ignored, the type comes from the model;
//! * rotX/Y/Z: like TextDrawSetPreviewRot; offsetY: zoom;
//! * offsetX != 0: special vehicle framing, offsetZ: its height offset (not supported yet);
//! * color1/color2/vinyl: vehicle colours and ORP vinyl.
//!
//! Requests come from the browser host thread; rendering happens on the game thread in the
//! CGame::Process hook, between frames, using RenderWare with GTA SA 1.0 US addresses
//! (from plugin-sdk). To keep frames smooth the game thread only draws (one model per frame,
//! into a render target that is reused) and reads the pixels back one frame later, when the
//! GPU is done with them; brightness, cropping and PNG encoding run on a worker thread.
//! Objects and vehicles (wheels, colours, ORP vinyls) are rendered; peds
//! answer "no image" so the page falls back to its prerendered picture.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use cef_ipc::Command;
use client_api::gta::rw::rwcore::RwRaster;

const SIZE: i32 = 768;
const CACHE_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub model: i32,
    pub rot: [f32; 3],
    pub offset: [f32; 3],
    pub color1: Option<i32>,
    pub color2: Option<i32>,
    pub vinyl: Option<i32>,
}

impl Spec {
    pub fn parse(spec: &str) -> Option<Spec> {
        let parts: Vec<&str> = spec.split('_').collect();
        if parts.len() < 8 {
            return None;
        }

        let model: i32 = parts[1].parse().ok().filter(|m| (0..20000).contains(m))?;
        let f = |i: usize| parts[i].parse::<f32>().ok().filter(|v| v.is_finite());
        let i = |i: usize| parts.get(i).and_then(|p| p.parse::<i32>().ok());

        Some(Spec {
            model,
            rot: [f(2)?, f(3)?, f(4)?],
            offset: [f(5)?, f(6)?, f(7)?],
            color1: i(8),
            color2: i(9),
            vinyl: i(10),
        })
    }

    #[cfg(test)]
    fn zoom(&self) -> f32 {
        if self.offset[1] > 0.01 { self.offset[1] } else { 1.0 }
    }
}

/// Pending requests: (request id, spec, first seen). Requests whose model is still streaming
/// in go back to the end of the queue until `LOAD_TIMEOUT`.
static QUEUE: Mutex<VecDeque<(u32, String, std::time::Instant)>> = Mutex::new(VecDeque::new());

const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `cef/no_snapshots` turns in-game rendering off (pages fall back to prerendered images).
fn disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        // Only tested with 0.3.7-R3; on other SA:MP builds the UI falls back to CDN renders.
        let version = client_api::samp::version::version();
        let supported = version == client_api::samp::version::Version::V037R3;
        let off = !supported || crate::utils::cef_dir().join("no_snapshots").exists();
        log::info!(
            "snapshots: {} (samp {:?})",
            if off { "disabled" } else { "enabled" },
            version
        );
        off
    })
}

/// `cef/snapshot_debug`: log every rendering step and vehicle part (for hangs/crashes and
/// wrong parts). Off by default: writing the log on the game thread costs frame time.
fn debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| crate::utils::cef_dir().join("snapshot_debug").exists())
}

/// Logged before each risky step, so a hang/crash log shows where it stopped.
fn step(model: i32, what: &str) {
    if debug() {
        log::info!("snapshot {}: {}", model, what);
    }
}

enum Outcome {
    /// The model is in the render target; read it back on the next frame.
    Drawn,
    /// Nothing to show (unknown model, ped, ...).
    Failed,
    /// The model (or its wheels) was requested from the streamer; retry on a later frame.
    Loading,
}

/// Specs being rendered (drawn, read back or encoded) and the requests waiting for them.
static IN_FLIGHT: Mutex<Option<HashMap<String, Vec<u32>>>> = Mutex::new(None);

thread_local! {
    /// Drawn on the previous frame, not read back yet: (spec, time spent drawing).
    static PENDING: Cell<Option<(String, Duration)>> = const { Cell::new(None) };
}
static CACHE: Mutex<Option<HashMap<String, Arc<Vec<u8>>>>> = Mutex::new(None);

/// From the IPC thread: answer from cache or queue for the game thread.
pub fn request(req: u32, spec: String) {
    match cached(&spec) {
        Some(png) => crate::browser::host::send(Command::SnapshotResult { req, png }),
        None if disabled() => crate::browser::host::send(Command::SnapshotResult { req, png: Vec::new() }),
        None => QUEUE.lock().push_back((req, spec, std::time::Instant::now())),
    }
}

/// Game thread, once per frame, from the CGame::Process hook (outside the scene camera update).
pub fn process_frame() {
    // Last frame's drawing has reached the GPU by now, so reading it back doesn't stall.
    if let Some((spec, draw)) = PENDING.with(|p| p.take()) {
        let started = Instant::now();
        let pixels = unsafe { target().and_then(|t| read_pixels(t.raster)) };
        finish(spec, pixels, draw, started.elapsed());
    }

    // Draw the next model. Cache hits, duplicates and models that are still streaming don't
    // count, so look through the queue once at most.
    let queued = QUEUE.lock().len();
    for _ in 0..queued {
        let Some((req, spec, since)) = QUEUE.lock().pop_front() else {
            break;
        };

        if let Some(png) = cached(&spec) {
            crate::browser::host::send(Command::SnapshotResult { req, png });
            continue;
        }

        {
            let mut in_flight = IN_FLIGHT.lock();
            let in_flight = in_flight.get_or_insert_with(HashMap::new);
            if let Some(waiting) = in_flight.get_mut(&spec) {
                waiting.push(req);
                continue;
            }
        }

        let Some(parsed) = Spec::parse(&spec) else {
            start(&spec, req);
            finish(spec, None, Duration::ZERO, Duration::ZERO);
            continue;
        };

        let started = Instant::now();
        match unsafe { render(&parsed) } {
            Outcome::Loading if since.elapsed() < LOAD_TIMEOUT => {
                QUEUE.lock().push_back((req, spec, since));
            }
            Outcome::Loading => {
                log::warn!(
                    "snapshot {:?}: model didn't stream in within {:?} ({})",
                    spec,
                    LOAD_TIMEOUT,
                    unsafe { load_state(parsed.model) }
                );
                start(&spec, req);
                finish(spec, None, Duration::ZERO, Duration::ZERO);
            }
            Outcome::Failed => {
                start(&spec, req);
                finish(spec, None, started.elapsed(), Duration::ZERO);
            }
            Outcome::Drawn => {
                start(&spec, req);
                PENDING.with(|p| p.set(Some((spec, started.elapsed()))));
                break; // one model per frame
            }
        }
    }

    release_models();
}

fn cached(spec: &str) -> Option<Vec<u8>> {
    CACHE.lock().as_ref().and_then(|c| c.get(spec)).map(|png| (**png).clone())
}

fn start(spec: &str, req: u32) {
    IN_FLIGHT
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(spec.to_string(), vec![req]);
}

type Job = (String, Option<Vec<u8>>, Duration, Duration);

/// Hands the read-back pixels to the worker thread, which answers every waiting request.
fn finish(spec: String, pixels: Option<Vec<u8>>, draw: Duration, read: Duration) {
    static WORKER: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = std::sync::OnceLock::new();

    let sender = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("snapshot encoder".into())
            .spawn(move || {
                for job in rx {
                    encode(job);
                }
            })
            .expect("snapshot encoder thread");
        Mutex::new(tx)
    });

    let job = (spec, pixels, draw, read);
    if let Err(std::sync::mpsc::SendError(job)) = sender.lock().send(job) {
        encode(job); // worker gone (shouldn't happen): do it here
    }
}

fn encode((spec, pixels, draw, read): Job) {
    let started = Instant::now();
    let png = pixels
        .and_then(to_rgba)
        .and_then(|rgba| {
            let rgba = auto_exposure(rgba);
            let (w, h, rgba) = crop_to_content(&rgba, SIZE as u32);
            encode_png(w, h, &rgba)
        })
        .unwrap_or_default();

    log::info!(
        "snapshot {:?}: {} bytes (game thread: draw {:?}, read {:?}; encode {:?})",
        spec,
        png.len(),
        draw,
        read,
        started.elapsed()
    );

    // Empty results are cached too, so a model we can't render isn't retried every time.
    // Cache first, then leave IN_FLIGHT: a request arriving in between finds one of them.
    {
        let mut cache = CACHE.lock();
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(spec.clone(), Arc::new(png.clone()));
    }

    let waiting = IN_FLIGHT.lock().as_mut().and_then(|f| f.remove(&spec)).unwrap_or_default();
    for req in waiting {
        crate::browser::host::send(Command::SnapshotResult { req, png: png.clone() });
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

// ---- GTA SA 1.0 US / RenderWare -------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct RwV3d {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
struct RwV2d {
    x: f32,
    y: f32,
}

#[repr(C)]
struct RwRgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

#[repr(C)]
struct RwRgbaReal {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[repr(C)]
struct RwMatrix {
    right: RwV3d,
    flags: u32,
    up: RwV3d,
    pad1: u32,
    at: RwV3d,
    pad2: u32,
    pos: RwV3d,
    pad3: u32,
}

type P = *mut c_void;

macro_rules! game_fn {
    ($name:ident: fn($($arg:ty),*) $(-> $ret:ty)? = $addr:expr) => {
        #[allow(non_snake_case)]
        unsafe fn $name() -> extern "C" fn($($arg),*) $(-> $ret)? {
            unsafe { std::mem::transmute::<usize, extern "C" fn($($arg),*) $(-> $ret)?>($addr) }
        }
    };
}

game_fn!(RequestModel: fn(i32, i32) = 0x4087E0);
game_fn!(SetModelIsDeletable: fn(i32) = 0x409C10);

// CStreamingInfo flags: keep the model while we wait for it, and load it before others.
const STREAMING_GAME_REQUIRED: i32 = 0x2;
const STREAMING_PRIORITY_REQUEST: i32 = 0x10;

/// Models we asked the streamer to keep (snapshot model and its wheels).
static REQUESTED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

unsafe fn request_model(model: i32) {
    unsafe {
        RequestModel()(model, STREAMING_GAME_REQUIRED | STREAMING_PRIORITY_REQUEST);
    }
    let mut requested = REQUESTED.lock();
    if !requested.contains(&model) {
        requested.push(model);
    }
}

/// Lets the streamer unload what we requested once no queued snapshot needs it.
fn release_models() {
    let queued: Vec<i32> = QUEUE
        .lock()
        .iter()
        .filter_map(|(_, spec, _)| Spec::parse(spec).map(|s| s.model))
        .collect();

    // Wheels are shared; keep everything while any snapshot is still queued.
    if !queued.is_empty() {
        return;
    }

    for model in REQUESTED.lock().drain(..) {
        unsafe { SetModelIsDeletable()(model) };
    }
}

/// What is (not) loaded for `model`, for the log.
unsafe fn load_state(model: i32) -> String {
    unsafe {
        let mi = model_info(model);
        if mi.is_null() {
            return "no model info".to_string();
        }
        let kind: u8 = vcall(mi, 4);
        let wheel = if kind == MODEL_INFO_VEHICLE {
            *((mi as *const u8).add(0x48) as *const i16) as i32
        } else {
            -1
        };
        let wheel_loaded = wheel > 0 && !model_info(wheel).is_null() && !rw_object(model_info(wheel)).is_null();
        format!(
            "type {}, model loaded {}, wheel model {} loaded {}",
            kind,
            !rw_object(mi).is_null(),
            wheel,
            wheel_loaded
        )
    }
}
game_fn!(RwCameraCreate: fn() -> P = 0x7EE4F0);
game_fn!(RwCameraDestroy: fn(P) -> i32 = 0x7EE4B0);
game_fn!(RwCameraBeginUpdate: fn(P) -> P = 0x7EE190);
game_fn!(RwCameraEndUpdate: fn(P) -> P = 0x7EE180);
game_fn!(RwCameraClear: fn(P, *const RwRgba, i32) -> P = 0x7EE340);
game_fn!(RwCameraSetViewWindow: fn(P, *const RwV2d) -> P = 0x7EE410);
game_fn!(RwCameraSetNearClipPlane: fn(P, f32) -> P = 0x7EE1D0);
game_fn!(RwCameraSetFarClipPlane: fn(P, f32) -> P = 0x7EE2A0);
game_fn!(RwFrameCreate: fn() -> P = 0x7F0410);
game_fn!(RwFrameDestroy: fn(P) -> i32 = 0x7F05A0);
game_fn!(RwFrameUpdateObjects: fn(P) -> P = 0x7F0910);
game_fn!(RwFrameRotate: fn(P, *const RwV3d, f32, i32) -> P = 0x7F1010);
game_fn!(RwFrameTranslate: fn(P, *const RwV3d, i32) -> P = 0x7F0E30);
game_fn!(RwFrameSetIdentity: fn(P) -> P = 0x7F10B0);
game_fn!(RwObjectHasFrameSetFrame: fn(P, P) = 0x804EF0);
game_fn!(RwRasterCreate: fn(i32, i32, i32, i32) -> *mut RwRaster = 0x7FB230);
game_fn!(RwRasterDestroy: fn(*mut RwRaster) -> i32 = 0x7FB020);
game_fn!(RwRasterLock: fn(*mut RwRaster, u8, i32) -> *mut u8 = 0x7FB2D0);
game_fn!(RwRasterUnlock: fn(*mut RwRaster) -> *mut RwRaster = 0x7FAEC0);
game_fn!(RpClumpDestroy: fn(P) -> i32 = 0x74A310);
game_fn!(RpAtomicDestroy: fn(P) -> i32 = 0x749DC0);
game_fn!(SetAmbientColours: fn(*const RwRgbaReal) = 0x735D50);
game_fn!(ActivateDirectional: fn() = 0x735C80);
game_fn!(ReSetAmbientAndDirectionalColours: fn() = 0x735C40);
game_fn!(SetupVehicleVariables: fn(P) = 0x733160);
game_fn!(RpWorldAddClump: fn(P, P) -> P = 0x751300);
game_fn!(RpWorldAddCamera: fn(P, P) -> P = 0x750F20);
game_fn!(RpWorldRemoveCamera: fn(P, P) -> P = 0x750F50);
game_fn!(RpWorldRemoveClump: fn(P, P) -> P = 0x751460);
game_fn!(RpWorldAddAtomic: fn(P, P) -> P = 0x750F90);
game_fn!(RpWorldRemoveAtomic: fn(P, P) -> P = 0x750FC0);

const SCENE_WORLD: usize = 0xC17038; // Scene.m_pRpWorld
const RW_RASTER_FORMAT_8888: i32 = 0x0500;
game_fn!(SetEditableMaterials: fn(P) = 0x4C8430);
game_fn!(ResetEditableMaterials: fn(P) = 0x4C8460);
game_fn!(RpClumpForAllAtomics: fn(P, extern "C" fn(P, P) -> P, P) -> P = 0x749B70);
game_fn!(RpGeometryForAllMaterials: fn(P, extern "C" fn(P, P) -> P, P) -> P = 0x74C790);
game_fn!(FindTxdSlot: fn(*const u8) -> i32 = 0x731850);
game_fn!(RwTexDictionaryFindNamedTexture: fn(P, *const u8) -> P = 0x7F39F0);

/// `CVehicleModelInfo::SetVehicleColour(prim, sec, tert, quat)` (thiscall).
unsafe fn set_vehicle_colour(model_info: P, c1: u8, c2: u8) {
    unsafe {
        let f: extern "thiscall" fn(P, u8, u8, u8, u8) = std::mem::transmute(0x4C84B0usize);
        f(model_info, c1, c2, 0, 0);
    }
}

const REMAP_TEXTURE: usize = 0xB4E47C; // CVehicleModelInfo::ms_pRemapTexture (paintjob)
const TXD_POOL: usize = 0xC8800C; // CTxdStore::ms_pTxdPool

const MODEL_INFO_PTRS: usize = 0xA9B0C8; // CModelInfo::ms_modelInfoPtrs
const SCENE_CAMERA: usize = 0xC17038 + 4; // Scene.m_pRwCamera (only read for its position)

const MODEL_INFO_VEHICLE: u8 = 6;
const MODEL_INFO_PED: u8 = 7;

const RP_ATOMIC: u8 = 1;
const RP_CLUMP: u8 = 2;

const RW_RASTER_ZBUFFER: i32 = 0x01;
const RW_RASTER_CAMERA_TEXTURE: i32 = 0x05;
const RW_RASTER_LOCK_READ: i32 = 0x02;
const RW_CAMERA_CLEAR_IMAGE_Z: i32 = 0x1 | 0x2;
const RW_COMBINE_REPLACE: i32 = 0;
const RW_COMBINE_POSTCONCAT: i32 = 2;

// RwRenderState values
const RS_ZTESTENABLE: u32 = 6;
const RS_ZWRITEENABLE: u32 = 8;
const RS_VERTEXALPHAENABLE: u32 = 12;
const RS_CULLMODE: u32 = 20;
const CULL_NONE: u32 = 1;

unsafe fn model_info(model: i32) -> P {
    unsafe { *(MODEL_INFO_PTRS as *const P).add(model as usize) }
}

/// `CBaseModelInfo` virtual call by vtable index (thiscall).
unsafe fn vcall<R>(this: P, index: usize) -> R {
    unsafe {
        let vtable = *(this as *const *const usize);
        let f: extern "thiscall" fn(P) -> R = std::mem::transmute(*vtable.add(index));
        f(this)
    }
}

unsafe fn rw_object(model_info: P) -> P {
    unsafe { *((model_info as *const u8).add(0x1C) as *const P) }
}

/// Bounding sphere (centre, radius) from the model's collision, if it has one.
unsafe fn bounds(model_info: P) -> (RwV3d, f32) {
    unsafe {
        let col = *((model_info as *const u8).add(0x14) as *const *const u8);
        if col.is_null() {
            return (RwV3d { x: 0.0, y: 0.0, z: 0.0 }, 1.0);
        }

        let centre = *(col.add(0x18) as *const RwV3d);
        let radius = *(col.add(0x24) as *const f32);
        (centre, if radius > 0.01 { radius } else { 1.0 })
    }
}

unsafe fn frame_of(object: P) -> P {
    unsafe { *((object as *const u8).add(4) as *const P) }
}

unsafe fn modelling(frame: P) -> *mut RwMatrix {
    unsafe { (frame as *mut u8).add(0x10) as *mut RwMatrix }
}

/// Draws the model into the render target, or asks the streamer for it.
unsafe fn render(spec: &Spec) -> Outcome {
    unsafe {
        let mi = model_info(spec.model);
        if mi.is_null() {
            log::info!("snapshot: model {} has no model info", spec.model);
            return Outcome::Failed;
        }

        step(spec.model, "model type");
        let kind: u8 = vcall(mi, 4); // GetModelType
        if kind == MODEL_INFO_PED {
            // Peds need a pose (a skinned clump without animation renders collapsed).
            log::info!("snapshot: model {} is a ped, not supported yet", spec.model);
            return Outcome::Failed;
        }

        // Vehicles get their wheels from a separate wheel model in CreateInstance.
        let wheel_model = if kind == MODEL_INFO_VEHICLE {
            *((mi as *const u8).add(0x48) as *const i16) as i32 // m_nWheelModelIndex
        } else {
            -1
        };
        let wheel_mi = if wheel_model > 0 { model_info(wheel_model) } else { std::ptr::null_mut() };

        // Never load synchronously here (we're inside the frame's rendering): request and let
        // the game's own streaming loop bring the model in, then retry on a later frame.
        let need_wheels = !wheel_mi.is_null() && rw_object(wheel_mi).is_null();
        if rw_object(mi).is_null() || need_wheels {
            step(spec.model, "request model");
            request_model(spec.model);
            if need_wheels {
                request_model(wheel_model);
            }
            return Outcome::Loading;
        }

        step(spec.model, "create instance");
        let instance: P = vcall(mi, 11); // CreateInstance()
        if instance.is_null() {
            return Outcome::Failed;
        }

        let object_type = *(instance as *const u8);
        let model_frame = frame_of(instance);
        let drawn = if model_frame.is_null() || (object_type != RP_ATOMIC && object_type != RP_CLUMP) {
            false
        } else {
            render_instance(spec, mi, kind, instance, object_type, model_frame)
        };

        step(spec.model, "destroy instance");

        match object_type {
            RP_CLUMP => {
                RpClumpDestroy()(instance);
            }
            RP_ATOMIC => {
                RpAtomicDestroy()(instance);
                if !model_frame.is_null() {
                    RwFrameDestroy()(model_frame);
                }
            }
            _ => {}
        }

        if drawn { Outcome::Drawn } else { Outcome::Failed }
    }
}

game_fn!(AtomicDefaultRenderCallBack: fn(P) -> P = 0x7491C0);
game_fn!(GetFrameNodeName: fn(P) -> *const std::ffi::c_char = 0x72FB30);

/// CVisibilityPlugins callbacks that don't draw but queue the atomic into the game's alpha
/// list (rendered later with the main scene): glass, rotors.
const ALPHA_RENDER_CALLBACKS: [usize; 6] = [
    0x733F80, // RenderVehicleHiDetailAlphaCB
    0x734370, // RenderVehicleHiDetailAlphaCB_BigVehicle
    0x7344A0, // RenderVehicleHiDetailAlphaCB_Boat
    0x734240, // RenderTrainHiDetailAlphaCB
    0x7340B0, // RenderHeliRotorAlphaCB
    0x734170, // RenderHeliTailRotorAlphaCB
];

const RP_ATOMIC_RENDER: u8 = 0x04;

// CVisibilityPlugins::GetAtomicId returns the atomic's component flags.
game_fn!(GetAtomicId: fn(P) -> i16 = 0x732370);
const ATOMIC_IS_OK_STATE: u16 = 0x1;
const ATOMIC_IS_DAM_STATE: u16 = 0x2;

extern "C" fn collect_atomic(atomic: P, data: P) -> P {
    unsafe { (*(data as *mut Vec<P>)).push(atomic) };
    atomic
}

/// A cloned vehicle clump has its body parts hidden until CAutomobile sets them up; show the
/// intact (`_ok`) parts, keep damaged (`_dam`) and low-LOD (`_vlo`) ones hidden.
unsafe fn show_vehicle_parts(atomics: &[P]) {
    unsafe {
        for &atomic in atomics {
            let frame = frame_of(atomic);
            if frame.is_null() {
                continue;
            }

            let name = GetFrameNodeName()(frame);
            let name = if name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(name).to_string_lossy().to_ascii_lowercase()
            };

            // The game tags each component atomic when it loads the model: intact or damaged
            // state. The frame name is only a fallback for atomics it didn't tag.
            let component = GetAtomicId()(atomic) as u16;
            let flags = (atomic as *mut u8).add(2); // RwObject.flags
            let was_visible = *flags & RP_ATOMIC_RENDER != 0;

            let visible = if component & ATOMIC_IS_DAM_STATE != 0 {
                false
            } else if component & ATOMIC_IS_OK_STATE != 0 {
                true
            } else {
                !(name.contains("_dam") || name.contains("_vlo"))
            };

            if visible {
                *flags |= RP_ATOMIC_RENDER;
            } else {
                *flags &= !RP_ATOMIC_RENDER;
            }

            if debug() {
                log::info!(
                    "snapshot: atomic {:?} component flags {:#06x}, visible {} -> {}",
                    name,
                    component,
                    was_visible,
                    visible
                );
            }
        }
    }
}

/// Draws visible atomics with RenderWare's default callback (the game's visibility callbacks
/// pick LODs by distance and defer transparent parts to the main scene): opaque parts first,
/// then the ones the game treats as transparent, so glass blends over the body.
unsafe fn render_atomics(atomics: &[P]) {
    unsafe {
        for transparent_pass in [false, true] {
            for &atomic in atomics {
                if *(atomic as *const u8).add(2) & RP_ATOMIC_RENDER == 0 {
                    continue;
                }

                let callback = *((atomic as *const u8).add(0x48) as *const usize);
                if ALPHA_RENDER_CALLBACKS.contains(&callback) != transparent_pass {
                    continue;
                }

                AtomicDefaultRenderCallBack()(atomic);
            }
        }
    }
}

/// World position of the game camera. The model is placed there for the snapshot: the game's
/// visibility callbacks (vehicle LODs, fading objects) measure distance from the game camera.
unsafe fn game_camera_position() -> RwV3d {
    unsafe {
        let camera = *(SCENE_CAMERA as *const P);
        if camera.is_null() {
            return RwV3d { x: 0.0, y: 0.0, z: 0.0 };
        }
        let frame = frame_of(camera);
        if frame.is_null() {
            return RwV3d { x: 0.0, y: 0.0, z: 0.0 };
        }
        // RwFrame.ltm (+0x50), RwMatrix.pos (+0x30)
        *((frame as *const u8).add(0x50 + 0x30) as *const RwV3d)
    }
}

/// Render target and camera, created once and reused for every snapshot (creating a render
/// target texture each time costs frame time).
#[derive(Clone, Copy)]
struct Target {
    raster: *mut RwRaster,
    camera: P,
    frame: P,
}

thread_local! {
    static TARGET: Cell<Option<Target>> = const { Cell::new(None) };
}

unsafe fn target() -> Option<Target> {
    if let Some(target) = TARGET.with(|t| t.get()) {
        return Some(target);
    }

    unsafe {
        // Explicit 8888: the default camera texture format has no alpha, so the background and
        // black parts of the model couldn't be told apart.
        let raster = RwRasterCreate()(SIZE, SIZE, 32, RW_RASTER_CAMERA_TEXTURE | RW_RASTER_FORMAT_8888);
        let zraster = RwRasterCreate()(SIZE, SIZE, 0, RW_RASTER_ZBUFFER);
        let camera = RwCameraCreate()();
        let frame = RwFrameCreate()();

        if raster.is_null() || zraster.is_null() || camera.is_null() || frame.is_null() {
            log::warn!("snapshot: couldn't create the render target");
            if !camera.is_null() {
                RwCameraDestroy()(camera);
            }
            if !frame.is_null() {
                RwFrameDestroy()(frame);
            }
            if !zraster.is_null() {
                RwRasterDestroy()(zraster);
            }
            if !raster.is_null() {
                RwRasterDestroy()(raster);
            }
            return None;
        }

        RwObjectHasFrameSetFrame()(camera, frame);
        *((camera as *mut u8).add(0x60) as *mut *mut RwRaster) = raster;
        *((camera as *mut u8).add(0x64) as *mut *mut RwRaster) = zraster;

        // The camera keeps the z raster.
        let target = Target { raster, camera, frame };
        TARGET.with(|t| t.set(Some(target)));
        Some(target)
    }
}

unsafe fn render_instance(spec: &Spec, mi: P, kind: u8, instance: P, object_type: u8, model_frame: P) -> bool {
    unsafe {
        // Model: centred on its bounding sphere, rotated X, Y, Z (degrees), moved to `base`.
        let (centre, radius) = bounds(mi);
        let base = game_camera_position();
        RwFrameSetIdentity()(model_frame);
        let back = RwV3d { x: -centre.x, y: -centre.y, z: -centre.z };
        RwFrameTranslate()(model_frame, &back, RW_COMBINE_REPLACE);
        for (axis, angle) in [
            // Negated: our camera is rolled 180° (see below), so a positive tilt would show
            // the model from underneath.
            (RwV3d { x: 1.0, y: 0.0, z: 0.0 }, -spec.rot[0]),
            (RwV3d { x: 0.0, y: 1.0, z: 0.0 }, spec.rot[1]),
            (RwV3d { x: 0.0, y: 0.0, z: 1.0 }, spec.rot[2]),
        ] {
            RwFrameRotate()(model_frame, &axis, angle, RW_COMBINE_POSTCONCAT);
        }
        RwFrameTranslate()(model_frame, &base, RW_COMBINE_POSTCONCAT);
        RwFrameUpdateObjects()(model_frame);

        // Camera in front of the model (+Y, vehicles face +Y) looking back at it, rolled 180°.
        // That's the convention the server's snapshot specs are authored for: vehicles come
        // as rot (5, 180, 25), where rotY 180 turns the model upright again for this camera.
        let view_window = 0.5;
        // Far enough for the whole model to fit (wheels of e.g. monster trucks stick out of the
        // collision sphere). The image is cropped to the model afterwards, so the zoom from
        // the spec isn't needed for framing.
        let distance = radius / view_window * 1.6;

        let Some(Target { camera, frame: camera_frame, .. }) = target() else {
            return false;
        };
        let mut drawn = false;

        {
            let m = &mut *modelling(camera_frame);
            // GTA convention: right = cross(up, at).
            m.right = RwV3d { x: -1.0, y: 0.0, z: 0.0 };
            m.up = RwV3d { x: 0.0, y: 0.0, z: -1.0 };
            m.at = RwV3d { x: 0.0, y: -1.0, z: 0.0 };
            m.pos = RwV3d { x: base.x, y: base.y + distance, z: base.z };
            m.flags = 0;
            RwFrameUpdateObjects()(camera_frame);

            RwCameraSetViewWindow()(camera, &RwV2d { x: view_window, y: view_window });
            RwCameraSetNearClipPlane()(camera, (distance - radius * 2.0).max(0.05));
            RwCameraSetFarClipPlane()(camera, distance + radius * 2.0 + 10.0);

            // Runs from the CGame::Process hook, i.e. between frames: the scene camera is not
            // updating, so our camera can begin/end its own update.
            step(spec.model, "begin update");

            // RenderWare lights atomics with the lights of the camera's current world (the
            // game's ambient + sun): put our camera (and the model, below) into the scene world.
            let world = *(SCENE_WORLD as *const P);
            if !world.is_null() {
                RpWorldAddCamera()(world, camera);
            }

            if !RwCameraBeginUpdate()(camera).is_null() {
                RwCameraClear()(camera, &RwRgba { r: 0, g: 0, b: 0, a: 0 }, RW_CAMERA_CLEAR_IMAGE_Z);

                let saved = [RS_ZTESTENABLE, RS_ZWRITEENABLE, RS_VERTEXALPHAENABLE, RS_CULLMODE]
                    .map(|s| (s, client_api::gta::rw::render_state(s)));
                client_api::gta::rw::set_render_state(RS_ZTESTENABLE, 1);
                client_api::gta::rw::set_render_state(RS_ZWRITEENABLE, 1);
                client_api::gta::rw::set_render_state(RS_VERTEXALPHAENABLE, 1);
                client_api::gta::rw::set_render_state(RS_CULLMODE, CULL_NONE);

                SetAmbientColours()(&RwRgbaReal { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
                ActivateDirectional()();

                let is_vehicle = kind == MODEL_INFO_VEHICLE && object_type == RP_CLUMP;
                let mut vinyl_backup = Vec::new();
                let saved_remap = *(REMAP_TEXTURE as *const P);

                step(spec.model, "render");

                if !world.is_null() {
                    match object_type {
                        RP_CLUMP => {
                            RpWorldAddClump()(world, instance);
                        }
                        _ => {
                            RpWorldAddAtomic()(world, instance);
                        }
                    }
                }
                if is_vehicle {
                    let c1 = spec.color1.unwrap_or(1).clamp(0, 255) as u8;
                    let c2 = spec.color2.unwrap_or(1).clamp(0, 255) as u8;
                    set_vehicle_colour(mi, c1, c2);
                    *(REMAP_TEXTURE as *mut P) = std::ptr::null_mut();
                    SetupVehicleVariables()(instance);
                    SetEditableMaterials()(instance);
                    vinyl_backup = apply_vinyl(spec, instance);
                }

                let atomics = match object_type {
                    RP_CLUMP => {
                        let mut atomics: Vec<P> = Vec::new();
                        RpClumpForAllAtomics()(instance, collect_atomic, &mut atomics as *mut _ as P);
                        atomics
                    }
                    _ => vec![instance],
                };

                if is_vehicle {
                    show_vehicle_parts(&atomics);
                }

                render_atomics(&atomics);

                if is_vehicle {
                    for (material, texture) in vinyl_backup {
                        *(material as *mut P) = texture;
                    }
                    ResetEditableMaterials()(instance);
                    *(REMAP_TEXTURE as *mut P) = saved_remap;
                }

                if !world.is_null() {
                    match object_type {
                        RP_CLUMP => {
                            RpWorldRemoveClump()(world, instance);
                        }
                        _ => {
                            RpWorldRemoveAtomic()(world, instance);
                        }
                    }
                }

                ReSetAmbientAndDirectionalColours()();
                for (state, value) in saved {
                    client_api::gta::rw::set_render_state(state, value);
                }

                RwCameraEndUpdate()(camera);
                drawn = true;
            }

            if !world.is_null() {
                RpWorldRemoveCamera()(world, camera);
            }
        }

        drawn
    }
}

// ---- ORP vinyls (same scheme as MainPlugin/Dll1/vinyls.cpp) -------------------------------

/// `MODELS/GENERIC/VINYLS.CONF`: `[texture]` sections listing keys `<model>_vinyl_<id>`.
fn vinyl_texture_name(model: i32, vinyl: i32) -> String {
    static MAP: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();

    let map = MAP.get_or_init(|| {
        let mut map = HashMap::new();
        let path = crate::utils::game_dir().join("MODELS").join("GENERIC").join("VINYLS.CONF");
        if let Ok(text) = std::fs::read_to_string(path) {
            let mut section = String::new();
            for line in text.lines().map(str::trim_end) {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    section = name.to_string();
                    map.insert(section.clone(), section.clone());
                } else if !section.is_empty() {
                    map.insert(line.to_string(), section.clone());
                }
            }
        }
        map
    });

    let key = format!("{}_vinyl_{}", model, vinyl);
    map.get(&key).cloned().unwrap_or(key)
}

/// Swaps the vinyl texture into the vehicle's `remap*` materials. Returns what to restore.
unsafe fn apply_vinyl(spec: &Spec, clump: P) -> Vec<(P, P)> {
    unsafe {
        let Some(vinyl) = spec.vinyl.filter(|v| *v > 0) else {
            return Vec::new();
        };

        let slot = FindTxdSlot()(c"vinyls".as_ptr() as *const u8);
        let pool = *(TXD_POOL as *const *const u8);
        if slot < 0 || pool.is_null() {
            return Vec::new();
        }

        // CPool<TxdDef>.m_pObjects; TxdDef { RwTexDictionary* dict; ... } (0xC bytes)
        let objects = *(pool as *const *const u8);
        let dict = *(objects.add(slot as usize * 0xC) as *const P);
        if dict.is_null() {
            return Vec::new();
        }

        let name = vinyl_texture_name(spec.model, vinyl);
        let Ok(cname) = std::ffi::CString::new(name.clone()) else {
            return Vec::new();
        };
        let texture = RwTexDictionaryFindNamedTexture()(dict, cname.as_ptr() as *const u8);
        if texture.is_null() {
            log::warn!("snapshot: vinyl texture {:?} not found", name);
            return Vec::new();
        }

        let mut materials: Vec<P> = Vec::new();
        RpClumpForAllAtomics()(clump, collect_atomic_materials, &mut materials as *mut _ as P);

        materials
            .into_iter()
            .map(|material| {
                let old = *(material as *const P);
                *(material as *mut P) = texture;
                (material, old)
            })
            .collect()
    }
}

extern "C" fn collect_atomic_materials(atomic: P, data: P) -> P {
    unsafe {
        let geometry = *((atomic as *const u8).add(0x18) as *const P);
        if !geometry.is_null() {
            RpGeometryForAllMaterials()(geometry, collect_remap_material, data);
        }
    }
    atomic
}

extern "C" fn collect_remap_material(material: P, data: P) -> P {
    unsafe {
        let texture = *(material as *const *const u8);
        if !texture.is_null() {
            let name = std::ffi::CStr::from_ptr(texture.add(0x10) as *const std::ffi::c_char);
            if name.to_bytes().to_ascii_lowercase().starts_with(b"remap") {
                (*(data as *mut Vec<P>)).push(material);
            }
        }
    }
    material
}

/// Reads the camera raster as tightly packed BGRA (game thread). RenderWare's own lock is tried
/// first; a camera texture is a D3D9 render target though, which can only be read back through
/// `GetRenderTargetData`, so that is the fallback.
unsafe fn read_pixels(raster: *mut RwRaster) -> Option<Vec<u8>> {
    unsafe {
        let bgra = match read_pixels_rw(raster) {
            Some(bgra) => Some(bgra),
            None => {
                log::info!("snapshot: RwRasterLock failed, reading through D3D9");
                read_pixels_d3d(raster)
            }
        };

        if bgra.is_none() {
            log::warn!("snapshot: couldn't read the rendered image");
        }
        bgra
    }
}

/// BGRA from the render target to RGBA (worker thread), or None if nothing was drawn.
fn to_rgba(bgra: Vec<u8>) -> Option<Vec<u8>> {
    {
        let alpha = bgra.chunks_exact(4).filter(|p| p[3] != 0).count();
        let colour = bgra.chunks_exact(4).filter(|p| p[0] | p[1] | p[2] != 0).count();
        if debug() {
            log::info!("snapshot: pixels with alpha {}, with colour {}", alpha, colour);
        }

        if alpha == 0 && colour == 0 {
            return None; // nothing drawn
        }

        // If the game didn't write alpha, treat any non-black pixel as opaque.
        let fix_alpha = alpha == 0;

        let mut rgba = Vec::with_capacity(bgra.len());
        for p in bgra.chunks_exact(4) {
            let a = if fix_alpha && (p[0] | p[1] | p[2]) != 0 { 255 } else { p[3] };
            rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
        }
        Some(rgba)
    }
}

/// Brightens dark renders (the game's light follows the time of day): scales the colour so
/// the average luminance of the model reaches a target, at most 3x.
fn auto_exposure(mut rgba: Vec<u8>) -> Vec<u8> {
    let (mut sum, mut count) = (0f64, 0u64);
    for p in rgba.chunks_exact(4).filter(|p| p[3] > 16) {
        sum += 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
        count += 1;
    }
    if count == 0 {
        return rgba;
    }

    let mean = sum / count as f64;
    let gain = (110.0 / mean.max(1.0)).clamp(1.0, 3.0);
    if gain > 1.02 {
        for p in rgba.chunks_exact_mut(4) {
            for c in &mut p[..3] {
                *c = (*c as f64 * gain).min(255.0) as u8;
            }
        }
    }
    rgba
}

/// Crops a square RGBA image to the model's bounding box plus a small margin, keeping it
/// square (the UI shows it with object-fit: contain).
fn crop_to_content(rgba: &[u8], size: u32) -> (u32, u32, Vec<u8>) {
    let n = size as usize;
    let (mut x0, mut y0, mut x1, mut y1) = (n, n, 0usize, 0usize);

    for y in 0..n {
        for x in 0..n {
            if rgba[(y * n + x) * 4 + 3] > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }

    if x1 < x0 || y1 < y0 {
        return (size, size, rgba.to_vec());
    }

    let side = (x1 - x0 + 1).max(y1 - y0 + 1);
    let side = (side + side / 12).min(n); // ~4% margin each side
    let cx = (x0 + x1).div_ceil(2);
    let cy = (y0 + y1).div_ceil(2);
    let left = cx.saturating_sub(side / 2).min(n - side);
    let top = cy.saturating_sub(side / 2).min(n - side);

    let mut out = Vec::with_capacity(side * side * 4);
    for y in top..top + side {
        out.extend_from_slice(&rgba[(y * n + left) * 4..(y * n + left + side) * 4]);
    }
    (side as u32, side as u32, out)
}

/// BGRA rows through RwRasterLock, or None if RenderWare can't lock this raster.
unsafe fn read_pixels_rw(raster: *mut RwRaster) -> Option<Vec<u8>> {
    unsafe {
        let bits = RwRasterLock()(raster, 0, RW_RASTER_LOCK_READ);
        if bits.is_null() {
            return None;
        }

        let stride = (*raster).stride.max(SIZE * 4) as usize;
        let row = SIZE as usize * 4;
        let mut bgra = vec![0u8; row * SIZE as usize];
        for y in 0..SIZE as usize {
            bgra[y * row..][..row].copy_from_slice(std::slice::from_raw_parts(bits.add(y * stride), row));
        }

        RwRasterUnlock()(raster);
        Some(bgra)
    }
}

/// BGRA rows of a render-target raster via D3D9 `GetRenderTargetData`.
unsafe fn read_pixels_d3d(raster: *mut RwRaster) -> Option<Vec<u8>> {
    use winapi::shared::d3d9::{IDirect3DDevice9, IDirect3DSurface9, IDirect3DTexture9};
    use winapi::shared::d3d9types::{D3DLOCK_READONLY, D3DLOCKED_RECT, D3DPOOL_SYSTEMMEM, D3DSURFACE_DESC};

    unsafe {
        // RwD3D9Raster plugin data: the texture is its first field.
        let ext_offset = *(0xB4E9E0usize as *const i32);
        if !(0x20..=0x400).contains(&ext_offset) {
            return None;
        }

        let texture = *((raster as *const u8).add(ext_offset as usize) as *const *mut IDirect3DTexture9);
        if texture.is_null() {
            return None;
        }
        let texture = &*texture;

        let mut desc = std::mem::zeroed::<D3DSURFACE_DESC>();
        if texture.GetLevelDesc(0, &mut desc) < 0 || desc.Width != SIZE as u32 || desc.Height != SIZE as u32 {
            log::warn!("snapshot: unexpected camera texture {}x{}", desc.Width, desc.Height);
            return None;
        }

        let mut surface: *mut IDirect3DSurface9 = std::ptr::null_mut();
        if texture.GetSurfaceLevel(0, &mut surface) < 0 || surface.is_null() {
            return None;
        }

        let mut device: *mut IDirect3DDevice9 = std::ptr::null_mut();
        if (*surface).GetDevice(&mut device) < 0 || device.is_null() {
            (*surface).Release();
            return None;
        }

        let mut system: *mut IDirect3DSurface9 = std::ptr::null_mut();
        let mut result = None;

        if (*device).CreateOffscreenPlainSurface(
            SIZE as u32,
            SIZE as u32,
            desc.Format,
            D3DPOOL_SYSTEMMEM,
            &mut system,
            std::ptr::null_mut(),
        ) >= 0
            && !system.is_null()
        {
            if (*device).GetRenderTargetData(surface, system) >= 0 {
                let mut locked = std::mem::zeroed::<D3DLOCKED_RECT>();
                if (*system).LockRect(&mut locked, std::ptr::null(), D3DLOCK_READONLY) >= 0 {
                    let row = SIZE as usize * 4;
                    let mut bgra = vec![0u8; row * SIZE as usize];
                    for y in 0..SIZE as usize {
                        let src = (locked.pBits as *const u8).add(y * locked.Pitch as usize);
                        bgra[y * row..][..row].copy_from_slice(std::slice::from_raw_parts(src, row));
                    }
                    (*system).UnlockRect();
                    result = Some(bgra);
                }
            } else {
                log::warn!("snapshot: GetRenderTargetData failed");
            }
            (*system).Release();
        }

        (*device).Release();
        (*surface).Release();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::Spec;

    #[test]
    fn crops_to_content_square() {
        let n = 100usize;
        let mut img = vec![0u8; n * n * 4];
        for y in 40..50 {
            for x in 20..80 {
                img[(y * n + x) * 4 + 3] = 255;
            }
        }
        let (w, h, out) = super::crop_to_content(&img, n as u32);
        assert_eq!(w, h);
        assert!((60..=70).contains(&w), "{}", w);
        assert_eq!(out.len(), (w * h * 4) as usize);

        let empty = vec![0u8; n * n * 4];
        assert_eq!(super::crop_to_content(&empty, n as u32).0, n as u32);
    }

    #[test]
    fn parses_full_and_short_specs() {
        let s = Spec::parse("es_15889_0.0_180.0_0.0_0.1_1.0_0.2_4_5_6").unwrap();
        assert_eq!(s.model, 15889);
        assert_eq!(s.rot, [0.0, 180.0, 0.0]);
        assert_eq!(s.offset, [0.1, 1.0, 0.2]);
        assert_eq!((s.color1, s.color2, s.vinyl), (Some(4), Some(5), Some(6)));

        let s = Spec::parse("veh_411_0_0_45_0_1.5_0").unwrap();
        assert_eq!((s.model, s.color1, s.vinyl), (411, None, None));
        assert!((s.zoom() - 1.5).abs() < 1e-6);

        assert!(Spec::parse("es_abc_0_0_0_0_1_0").is_none());
        assert!(Spec::parse("es_1_0_0").is_none());
        assert!(Spec::parse("sprite_acs15").is_none());
    }
}
