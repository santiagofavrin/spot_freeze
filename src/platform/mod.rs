//! Platform seam: the boundary between the portable core (overlay controller,
//! modes, compositing) and the per-OS shells. Every backend provides an
//! [`OverlaySurface`] implementation (how a composed frame reaches a monitor),
//! a [`SurfaceFactory`] creating one per monitor, and [`PlatformServices`]
//! (live cursor position + clipboard image export).
//!
//! The Windows adapters live in `windows`; the Wayland and macOS shells land
//! with their backends.

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect};
use crate::overlay::events::OverlayEventSink;
use anyhow::Result;
use std::rc::Rc;

#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod shared;
#[cfg(windows)]
pub mod windows;

/// One overlay surface covering exactly one monitor, presenting composed
/// frames in the [`DibBuffer`] format.
///
/// Dropping the surface closes it (window destroyed, layer surface unmapped);
/// the controller owns surfaces as `Box<dyn OverlaySurface>` and relies on
/// drop for teardown. Implementations may additionally expose an inherent
/// `close()` for explicit early teardown.
pub trait OverlaySurface {
    /// Re-composite from `frame`, which matches the surface's monitor rect in
    /// physical pixels. `dirty: Some(rect)` re-composites only that
    /// monitor-local region (the per-mouse-move fast path); `None` presents
    /// the full frame.
    fn present(&mut self, frame: &DibBuffer, dirty: Option<Rect>) -> Result<()>;

    /// `true` when a [`present`](Self::present) right now would complete
    /// without waiting. Platforms whose presentation is immediate (Windows,
    /// macOS) always report `true`; the Wayland surface reports whether a
    /// buffer slot is free, letting the controller defer repaints instead of
    /// blocking the UI thread on the compositor (input-backlog prevention).
    fn can_present(&mut self) -> bool {
        true
    }
}

/// Creates one [`OverlaySurface`] per monitor: `(monitor_index, monitor_rect,
/// all_monitor_rects, event_sink)`. `all_monitor_rects` lists every monitor's
/// virtual-screen rect in index order (shared, immutable) so the surface can
/// route focus-delivered input to the monitor actually under the cursor.
pub type SurfaceFactory =
    dyn Fn(usize, Rect, Rc<Vec<Rect>>, OverlayEventSink) -> Result<Box<dyn OverlaySurface>>;

/// OS services the portable controller needs.
pub trait PlatformServices {
    /// Current cursor position in virtual-screen coordinates; `None` when the
    /// platform cannot report it (e.g. Wayland has no global cursor query —
    /// the controller tolerates `None`).
    fn cursor_position_virtual(&self) -> Option<Point>;

    /// Copy a frame to the system clipboard as an image (`CF_DIB` on Windows,
    /// PNG elsewhere — see [`crate::capture::png::encode_png`]).
    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()>;
}
