use cef_sys::{cef_main_args_t, cef_settings_t};

pub mod app;
pub mod browser;
pub mod client;
pub mod command_line;
pub mod handlers;
pub mod process_message;
pub mod ref_counted;
pub mod scheme;
pub mod settings;
pub mod task;
pub mod types;
pub mod v8;

mod rust_to_c;

use crate::app::App;

/// Newer CEF: libcef must be told which `CEF_API_VERSION` the bindings were generated for
/// before any other call, otherwise it may use different struct layouts.
#[cfg(not(target_arch = "x86"))]
pub fn configure_api_version() {
    unsafe {
        cef_sys::cef_api_hash(cef_sys::CEF_API_VERSION, 0);
    }
}

#[cfg(target_arch = "x86")]
pub fn configure_api_version() {}

pub fn execute_process<T: App>(args: &cef_main_args_t, app: Option<T>) -> i32 {
    configure_api_version();

    let app_ptr = app
        .map(|app| self::rust_to_c::app::wrap(app))
        .unwrap_or(std::ptr::null_mut());

    unsafe { cef_sys::cef_execute_process(args, app_ptr, std::ptr::null_mut()) }
}

pub fn initialize<T: App>(
    args: Option<&cef_main_args_t>, settings: &cef_settings_t, app: Option<T>,
) -> i32 {
    configure_api_version();

    let args = args
        .map(|args| args as *const _)
        .unwrap_or(std::ptr::null());

    let app_ptr = app
        .map(|app| self::rust_to_c::app::wrap(app))
        .unwrap_or(std::ptr::null_mut());

    unsafe { cef_sys::cef_initialize(args, settings, app_ptr, std::ptr::null_mut()) }
}

pub fn run_message_loop() {
    unsafe {
        cef_sys::cef_run_message_loop();
    }
}

pub fn do_message_loop_work() {
    unsafe {
        cef_sys::cef_do_message_loop_work();
    }
}

pub fn quit_message_loop() {
    unsafe {
        cef_sys::cef_quit_message_loop();
    }
}

pub fn shutdown() {
    unsafe {
        cef_sys::cef_shutdown();
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProcessId {
    None,
    Browser,
    Renderer,
}

impl From<ProcessId> for cef_sys::cef_process_id_t::Type {
    fn from(value: ProcessId) -> cef_sys::cef_process_id_t::Type {
        match value {
            ProcessId::None => 0,
            ProcessId::Browser => 0,
            ProcessId::Renderer => 1,
        }
    }
}

impl From<cef_sys::cef_process_id_t::Type> for ProcessId {
    fn from(val: cef_sys::cef_process_id_t::Type) -> ProcessId {
        match val {
            0 => ProcessId::Browser,
            1 => ProcessId::Renderer,
            _ => ProcessId::None,
        }
    }
}
