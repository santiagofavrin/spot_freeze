//! Windows GDI implementation of the capture seam: monitor enumeration,
//! `BitBlt` capture into owned DIB buffers, and `CF_DIB` clipboard export.
//!
//! # Implementation notes (for the integration stage)
//!
//! - **Capture source DC.** [`GdiCapturer::capture_all`] creates ONE source DC
//!   covering the *whole virtual screen* (`CreateDCW("DISPLAY", NULL, …)`) and
//!   `BitBlt`s each monitor out of its **virtual-screen rect** (which may be
//!   negative). This is the battle-tested multi-monitor approach (the same one
//!   Pillow / ZoomIt use): a whole-virtual-screen DC is unambiguously in
//!   virtual-screen coordinates, whereas the coordinate origin of a *per-device*
//!   DC (`CreateDCW` on a specific `\\.\DISPLAYn` name) is not consistently
//!   documented and could not be verified headless. It is also faster (one DC
//!   for all monitors). The per-monitor [`MonitorInfo::device_name`] is still
//!   populated for completeness / future per-device use.
//! - **Top-down DIB sections.** Each monitor is captured into a 32 bpp
//!   **top-down** DIB section (negative `biHeight`), matching [`DibBuffer`]'s
//!   row order exactly, so the pixels are copied out with a single contiguous
//!   `memcpy` (no `GetDIBits` round-trip, no row flip). `BitBlt` does not write
//!   a meaningful alpha channel, so every alpha byte is forced to `255`.
//! - **Clipboard.** [`copy_dib_to_clipboard`] packs a `CF_DIB` as a 40-byte
//!   `BITMAPINFOHEADER` + **bottom-up** BGRA pixels (the maximally compatible
//!   clipboard layout; see the function docs). `GlobalAlloc`/`GlobalLock`/
//!   `GlobalUnlock`/`GlobalFree` are declared via a private `extern "system"`
//!   block because the `Win32_System_Memory` feature is not enabled in the
//!   (frozen) `Cargo.toml`; this adds no new crate dependency.

use crate::capture::{Capturer, DibBuffer, MonitorInfo};
use crate::geometry::Rect;
use anyhow::{Context, Result, anyhow, bail};

/// List all attached monitors (virtual-screen bounds + per-monitor DPI).
///
/// Rects are in **virtual-screen** coordinates (primary top-left `(0, 0)`,
/// secondaries possibly negative), physical pixels. Order follows
/// `EnumDisplayMonitors`, which is also the order [`Capturer::capture_all`]
/// returns buffers in.
///
/// Only called from the Win32 code path — never from headless tests.
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
    use windows::core::BOOL;

    unsafe extern "system" fn enum_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        // SAFETY: `lparam.0` is the `*mut Vec<MonitorInfo>` we passed to
        // `EnumDisplayMonitors` below. It outlives the whole enumeration and is
        // only touched from this callback, on the same thread, so no aliasing.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a valid, correctly sized `MONITORINFOEXW`; the cast
        // to `*mut MONITORINFO` is valid because `MONITORINFOEXW` is `repr(C)`
        // and begins with a `MONITORINFO` member.
        let got = unsafe { GetMonitorInfoW(hmonitor, &mut info as *mut MONITORINFOEXW as *mut _) };
        if got.as_bool() {
            let rc = info.monitorInfo.rcMonitor;
            // Best effort: fall back to the 96-DPI baseline when unavailable.
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            let _ =
                unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
            out.push(MonitorInfo {
                rect: Rect::new(
                    rc.left,
                    rc.top,
                    (rc.right - rc.left) as u32,
                    (rc.bottom - rc.top) as u32,
                ),
                dpi_x,
                dpi_y,
                is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
                device_name: utf16_to_string(&info.szDevice),
            });
        }
        BOOL(1) // always continue enumeration
    }

    let mut monitors: Vec<MonitorInfo> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut monitors as *mut _ as isize),
        )
        .ok()
        .context("EnumDisplayMonitors failed")?;
    }
    Ok(monitors)
}

/// GDI `BitBlt` implementation of [`Capturer`] (fast, no DXGI complexity).
pub struct GdiCapturer {
    _private: (),
}

impl GdiCapturer {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for GdiCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl Capturer for GdiCapturer {
    fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
        use windows::Win32::Graphics::Gdi::CreateDCW;
        use windows::core::{PCWSTR, w};

        let monitors = enumerate_monitors()?;
        if monitors.is_empty() {
            return Ok(Vec::new());
        }

        // One source DC covering the WHOLE virtual screen; each monitor is then
        // BitBlt out of its virtual-screen rect (see module docs). All GDI
        // handles created below are released by their RAII guards even on error.
        // SAFETY: `src` is a valid display DC for the rest of this block.
        let src = unsafe {
            OwnedDc::new(CreateDCW(
                w!("DISPLAY"),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
            ))
            .context("CreateDCW(DISPLAY) failed")?
        };

