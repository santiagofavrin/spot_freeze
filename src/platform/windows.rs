//! Windows adapters for the platform seam: [`OverlayWindow`] as an
//! [`OverlaySurface`] factory, and cursor/clipboard services over Win32.

use crate::capture::{DibBuffer, copy_dib_to_clipboard};
use crate::geometry::{Point, Rect};
use crate::overlay::events::OverlayEventSink;
use crate::overlay::window::OverlayWindow;
use crate::platform::{OverlaySurface, PlatformServices};
use anyhow::Result;
use std::rc::Rc;
use ::windows::Win32::Foundation::POINT;
use ::windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// [`SurfaceFactory`](crate::platform::SurfaceFactory) implementation: one
/// layered [`OverlayWindow`] per monitor.
pub fn create_overlay_surface(
    monitor_index: usize,
    monitor_rect: Rect,
    monitors: Rc<Vec<Rect>>,
    sink: OverlayEventSink,
) -> Result<Box<dyn OverlaySurface>> {
    Ok(Box::new(OverlayWindow::create(
        monitor_index,
        monitor_rect,
        monitors,
        sink,
    )?))
}

/// [`PlatformServices`] over `GetCursorPos` and the `CF_DIB` clipboard.
pub struct WindowsServices;

impl PlatformServices for WindowsServices {
    /// Current cursor position in virtual-screen coordinates; `None` on failure.
    fn cursor_position_virtual(&self) -> Option<Point> {
        let mut pt = POINT::default();
        // SAFETY: read-only query writing to a caller-provided POINT; touches no
        // window, hook, clipboard, or input state. Never called from tests.
        unsafe { GetCursorPos(&mut pt) }.ok()?;
        Some(Point::new(pt.x, pt.y))
    }

    /// `CF_DIB` clipboard copy (the maximally paste-compatible format).
    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
        copy_dib_to_clipboard(frame)
    }
}
