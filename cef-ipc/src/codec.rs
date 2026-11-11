//! Minimal little-endian binary encoding used by the protocol.

use std::io;

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(64) }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
        self
    }

    pub fn f32s(&mut self, v: &[f32]) -> &mut Self {
        self.u32(v.len() as u32);
        for x in v {
            self.buf.extend_from_slice(&x.to_le_bytes());
        }
        self
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "cef-ipc: message truncated")
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(eof)?;
        let slice = self.buf.get(self.pos..end).ok_or_else(eof)?;
        self.pos = end;
        Ok(slice)
    }

    fn arr<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> io::Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }

    pub fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }

    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }

    pub fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_le_bytes(self.arr()?))
    }

    pub fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn string(&mut self) -> io::Result<String> {
        let bytes = self.bytes()?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn f32s(&mut self) -> io::Result<Vec<f32>> {
        let len = self.u32()? as usize;
        let raw = self.take(len.checked_mul(4).ok_or_else(eof)?)?;
        Ok(raw
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }
}
