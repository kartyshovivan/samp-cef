//! Shared-memory frame buffer for one browser.
//!
//! The host copies the dirty parts of every `OnPaint` into the section and accumulates
//! the dirty rects; the game uploads those rects into its D3D9 texture straight from the
//! section and clears them. Access is guarded by a spinlock stored in the section itself,
//! held only for the duration of a copy.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use winapi::shared::minwindef::FALSE;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::memoryapi::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use winapi::um::winnt::{HANDLE, PAGE_READWRITE};

const MAGIC: u32 = 0x4F52_5046; // "ORPF"
pub const MAX_RECTS: usize = 32;
/// Pixels start on their own page.
const HEADER_SIZE: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn full(width: u32, height: u32) -> Rect {
        Rect { x: 0, y: 0, width: width as i32, height: height as i32 }
    }

    /// Clips to `[0, width) x [0, height)`.
    pub fn clip(&self, width: u32, height: u32) -> Option<Rect> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.width).min(width as i32);
        let y1 = (self.y + self.height).min(height as i32);

        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }

    fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.width).max(other.x + other.width);
        let y1 = (self.y + self.height).max(other.y + other.height);
        Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }
}

#[repr(C)]
struct Header {
    magic: u32,
    lock: AtomicU32,
    /// Incremented by the host after every paint.
    seq: AtomicU32,
    capacity: u32,
    width: u32,
    height: u32,
    rect_count: u32,
    rects: [Rect; MAX_RECTS],
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_SIZE);

pub struct FrameBuffer {
    name: String,
    mapping: HANDLE,
    view: NonNull<u8>,
    capacity: usize,
}

// The section is process-shared memory guarded by its own spinlock.
unsafe impl Send for FrameBuffer {}
unsafe impl Sync for FrameBuffer {}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

impl FrameBuffer {
    /// Host side: creates a section able to hold `capacity` bytes of pixels.
    pub fn create(name: &str, capacity: usize) -> io::Result<FrameBuffer> {
        let total = HEADER_SIZE + capacity;
        let wname = wide(name);

        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null_mut(),
                PAGE_READWRITE,
                ((total as u64) >> 32) as u32,
                total as u32,
                wname.as_ptr(),
            )
        };

        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }

        let fb = Self::map(name, mapping, capacity)?;

        unsafe {
            let header = fb.header_ptr();
            (*header).capacity = capacity as u32;
            (*header).width = 0;
            (*header).height = 0;
            (*header).rect_count = 0;
            (*header).lock.store(0, Ordering::Relaxed);
            (*header).seq.store(0, Ordering::Relaxed);
            (*header).magic = MAGIC;
        }

        Ok(fb)
    }

    /// Game side: opens a section created by the host.
    pub fn open(name: &str) -> io::Result<FrameBuffer> {
        let wname = wide(name);
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, wname.as_ptr()) };

        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }

        // Map just the header first to learn the capacity.
        let probe = Self::map(name, mapping, 0)?;
        let (magic, capacity) = unsafe {
            let header = probe.header_ptr();
            ((*header).magic, (*header).capacity as usize)
        };
        probe.unmap_only();

        if magic != MAGIC {
            unsafe { CloseHandle(mapping) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cef-ipc: bad frame buffer"));
        }

        Self::map(name, mapping, capacity)
    }

    fn map(name: &str, mapping: HANDLE, capacity: usize) -> io::Result<FrameBuffer> {
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, HEADER_SIZE + capacity) };

        match NonNull::new(view as *mut u8) {
            Some(view) => Ok(FrameBuffer { name: name.to_string(), mapping, view, capacity }),
            None => {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(mapping) };
                Err(err)
            }
        }
    }

    /// Unmaps the view but keeps the mapping handle open (used while probing).
    fn unmap_only(self) -> HANDLE {
        unsafe { UnmapViewOfFile(self.view.as_ptr() as _) };
        let mapping = self.mapping;
        std::mem::forget(self);
        mapping
    }

    fn header_ptr(&self) -> *mut Header {
        self.view.as_ptr() as *mut Header
    }

    fn header(&self) -> &Header {
        unsafe { &*self.header_ptr() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Paint counter; cheap to poll without taking the lock.
    pub fn seq(&self) -> u32 {
        self.header().seq.load(Ordering::Acquire)
    }

    pub fn lock(&self) -> FrameGuard<'_> {
        let lock = &self.header().lock;
        let mut spins = 0u32;

        while lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;

            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }

        FrameGuard { fb: self }
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view.as_ptr() as _);
            CloseHandle(self.mapping);
        }
    }
}

