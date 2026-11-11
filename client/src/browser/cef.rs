//! CEF runs out of process in `cef/cef_host.exe`; this module only starts and drives it.
//! See `browser::host` for the IPC and `cef-host` for the browser process itself.

use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::app::Event;
use crate::browser::client::WebClient;

pub fn initialize(event_tx: Sender<Event>) {
    // OSR frames are timed by CEF in the host; the game loop still benefits from 1 ms
    // timer resolution for smooth uploads.
    let timer = unsafe { winapi::um::timeapi::timeBeginPeriod(1) };
    log::info!("timeBeginPeriod(1) => {}", timer);

    crate::browser::host::start(event_tx);
}

pub fn create_browser(client: Arc<WebClient>, url: &str) {
    log::trace!("create_browser({}, {:?})", client.id(), url);
    client.create_remote(url);
}

pub fn shutdown() {
    crate::browser::host::send(cef_ipc::Command::Shutdown);
}
