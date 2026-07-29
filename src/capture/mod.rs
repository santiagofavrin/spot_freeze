//! Screen capture: the [`DibBuffer`] pixel container, [`MonitorInfo`], and the
//! [`Capturer`] snapshot-source trait — all pure data, freely constructed in
//! tests. The Windows GDI implementation (enumeration, `BitBlt` capture,
//! `CF_DIB` clipboard) lives in `gdi`; PNG encoding lives in [`png`].

use crate::geometry::Rect;
use anyhow::Result;

#[cfg(windows)]
mod gdi;
pub mod png;

#[cfg(windows)]
pub use gdi::{GdiCapturer, copy_dib_to_clipboard, enumerate_monitors};

/// One attached display monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Bounds in VIRTUAL-SCREEN coordinates (primary monitor top-left is
    /// `(0, 0)`; secondary monitors may be negative), physical pixels.
    pub rect: Rect,
    /// Effective DPI along X (96 = 100% scaling). Informational only — captures
    /// and overlays are in physical pixels.
    pub dpi_x: u32,
    /// Effective DPI along Y.
    pub dpi_y: u32,
    pub is_primary: bool,
    /// Platform device/output name, e.g. `\\.\DISPLAY1` on Windows.
    pub device_name: String,
}

/// Owned 32-bit pixel buffer.
///
/// **Format contract: BGRA, 8 bits/channel, NON-premultiplied alpha, top-down
/// row order, tightly packed (`stride == width * 4`).** Screen captures are
/// opaque: every alpha byte is 255. Pixel `(x, y)` lives at
/// `pixels[y * stride + x * 4 ..][..4]` as `[B, G, R, A]`.
/// No OS handles — freely constructible in tests.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DibBuffer {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row; always `width * 4`.
    pub stride: u32,
    /// `len() == stride * height`.
    pub pixels: Vec<u8>,
}

impl DibBuffer {
    /// Zeroed buffer with `stride = width * 4` and `pixels.len() = stride * height`.
    pub fn new(width: u32, height: u32) -> Self {
        // `saturating_mul` only guards against absurd (non-monitor-sized) inputs;
        // real displays are far below the overflow threshold.
        let stride = width.saturating_mul(4);
        let len = (stride as usize).saturating_mul(height as usize);
        Self {
            width,
            height,
            stride,
            pixels: vec![0; len],
        }
    }

    /// `[B, G, R, A]` at `(x, y)` in buffer-local coordinates; `None` when out
    /// of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = (y as usize) * (self.stride as usize) + (x as usize) * 4;
        // One slice bounds check covers all four bytes.
        self.pixels
            .get(idx..idx + 4)
            .map(|px| [px[0], px[1], px[2], px[3]])
    }
}

/// Snapshot source. Injected into the overlay controller as `&dyn Capturer` so
/// tests can substitute in-memory fakes.
pub trait Capturer {
    /// Capture every monitor ONCE, returning one full-frame buffer per monitor
    /// in the same order as the platform's monitor enumeration. Each buffer's
    /// size equals its `MonitorInfo.rect` size in physical pixels.
    fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- DibBuffer::new ---------------------------------------------------

    #[test]
    fn new_has_expected_layout() {
        let b = DibBuffer::new(3, 2);
        assert_eq!(b.width, 3);
        assert_eq!(b.height, 2);
        assert_eq!(b.stride, 12);
        assert_eq!(b.pixels.len(), 24);
        assert!(b.pixels.iter().all(|&x| x == 0));
    }

    #[test]
    fn new_zero_sized_is_empty() {
        let b = DibBuffer::new(0, 0);
        assert_eq!(b.stride, 0);
        assert!(b.pixels.is_empty());
        let b2 = DibBuffer::new(5, 0);
        assert!(b2.pixels.is_empty());
    }

    // -- DibBuffer::pixel -------------------------------------------------

    #[test]
    fn pixel_reads_bgra_in_order() {
        let mut b = DibBuffer::new(2, 2);
        // Write pixel (1, 0) as [B, G, R, A] = [10, 20, 30, 40].
        let idx = 4; // y=0, x=1
        b.pixels[idx..idx + 4].copy_from_slice(&[10, 20, 30, 40]);
        assert_eq!(b.pixel(1, 0), Some([10, 20, 30, 40]));
        assert_eq!(b.pixel(0, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn pixel_out_of_bounds_returns_none() {
        let b = DibBuffer::new(2, 2);
        assert_eq!(b.pixel(2, 0), None); // x == width
        assert_eq!(b.pixel(0, 2), None); // y == height
        assert_eq!(b.pixel(9, 9), None);
    }
}
