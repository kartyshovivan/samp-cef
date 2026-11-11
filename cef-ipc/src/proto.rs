//! Messages exchanged between the game (client.dll) and cef_host.exe.

use std::io;

use crate::codec::{Reader, Writer};

/// A CEF list value, transferable across processes.
/// Binary and dictionary values are not used by the UI and travel as `Null`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i32),
    Double(f64),
    String(String),
    List(Vec<Value>),
}

impl Value {
    fn encode(&self, w: &mut Writer) {
        match self {
            Value::Null => {
                w.u8(0);
            }
            Value::Bool(v) => {
                w.u8(1).bool(*v);
            }
            Value::Int(v) => {
                w.u8(2).i32(*v);
            }
            Value::Double(v) => {
                w.u8(3).f64(*v);
            }
            Value::String(v) => {
                w.u8(4).str(v);
            }
            Value::List(items) => {
                w.u8(5).u32(items.len() as u32);
                for item in items {
                    item.encode(w);
                }
            }
        }
    }

    fn decode(r: &mut Reader, depth: u32) -> io::Result<Value> {
        if depth > 32 {
            return Err(invalid("value nesting too deep"));
        }

        Ok(match r.u8()? {
            0 => Value::Null,
            1 => Value::Bool(r.bool()?),
            2 => Value::Int(r.i32()?),
            3 => Value::Double(r.f64()?),
            4 => Value::String(r.string()?),
            5 => {
                let len = r.u32()? as usize;
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(Value::decode(r, depth + 1)?);
                }
                Value::List(items)
            }
            t => return Err(invalid(&format!("unknown value tag {}", t))),
        })
    }
}

fn encode_values(w: &mut Writer, values: &[Value]) {
    w.u32(values.len() as u32);
    for v in values {
        v.encode(w);
    }
}

fn decode_values(r: &mut Reader) -> io::Result<Vec<Value>> {
    let len = r.u32()? as usize;
    let mut out = Vec::with_capacity(len.min(1024));
    for _ in 0..len {
        out.push(Value::decode(r, 0)?);
    }
    Ok(out)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("cef-ipc: {}", msg))
}

/// `cef_key_event_t` without the CEF dependency.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct KeyEvent {
    pub kind: i32,
    pub modifiers: u32,
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub is_system_key: i32,
    pub character: u16,
    pub unmodified_character: u16,
    pub focus_on_editable_field: i32,
}

/// Mouse buttons, same numbering as `cef_mouse_button_type_t`.
pub const MOUSE_LEFT: u8 = 0;
pub const MOUSE_MIDDLE: u8 = 1;
pub const MOUSE_RIGHT: u8 = 2;

/// Client (game) -> host.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Hello { version: u32 },
    /// `external_frames`: the browser paints only on [`Command::BeginFrame`] (game-driven).
    CreateBrowser { id: u32, url: String, width: u32, height: u32, hidden: bool, audio: bool, external_frames: bool },
    CloseBrowser { id: u32, force: bool },
    Resize { id: u32, width: u32, height: u32 },
    Hide { id: u32, hidden: bool },
    Invalidate { id: u32 },
    MouseMove { id: u32, x: i32, y: i32, modifiers: u32 },
    MouseClick { id: u32, x: i32, y: i32, modifiers: u32, button: u8, down: bool },
    MouseWheel { id: u32, x: i32, y: i32, delta: i32 },
    Key { id: u32, event: KeyEvent },
    /// Sends `trigger_event` to the renderer of browser `id`.
    TriggerEvent { id: u32, name: String, args: Value },
    LoadUrl { id: u32, url: String },
    DevTools { id: u32, enabled: bool },
    FrameRate { id: u32, fps: i32 },
    AudioMuted { id: u32, muted: bool },
    Shutdown,
    /// Asks the browser to produce a frame (only for `external_frames` browsers).
    BeginFrame { id: u32 },
    /// Answer to [`Event::SnapshotRequest`]: a PNG, or empty if the model couldn't be rendered.
    SnapshotResult { req: u32, png: Vec<u8> },
}

