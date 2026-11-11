//! cef_host.exe: runs the CEF browser process outside of gta_sa.exe.
//!
//! Started by client.dll with:
//!   cef_host.exe --ipc=<pipe base> --cef-dir=<path> --parent-pid=<pid> --hwnd=<game hwnd> [--gpu]
//!
//! Commands arrive on one named pipe, events go back on another, frames are shared via
//! `cef_ipc::FrameBuffer`. The host exits when the game closes the pipe or dies.

#![windows_subsystem = "windows"]

mod app;
mod assets_scheme;
mod browser;
mod ipc;
mod paths;
mod snapshot_scheme;
mod stats;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cef_ipc::pipe::{self, Direction};
use cef_ipc::{Command, Event, PROTOCOL_VERSION};
use winapi::um::libloaderapi::GetModuleHandleA;

struct Args {
    ipc: String,
    cef_dir: PathBuf,
    parent_pid: u32,
    hwnd: usize,
    gpu: bool,
}

fn parse_args() -> Option<Args> {
    let mut ipc = None;
    let mut cef_dir = None;
    let mut parent_pid = 0;
    let mut hwnd = 0;
    let mut gpu = false;

    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--ipc=") {
            ipc = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--cef-dir=") {
            cef_dir = Some(PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--parent-pid=") {
            parent_pid = v.parse().unwrap_or(0);
        } else if let Some(v) = arg.strip_prefix("--hwnd=") {
            hwnd = v.parse().unwrap_or(0);
        } else if arg == "--gpu" {
            gpu = true;
        }
    }

    Some(Args { ipc: ipc?, cef_dir: cef_dir?, parent_pid, hwnd, gpu })
}

fn init_logger(cef_dir: &std::path::Path) {
    let level = if cef_dir.join("host_trace").exists() {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Info
    };

    if let Ok(file) = std::fs::File::create(cef_dir.join("cef_host.log")) {
        let config = simplelog::ConfigBuilder::new()
            .set_time_format_rfc3339()
            .build();
        let _ = simplelog::WriteLogger::init(level, config, file);
    }
}

/// Exits as soon as the game process dies, even if it couldn't close the pipes.
fn watch_parent(pid: u32) {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::INFINITE;
    use winapi::um::winnt::SYNCHRONIZE;

    if pid == 0 {
        return;
    }

    std::thread::spawn(move || unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);

        if handle.is_null() {
            log::warn!("can't open parent process {}", pid);
            return;
        }

        WaitForSingleObject(handle, INFINITE);
        CloseHandle(handle);

        log::info!("game process exited, terminating");
        std::process::exit(0);
    });
}

fn initialize_cef(args: &Args) {
    let instance = unsafe { GetModuleHandleA(std::ptr::null()) };
    let main_args = cef_sys::cef_main_args_t { instance: instance as *mut _ };

    let dir = &args.cef_dir;
    let s = |p: PathBuf| cef::types::string::into_cef_string(&p.to_string_lossy());

    let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_settings_t>() };
    settings.size = std::mem::size_of::<cef_sys::cef_settings_t>();
    settings.no_sandbox = 1;
    let runtime = paths::runtime_dir();
    settings.browser_subprocess_path = s(runtime.join("renderer.exe"));
    settings.windowless_rendering_enabled = 1;
    settings.multi_threaded_message_loop = 1;
    settings.log_severity = cef_sys::cef_log_severity_t::LOGSEVERITY_ERROR;
    // CEF 89 (32-bit host) and CEF 156 (64-bit host) profiles are not compatible: separate
    // folders. cache_path must be inside root_cache_path.
    let cache_root = if cfg!(target_arch = "x86") { "cache" } else { "cache156" };
    settings.root_cache_path = s(dir.join(cache_root));
    settings.cache_path = s(dir.join(cache_root).join("default"));
    settings.locales_dir_path = s(runtime.join("locales"));
    settings.resources_dir_path = s(runtime.clone());
    settings.log_file = s(dir.join("cef.log"));

    let app = app::HostApp { gpu: args.gpu };

    log::info!("cef::initialize (gpu: {})", args.gpu);
    let result = cef::initialize(Some(&main_args), &settings, Some(app));
    log::info!("cef::initialize => {}", result);
}

