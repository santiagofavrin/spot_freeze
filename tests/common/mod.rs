//! Shared helpers for SpotFreeze integration tests.
//!
//! Headless-safe and std-only: no windows, no real hotkeys, no clipboard, no
//! screen capture. Temp files live under `std::env::temp_dir()` with unique
//! names and are removed by [`TempDirGuard`] on drop (also on test panic).
#![allow(dead_code)]

use spotfreeze::capture::DibBuffer;
use spotfreeze::settings::model::Rgb;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// The default overlay veil color (`overlay.color` documented default: black).
pub const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Unique (per process + per call) temp directory path. Does NOT create it.
pub fn unique_temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "spotfreeze_itest_{}_{}_{}",
        tag,
        std::process::id(),
        n
    ))
}

/// RAII guard that recursively removes the temp directory on drop.
pub struct TempDirGuard(PathBuf);

impl TempDirGuard {
    /// Create a fresh unique temp directory and its cleanup guard.
    pub fn create(tag: &str) -> (PathBuf, TempDirGuard) {
        let dir = unique_temp_dir(tag);
        std::fs::create_dir_all(&dir).expect("create unique temp dir");
        let guard = TempDirGuard(dir.clone());
        (dir, guard)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a [`DibBuffer`] from a per-pixel generator returning `[B, G, R, A]`
/// (buffer-local coordinates, top-down row order — the crate pixel contract).
pub fn buffer_with(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> DibBuffer {
    let mut buf = DibBuffer::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let px = f(x, y);
            let off = (y * buf.stride + x * 4) as usize;
            buf.pixels[off..off + 4].copy_from_slice(&px);
        }
    }
    buf
}

/// Synthetic "monitor A" pattern: encodes the coordinate, fully opaque.
pub fn pattern_a(x: u32, y: u32) -> [u8; 4] {
    [
        (x & 0xFF) as u8,
        (y & 0xFF) as u8,
        ((x + y) & 0xFF) as u8,
        255,
    ]
}

/// Synthetic "monitor B" pattern: visibly different from [`pattern_a`], opaque.
pub fn pattern_b(x: u32, y: u32) -> [u8; 4] {
    [
        (255 - (x & 0xFF)) as u8,
        ((x * 3 + y) & 0xFF) as u8,
        ((y * 7 + 1) & 0xFF) as u8,
        255,
    ]
}

/// The documented darken formula from `overlay::composite::darken`:
/// `channel' = channel * (255 - dim_alpha) / 255` (integer truncation).
/// Equivalent to [`dim_color_channel`] with a black veil color.
pub fn darkened_channel(c: u8, dim_alpha: u8) -> u8 {
    (c as u32 * (255 - dim_alpha as u32) / 255) as u8
}

/// Fully darkened `[B, G, R, A]` pixel per the documented formula.
pub fn darkened_pixel(p: [u8; 4], dim_alpha: u8) -> [u8; 4] {
    [
        darkened_channel(p[0], dim_alpha),
        darkened_channel(p[1], dim_alpha),
        darkened_channel(p[2], dim_alpha),
        p[3], // alpha untouched
    ]
}

/// SPEC-ASSUMED colored-veil formula for the reworked
/// `overlay::composite::darken(buf, dim_alpha, color)` (SHARED API SPEC):
/// `channel' = (channel * (255 - dim_alpha) + color_channel * dim_alpha) / 255`
/// in ONE division (single truncation), mirroring the old black-veil floor
/// math (`color = black` reduces to it exactly) and giving
/// `dim_alpha = 255 => exactly the veil color`.
///
/// INTEGRATION FLAG: if the landed `darken` truncates the two terms
/// separately (`c*(255-a)/255 + color*a/255`), expectations computed with
/// this helper can be off by 1 — switch this helper to the landed formula
/// instead of weakening assertions.
pub fn dim_color_channel(c: u8, color_ch: u8, dim_alpha: u8) -> u8 {
    ((c as u32 * (255 - dim_alpha as u32) + color_ch as u32 * dim_alpha as u32) / 255) as u8
}

/// Colored-veil darkened `[B, G, R, A]` pixel per [`dim_color_channel`].
/// Buffer channels are BGRA: `color.b` blends into channel 0, `color.g`
/// into 1, `color.r` into 2; alpha untouched.
pub fn dimmed_pixel_with(p: [u8; 4], dim_alpha: u8, color: Rgb) -> [u8; 4] {
    [
        dim_color_channel(p[0], color.b, dim_alpha),
        dim_color_channel(p[1], color.g, dim_alpha),
        dim_color_channel(p[2], color.r, dim_alpha),
        p[3], // alpha untouched
    ]
}
