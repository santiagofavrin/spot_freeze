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
//! Permissions: Screen Recording (TCC) is REQUIRED, but the
//! `CGPreflightScreenCaptureAccess` preflight is only ADVISORY — it is a
//! known stale false-negative for rebuilt ad-hoc-signed binaries (each
//! re-sign changes the cdhash, so System Settings can show the grant while
//! the preflight says no). A false preflight triggers one
//! `CGRequestScreenCaptureAccess` — refreshing TCC state, and prompting on
//! a genuinely undetermined first run — and the capture proceeds
//! regardless: the real denial signal is ScreenCaptureKit's own result (a
//! user-declined / TCC-flavored listing error, or an empty display list
//! under a false preflight — see `is_permission_denial`). Only a genuine
//! denial produces the enable-and-restart guidance; every other failure
//! surfaces verbatim.
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
    CGImageByteOrderInfo, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
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

/// The `SCStream` error domain: ScreenCaptureKit reports a user-declined
/// capture here.
const SC_STREAM_ERROR_DOMAIN: &str = "com.apple.ScreenCaptureKit.SCStreamErrorDomain";

/// `SCStreamErrorUserDeclined` — the user (via TCC) refused screen capture.
const SC_STREAM_ERROR_USER_DECLINED: isize = -3801;

