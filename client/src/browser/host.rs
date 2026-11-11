//! Connection to the out-of-process CEF host (`cef/cef_host.exe`).
//!
//! The game thread only ever pushes [`Command`]s into a channel; a writer thread sends
//! them. A reader thread receives [`Event`]s and routes them to the [`WebClient`]s or to
//! the app event loop, exactly where the in-process CEF callbacks used to go.

use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use cef::types::list::{List, ValueType};
use cef::types::string::CefString;
use cef_ipc::pipe::{self, Direction, PendingPipe};
use cef_ipc::{Command, Event, PROTOCOL_VERSION, Value};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use crate::app::Event as AppEvent;
use crate::browser::client::WebClient;

struct ClientRef(Weak<WebClient>);

// WebClient is shared with background threads the same way it was shared with CEF's
// threads before: all of its mutable state sits behind locks/atomics.
unsafe impl Send for ClientRef {}

static CLIENTS: Mutex<Option<HashMap<u32, ClientRef>>> = Mutex::new(None);
static COMMANDS: Mutex<Option<Sender<Command>>> = Mutex::new(None);

pub fn register(client: &Arc<WebClient>) {
    CLIENTS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(client.id(), ClientRef(Arc::downgrade(client)));
}

pub fn unregister(client: &WebClient) {
    if let Some(map) = CLIENTS.lock().as_mut()
        && map.get(&client.id()).is_some_and(|c| std::ptr::eq(c.0.as_ptr(), client))
    {
        map.remove(&client.id());
    }
}

fn client(id: u32) -> Option<Arc<WebClient>> {
    CLIENTS.lock().as_ref()?.get(&id)?.0.upgrade()
}

/// Queues a command for the host. Never blocks.
pub fn send(cmd: Command) {
    if let Some(tx) = COMMANDS.lock().as_ref() {
        let _ = tx.send(cmd);
    }
}

/// Which browser host runs CEF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    /// `cef/host/cef_host.exe`: 64-bit, CEF 156. Needs 64-bit Windows 10 or newer.
    Modern,
    /// `cef/cef_host.exe`: 32-bit, CEF 89 (same libcef.dll the game loads). Runs everywhere,
    /// including 32-bit Windows and Windows 7/8.
    Legacy,
}

/// Picks the host for this machine: modern on 64-bit Windows 10+, legacy otherwise.
/// `cef/force_legacy_host` forces the legacy one (for testing).
pub fn host_kind() -> HostKind {
    static KIND: std::sync::OnceLock<HostKind> = std::sync::OnceLock::new();

    *KIND.get_or_init(|| {
        let cef_dir = crate::utils::cef_dir();
        let os_64bit = is_64bit_windows();
        let (major, minor, build) = windows_version();
        let modern_present = cef_dir.join("host").join("cef_host.exe").exists();
        let forced_legacy = cef_dir.join("force_legacy_host").exists();

        // Chromium dropped Windows 7/8/8.1 after version 109, so CEF 156 needs Windows 10+.
        let kind = if os_64bit && major >= 10 && modern_present && !forced_legacy {
            HostKind::Modern
        } else {
            HostKind::Legacy
        };

        log::info!(
            "CEF host: {:?} (64-bit OS: {}, Windows {}.{}.{}, modern host present: {}, forced legacy: {})",
            kind,
            os_64bit,
            major,
            minor,
            build,
            modern_present,
            forced_legacy
        );

        kind
    })
}

/// The game is a 32-bit process; on 64-bit Windows it runs under WOW64.
fn is_64bit_windows() -> bool {
    use winapi::um::processthreadsapi::GetCurrentProcess;
    use winapi::um::wow64apiset::IsWow64Process;

    let mut wow64 = 0;
    unsafe { IsWow64Process(GetCurrentProcess(), &mut wow64) != 0 && wow64 != 0 }
}

/// Real Windows version (RtlGetVersion ignores the compatibility shims GetVersionEx applies).
fn windows_version() -> (u32, u32, u32) {
    use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress};
    use winapi::um::winnt::OSVERSIONINFOW;

    type RtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;

    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr());
        if ntdll.is_null() {
            return (0, 0, 0);
        }

        let proc = GetProcAddress(ntdll, c"RtlGetVersion".as_ptr());
        if proc.is_null() {
            return (0, 0, 0);
        }

        let rtl_get_version: RtlGetVersion = std::mem::transmute(proc);
        let mut info = std::mem::zeroed::<OSVERSIONINFOW>();
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

        if rtl_get_version(&mut info) != 0 {
            return (0, 0, 0);
        }

        (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
    }
}

