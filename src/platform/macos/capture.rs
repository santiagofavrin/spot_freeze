//! Screen capture via ScreenCaptureKit: one one-shot
//! `SCScreenshotManager.captureImageWithFilter` per display, producing the
//! [`DibBuffer`] BGRA frames the overlay pipeline consumes.
//!
//! API choice: `SCScreenshotManager` (macOS 14) rather than an `SCStream`
//! first-frame grab. It is the purpose-built still-image API — no stream
//! setup, no delegate, no CVPixelBuffer locking — and its `CGImage` result is
//! documented as BGRA for SDR captures. objc2-screen-capture-kit 0.3.2 covers
//! it, so the `SCStream` fallback is unnecessary.
//!
//! Monitors are enumerated from `NSScreen.screens()` (index 0 is the primary
//! screen — the Cocoa global-origin screen), and each screen is matched to
//! its `SCDisplay` by `CGDirectDisplayID` (read from the screen's
//! `NSScreenNumber` device-description entry), so capture order, monitor
//! order, and the overlay-surface order are the same enumeration.
//!
//! Pixel path: the screenshot `CGImage` is expected to be 32-bit BGRA
//! (8 bpc × 4); its data-provider bytes are copied honoring `bytesPerRow`
//! and alpha is forced to 255 (screen content is opaque). Any other layout
//! falls back to redrawing into a BGRA `CGBitmapContext`.
//!
//! Permissions: Screen Recording (TCC) is REQUIRED. Checked up-front with
//! `CGPreflightScreenCaptureAccess`; on denial the error tells the user where
//! to enable it (the shell surfaces it — nothing here panics or prompts).
//!
//! Async: both SCK entry points are completion-handler based. The handlers
//! fire on SCK's own queue, so the main thread simply waits on a channel
//! with a timeout; the block converts pixels in place and ships a plain
//! `Result<DibBuffer, String>` back.

use crate::capture::{Capturer, DibBuffer, MonitorInfo};
use crate::platform::macos::coords::{CocoaRect, cocoa_rect_to_virtual};
use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::NSScreen;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGPreflightScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};
use std::sync::mpsc;
use std::time::Duration;

/// `kCVPixelFormatType_32BGRA` (`'BGRA'`): the pixel format requested from
/// ScreenCaptureKit, matching the [`DibBuffer`] memory layout.
const K_CV_PIXEL_FORMAT_32_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// `kCGImageAlphaPremultipliedFirst | kCGImageByteOrder32Little` — the
/// CoreGraphics bitmap description of a [`DibBuffer`]. With alpha forced to
/// 255 everywhere, premultiplied and non-premultiplied bytes are identical.
const BITMAP_INFO_BGRA: u32 =
    CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;

/// Upper bound for a ScreenCaptureKit round-trip before it is declared dead.
const SCK_TIMEOUT: Duration = Duration::from_secs(10);

/// One attached screen, flattened into value data (no AppKit objects, so the
/// other backend modules can hold it freely).
pub(crate) struct ScreenDesc {
    /// `CGDirectDisplayID` from the screen's `NSScreenNumber` entry; 0 when
    /// the entry is missing (never seen in practice — matching then fails
    /// loudly at capture time instead of silently picking a wrong display).
    pub display_id: u32,
    /// `NSScreen.frame` in Cocoa global points.
    pub frame: CocoaRect,
    /// `NSScreen.backingScaleFactor` (points → physical pixels).
    pub scale: f64,
    /// `true` for `screens()[0]` — the primary screen: the Cocoa global
    /// coordinate origin sits at ITS bottom-left corner. This is the Windows
    /// "primary monitor" analog; `NSScreen.mainScreen` (focus-tracking) is
    /// deliberately NOT used.
    pub is_primary: bool,
}

/// The primary screen's frame height in points — the y-flip reference for
/// every Cocoa ↔ virtual conversion (see [`crate::platform::macos::coords`]).
pub(crate) fn primary_height(screens: &[ScreenDesc]) -> f64 {
    screens.first().map_or(0.0, |s| s.frame.height)
}

