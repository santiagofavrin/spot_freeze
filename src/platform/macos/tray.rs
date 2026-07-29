//! Status-bar tray: an `NSStatusItem` with a runtime-drawn template icon and
//! a menu with "Edit Settings…" and "Exit SpotFreeze".
//!
//! Interaction idiom: the menu is set directly on the status item, so a
//! single click (either button) opens it. That is the standard AppKit status
//! item behavior; the alternative — left-click opens settings directly and
//! only right-click shows the menu — would need `popUpStatusItemMenu:`,
//! deprecated since macOS 10.14, plus manual event-mask plumbing. Windows
//! keeps its left-click shortcut; macOS follows the platform convention.
//!
//! The icon mirrors the Windows motif (light circle on a dark square) drawn
//! at runtime: an opaque dark rounded square with a TRANSPARENT circle,
//! rendered as a template image. A template `NSImage` draws its alpha channel
//! in the menu bar's current tint, so the circle reads as the "light" part
//! (the menu bar shows through) and the square takes the tint — the motif
//! survives both light and dark menu bar styles. The mask is a pure function
//! ([`icon_mask`]) with headless tests; only the CGImage/NSImage wrap is glue.
//!
//! The menu target is a small `NSObject` subclass whose action methods
//! forward into the app's `Rc<dyn Fn(TrayEvent)>` sink — the same shape the
//! Windows tray uses. `NSMenuItem.target` is NOT retained by AppKit, so the
//! delegate object is owned by [`MacTray`] for the app's lifetime.

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSSquareStatusItemLength, NSStatusBar, NSStatusItem,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{NSSize, NSString};
use std::ptr::NonNull;
use std::rc::Rc;

/// User intents reported by the tray (macOS set — see module docs for why
/// there is no separate left-click event).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayEvent {
    /// "Edit Settings…" chosen from the menu.
    MenuSettings,
    /// "Exit SpotFreeze" chosen from the menu. The tray itself never asks and
    /// never exits — the app runs its Yes/No confirmation flow.
    MenuExit,
}

/// Icon bitmap size in PIXELS (drawn at 22pt for the menu bar, @2x).
const ICON_PIXELS: usize = 44;
/// Icon size in points.
const ICON_POINTS: f64 = 22.0;
/// Circle radius as a fraction of the icon size (matches the Windows icon).
const CIRCLE_RADIUS_FRAC: f64 = 0.32;
/// Rounded-square corner radius as a fraction of the icon size.
const CORNER_RADIUS_FRAC: f64 = 0.22;

