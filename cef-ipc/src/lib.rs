//! IPC between `client.dll` (inside gta_sa.exe) and `cef_host.exe` (out-of-process CEF).
//!
//! * Two named pipes carry length-prefixed [`Command`]s (client -> host) and
//!   [`Event`]s (host -> client). One pipe per direction, because synchronous I/O on a
//!   single duplex handle serializes reads and writes and would deadlock.
//! * Every browser gets a [`FrameBuffer`]: a shared-memory section holding the latest
//!   BGRA frame plus the dirty rects accumulated since the client last uploaded it.

pub mod codec;
pub mod frame;
pub mod pipe;
pub mod proto;

pub use frame::FrameBuffer;
pub use proto::{Command, Event, KeyEvent, Value};

/// Bumped whenever the wire format changes; host and client must match.
pub const PROTOCOL_VERSION: u32 = 3;

/// Pipe names for a session. `base` is unique per game process.
pub fn pipe_names(base: &str) -> (String, String) {
    (
        format!(r"\\.\pipe\{}_c2h", base),
        format!(r"\\.\pipe\{}_h2c", base),
    )
}