/// `NSScreen.screens()` in AppKit order as plain data.
pub(crate) fn enumerate_screens(mtm: MainThreadMarker) -> Vec<ScreenDesc> {
    NSScreen::screens(mtm)
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let frame = screen.frame();
            ScreenDesc {
                display_id: display_id(&screen),
                frame: CocoaRect::new(
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height,
                ),
                scale: screen.backingScaleFactor(),
                is_primary: index == 0,
            }
        })
        .collect()
}

/// The screen's `CGDirectDisplayID`, from `deviceDescription["NSScreenNumber"]`.
/// (The key constant is not in the bindings; it is the literal string.)
fn display_id(screen: &NSScreen) -> u32 {
    screen
        .deviceDescription()
        .objectForKey(&NSString::from_str("NSScreenNumber"))
        .and_then(|obj| obj.downcast_ref::<NSNumber>().map(|n| n.unsignedIntValue()))
        .unwrap_or(0)
}

/// Build the [`MonitorInfo`] list (virtual-screen physical pixels) for an
/// enumerated screen set, in the same order.
fn monitor_infos(screens: &[ScreenDesc]) -> Vec<MonitorInfo> {
    let primary_height = primary_height(screens);
    screens
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let dpi = (96.0 * screen.scale).round() as u32;
            MonitorInfo {
                rect: cocoa_rect_to_virtual(screen.frame, screen.scale, primary_height),
                dpi_x: dpi,
                dpi_y: dpi,
                is_primary: screen.is_primary,
                device_name: if screen.display_id != 0 {
                    screen.display_id.to_string()
                } else {
                    format!("display-{index}")
                },
            }
        })
        .collect()
}

/// [`Capturer`] over ScreenCaptureKit. Stateless: every call enumerates
/// screens and shareable content fresh, so display hot-plug between freezes
/// is picked up.
pub struct MacCapturer;

impl MacCapturer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl Capturer for MacCapturer {
    /// One screenshot per display at native pixel size, in `NSScreen` order.
    fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
        if !CGPreflightScreenCaptureAccess() {
            bail!(
                "SpotFreeze does not have Screen Recording permission.\n\
                 Enable it in System Settings → Privacy & Security → Screen Recording, \
                 then restart SpotFreeze."
            );
        }
        let mtm = MainThreadMarker::new()
            .context("screen capture must run on the application's main thread")?;
        let screens = enumerate_screens(mtm);
        if screens.is_empty() {
            bail!("no displays found");
        }
        let displays = shareable_displays()?;

        let mut out = Vec::with_capacity(screens.len());
        for (screen, info) in screens.iter().zip(monitor_infos(&screens)) {
            let display = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == screen.display_id)
                .ok_or_else(|| {
                    anyhow!("display {} is not available for capture", info.device_name)
                })?;
            let width_px = (screen.frame.width * screen.scale).round() as usize;
            let height_px = (screen.frame.height * screen.scale).round() as usize;
            let frame = capture_display(&display, width_px, height_px)
                .with_context(|| format!("capturing display {}", info.device_name))?;
            out.push((info, frame));
        }
        Ok(out)
    }
}

/// Fetch `SCShareableContent.displays()` (desktop windows INCLUDED — the
/// freeze must show exactly what was on screen; only off-screen windows are
/// filtered out). Blocks the calling thread until SCK answers or times out.
fn shareable_displays() -> Result<Retained<NSArray<SCDisplay>>> {
    let (tx, rx) = mpsc::channel::<Result<Retained<NSArray<SCDisplay>>, String>>();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if content.is_null() {
                Err(describe_error(error))
            } else {
                // SAFETY: the completion handler hands over a valid object for the
                // duration of the call; `displays()` retains the array it returns,
                // which then crosses the channel.
                Ok(unsafe { (&*content).displays() })
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the block outlives the call: SCK retains the handler until the
    // completion has run, and we wait for that completion below.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &block,
        );
    }
    rx.recv_timeout(SCK_TIMEOUT)
        .map_err(|_| anyhow!("ScreenCaptureKit did not deliver the shareable content list"))?
        .map_err(|e| anyhow!("listing shareable displays failed: {e}"))
}