pub struct FrameGuard<'a> {
    fb: &'a FrameBuffer,
}

impl FrameGuard<'_> {
    fn header(&self) -> &Header {
        unsafe { &*self.fb.header_ptr() }
    }

    fn header_mut(&mut self) -> &mut Header {
        unsafe { &mut *self.fb.header_ptr() }
    }

    fn pixels_ptr(&self) -> *mut u8 {
        unsafe { self.fb.view.as_ptr().add(HEADER_SIZE) }
    }

    pub fn width(&self) -> u32 {
        self.header().width
    }

    pub fn height(&self) -> u32 {
        self.header().height
    }

    pub fn rects(&self) -> &[Rect] {
        let header = self.header();
        &header.rects[..(header.rect_count as usize).min(MAX_RECTS)]
    }

    /// The current frame, `width * height * 4` bytes of BGRA.
    pub fn pixels(&self) -> &[u8] {
        let header = self.header();
        let len = (header.width as usize * header.height as usize * 4).min(self.fb.capacity);
        unsafe { std::slice::from_raw_parts(self.pixels_ptr(), len) }
    }

    pub fn clear_rects(&mut self) {
        self.header_mut().rect_count = 0;
    }

    fn push_rect(&mut self, rect: Rect) {
        let header = self.header_mut();
        let count = header.rect_count as usize;

        if count < MAX_RECTS {
            header.rects[count] = rect;
            header.rect_count += 1;
        } else {
            let merged = header.rects[..count].iter().fold(rect, |acc, r| acc.union(r));
            header.rects[0] = merged;
            header.rect_count = 1;
        }
    }

    /// Host side: stores an `OnPaint` buffer. Returns `false` if it doesn't fit.
    pub fn write_paint(&mut self, src: &[u8], width: u32, height: u32, dirty: &[Rect]) -> bool {
        let len = width as usize * height as usize * 4;

        if len == 0 || len > self.fb.capacity || src.len() < len {
            return false;
        }

        let dst = self.pixels_ptr();
        let resized = self.width() != width || self.height() != height;

        if resized {
            let header = self.header_mut();
            header.width = width;
            header.height = height;
            header.rect_count = 0;

            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len) };
            self.push_rect(Rect::full(width, height));
        } else {
            let pitch = width as usize * 4;

            for rect in dirty {
                let Some(rect) = rect.clip(width, height) else {
                    continue;
                };

                let x = rect.x as usize * 4;
                let row_len = rect.width as usize * 4;

                for y in rect.y as usize..(rect.y + rect.height) as usize {
                    let offset = y * pitch + x;
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.as_ptr().add(offset), dst.add(offset), row_len)
                    };
                }

                self.push_rect(rect);
            }
        }

        self.header().seq.fetch_add(1, Ordering::Release);
        true
    }
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        self.header().lock.store(0, Ordering::Release);
    }
}

/// Tile size used to find what really changed between two frames.
pub const TILE: usize = 64;