/// Action target for the tray menu, forwarding to the app's sink.
struct TrayIvars {
    sink: Rc<dyn Fn(TrayEvent)>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `TrayDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotFreezeTrayDelegate"]
    #[ivars = TrayIvars]
    struct TrayDelegate;

    impl TrayDelegate {
        #[unsafe(method(editSettings:))]
        fn edit_settings(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuSettings);
        }

        #[unsafe(method(exitApp:))]
        fn exit_app(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuExit);
        }
    }
);

impl TrayDelegate {
    fn new(mtm: MainThreadMarker, sink: Rc<dyn Fn(TrayEvent)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TrayIvars { sink });
        // SAFETY: `this` is a freshly allocated TrayDelegate; `init` is
        // NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// The status item plus the objects it depends on.
pub struct MacTray {
    status_item: Retained<NSStatusItem>,
    /// Keeps the menu action target alive (targets are not retained).
    _delegate: Retained<TrayDelegate>,
}

impl MacTray {
    /// Create the status item with the template icon, tooltip, and menu.
    pub fn create(
        mtm: MainThreadMarker,
        tooltip: &str,
        sink: Rc<dyn Fn(TrayEvent)>,
    ) -> Result<Self> {
        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
        let button = status_item
            .button(mtm)
            .context("the status item has no button")?;
        let image = make_icon_image()?;
        button.setImage(Some(&image));
        button.setToolTip(Some(&NSString::from_str(tooltip)));

        let delegate = TrayDelegate::new(mtm, sink);
        let target: &AnyObject = &delegate;
        let menu = NSMenu::new(mtm);
        // SAFETY: `delegate` is a valid action target and outlives the menu
        // (owned by this MacTray); the selectors exist on it.
        unsafe {
            let settings = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Edit Settings…"),
                Some(sel!(editSettings:)),
                &NSString::from_str(""),
            );
            settings.setTarget(Some(target));
            menu.addItem(&settings);
            let exit = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Exit SpotFreeze"),
                Some(sel!(exitApp:)),
                &NSString::from_str(""),
            );
            exit.setTarget(Some(target));
            menu.addItem(&exit);
        }
        status_item.setMenu(Some(&menu));

        Ok(Self {
            status_item,
            _delegate: delegate,
        })
    }

    /// Update the button tooltip (follows the live freeze binding).
    pub fn set_tooltip(&self, mtm: MainThreadMarker, tooltip: &str) {
        if let Some(button) = self.status_item.button(mtm) {
            button.setToolTip(Some(&NSString::from_str(tooltip)));
        }
    }

    /// Remove the icon from the status bar. Idempotent.
    pub fn remove(&self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
    }
}

impl Drop for MacTray {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Template-image mask, `size`×`size` BGRA: opaque dark rounded square with a
/// clear circle (alpha is all a template image renders; the gray matches the
/// Windows icon for non-template contexts). Pure — headless-tested.
fn icon_mask(size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size * size * 4];
    let center = (size as f64 - 1.0) / 2.0;
    let circle_radius = size as f64 * CIRCLE_RADIUS_FRAC;
    let corner_radius = size as f64 * CORNER_RADIUS_FRAC;
    for y in 0..size {
        for x in 0..size {
            let offset = (y * size + x) * 4;
            let in_circle = {
                let dx = x as f64 - center;
                let dy = y as f64 - center;
                dx * dx + dy * dy <= circle_radius * circle_radius
            };
            if rounded_rect_contains(x as f64 + 0.5, y as f64 + 0.5, size as f64, corner_radius)
                && !in_circle
            {
                // Channels equal, so BGRA vs RGBA order is irrelevant here.
                out[offset] = 32;
                out[offset + 1] = 32;
                out[offset + 2] = 32;
                out[offset + 3] = 255;
            }
        }
    }
    out
}

/// Point-in-rounded-rect test for a `size`×`size` square with corner radius
/// `radius` (distance to the nearest inner-bounds point ≤ radius).
fn rounded_rect_contains(x: f64, y: f64, size: f64, radius: f64) -> bool {
    let inner_x = x.clamp(radius, size - radius);
    let inner_y = y.clamp(radius, size - radius);
    let dx = x - inner_x;
    let dy = y - inner_y;
    dx * dx + dy * dy <= radius * radius
}

/// `CGDataProvider` release callback: frees the pixel box whose address was
/// passed as `info` (see [`make_icon_image`]).
///
/// # Safety
/// `info` must be the `Box<Vec<u8>>` pointer handed to `with_data`; called at
/// most once, when the provider's last reference dies.
unsafe extern "C-unwind" fn release_pixel_box(
    info: *mut std::ffi::c_void,
    _data: NonNull<std::ffi::c_void>,
    _size: usize,
) {
    // SAFETY: upheld by the caller contract above.
    drop(unsafe { Box::from_raw(info as *mut Vec<u8>) });
}

/// Wrap [`icon_mask`] in a template `NSImage`. The pixel buffer is OWNED by
/// the `CGDataProvider` from here on (its release callback frees the box),
/// which keeps the bytes alive for as long as any derived `CGImage`/`NSImage`
/// might read them — regardless of whether AppKit retained or copied the
/// image data. (`NSImage` is `AnyThread`, so no main-thread marker is needed.)
fn make_icon_image() -> Result<Retained<NSImage>> {
    let boxed = Box::into_raw(Box::new(icon_mask(ICON_PIXELS)));
    // SAFETY: `boxed` is a live Box<Vec<u8>>; its heap buffer address is
    // stable for the box's lifetime (the Vec is never grown).
    let (ptr, len) = unsafe { ((*boxed).as_ptr(), (*boxed).len()) };
    // SAFETY: `info`/`data` are valid for the provider's lifetime; the
    // release callback frees the box exactly once.
    let provider = unsafe {
        CGDataProvider::with_data(boxed.cast(), ptr.cast(), len, Some(release_pixel_box))
    };
    let Some(provider) = provider else {
        // SAFETY: provider creation failed, so the release callback will
        // never run; reclaim the box ourselves.
        drop(unsafe { Box::from_raw(boxed) });
        bail!("CGDataProviderCreateWithData failed for the tray icon");
    };
    let color_space =
        CGColorSpace::new_device_rgb().context("CGColorSpaceCreateDeviceRGB failed")?;
    // SAFETY: all arguments valid; `decode` stays null (no decode array).
    let image = unsafe {
        CGImage::new(
            ICON_PIXELS,
            ICON_PIXELS,
            8,
            32,
            ICON_PIXELS * 4,
            Some(&color_space),
            CGBitmapInfo(
                CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
            ),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .context("CGImageCreate failed for the tray icon")?;
    let ns_image = NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &image,
        NSSize {
            width: ICON_POINTS,
            height: ICON_POINTS,
        },
    );
    ns_image.setTemplate(true);
    Ok(ns_image)
}

#[cfg(test)]
mod tests {
    //! Headless-safe: the pure mask builder only — no AppKit objects.
    use super::*;

    fn alpha(mask: &[u8], size: usize, x: usize, y: usize) -> u8 {
        mask[(y * size + x) * 4 + 3]
    }

    #[test]
    fn mask_has_dark_square_and_clear_circle() {
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        // Center of the icon: inside the circle → transparent.
        assert_eq!(alpha(&mask, size, size / 2, size / 2), 0);
        // Mid-edge point: inside the square, outside the circle → opaque.
        assert_eq!(alpha(&mask, size, size / 2, 1), 255);
        assert_eq!(alpha(&mask, size, 1, size / 2), 255);
        // The opaque pixels are the dark gray of the Windows motif.
        let offset = (size + size / 2) * 4;
        assert_eq!(&mask[offset..offset + 4], &[32, 32, 32, 255]);
    }

    #[test]
    fn mask_corners_are_clear() {
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
            assert_eq!(
                alpha(&mask, size, x, y),
                0,
                "corner ({x},{y}) must be clear"
            );
        }
    }

    #[test]
    fn circle_radius_matches_the_windows_motif() {
        // Same relative radius as the Windows build_icon_masks (0.32×size):
        // a pixel just inside the radius is clear, one just outside opaque.
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        let center = (size as f64 - 1.0) / 2.0;
        let radius = size as f64 * CIRCLE_RADIUS_FRAC;
        let inside = (center - radius + 1.0).round() as usize;
        let outside = (center - radius - 1.0).round() as usize;
        assert_eq!(alpha(&mask, size, inside, size / 2), 0, "inside the circle");
        assert_eq!(
            alpha(&mask, size, outside, size / 2),
            255,
            "outside the circle"
        );
    }

    #[test]
    fn rounded_rect_math() {
        // Center band: everywhere inside.
        assert!(rounded_rect_contains(5.0, 2.0, 10.0, 2.0));
        assert!(rounded_rect_contains(5.0, 8.0, 10.0, 2.0));
        // Corner arc center is (2,2): the arc passes through (2,0) and (0,2).
        assert!(rounded_rect_contains(2.0, 0.5, 10.0, 2.0));
        // The far corner point is outside the arc.
        assert!(!rounded_rect_contains(0.1, 0.1, 10.0, 2.0));
    }

    #[test]
    fn mask_size_and_len_are_consistent() {
        let mask = icon_mask(16);
        assert_eq!(mask.len(), 16 * 16 * 4);
    }
}
