//! Paint statistics for cef_host.log, written every few seconds while the UI paints.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use parking_lot::Mutex;

static PAINTS: AtomicU64 = AtomicU64::new(0);
static COPY_US: AtomicU64 = AtomicU64::new(0);
static COPY_MAX_US: AtomicU64 = AtomicU64::new(0);
static DIRTY_PX: AtomicU64 = AtomicU64::new(0);
static GAP_MAX_US: AtomicU64 = AtomicU64::new(0);

static STATE: Mutex<Option<(Instant, Instant)>> = Mutex::new(None);

const WINDOW_SECS: f64 = 5.0;

/// One `OnPaint` (view) delivered by CEF; `copy_us` is the time spent writing it to shm.
pub fn paint(copy_us: u64, dirty_px: u64) {
    PAINTS.fetch_add(1, Relaxed);
    COPY_US.fetch_add(copy_us, Relaxed);
    COPY_MAX_US.fetch_max(copy_us, Relaxed);
    DIRTY_PX.fetch_add(dirty_px, Relaxed);

    let now = Instant::now();
    let mut state = STATE.lock();
    let (window_start, last_paint) = state.get_or_insert((now, now));

    GAP_MAX_US.fetch_max(now.duration_since(*last_paint).as_micros() as u64, Relaxed);
    *last_paint = now;

    let secs = now.duration_since(*window_start).as_secs_f64();
    if secs < WINDOW_SECS {
        return;
    }
    *window_start = now;

    let paints = PAINTS.swap(0, Relaxed);
    let copy_us = COPY_US.swap(0, Relaxed);
    let copy_max = COPY_MAX_US.swap(0, Relaxed);
    let dirty = DIRTY_PX.swap(0, Relaxed);
    let gap = GAP_MAX_US.swap(0, Relaxed);

    log::info!(
        "[stats] paints {:.1}/s | copy avg {:.2} ms, max {:.2} ms | dirty {:.1} Mpx/s | max gap {:.0} ms",
        paints as f64 / secs,
        copy_us as f64 / paints.max(1) as f64 / 1000.0,
        copy_max as f64 / 1000.0,
        dirty as f64 / secs / 1_000_000.0,
        gap as f64 / 1000.0,
    );
}
