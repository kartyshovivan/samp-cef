//! Event channel back to the game process.

use std::fs::File;

use cef_ipc::Event;
use parking_lot::Mutex;

static EVENTS: Mutex<Option<File>> = Mutex::new(None);

pub fn set_writer(file: File) {
    *EVENTS.lock() = Some(file);
}

/// Sends an event to the game. If the pipe is gone the game has exited: shut down.
pub fn send(event: Event) {
    let payload = event.encode();
    let mut guard = EVENTS.lock();

    let Some(file) = guard.as_mut() else {
        return;
    };

    if let Err(err) = cef_ipc::pipe::write_message(file, &payload) {
        log::error!("event pipe broken ({}), exiting", err);
        guard.take();
        std::process::exit(0);
    }
}
