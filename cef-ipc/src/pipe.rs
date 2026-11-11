//! Length-prefixed messages over Windows named pipes.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::time::{Duration, Instant};

use winapi::shared::winerror::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::namedpipeapi::{ConnectNamedPipe, CreateNamedPipeW};
use winapi::um::winbase::{
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Upper bound for a single message (a 4K frame never goes through the pipe, only popups).
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

const PIPE_BUFFER: u32 = 1024 * 1024;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Which way data flows through a pipe, from the creating (server) side's point of view.
#[derive(Clone, Copy, Debug)]
pub enum Direction {
    /// The server reads, the other side writes.
    Inbound,
    /// The server writes, the other side reads.
    Outbound,
}

/// Server end of a pipe: created by the game process before spawning the host.
pub struct PendingPipe {
    file: File,
}

impl PendingPipe {
    pub fn create(name: &str, direction: Direction) -> io::Result<PendingPipe> {
        let access = match direction {
            Direction::Inbound => PIPE_ACCESS_INBOUND,
            Direction::Outbound => PIPE_ACCESS_OUTBOUND,
        };

        let name = wide(name);

        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                access,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        Ok(PendingPipe {
            file: unsafe { File::from_raw_handle(handle as _) },
        })
    }

    /// Blocks until the other side opens the pipe.
    pub fn accept(self) -> io::Result<File> {
        use std::os::windows::io::AsRawHandle;

        let ok = unsafe { ConnectNamedPipe(self.file.as_raw_handle() as _, std::ptr::null_mut()) };

        if ok == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            return Err(io::Error::last_os_error());
        }

        Ok(self.file)
    }
}

/// Opens the client end of a pipe created by [`PendingPipe::create`].
pub fn connect(name: &str, direction: Direction, timeout: Duration) -> io::Result<File> {
    let started = Instant::now();

    loop {
        let mut options = OpenOptions::new();

        // `direction` is the server's view: an inbound server pipe is written by us.
        match direction {
            Direction::Inbound => options.write(true),
            Direction::Outbound => options.read(true),
        };

        match options.open(name) {
            Ok(file) => return Ok(file),
            Err(err) => {
                let retry = err.kind() == io::ErrorKind::NotFound
                    || err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32);

                if !retry || started.elapsed() > timeout {
                    return Err(err);
                }

                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Writes one message: `u32` length followed by the payload.
pub fn write_message(file: &mut File, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    file.write_all(&buf)
}

/// Reads one message into `buf` (replacing its contents).
pub fn read_message(file: &mut File, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut len = [0u8; 4];
    file.read_exact(&mut len)?;

    let len = u32::from_le_bytes(len) as usize;

    if len > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cef-ipc: message of {} bytes is too large", len),
        ));
    }

    buf.resize(len, 0);
    file.read_exact(buf)
}