impl FrameBuffer {
    /// Game side: takes the lock only if it's free right now.
    pub fn try_lock(&self) -> Option<FrameGuard<'_>> {
        self.header()
            .lock
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| FrameGuard { fb: self })
    }

    /// Host side: stores an `OnPaint` buffer, publishing only the tiles that actually
    /// differ from the previous frame. CEF's `hint` rects bound the area to check (with
    /// GPU compositing CEF reports the whole view as dirty on every frame).
    ///
    /// The comparison runs without the lock: only the host ever writes pixels, so the
    /// previous frame is stable. The lock is held just for copying changed tiles.
    /// Returns the number of changed pixels, or `None` if the frame doesn't fit.
    pub fn write_paint_diff(&self, src: &[u8], width: u32, height: u32, hint: &[Rect]) -> Option<u64> {
        let len = width as usize * height as usize * 4;

        if len == 0 || len > self.capacity || src.len() < len {
            return None;
        }

        let (cur_w, cur_h) = {
            let header = self.header();
            (header.width, header.height)
        };

        if cur_w != width || cur_h != height {
            self.lock().write_paint(src, width, height, &[]);
            return Some(width as u64 * height as u64);
        }

        let changed = self.diff_tiles(src, width, height, hint);

        if changed.is_empty() {
            return Some(0);
        }

        let pixels = changed.iter().map(|r| r.width as u64 * r.height as u64).sum();
        self.lock().write_paint(src, width, height, &changed);
        Some(pixels)
    }

    /// Rects (tile aligned, clipped to the frame) where `src` differs from the stored frame.
    fn diff_tiles(&self, src: &[u8], width: u32, height: u32, hint: &[Rect]) -> Vec<Rect> {
        let (w, h) = (width as usize, height as usize);
        let tiles_x = w.div_ceil(TILE);
        let tiles_y = h.div_ceil(TILE);
        let pitch = w * 4;
        let dst = unsafe { std::slice::from_raw_parts(self.view.as_ptr().add(HEADER_SIZE), w * h * 4) };

        // Which tiles CEF says might have changed.
        let mut candidate = vec![false; tiles_x * tiles_y];
        for rect in hint.iter().filter_map(|r| r.clip(width, height)) {
            let tx0 = rect.x as usize / TILE;
            let ty0 = rect.y as usize / TILE;
            let tx1 = (rect.x + rect.width - 1) as usize / TILE;
            let ty1 = (rect.y + rect.height - 1) as usize / TILE;

            for ty in ty0..=ty1 {
                candidate[ty * tiles_x + tx0..=ty * tiles_x + tx1].fill(true);
            }
        }

        let mut rects: Vec<Rect> = Vec::new();
        // Runs from the previous tile row that may still grow downwards: (index in rects, tx0, tx1).
        let mut open: Vec<(usize, usize, usize)> = Vec::new();
        let mut changed = vec![false; tiles_x];

        for ty in 0..tiles_y {
            let y0 = ty * TILE;
            let y1 = (y0 + TILE).min(h);
            let mut next_open = Vec::new();

            let row_candidates = &candidate[ty * tiles_x..(ty + 1) * tiles_x];
            changed.fill(false);

            if row_candidates.iter().any(|&c| c) {
                // Span covering every candidate tile of this band.
                let first = row_candidates.iter().position(|&c| c).unwrap_or(0);
                let last = row_candidates.iter().rposition(|&c| c).unwrap_or(0);
                let span = first * TILE * 4..((last + 1) * TILE).min(w) * 4;

                for y in y0..y1 {
                    let row = y * pitch;

                    // Most rows don't change: one memcmp over the span skips them.
                    if src[row + span.start..row + span.end] == dst[row + span.start..row + span.end] {
                        continue;
                    }

                    for tx in first..=last {
                        if changed[tx] || !row_candidates[tx] {
                            continue;
                        }

                        let x0 = row + tx * TILE * 4;
                        let x1 = row + ((tx + 1) * TILE).min(w) * 4;
                        changed[tx] = src[x0..x1] != dst[x0..x1];
                    }
                }
            }

            let mut tx = 0;

            while tx < tiles_x {
                if !changed[tx] {
                    tx += 1;
                    continue;
                }

                let start = tx;
                while tx + 1 < tiles_x && changed[tx + 1] {
                    tx += 1;
                }
                let end = tx;
                tx += 1;

                let x = (start * TILE) as i32;
                let run_w = (((end + 1) * TILE).min(w) - start * TILE) as i32;

                // Same horizontal extent as a run right above: extend it downwards.
                if let Some(&(idx, ..)) = open.iter().find(|&&(_, a, b)| a == start && b == end) {
                    rects[idx].height += (y1 - y0) as i32;
                    next_open.push((idx, start, end));
                } else {
                    rects.push(Rect { x, y: y0 as i32, width: run_w, height: (y1 - y0) as i32 });
                    next_open.push((rects.len() - 1, start, end));
                }
            }

            open = next_open;
        }

        rects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_writes_client_reads() {
        let name = format!("cef_ipc_test_{}", std::process::id());
        let host = FrameBuffer::create(&name, 4 * 4 * 4).unwrap();
        let client = FrameBuffer::open(&name).unwrap();
        assert_eq!(client.capacity(), 64);

        let frame: Vec<u8> = (0..64).collect();
        assert!(host.lock().write_paint(&frame, 4, 4, &[]));
        assert_eq!(client.seq(), 1);

        {
            let mut g = client.lock();
            assert_eq!((g.width(), g.height()), (4, 4));
            assert_eq!(g.rects(), &[Rect::full(4, 4)]);
            assert_eq!(g.pixels(), &frame[..]);
            g.clear_rects();
        }

        let mut frame2 = frame.clone();
        frame2[4 * 4 + 4..4 * 4 + 8].copy_from_slice(&[9, 9, 9, 9]); // pixel (1,1)
        let dirty = [Rect { x: 1, y: 1, width: 1, height: 1 }];
        assert!(host.lock().write_paint(&frame2, 4, 4, &dirty));

        let g = client.lock();
        assert_eq!(g.rects(), &dirty);
        assert_eq!(g.pixels(), &frame2[..]);
    }

    #[test]
    fn diff_publishes_only_changed_tiles() {
        let name = format!("cef_ipc_test3_{}", std::process::id());
        let (w, h) = (300u32, 200u32);
        let fb = FrameBuffer::create(&name, (w * h * 4) as usize).unwrap();
        let full = [Rect::full(w, h)];

        let mut frame = vec![7u8; (w * h * 4) as usize];
        assert_eq!(fb.write_paint_diff(&frame, w, h, &full), Some((w * h) as u64));
        fb.lock().clear_rects();
        let seq = fb.seq();

        // Identical frame: nothing published, seq unchanged.
        assert_eq!(fb.write_paint_diff(&frame, w, h, &full), Some(0));
        assert_eq!(fb.seq(), seq);

        // Change one pixel at (130, 70) -> tile (2, 1) -> rect x 128..192, y 64..128.
        let off = (70 * w as usize + 130) * 4;
        frame[off] = 1;
        assert_eq!(fb.write_paint_diff(&frame, w, h, &full), Some(64 * 64));
        {
            let g = fb.lock();
            assert_eq!(g.rects(), &[Rect { x: 128, y: 64, width: 64, height: 64 }]);
            assert_eq!(g.pixels()[off], 1);
        }
        fb.lock().clear_rects();

        // A change outside CEF's hint is ignored (trust CEF's damage bound).
        frame[0] = 2;
        let hint = [Rect { x: 200, y: 100, width: 10, height: 10 }];
        assert_eq!(fb.write_paint_diff(&frame, w, h, &hint), Some(0));

        // Two vertically adjacent changed tiles with the same extent merge into one rect;
        // the right edge tile is clipped to the frame width (300 = 4*64 + 44).
        frame[(10 * w as usize + 299) * 4] = 3;
        frame[(130 * w as usize + 299) * 4] = 3;
        frame[(70 * w as usize + 299) * 4] = 3;
        fb.write_paint_diff(&frame, w, h, &full);
        let g = fb.lock();
        assert!(g.rects().contains(&Rect { x: 256, y: 0, width: 44, height: 192 }), "{:?}", g.rects());
    }

    #[test]
    fn try_lock_fails_while_locked() {
        let name = format!("cef_ipc_test4_{}", std::process::id());
        let fb = FrameBuffer::create(&name, 64).unwrap();
        let g = fb.lock();
        assert!(fb.try_lock().is_none());
        drop(g);
        assert!(fb.try_lock().is_some());
    }

    #[test]
    fn too_many_rects_collapse() {
        let name = format!("cef_ipc_test2_{}", std::process::id());
        let fb = FrameBuffer::create(&name, 100 * 100 * 4).unwrap();
        let frame = vec![0u8; 100 * 100 * 4];
        fb.lock().write_paint(&frame, 100, 100, &[]);
        fb.lock().clear_rects();

        let dirty: Vec<Rect> = (0..40).map(|i| Rect { x: i, y: i, width: 1, height: 1 }).collect();
        fb.lock().write_paint(&frame, 100, 100, &dirty);

        let g = fb.lock();
        assert!(g.rects().len() <= MAX_RECTS);
        let bbox = g.rects().iter().fold(g.rects()[0], |a, r| a.union(r));
        assert_eq!(bbox, Rect { x: 0, y: 0, width: 40, height: 40 });
    }
}
