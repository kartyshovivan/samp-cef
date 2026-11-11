//! Lightweight render statistics, logged every few seconds to `cef_client.log`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use parking_lot::Mutex;

static PAINTS: AtomicU64 = AtomicU64::new(0);
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);
static PAINT_GAP_MAX_US: AtomicU64 = AtomicU64::new(0);
static LAST_PAINT_US: AtomicU64 = AtomicU64::new(0);
static UPLOADS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_US: AtomicU64 = AtomicU64::new(0);
static UPLOAD_PX: AtomicU64 = AtomicU64::new(0);
static GAME_FRAMES: AtomicU64 = AtomicU64::new(0);

static START: Mutex<Option<Instant>> = Mutex::new(None);
static WINDOW: Mutex<Option<Instant>> = Mutex::new(None);

const WINDOW_SECS: f64 = 5.0;

fn now_us() -> u64 {
    let mut start = START.lock();
    start.get_or_insert_with(Instant::now).elapsed().as_micros() as u64
}

/// Game thread: one texture upload of `px` pixels took `us`; `seq` is the host's paint
/// counter for that browser, so the gap since the previous upload counts CEF frames.
pub fn upload(us: u64, px: u64, seq: u32) {
    let now = now_us();
    let last = LAST_PAINT_US.swap(now, Relaxed);

    if last != 0 {
        PAINT_GAP_MAX_US.fetch_max(now - last, Relaxed);
    }

    let prev = LAST_SEQ.swap(seq as u64, Relaxed);
    if prev != 0 && (seq as u64) > prev {
        PAINTS.fetch_add(seq as u64 - prev, Relaxed);
    }

    UPLOADS.fetch_add(1, Relaxed);
    UPLOAD_US.fetch_add(us, Relaxed);
    UPLOAD_PX.fetch_add(px, Relaxed);
}

/// Game thread, once per frame. Logs and resets the counters every `WINDOW_SECS`.
pub fn game_frame() {
    GAME_FRAMES.fetch_add(1, Relaxed);

    let mut window = WINDOW.lock();
    let started = *window.get_or_insert_with(Instant::now);
    let secs = started.elapsed().as_secs_f64();

    if secs < WINDOW_SECS {
        return;
    }

    *window = Some(Instant::now());

    let game = GAME_FRAMES.swap(0, Relaxed);
    let paints = PAINTS.swap(0, Relaxed);
    let gap = PAINT_GAP_MAX_US.swap(0, Relaxed);
    let uploads = UPLOADS.swap(0, Relaxed);
    let upload_us = UPLOAD_US.swap(0, Relaxed);
    let upload_px = UPLOAD_PX.swap(0, Relaxed);

    let avg = |total: u64, n: u64| if n == 0 { 0.0 } else { total as f64 / n as f64 / 1000.0 };

    log::info!(
        "[stats] game {:.0} fps | cef frames {:.0}/s | uploads {:.0}/s, max gap {:.0} ms, avg {:.2} ms, {:.1} Mpx/s",
        game as f64 / secs,
        paints as f64 / secs,
        uploads as f64 / secs,
        gap as f64 / 1000.0,
        avg(upload_us, uploads),
        upload_px as f64 / secs / 1_000_000.0,
    );
}
