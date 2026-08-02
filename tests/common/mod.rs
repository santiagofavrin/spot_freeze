//! Shared helpers for SpotFreeze integration tests.
//!
//! Headless-safe and std-only: no windows, no real hotkeys, no clipboard, no
//! screen capture. Temp files live under `std::env::temp_dir()` with unique
//! names and are removed by [`TempDirGuard`] on drop (also on test panic).
//! The controller fakes ([`FakeFreeze`]) drive the real
//! [`OverlayController`] over in-memory captures and recording surfaces.
#![allow(dead_code)]

use anyhow::Result;
use spotfreeze::capture::{Capturer, DibBuffer, MonitorInfo};
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::controller::OverlayController;
use spotfreeze::overlay::events::OverlayEventSink;
use spotfreeze::platform::{OverlaySurface, PlatformServices, SurfaceFactory};
use spotfreeze::settings::model::{AppSettings, Rgb};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
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

// ---------------------------------------------------------------------------
// Controller fakes: drive the real OverlayController headless.
// ---------------------------------------------------------------------------

/// In-memory capturer: hands out clones of the configured captures.
pub struct FakeCapturer {
    pub captured: Vec<(MonitorInfo, DibBuffer)>,
}

impl Capturer for FakeCapturer {
    fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
        Ok(self.captured.clone())
    }
}

/// Overlay surface recording every presented frame in a shared log.
pub struct RecordingSurface {
    pub presents: Rc<RefCell<Vec<DibBuffer>>>,
}

impl OverlaySurface for RecordingSurface {
    fn present(&mut self, frame: &DibBuffer, _dirty: Option<Rect>) -> Result<()> {
        self.presents.borrow_mut().push(frame.clone());
        Ok(())
    }
}

/// Services double: fixed cursor position, clipboard writes recorded.
pub struct FakeServices {
    pub cursor: Point,
    pub copied: Rc<RefCell<Vec<DibBuffer>>>,
}

impl PlatformServices for FakeServices {
    fn cursor_position_virtual(&self) -> Option<Point> {
        Some(self.cursor)
    }

    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
        self.copied.borrow_mut().push(frame.clone());
        Ok(())
    }
}

/// Minimal monitor metadata for a rect (96 DPI, primary at the origin).
pub fn monitor_info(rect: Rect) -> MonitorInfo {
    MonitorInfo {
        rect,
        dpi_x: 96,
        dpi_y: 96,
        is_primary: rect.x == 0 && rect.y == 0,
        device_name: String::new(),
    }
}

/// A frozen session over fake monitors plus the recording handles.
pub struct FakeFreeze {
    pub controller: OverlayController,
    pub services: FakeServices,
    pub captured: Vec<(MonitorInfo, DibBuffer)>,
    /// Per-monitor presented frames (blend-path surfaces), in present order.
    pub presents: Vec<Rc<RefCell<Vec<DibBuffer>>>>,
    pub copied: Rc<RefCell<Vec<DibBuffer>>>,
}

impl FakeFreeze {
    /// Freeze over `captured` with `settings`, faking the live cursor at
    /// `cursor` (virtual-screen coordinates).
    pub fn new(
        captured: Vec<(MonitorInfo, DibBuffer)>,
        settings: &AppSettings,
        cursor: Point,
    ) -> Self {
        let presents: Vec<Rc<RefCell<Vec<DibBuffer>>>> = (0..captured.len())
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let copied = Rc::new(RefCell::new(Vec::new()));
        let mut session = Self {
            controller: OverlayController::new(),
            services: FakeServices {
                cursor,
                copied: copied.clone(),
            },
            captured,
            presents,
            copied,
        };
        session.refreeze(settings);
        session
    }

    /// Freeze again over the same fake monitors (recordings accumulate: the
    /// new surfaces share the handles). A no-op while already frozen.
    pub fn refreeze(&mut self, settings: &AppSettings) {
        let factory_presents = self.presents.clone();
        let factory = move |index: usize,
                            _rect: Rect,
                            _rects: Rc<Vec<Rect>>,
                            _sink: OverlayEventSink|
              -> Result<Box<dyn OverlaySurface>> {
            Ok(Box::new(RecordingSurface {
                presents: factory_presents[index].clone(),
            }))
        };
        let factory: &SurfaceFactory = &factory;
        self.controller
            .freeze(
                &FakeCapturer {
                    captured: self.captured.clone(),
                },
                settings,
                factory,
                &self.services,
            )
            .expect("freeze with fakes");
    }

    /// The last frame presented on `monitor` (panics when none).
    pub fn last_present(&self, monitor: usize) -> DibBuffer {
        self.presents[monitor]
            .borrow()
            .last()
            .expect("at least one present")
            .clone()
    }
}

/// `true` when the frame's 6 px border band is ENTIRELY white — the removed
/// mode-change flash's signature. Any single non-white band pixel clears the
/// frame (the amber capture indicator, the two-tone snip ring and the legend
/// never fill the whole band).
pub fn has_white_border_band(buf: &DibBuffer) -> bool {
    const BAND: u32 = 6;
    if buf.pixels.is_empty() {
        return false;
    }
    for y in 0..buf.height {
        for x in 0..buf.width {
            let in_band = x < BAND || y < BAND || x + BAND >= buf.width || y + BAND >= buf.height;
            if in_band && buf.pixel(x, y).unwrap() != [255, 255, 255, 255] {
                return false;
            }
        }
    }
    true
}