/// Field separator for the plain-data error encoding ([`encode_error`] /
/// [`decode_error`]); U+001F never appears in an NSError domain or code.
const ERROR_FIELD_SEP: char = '\u{1f}';

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
        // The TCC preflight is ADVISORY only: it is a known stale
        // false-negative for rebuilt ad-hoc-signed binaries (each re-sign
        // changes the cdhash, so System Settings can show the grant while
        // the preflight says no). A false preflight triggers one access
        // request — refreshing TCC state, and prompting on a genuinely
        // undetermined first run — but never gates the capture: a real
        // denial is detected from ScreenCaptureKit's own results (see
        // [`is_permission_denial`]).
        let preflight_ok = CGPreflightScreenCaptureAccess();
        if !preflight_ok {
            let _ = CGRequestScreenCaptureAccess();
        }
        let mtm = MainThreadMarker::new()
            .context("screen capture must run on the application's main thread")?;
        let screens = enumerate_screens(mtm);
        if screens.is_empty() {
            bail!("no displays found");
        }
        let displays = shareable_displays(preflight_ok)?;
        if displays.is_empty() && is_permission_denial(preflight_ok, true, None, None, None) {
            return Err(permission_error());
        }

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
///
/// A listing failure is classified against `preflight_ok`: a genuine
/// permission denial becomes the friendly enable-and-restart guidance,
/// anything else surfaces verbatim.
fn shareable_displays(preflight_ok: bool) -> Result<Retained<NSArray<SCDisplay>>> {
    let (tx, rx) = mpsc::channel::<Result<Retained<NSArray<SCDisplay>>, String>>();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if content.is_null() {
                Err(encode_error(error))
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
    let result = rx
        .recv_timeout(SCK_TIMEOUT)
        .map_err(|_| anyhow!("ScreenCaptureKit did not deliver the shareable content list"))?;
    result.map_err(|encoded| {
        let (domain, code, description) = decode_error(&encoded);
        if is_permission_denial(preflight_ok, false, domain, code, Some(description)) {
            permission_error()
        } else {
            anyhow!("listing shareable displays failed: {description}")
        }
    })
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

/// The friendly denial guidance. Surfaced only when ScreenCaptureKit itself
/// confirms the denial — never from the advisory preflight alone, which is
/// a known stale false-negative for rebuilt ad-hoc-signed binaries.
fn permission_error() -> anyhow::Error {
    anyhow!(
        "SpotFreeze does not have Screen Recording permission.\n\
         Enable it in System Settings → Privacy & Security → Screen Recording, \
         then restart SpotFreeze."
    )
}

/// Encode an `NSError` pointer (null-tolerant) as plain channel data:
/// `domain ␟ code ␟ human description`. Only plain data may cross the
/// completion-handler channel, so everything the permission classifier
/// needs is flattened into the string here and decoded back out on the
/// receiving thread by [`decode_error`].
fn encode_error(error: *mut NSError) -> String {
    if error.is_null() {
        return format!("{ERROR_FIELD_SEP}{ERROR_FIELD_SEP}unknown ScreenCaptureKit error");
    }
    let (domain, code) = {
        // SAFETY: non-null pointer to a valid NSError, borrowed for this scope.
        let error = unsafe { &*error };
        (nsstring_to_string(&error.domain()), error.code())
    };
    format!(
        "{domain}{ERROR_FIELD_SEP}{code}{ERROR_FIELD_SEP}{}",
        describe_error(error)
    )
}

/// Split an [`encode_error`] string back into `(domain, code, description)`.
/// A plain message with no separators decodes to `(None, None, message)`,
/// so hand-written errors still surface verbatim.
fn decode_error(encoded: &str) -> (Option<&str>, Option<isize>, &str) {
    let mut fields = encoded.splitn(3, ERROR_FIELD_SEP);
    let (Some(domain), Some(code), Some(description)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return (None, None, encoded);
    };
    (
        (!domain.is_empty()).then_some(domain),
        code.parse().ok(),
        description,
    )
}

/// Decide whether a ScreenCaptureKit failure means the user has declined
/// Screen Recording permission — as opposed to a transient failure that
/// should surface verbatim. Pure (no OS calls) so it is headless-testable.
///
/// Genuine-denial signals, most specific first:
/// - `SCStreamErrorUserDeclined` (-3801) in the `SCStream` error domain;
/// - an error domain or description pointing at TCC / "declined" /
///   "screen recording" (the wording varies across macOS releases);
/// - no error at all, but zero capturable displays while the (advisory)
///   preflight says permission is missing — how a denial surfaces on
///   systems that do not raise -3801.
fn is_permission_denial(
    preflight_ok: bool,
    displays_empty: bool,
    error_domain: Option<&str>,
    error_code: Option<isize>,
    error_description: Option<&str>,
) -> bool {
    if error_domain == Some(SC_STREAM_ERROR_DOMAIN)
        && error_code == Some(SC_STREAM_ERROR_USER_DECLINED)
    {
        return true;
    }
    let mentions_denial = |text: &str| {
        let text = text.to_ascii_lowercase();
        text.contains("tcc") || text.contains("declined") || text.contains("screen recording")
    };
    if error_domain.is_some_and(mentions_denial) || error_description.is_some_and(mentions_denial) {
        return true;
    }
    displays_empty && !preflight_ok
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
            crate::geometry::Rect::new(-1920, 0, 1920, 1080)
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

    #[test]
    fn permission_denial_detects_scstream_user_declined() {
        // The definitive signal — even a stale-TRUE preflight must not mask it.
        assert!(is_permission_denial(
            true,
            false,
            Some(SC_STREAM_ERROR_DOMAIN),
            Some(SC_STREAM_ERROR_USER_DECLINED),
            Some("The user declined TCC."),
        ));
    }

    #[test]
    fn permission_denial_detects_wording_variants() {
        // Domain/code vary across macOS releases; TCC-ish wording is enough.
        assert!(is_permission_denial(
            false,
            false,
            None,
            None,
            Some("The user declined TCC."),
        ));
        assert!(is_permission_denial(
            false,
            false,
            None,
            Some(-1),
            Some("Screen Recording is not allowed for this app."),
        ));
        assert!(is_permission_denial(
            false,
            false,
            Some("com.apple.TCC.error"),
            None,
            None,
        ));
    }

    #[test]
    fn permission_denial_detects_empty_displays_under_false_preflight() {
        assert!(is_permission_denial(false, true, None, None, None));
        // A true preflight means an empty list is NOT a denial signal.
        assert!(!is_permission_denial(true, true, None, None, None));
    }

    #[test]
    fn permission_denial_leaves_other_failures_alone() {
        // Same domain, different code: a real SCK failure, not a denial.
        assert!(!is_permission_denial(
            false,
            false,
            Some(SC_STREAM_ERROR_DOMAIN),
            Some(-3808),
            Some("The stream failed to start."),
        ));
        assert!(!is_permission_denial(
            false,
            false,
            None,
            None,
            Some("unknown ScreenCaptureKit error"),
        ));
        assert!(!is_permission_denial(true, false, None, None, None));
    }

    #[test]
    fn error_encoding_round_trips_through_decode() {
        let encoded = format!(
            "{SC_STREAM_ERROR_DOMAIN}{ERROR_FIELD_SEP}{SC_STREAM_ERROR_USER_DECLINED}{ERROR_FIELD_SEP}The user declined TCC."
        );
        let (domain, code, description) = decode_error(&encoded);
        assert_eq!(domain, Some(SC_STREAM_ERROR_DOMAIN));
        assert_eq!(code, Some(SC_STREAM_ERROR_USER_DECLINED));
        assert_eq!(description, "The user declined TCC.");
    }

    #[test]
    fn decode_error_tolerates_a_plain_message() {
        let (domain, code, description) = decode_error("unknown ScreenCaptureKit error");
        assert_eq!(domain, None);
        assert_eq!(code, None);
        assert_eq!(description, "unknown ScreenCaptureKit error");
    }
}