/// Browsers paint only when the game loop asks (`Command::BeginFrame`), so UI frames line up
/// with game frames instead of beating against CEF's own 60 Hz timer.
/// Modern host (CEF 156): on by default, `cef/no_external_frames` turns it off.
/// Legacy host (CEF 89): off by default (it drops BeginFrames, ~30 fps), `cef/external_frames`
/// turns it on.
pub fn external_frames() -> bool {
    static EXTERNAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    *EXTERNAL.get_or_init(|| {
        let cef_dir = crate::utils::cef_dir();
        let enabled = match host_kind() {
            HostKind::Modern => !cef_dir.join("no_external_frames").exists(),
            HostKind::Legacy => cef_dir.join("external_frames").exists(),
        };
        log::info!("CEF game-driven frames: {}", if enabled { "on" } else { "off" });
        enabled
    })
}

pub fn is_running() -> bool {
    COMMANDS.lock().is_some()
}

/// Starts cef_host.exe and the IPC threads.
pub fn start(event_tx: Sender<AppEvent>) {
    if is_running() {
        return;
    }

    let cef_dir = crate::utils::cef_dir();
    let kind = host_kind();

    // The 64-bit CEF lives in its own folder: this process already has the 32-bit libcef.dll
    // from `cef/` loaded (plugin API list values). The legacy host uses that same CEF 89.
    let host_exe = match kind {
        HostKind::Modern => cef_dir.join("host").join("cef_host.exe"),
        HostKind::Legacy => cef_dir.join("cef_host.exe"),
    };

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let base = format!("orp_cef_{}_{:x}", std::process::id(), nonce);
    let (c2h_name, h2c_name) = cef_ipc::pipe_names(&base);

    let (c2h, h2c) = match (
        PendingPipe::create(&c2h_name, Direction::Outbound),
        PendingPipe::create(&h2c_name, Direction::Inbound),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            log::error!(
                "cef host: can't create pipes: {:?} {:?}",
                a.err(),
                b.err()
            );
            return;
        }
    };

    let hwnd = client_api::gta::hwnd() as usize;
    // Modern host (CEF 156): GPU by default, the real UI renders at 60 fps (slow drag-scroll
    // included), software tops out at ~30 fps; `cef/no_gpu` forces software.
    // Legacy host (CEF 89): software by default, its GPU compositing stalls ~250 ms per card
    // on slow drag-scroll; `cef/gpu` opts in.
    let gpu = match kind {
        HostKind::Modern => !cef_dir.join("no_gpu").exists(),
        HostKind::Legacy => cef_dir.join("gpu").exists(),
    };

    let mut command = std::process::Command::new(&host_exe);
    // Same working directory as the game: pages use drive-less URLs such as
    // `file:///frontend/index.html`, which Chromium resolves against it.
    command
        .current_dir(crate::utils::game_dir())
        .arg(format!("--ipc={}", base))
        .arg(format!("--cef-dir={}", cef_dir.display()))
        .arg(format!("--parent-pid={}", std::process::id()))
        .arg(format!("--hwnd={}", hwnd));

    if gpu {
        command.arg("--gpu");
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::error!("cef host: can't start {}: {}", host_exe.display(), err);
            return;
        }
    };

    log::info!("cef host started: pid {} (gpu: {})", child.id(), gpu);
    kill_with_game(&child);

    let (tx, rx) = unbounded::<Command>();
    let _ = tx.send(Command::Hello { version: PROTOCOL_VERSION });
    *COMMANDS.lock() = Some(tx);

    std::thread::spawn(move || match child.wait() {
        Ok(status) => log::error!("cef host exited: {}", status),
        Err(err) => log::error!("cef host wait failed: {}", err),
    });

    std::thread::spawn(move || {
        // The host opens the command pipe first, then the event pipe.
        let commands = match c2h.accept() {
            Ok(file) => file,
            Err(err) => return log::error!("cef host: command pipe accept failed: {}", err),
        };

        let events = match h2c.accept() {
            Ok(file) => file,
            Err(err) => return log::error!("cef host: event pipe accept failed: {}", err),
        };

        log::info!("cef host connected");

        std::thread::spawn(move || writer_loop(commands, rx));
        reader_loop(events, event_tx);
    });
}

/// Puts the host in a job object that kills it (and its renderer processes) when the
/// game exits, however it exits.
fn kill_with_game(child: &std::process::Child) {
    use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
    use winapi::um::winnt::{
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());

        if job.is_null() {
            log::warn!("cef host: CreateJobObject failed");
            return;
        }

        let mut info = std::mem::zeroed::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );

        if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
            log::warn!("cef host: AssignProcessToJobObject failed");
        }

        // Intentionally leaked: the handle closes when gta_sa.exe exits.
    }
}