fn handle(cmd: Command, hwnd: usize) -> bool {
    match cmd {
        Command::Hello { version } => {
            if version != PROTOCOL_VERSION {
                log::error!("protocol mismatch: game {} host {}", version, PROTOCOL_VERSION);
            }
        }

        Command::CreateBrowser { id, url, width, height, hidden, audio, external_frames } => {
            browser::create(id, &url, width, height, hidden, audio, external_frames, hwnd);
        }

        Command::BeginFrame { id } => {
            if let Some(b) = browser::get(id) {
                b.begin_frame();
            }
        }

        Command::CloseBrowser { id, force } => {
            if let Some(b) = browser::get(id) {
                b.close(force);
            }
        }

        Command::Resize { id, width, height } => {
            if let Some(b) = browser::get(id) {
                b.resize(width, height);
            }
        }

        Command::Hide { id, hidden } => {
            if let Some(b) = browser::get(id) {
                b.set_hidden(hidden);
            }
        }

        Command::Invalidate { id } => {
            if let Some(b) = browser::get(id) {
                b.invalidate();
            }
        }

        Command::MouseMove { id, x, y, modifiers } => {
            if let Some(b) = browser::get(id) {
                b.mouse_move(x, y, modifiers);
            }
        }

        Command::MouseClick { id, x, y, modifiers, button, down } => {
            if let Some(b) = browser::get(id) {
                b.mouse_click(x, y, modifiers, button, down);
            }
        }

        Command::MouseWheel { id, x, y, delta } => {
            if let Some(b) = browser::get(id) {
                b.mouse_wheel(x, y, delta);
            }
        }

        Command::Key { id, event } => {
            if let Some(b) = browser::get(id) {
                b.key(event);
            }
        }

        Command::TriggerEvent { id, name, args } => {
            if let Some(b) = browser::get(id) {
                b.trigger_event(&name, &args);
            }
        }

        Command::LoadUrl { id, url } => {
            if let Some(b) = browser::get(id) {
                b.load_url(&url);
            }
        }

        Command::DevTools { id, enabled } => {
            if let Some(b) = browser::get(id) {
                b.toggle_dev_tools(enabled);
            }
        }

        Command::FrameRate { id, fps } => {
            if let Some(b) = browser::get(id) {
                b.set_frame_rate(fps);
            }
        }

        Command::AudioMuted { id, muted } => {
            if let Some(b) = browser::get(id) {
                b.set_audio_muted(muted);
            }
        }

        Command::SnapshotResult { req, png } => {
            log::trace!("snapshot result {} ({} bytes)", req, png.len());
            snapshot_scheme::complete(req, png);
        }

        Command::Shutdown => return false,
    }

    true
}

fn shutdown() {
    log::info!("shutting down");

    for b in browser::all() {
        b.close(true);
    }

    let started = Instant::now();
    while browser::count() > 0 && started.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }

    cef::shutdown();
    log::info!("cef::shutdown done");
}

fn main() {
    let Some(args) = parse_args() else {
        // Started by hand or by an old client.dll: nothing to do.
        return;
    };

    paths::init(args.cef_dir.clone());
    init_logger(&args.cef_dir);
    log::info!("cef_host started: pid {} parent {}", std::process::id(), args.parent_pid);

    watch_parent(args.parent_pid);
    unsafe { winapi::um::timeapi::timeBeginPeriod(1) };

    let (c2h, h2c) = cef_ipc::pipe_names(&args.ipc);

    let mut commands = match pipe::connect(&c2h, Direction::Outbound, Duration::from_secs(10)) {
        Ok(file) => file,
        Err(err) => {
            log::error!("can't open command pipe {}: {}", c2h, err);
            return;
        }
    };

    match pipe::connect(&h2c, Direction::Inbound, Duration::from_secs(10)) {
        Ok(file) => ipc::set_writer(file),
        Err(err) => {
            log::error!("can't open event pipe {}: {}", h2c, err);
            return;
        }
    }

    ipc::send(Event::Hello { version: PROTOCOL_VERSION });

    initialize_cef(&args);

    let mut buf = Vec::new();

    loop {
        if let Err(err) = pipe::read_message(&mut commands, &mut buf) {
            log::info!("command pipe closed: {}", err);
            break;
        }

        match Command::decode(&buf) {
            Ok(cmd) => {
                log::trace!("command: {:?}", cmd);

                if !handle(cmd, args.hwnd) {
                    break;
                }
            }
            Err(err) => log::error!("bad command: {}", err),
        }
    }

    shutdown();
}