/// Capture one display at `width_px`×`height_px` physical pixels into a
/// [`DibBuffer`]. The one-shot manager runs the screenshot on its own queue
/// and calls back once; the pixel conversion happens inside the callback so
/// only plain data crosses the channel.
fn capture_display(display: &SCDisplay, width_px: usize, height_px: usize) -> Result<DibBuffer> {
    // SAFETY: all objects are valid, locally-owned instances; the empty
    // exclusion list means "capture the whole display".
    let (filter, config) = unsafe {
        let empty: Retained<NSArray<SCWindow>> = NSArray::from_slice(&[]);
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &empty,
        );
        let config = SCStreamConfiguration::new();
        config.setWidth(width_px);
        config.setHeight(height_px);
        config.setPixelFormat(K_CV_PIXEL_FORMAT_32_BGRA);
        config.setShowsCursor(false);
        (filter, config)
    };

    let (tx, rx) = mpsc::channel::<Result<DibBuffer, String>>();
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            Err(describe_error(error))
        } else {
            // SAFETY: the completion handler hands over a valid CGImage for
            // the duration of the call; the pixels are copied out here, so
            // nothing borrowed escapes.
            unsafe { cgimage_to_dib(&*image) }.map_err(|e| format!("{e:#}"))
        };
        let _ = tx.send(result);
    });
    // SAFETY: filter/config are alive for the call; SCK retains the handler
    // until the completion has run, and we wait for that completion below.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&block),
        );
    }
    rx.recv_timeout(SCK_TIMEOUT)
        .map_err(|_| anyhow!("ScreenCaptureKit timed out capturing a display"))?
        .map_err(|e| anyhow!("{e}"))
}

/// Copy a screenshot `CGImage` into a tightly-packed BGRA [`DibBuffer`].
///
/// Fast path (the documented SDR case: 8 bpc, 32 bpp): read the image's own
/// bytes via its data provider, honoring `bytesPerRow`. Fallback (any other
/// layout or an absent provider): redraw into a fresh BGRA bitmap context,
/// letting CoreGraphics convert. Both paths force A = 255 (screen content is
/// opaque and the crate's buffers are non-premultiplied).
///
/// # Safety
/// `image` must be a valid `CGImage` — trivially true for references.
unsafe fn cgimage_to_dib(image: &CGImage) -> Result<DibBuffer> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        bail!("ScreenCaptureKit returned an empty image");
    }
    let mut dib = DibBuffer::new(width as u32, height as u32);
    let stride = dib.stride as usize;

    let raw_ok = CGImage::bits_per_component(Some(image)) == 8
        && CGImage::bits_per_pixel(Some(image)) == 32
        && CGImage::bytes_per_row(Some(image)) >= width * 4;
    let copied = raw_ok
        && CGImage::data_provider(Some(image))
            .and_then(|provider| CGDataProvider::data(Some(&provider)))
            .is_some_and(|data| {
                // SAFETY: `data` is a +1 CFData we own and never mutate; the
                // slice is used only within this scope.
                let bytes = unsafe { data.as_bytes_unchecked() };
                let bytes_per_row = CGImage::bytes_per_row(Some(image));
                if bytes.len() < bytes_per_row * (height - 1) + width * 4 {
                    return false; // short buffer: fall through to the redraw path
                }
                for y in 0..height {
                    dib.pixels[y * stride..y * stride + width * 4]
                        .copy_from_slice(&bytes[y * bytes_per_row..y * bytes_per_row + width * 4]);
                }
                true
            });

    if !copied {
        let color_space =
            CGColorSpace::new_device_rgb().context("CGColorSpaceCreateDeviceRGB failed")?;
        // SAFETY: the context writes into `dib.pixels`, which outlives it
        // (the context is dropped at the end of this scope); the draw is
        // synchronous, so the buffer is complete before the alpha pass.
        let context = unsafe {
            CGBitmapContextCreate(
                dib.pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                stride,
                Some(&color_space),
                BITMAP_INFO_BGRA,
            )
        }
        .context("CGBitmapContextCreate failed")?;
        CGContext::draw_image(
            Some(&context),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: width as f64,
                    height: height as f64,
                },
            },
            Some(image),
        );
    }

    force_opaque(&mut dib.pixels);
    Ok(dib)
}

/// Force every alpha byte to 255 (BGRA buffers are non-premultiplied and
/// screen captures are opaque).
fn force_opaque(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 255;
    }
}

