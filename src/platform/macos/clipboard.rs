//! Clipboard and live-cursor services for the platform seam
//! ([`PlatformServices`]).
//!
//! Images go to the general pasteboard as PNG (`NSPasteboardTypePNG`) — the
//! format macOS apps accept most broadly — encoded by the shared
//! [`crate::capture::png::encode_png`]. The cursor query uses
//! `NSEvent.mouseLocation` (Cocoa global points) converted through
//! [`crate::platform::macos::coords`]; the scale of the screen the cursor is
//! on applies, falling back to the primary screen's when the cursor is
//! outside every screen (transient display-change states).

use crate::capture::DibBuffer;
use crate::geometry::Point;
use crate::platform::PlatformServices;
use crate::platform::macos::capture::{enumerate_screens, primary_height};
use crate::platform::macos::coords::{CocoaPoint, cocoa_point_to_virtual};
use anyhow::{Result, bail};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSPasteboard, NSPasteboardTypePNG};
use objc2_foundation::NSData;

/// [`PlatformServices`] over `NSPasteboard` and `NSEvent.mouseLocation`.
pub struct MacServices;

impl PlatformServices for MacServices {
    /// Current cursor position in virtual-screen physical pixels; `None`
    /// when not on the main thread or no screens exist.
    fn cursor_position_virtual(&self) -> Option<Point> {
        let mtm = MainThreadMarker::new()?;
        let screens = enumerate_screens(mtm);
        if screens.is_empty() {
            return None;
        }
        let cursor = NSEvent::mouseLocation();
        let cursor = CocoaPoint::new(cursor.x, cursor.y);
        let scale = screens
            .iter()
            .find(|s| s.frame.contains(cursor))
            .map_or(screens[0].scale, |s| s.scale);
        Some(cocoa_point_to_virtual(
            cursor,
            scale,
            primary_height(&screens),
        ))
    }

    /// PNG on the general pasteboard (replacing its previous contents).
    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
        let png = crate::capture::png::encode_png(frame)?;
        // SAFETY: the buffer is valid for the call; NSData copies the bytes.
        let data = unsafe { NSData::dataWithBytes_length(png.as_ptr().cast(), png.len()) };
        // SAFETY: NSPasteboardTypePNG is a valid, never-null AppKit global.
        let png_type = unsafe { NSPasteboardTypePNG };
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if pasteboard.setData_forType(Some(&data), png_type) {
            Ok(())
        } else {
            bail!("the pasteboard refused the PNG data")
        }
    }
}