        let mut out = Vec::with_capacity(monitors.len());
        for mon in &monitors {
            let buf = unsafe { capture_monitor(src.hdc(), mon) }
                .with_context(|| format!("capturing {}", mon.device_name))?;
            out.push((mon.clone(), buf));
        }
        Ok(out)
    }
}

/// Copy a frame to the clipboard as `CF_DIB` (converts our top-down BGRA buffer
/// to the bottom-up DIB the clipboard expects).
///
/// Clobbers the system clipboard — NEVER call from tests.
pub fn copy_dib_to_clipboard(dib: &DibBuffer) -> Result<()> {
    use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

    if dib.width == 0 || dib.height == 0 {
        bail!("cannot copy an empty DIB to the clipboard");
    }

    // Pure: build the packed CF_DIB payload (header + bottom-up pixels) up front.
    let packed = pack_cfdib(dib);

    // SAFETY: standard clipboard sequence. The clipboard is always closed again,
    // even if setting the data fails, via the explicit `CloseClipboard` below.
    unsafe {
        OpenClipboard(None).context("OpenClipboard failed")?;
        let result = set_clipboard_dib(&packed);
        let _ = CloseClipboard();
        result
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (no `windows` types) — exercised by the unit tests below.
// ---------------------------------------------------------------------------

/// Convert a NUL-terminated (or fully-used) UTF-16 buffer to a `String`,
/// stopping at the first NUL. Used for `MONITORINFOEXW.szDevice`.
fn utf16_to_string(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Size in bytes of a `BITMAPINFOHEADER`.
const BITMAPINFOHEADER_SIZE: usize = 40;
/// `BI_RGB` compression value (no compression).
const BI_RGB_VALUE: u32 = 0;
/// Clipboard format number for `CF_DIB` (`windows::Win32::System::Ole::CF_DIB`
/// is not behind an enabled feature; the value is a fixed Win32 constant).
const CF_DIB_VALUE: u32 = 8;

/// Pack a [`DibBuffer`] into a `CF_DIB` payload: a 40-byte little-endian
/// `BITMAPINFOHEADER` followed by BGRA pixel data in **bottom-up** row order
/// with every alpha byte forced to `255`.
///
/// Bottom-up (positive `biHeight`) is the traditional, most widely compatible
/// clipboard DIB layout — essentially every consumer of `CF_DIB` handles it,
/// whereas top-down DIBs trip up some older paste targets.
fn pack_cfdib(dib: &DibBuffer) -> Vec<u8> {
    let height = dib.height as usize;
    let stride = dib.stride as usize;
    let image_bytes = stride.saturating_mul(height);

    let mut out = Vec::with_capacity(BITMAPINFOHEADER_SIZE + image_bytes);

    // ---- BITMAPINFOHEADER (all fields little-endian) ----
    out.extend_from_slice(&(BITMAPINFOHEADER_SIZE as u32).to_le_bytes()); // biSize
    out.extend_from_slice(&(dib.width as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(dib.height as i32).to_le_bytes()); // biHeight (+ => bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&BI_RGB_VALUE.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(image_bytes as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // ---- pixel data, bottom-up (emit source rows in reverse order) ----
    for row in 0..height {
        let src_row = height - 1 - row;
        let start = src_row * stride;
        out.extend_from_slice(&dib.pixels[start..start + stride]);
    }

    // Force alpha to 255 (the buffer is opaque; BI_RGB readers ignore the byte,
    // but keeping it 0xFF avoids any reader that treats it as transparency).
    for px in out[BITMAPINFOHEADER_SIZE..].chunks_exact_mut(4) {
        px[3] = 255;
    }

    out
}

// ---------------------------------------------------------------------------
// Win32-only implementation details (never constructed in headless tests).
// ---------------------------------------------------------------------------

/// RAII guard that `DeleteDC`s a GDI device context on drop.
struct OwnedDc(windows::Win32::Graphics::Gdi::HDC);

impl OwnedDc {
    /// Wrap a raw HDC, failing if it is invalid (null / `-1`).
    fn new(hdc: windows::Win32::Graphics::Gdi::HDC) -> Result<Self> {
        if hdc.is_invalid() {
            Err(anyhow!("invalid HDC"))
        } else {
            Ok(Self(hdc))
        }
    }

    fn hdc(&self) -> windows::Win32::Graphics::Gdi::HDC {
        self.0
    }
}

impl Drop for OwnedDc {
    fn drop(&mut self) {
        use windows::Win32::Graphics::Gdi::DeleteDC;
        if !self.0.is_invalid() {
            // SAFETY: we own this DC; it has not been deleted elsewhere.
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

/// RAII guard that `DeleteObject`s a GDI bitmap on drop.
struct OwnedBitmap(windows::Win32::Graphics::Gdi::HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        use windows::Win32::Graphics::Gdi::DeleteObject;
        if !self.0.is_invalid() {
            // SAFETY: we own this bitmap; it is deselected before drop.
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }
}

/// Capture one monitor from the shared whole-virtual-screen source DC into an
/// opaque top-down [`DibBuffer`]. All intermediate GDI handles are released by
/// RAII guards regardless of the outcome.
///
/// `mon.rect` is in virtual-screen coordinates, matching `src`'s coordinate
/// space, so the `BitBlt` source origin is `(rect.x, rect.y)`.
///
/// SAFETY: `src` must be a valid display DC covering the virtual screen.
unsafe fn capture_monitor(
    src: windows::Win32::Graphics::Gdi::HDC,
    mon: &MonitorInfo,
) -> Result<DibBuffer> {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, GdiFlush, HGDIOBJ, SRCCOPY, SelectObject,
    };

    let w = mon.rect.width as i32;
    let h = mon.rect.height as i32;
    if w <= 0 || h <= 0 {
        return Ok(DibBuffer::new(0, 0));
    }

    // Memory DC compatible with the screen.
    let mem = OwnedDc::new(unsafe { CreateCompatibleDC(Some(src)) })
        .context("CreateCompatibleDC failed")?;

    // 32 bpp, TOP-DOWN (negative height) DIB section so its bits match
    // DibBuffer's row order and can be copied out contiguously.
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` is a valid BITMAPINFO; `bits` receives the section base.
    let hbmp = unsafe {
        CreateDIBSection(Some(src), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .context("CreateDIBSection failed")?
    };
    let hbmp = OwnedBitmap(hbmp);
    if bits.is_null() {
        bail!("CreateDIBSection returned a null bit pointer");
    }

    // Select the DIB into the memory DC, remembering the previous object so we
    // can restore it before the bitmap is deleted.
    // SAFETY: both handles are valid.
    let old: HGDIOBJ = unsafe { SelectObject(mem.hdc(), hbmp.0.into()) };

    // BitBlt the monitor's virtual rect from the shared virtual-screen DC.
    // SAFETY: valid source/destination DCs with a selected bitmap of `w`×`h`.
    let blt = unsafe {
        BitBlt(
            mem.hdc(),
            0,
            0,
            w,
            h,
            Some(src),
            mon.rect.x,
            mon.rect.y,
            SRCCOPY,
        )
    };
    // Restore the original object so `hbmp` can be safely deleted on drop.
    if !old.is_invalid() {
        unsafe { SelectObject(mem.hdc(), old) };
    }
    blt.context("BitBlt failed")?;

    // Ensure the blit has landed in the DIB before we read its memory.
    unsafe {
        let _ = GdiFlush();
    }

    // Copy the pixels out (contiguous: 32 bpp stride == width*4, matching
    // DibBuffer), then force alpha to 255 because BitBlt leaves it undefined.
    let mut buf = DibBuffer::new(mon.rect.width, mon.rect.height);
    // SAFETY: the DIB section is exactly `buf.pixels.len()` bytes (32 bpp).
    unsafe {
        std::ptr::copy_nonoverlapping(bits as *const u8, buf.pixels.as_mut_ptr(), buf.pixels.len());
    }
    for px in buf.pixels.chunks_exact_mut(4) {
        px[3] = 255;
    }

    Ok(buf)
}

/// `GMEM_MOVEABLE | GMEM_ZEROINIT` — required for clipboard `SetClipboardData`.
const GHND: u32 = 0x0042;

// `GlobalAlloc`/`GlobalLock`/`GlobalUnlock`/`GlobalFree` live behind the
// `Win32_System_Memory` feature, which is NOT enabled in the frozen Cargo.toml.
// Declare them directly against kernel32 (no new crate dependency). Raw
// pointers are used for the handle so no `windows`-type layout is relied on.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut std::ffi::c_void;
    fn GlobalLock(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(mem: *mut std::ffi::c_void) -> i32;
    fn GlobalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// Inner clipboard setter: assumes the clipboard is already open. On success the
/// system takes ownership of the global handle; on failure we free it ourselves.
///
/// SAFETY: must only be called between a successful `OpenClipboard` and the
/// matching `CloseClipboard`.
unsafe fn set_clipboard_dib(packed: &[u8]) -> Result<()> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};

    unsafe {
        EmptyClipboard().context("EmptyClipboard failed")?;

        let mem = GlobalAlloc(GHND, packed.len());
        if mem.is_null() {
            bail!("GlobalAlloc failed");
        }

        let dst = GlobalLock(mem);
        if dst.is_null() {
            let _ = GlobalFree(mem);
            bail!("GlobalLock failed");
        }
        // SAFETY: `dst` points to `packed.len()` freshly allocated bytes.
        std::ptr::copy_nonoverlapping(packed.as_ptr(), dst as *mut u8, packed.len());
        let _ = GlobalUnlock(mem);

        match SetClipboardData(CF_DIB_VALUE, Some(HANDLE(mem))) {
            Ok(_) => Ok(()), // the system owns `mem` now — do NOT free it
            Err(e) => {
                let _ = GlobalFree(mem);
                Err(e).context("SetClipboardData failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- utf16_to_string --------------------------------------------------

    #[test]
    fn utf16_stops_at_nul() {
        let buf: [u16; 5] = [u16::from(b'A'), u16::from(b'B'), 0, u16::from(b'C'), 0];
        assert_eq!(utf16_to_string(&buf), "AB");
    }

    #[test]
    fn utf16_without_nul_uses_whole_buffer() {
        let buf: [u16; 3] = [u16::from(b'X'), u16::from(b'Y'), u16::from(b'Z')];
        assert_eq!(utf16_to_string(&buf), "XYZ");
    }

    #[test]
    fn utf16_empty_and_all_nul() {
        assert_eq!(utf16_to_string(&[]), "");
        assert_eq!(utf16_to_string(&[0, 0, 0]), "");
    }

    #[test]
    fn utf16_decodes_device_name_shape() {
        // "\\.\DISPLAY1" as UTF-16, NUL-padded to 32 like MONITORINFOEXW.szDevice.
        let text = "\\\\.\\DISPLAY1";
        let mut buf = [0u16; 32];
        for (i, u) in text.encode_utf16().enumerate() {
            buf[i] = u;
        }
        assert_eq!(utf16_to_string(&buf), text);
    }

    // -- pack_cfdib -------------------------------------------------------

    /// 2×2 buffer with distinct, alpha != 255 bytes to catch ordering bugs.
    fn sample_2x2() -> DibBuffer {
        // rows top-down:
        // y=0: [1,2,3,4] [5,6,7,8]
        // y=1: [9,10,11,12] [13,14,15,16]
        DibBuffer {
            width: 2,
            height: 2,
            stride: 8,
            pixels: vec![
                1, 2, 3, 4, 5, 6, 7, 8, // row 0
                9, 10, 11, 12, 13, 14, 15, 16, // row 1
            ],
        }
    }

    #[test]
    fn pack_cfdib_total_length() {
        let packed = pack_cfdib(&sample_2x2());
        assert_eq!(packed.len(), BITMAPINFOHEADER_SIZE + 2 * 2 * 4);
    }

    #[test]
    fn pack_cfdib_header_fields() {
        let packed = pack_cfdib(&sample_2x2());
        let u32_at = |off: usize| u32::from_le_bytes(packed[off..off + 4].try_into().unwrap());
        let i32_at = |off: usize| i32::from_le_bytes(packed[off..off + 4].try_into().unwrap());
        let u16_at = |off: usize| u16::from_le_bytes(packed[off..off + 2].try_into().unwrap());

        assert_eq!(u32_at(0), BITMAPINFOHEADER_SIZE as u32); // biSize
        assert_eq!(i32_at(4), 2); // biWidth
        assert_eq!(i32_at(8), 2); // biHeight (positive => bottom-up)
        assert_eq!(u16_at(12), 1); // biPlanes
        assert_eq!(u16_at(14), 32); // biBitCount
        assert_eq!(u32_at(16), BI_RGB_VALUE); // biCompression
        assert_eq!(u32_at(20), 16); // biSizeImage
        assert_eq!(i32_at(24), 0); // biXPelsPerMeter
        assert_eq!(i32_at(28), 0); // biYPelsPerMeter
        assert_eq!(u32_at(32), 0); // biClrUsed
        assert_eq!(u32_at(36), 0); // biClrImportant
    }

    #[test]
    fn pack_cfdib_flips_rows_and_forces_alpha() {
        let packed = pack_cfdib(&sample_2x2());
        let px = &packed[BITMAPINFOHEADER_SIZE..];
        // First emitted row must be the source's BOTTOM row (y=1), alpha -> 255.
        assert_eq!(&px[0..8], &[9, 10, 11, 255, 13, 14, 15, 255]);
        // Second emitted row is the source's TOP row (y=0).
        assert_eq!(&px[8..16], &[1, 2, 3, 255, 5, 6, 7, 255]);
        // Every alpha byte is 255.
        assert!(px.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn pack_cfdib_single_pixel() {
        let dib = DibBuffer {
            width: 1,
            height: 1,
            stride: 4,
            pixels: vec![200, 100, 50, 0],
        };
        let packed = pack_cfdib(&dib);
        assert_eq!(packed.len(), BITMAPINFOHEADER_SIZE + 4);
        assert_eq!(&packed[BITMAPINFOHEADER_SIZE..], &[200, 100, 50, 255]);
    }
}