/// Human-readable description of an `NSError` pointer (null-tolerant).
fn describe_error(error: *mut NSError) -> String {
    if error.is_null() {
        return "unknown ScreenCaptureKit error".into();
    }
    // SAFETY: non-null pointer to a valid NSError, borrowed for this scope.
    let error = unsafe { &*error };
    let description = nsstring_to_string(&error.localizedDescription());
    if description.is_empty() {
        format!("ScreenCaptureKit error {}", error.code())
    } else {
        description
    }
}

/// `NSString` → `String` via `UTF8String` (empty on the rare nil return).
pub(crate) fn nsstring_to_string(s: &NSString) -> String {
    let ptr = s.UTF8String();
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: UTF8String returns a valid NUL-terminated C string owned by
    // the NSString, which outlives this call.
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    //! Headless-safe: pure pixel/constant logic only — no capture, no
    //! screens, no AppKit objects (those paths are exercised by the macOS CI
    //! job's build, not its unit tests).
    use super::*;

    #[test]
    fn pixel_format_constant_is_32bgra() {
        // 'BGRA' as a big-endian FourCC, per CoreVideo's pixel format list.
        assert_eq!(K_CV_PIXEL_FORMAT_32_BGRA, 0x4247_5241);
    }

    #[test]
    fn bitmap_info_is_bgra_little_endian_premultiplied_first() {
        assert_eq!(
            BITMAP_INFO_BGRA,
            CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0
        );
    }

    #[test]
    fn force_opaque_sets_every_alpha_byte() {
        let mut pixels = vec![1, 2, 3, 0, 5, 6, 7, 200, 9, 10, 11, 255];
        force_opaque(&mut pixels);
        assert_eq!(pixels, vec![1, 2, 3, 255, 5, 6, 7, 255, 9, 10, 11, 255]);
    }

    #[test]
    fn force_opaque_ignores_a_trailing_partial_pixel() {
        let mut pixels = vec![0, 0, 0, 0, 9, 9];
        force_opaque(&mut pixels);
        assert_eq!(pixels, vec![0, 0, 0, 255, 9, 9]);
    }

    #[test]
    fn monitor_info_uses_virtual_pixels_and_scaled_dpi() {
        let screens = vec![
            ScreenDesc {
                display_id: 1,
                frame: CocoaRect::new(0.0, 0.0, 1440.0, 900.0),
                scale: 2.0,
                is_primary: true,
            },
            ScreenDesc {
                display_id: 2,
                frame: CocoaRect::new(-1920.0, -180.0, 1920.0, 1080.0),
                scale: 1.0,
                is_primary: false,
            },
        ];
        let infos = monitor_infos(&screens);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].rect, crate::geometry::Rect::new(0, 0, 2880, 1800));
        assert_eq!(infos[0].dpi_x, 192);
        assert!(infos[0].is_primary);
        assert_eq!(infos[0].device_name, "1");
        assert_eq!(
            infos[1].rect,
            crate::geometry::Rect::new(-1920, 180, 1920, 1080)
        );
        assert_eq!(infos[1].dpi_x, 96);
        assert!(!infos[1].is_primary);
    }

    #[test]
    fn monitor_info_falls_back_to_indexed_name_without_display_id() {
        let screens = vec![
            ScreenDesc {
                display_id: 1,
                frame: CocoaRect::new(0.0, 0.0, 100.0, 100.0),
                scale: 1.0,
                is_primary: true,
            },
            ScreenDesc {
                display_id: 0,
                frame: CocoaRect::new(100.0, 0.0, 100.0, 100.0),
                scale: 1.0,
                is_primary: false,
            },
        ];
        let infos = monitor_infos(&screens);
        assert_eq!(infos[1].device_name, "display-1");
    }

    #[test]
    fn primary_height_is_the_first_screens_height() {
        let screens = vec![ScreenDesc {
            display_id: 1,
            frame: CocoaRect::new(0.0, 0.0, 1440.0, 900.0),
            scale: 2.0,
            is_primary: true,
        }];
        assert_eq!(primary_height(&screens), 900.0);
        assert_eq!(primary_height(&[]), 0.0);
    }
}