fn writer_loop(mut file: std::fs::File, rx: Receiver<Command>) {
    for cmd in rx {
        let shutdown = matches!(cmd, Command::Shutdown);

        if let Err(err) = pipe::write_message(&mut file, &cmd.encode()) {
            log::error!("cef host: command pipe broken: {}", err);
            break;
        }

        if shutdown {
            break;
        }
    }
}

fn reader_loop(mut file: std::fs::File, event_tx: Sender<AppEvent>) {
    let mut buf = Vec::new();

    loop {
        if let Err(err) = pipe::read_message(&mut file, &mut buf) {
            log::error!("cef host: event pipe closed: {}", err);
            break;
        }

        match Event::decode(&buf) {
            Ok(event) => dispatch(event, &event_tx),
            Err(err) => log::error!("cef host: bad event: {}", err),
        }
    }
}

fn dispatch(event: Event, event_tx: &Sender<AppEvent>) {
    match event {
        Event::Hello { version } => {
            log::info!("cef host protocol {}", version);

            if version != PROTOCOL_VERSION {
                log::error!("cef host protocol mismatch: host {} client {}", version, PROTOCOL_VERSION);
            }
        }

        Event::Ready => {
            let _ = event_tx.send(AppEvent::CefInitialize);
        }

        Event::AfterCreated { id } => {
            if let Some(client) = client(id) {
                client.on_after_created();
            }
        }

        Event::LoadEnd { id, status } => {
            if let Some(client) = client(id) {
                client.on_load_end();
            }

            let _ = event_tx.send(AppEvent::BrowserCreated(id, status));
        }

        Event::BeforeClose { id } => {
            if let Some(client) = client(id) {
                client.on_before_close();
            }
        }

        Event::ProcessMessage { id, name, args } => {
            if let Some(client) = client(id) {
                client.on_process_message(&name, &values_to_list(&args));
            }
        }

        Event::FrameBuffer { id, name, .. } => {
            if let Some(client) = client(id) {
                match cef_ipc::FrameBuffer::open(&name) {
                    Ok(fb) => client.set_frame_buffer(fb),
                    Err(err) => log::error!("browser {}: can't open frame buffer {}: {}", id, name, err),
                }
            }
        }

        Event::PopupShow { id, show } => {
            if let Some(client) = client(id) {
                client.on_popup_show(show);
            }
        }

        Event::PopupSize { id, x, y, width, height } => {
            if let Some(client) = client(id) {
                client.on_popup_size(cef_sys::cef_rect_t { x, y, width, height });
            }
        }

        Event::PopupPaint { id, pixels, .. } => {
            if let Some(client) = client(id) {
                client.on_popup_paint(&pixels);
            }
        }

        Event::AudioStarted { id, stream, channels, sample_rate, frames_per_buffer } => {
            if let Some(client) = client(id) {
                client.on_audio_stream_started(stream, channels, sample_rate, frames_per_buffer);
            }
        }

        Event::AudioPacket { id, stream, frames, pts, data } => {
            if let Some(client) = client(id) {
                client.on_audio_stream_packet(stream, frames, pts, &data);
            }
        }

        Event::SnapshotRequest { req, spec } => {
            crate::snapshot::request(req, spec);
        }

        Event::AudioStopped { id, stream } => {
            if let Some(client) = client(id) {
                client.on_audio_stream_stopped(stream);
            }
        }
    }
}

/// Builds a CEF list (still created by libcef.dll inside the game) for plugin callbacks.
pub fn values_to_list(values: &[Value]) -> List {
    let list = List::new();
    fill_list(&list, values);
    list
}

fn fill_list(list: &List, values: &[Value]) {
    for (idx, value) in values.iter().enumerate() {
        match value {
            Value::Null => list.set_null(idx),
            Value::Bool(v) => list.set_bool(idx, *v),
            Value::Int(v) => list.set_integer(idx, *v),
            Value::Double(v) => list.set_double(idx, *v),
            Value::String(v) => list.set_string(idx, &CefString::new(v)),
            Value::List(items) => {
                let inner = List::new();
                fill_list(&inner, items);
                list.set_list(idx, inner);
            }
        }
    }
}

/// Converts a CEF list (from plugins / the server) into a transferable value.
pub fn list_to_value(list: &List) -> Value {
    Value::List(read_list(list, 0))
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