/// Host -> client (game).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Hello { version: u32 },
    /// CEF context initialized; browsers can be created.
    Ready,
    AfterCreated { id: u32 },
    LoadEnd { id: u32, status: i32 },
    BeforeClose { id: u32 },
    /// A process message from the renderer (`set_focus`, `hide`, `emit_event`, ...).
    ProcessMessage { id: u32, name: String, args: Vec<Value> },
    /// The browser's frames now live in shared memory section `name`.
    FrameBuffer { id: u32, name: String, capacity: u32 },
    PopupShow { id: u32, show: bool },
    PopupSize { id: u32, x: i32, y: i32, width: i32, height: i32 },
    PopupPaint { id: u32, width: u32, height: u32, pixels: Vec<u8> },
    AudioStarted { id: u32, stream: i32, channels: i32, sample_rate: i32, frames_per_buffer: i32 },
    /// Planar PCM: `channels` consecutive blocks of `frames` samples.
    AudioPacket { id: u32, stream: i32, frames: i32, pts: i64, data: Vec<f32> },
    AudioStopped { id: u32, stream: i32 },
    /// A page requested `sampcef://snapshot/<spec>`: render a game model (entity snapshot
    /// string, see `client/src/snapshot.rs`) and answer with [`Command::SnapshotResult`].
    SnapshotRequest { req: u32, spec: String },
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();

        match self {
            Command::Hello { version } => {
                w.u8(0).u32(*version);
            }
            Command::CreateBrowser { id, url, width, height, hidden, audio, external_frames } => {
                w.u8(1).u32(*id).str(url).u32(*width).u32(*height).bool(*hidden).bool(*audio).bool(*external_frames);
            }
            Command::CloseBrowser { id, force } => {
                w.u8(2).u32(*id).bool(*force);
            }
            Command::Resize { id, width, height } => {
                w.u8(3).u32(*id).u32(*width).u32(*height);
            }
            Command::Hide { id, hidden } => {
                w.u8(4).u32(*id).bool(*hidden);
            }
            Command::Invalidate { id } => {
                w.u8(5).u32(*id);
            }
            Command::MouseMove { id, x, y, modifiers } => {
                w.u8(6).u32(*id).i32(*x).i32(*y).u32(*modifiers);
            }
            Command::MouseClick { id, x, y, modifiers, button, down } => {
                w.u8(7).u32(*id).i32(*x).i32(*y).u32(*modifiers).u8(*button).bool(*down);
            }
            Command::MouseWheel { id, x, y, delta } => {
                w.u8(8).u32(*id).i32(*x).i32(*y).i32(*delta);
            }
            Command::Key { id, event: e } => {
                w.u8(9)
                    .u32(*id)
                    .i32(e.kind)
                    .u32(e.modifiers)
                    .i32(e.windows_key_code)
                    .i32(e.native_key_code)
                    .i32(e.is_system_key)
                    .u16(e.character)
                    .u16(e.unmodified_character)
                    .i32(e.focus_on_editable_field);
            }
            Command::TriggerEvent { id, name, args } => {
                w.u8(10).u32(*id).str(name);
                args.encode(&mut w);
            }
            Command::LoadUrl { id, url } => {
                w.u8(11).u32(*id).str(url);
            }
            Command::DevTools { id, enabled } => {
                w.u8(12).u32(*id).bool(*enabled);
            }
            Command::FrameRate { id, fps } => {
                w.u8(13).u32(*id).i32(*fps);
            }
            Command::AudioMuted { id, muted } => {
                w.u8(14).u32(*id).bool(*muted);
            }
            Command::Shutdown => {
                w.u8(15);
            }
            Command::BeginFrame { id } => {
                w.u8(16).u32(*id);
            }
            Command::SnapshotResult { req, png } => {
                w.u8(17).u32(*req).bytes(png);
            }
        }

        w.buf
    }

    pub fn decode(buf: &[u8]) -> io::Result<Command> {
        let mut r = Reader::new(buf);

        Ok(match r.u8()? {
            0 => Command::Hello { version: r.u32()? },
            1 => Command::CreateBrowser {
                id: r.u32()?,
                url: r.string()?,
                width: r.u32()?,
                height: r.u32()?,
                hidden: r.bool()?,
                audio: r.bool()?,
                external_frames: r.bool()?,
            },
            2 => Command::CloseBrowser { id: r.u32()?, force: r.bool()? },
            3 => Command::Resize { id: r.u32()?, width: r.u32()?, height: r.u32()? },
            4 => Command::Hide { id: r.u32()?, hidden: r.bool()? },
            5 => Command::Invalidate { id: r.u32()? },
            6 => Command::MouseMove { id: r.u32()?, x: r.i32()?, y: r.i32()?, modifiers: r.u32()? },
            7 => Command::MouseClick {
                id: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                modifiers: r.u32()?,
                button: r.u8()?,
                down: r.bool()?,
            },
            8 => Command::MouseWheel { id: r.u32()?, x: r.i32()?, y: r.i32()?, delta: r.i32()? },
            9 => Command::Key {
                id: r.u32()?,
                event: KeyEvent {
                    kind: r.i32()?,
                    modifiers: r.u32()?,
                    windows_key_code: r.i32()?,
                    native_key_code: r.i32()?,
                    is_system_key: r.i32()?,
                    character: r.u16()?,
                    unmodified_character: r.u16()?,
                    focus_on_editable_field: r.i32()?,
                },
            },
            10 => Command::TriggerEvent {
                id: r.u32()?,
                name: r.string()?,
                args: Value::decode(&mut r, 0)?,
            },
            11 => Command::LoadUrl { id: r.u32()?, url: r.string()? },
            12 => Command::DevTools { id: r.u32()?, enabled: r.bool()? },
            13 => Command::FrameRate { id: r.u32()?, fps: r.i32()? },
            14 => Command::AudioMuted { id: r.u32()?, muted: r.bool()? },
            15 => Command::Shutdown,
            16 => Command::BeginFrame { id: r.u32()? },
            17 => Command::SnapshotResult { req: r.u32()?, png: r.bytes()?.to_vec() },
            t => return Err(invalid(&format!("unknown command tag {}", t))),
        })
    }
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();

        match self {
            Event::Hello { version } => {
                w.u8(0).u32(*version);
            }
            Event::Ready => {
                w.u8(1);
            }
            Event::AfterCreated { id } => {
                w.u8(2).u32(*id);
            }
            Event::LoadEnd { id, status } => {
                w.u8(3).u32(*id).i32(*status);
            }
            Event::BeforeClose { id } => {
                w.u8(4).u32(*id);
            }
            Event::ProcessMessage { id, name, args } => {
                w.u8(5).u32(*id).str(name);
                encode_values(&mut w, args);
            }
            Event::FrameBuffer { id, name, capacity } => {
                w.u8(6).u32(*id).str(name).u32(*capacity);
            }
            Event::PopupShow { id, show } => {
                w.u8(7).u32(*id).bool(*show);
            }
            Event::PopupSize { id, x, y, width, height } => {
                w.u8(8).u32(*id).i32(*x).i32(*y).i32(*width).i32(*height);
            }
            Event::PopupPaint { id, width, height, pixels } => {
                w.u8(9).u32(*id).u32(*width).u32(*height).bytes(pixels);
            }
            Event::AudioStarted { id, stream, channels, sample_rate, frames_per_buffer } => {
                w.u8(10)
                    .u32(*id)
                    .i32(*stream)
                    .i32(*channels)
                    .i32(*sample_rate)
                    .i32(*frames_per_buffer);
            }
            Event::AudioPacket { id, stream, frames, pts, data } => {
                w.u8(11).u32(*id).i32(*stream).i32(*frames).i64(*pts).f32s(data);
            }
            Event::AudioStopped { id, stream } => {
                w.u8(12).u32(*id).i32(*stream);
            }
            Event::SnapshotRequest { req, spec } => {
                w.u8(13).u32(*req).str(spec);
            }
        }

        w.buf
    }

    pub fn decode(buf: &[u8]) -> io::Result<Event> {
        let mut r = Reader::new(buf);

        Ok(match r.u8()? {
            0 => Event::Hello { version: r.u32()? },
            1 => Event::Ready,
            2 => Event::AfterCreated { id: r.u32()? },
            3 => Event::LoadEnd { id: r.u32()?, status: r.i32()? },
            4 => Event::BeforeClose { id: r.u32()? },
            5 => Event::ProcessMessage {
                id: r.u32()?,
                name: r.string()?,
                args: decode_values(&mut r)?,
            },
            6 => Event::FrameBuffer { id: r.u32()?, name: r.string()?, capacity: r.u32()? },
            7 => Event::PopupShow { id: r.u32()?, show: r.bool()? },
            8 => Event::PopupSize {
                id: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.i32()?,
                height: r.i32()?,
            },
            9 => Event::PopupPaint {
                id: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                pixels: r.bytes()?.to_vec(),
            },
            10 => Event::AudioStarted {
                id: r.u32()?,
                stream: r.i32()?,
                channels: r.i32()?,
                sample_rate: r.i32()?,
                frames_per_buffer: r.i32()?,
            },
            11 => Event::AudioPacket {
                id: r.u32()?,
                stream: r.i32()?,
                frames: r.i32()?,
                pts: r.i64()?,
                data: r.f32s()?,
            },
            12 => Event::AudioStopped { id: r.u32()?, stream: r.i32()? },
            13 => Event::SnapshotRequest { req: r.u32()?, spec: r.string()? },
            t => return Err(invalid(&format!("unknown event tag {}", t))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrip() {
        let cmds = vec![
            Command::Hello { version: 7 },
            Command::CreateBrowser {
                id: 5,
                url: "file:///frontend/index.html".into(),
                width: 2560,
                height: 1440,
                hidden: true,
                audio: false,
                external_frames: true,
            },
            Command::Key {
                id: 1,
                event: KeyEvent { kind: 3, character: 0x0444, ..Default::default() },
            },
            Command::TriggerEvent {
                id: 2,
                name: "ev".into(),
                args: Value::List(vec![
                    Value::Int(-1),
                    Value::String("привет".into()),
                    Value::List(vec![Value::Bool(true), Value::Double(1.5), Value::Null]),
                ]),
            },
            Command::Shutdown,
            Command::BeginFrame { id: 9 },
            Command::SnapshotResult { req: 4, png: vec![0x89, b'P', b'N', b'G'] },
        ];

        for cmd in cmds {
            assert_eq!(Command::decode(&cmd.encode()).unwrap(), cmd);
        }
    }

    #[test]
    fn event_roundtrip() {
        let events = vec![
            Event::Ready,
            Event::ProcessMessage {
                id: 3,
                name: "emit_event".into(),
                args: vec![Value::String("x".into()), Value::Int(4)],
            },
            Event::FrameBuffer { id: 3, name: "orp_cef_1_3_0".into(), capacity: 1 << 20 },
            Event::AudioPacket { id: 1, stream: 2, frames: 2, pts: 99, data: vec![0.5, -0.5, 1.0, 0.0] },
            Event::PopupPaint { id: 1, width: 1, height: 1, pixels: vec![1, 2, 3, 4] },
            Event::SnapshotRequest { req: 4, spec: "es_15889_0.0_180.0_0.0_0.1_1.0_0.2_4_5_6".into() },
        ];

        for ev in events {
            assert_eq!(Event::decode(&ev.encode()).unwrap(), ev);
        }
    }

    #[test]
    fn truncated_is_error() {
        let buf = Command::LoadUrl { id: 1, url: "abc".into() }.encode();
        assert!(Command::decode(&buf[..buf.len() - 1]).is_err());
    }
}
