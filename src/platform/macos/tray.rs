//! Status-bar tray: an `NSStatusItem` with a runtime-drawn template icon and
//! a menu with a disabled version line, "Spotlight", "Screenshot",
//! "Settings…", "Open Settings Folder", "Reload Settings", and
//! "Exit SpotFreeze".
//!
//! Interaction idiom: the menu is set directly on the status item, so a
//! single click (either button) opens it. That is the standard AppKit status
//! item behavior; the alternative — left-click opens settings directly and
//! only right-click shows the menu — would need `popUpStatusItemMenu:`,
//! deprecated since macOS 10.14, plus manual event-mask plumbing. Windows
//! keeps its left-click shortcut; macOS follows the platform convention.
//!
//! The icon is the "frost spotlight" motif drawn at runtime: an opaque dark
//! rounded square with the spotlight circle, a frost ring with radial ticks,
//! and a small sparkle knocked out TRANSPARENT, rendered as a template image.
//! A template `NSImage` draws its alpha channel in the menu bar's current
//! tint: the square takes the tint and the knocked-out shapes show the bare
//! menu bar — the motif survives both light and dark menu bar styles. The
//! mask is a pure function ([`icon_mask`]) with headless tests; only the
//! CGImage/NSImage wrap is glue.
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
    /// "Spotlight" chosen from the menu: freeze into spotlight mode, or
    /// activate the spotlight layer when already frozen.
    MenuSpotlight,
    /// "Screenshot" chosen from the menu: freeze first when unfrozen, then
    /// enter snip/capture mode.
    MenuScreenshot,
    /// "Settings…" chosen from the menu: open the native settings window.
    MenuSettings,
    /// "Open Settings Folder" chosen from the menu: reveal
    /// `spotfreeze.jsonc` in Finder (selected, in its folder).
    MenuOpenSettingsFolder,
    /// "Check for updates…" chosen from the menu.
    MenuUpdate,
    /// "Reload Settings" chosen from the menu: re-read the JSONC file
    /// immediately (a changed freeze binding is re-registered on the spot).
    MenuReloadSettings,
    /// "Exit SpotFreeze" chosen from the menu. The tray itself never asks and
    /// never exits — the app runs its Yes/No confirmation flow.
    MenuExit,
}

/// Icon bitmap size in PIXELS (drawn at 22pt for the menu bar, @2x).
const ICON_PIXELS: usize = 44;
/// Icon size in points.
const ICON_POINTS: f64 = 22.0;
/// Circle (spotlight hole) radius as a fraction of the icon size.
const CIRCLE_RADIUS_FRAC: f64 = 0.24;
/// Frost ring annulus around the circle, fractions of the icon size.
const RING_INNER_FRAC: f64 = 0.33;
const RING_OUTER_FRAC: f64 = 0.375;
/// Frost ticks: 4 radial notches on the axes, from RING outward.
const TICK_INNER_FRAC: f64 = 0.405;
const TICK_OUTER_FRAC: f64 = 0.46;
const TICK_HALF_WIDTH_FRAC: f64 = 0.023;
/// Sparkle: 4-point astroid at 45° upper-right, outside the ring.
const SPARKLE_DIST_FRAC: f64 = 0.46;
const SPARKLE_RADIUS_FRAC: f64 = 0.075;
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
        #[unsafe(method(spotlight:))]
        fn spotlight(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuSpotlight);
        }

        #[unsafe(method(screenshot:))]
        fn screenshot(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuScreenshot);
        }

        #[unsafe(method(editSettings:))]
        fn edit_settings(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuSettings);
        }

        #[unsafe(method(openSettingsFolder:))]
        fn open_settings_folder(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuOpenSettingsFolder);
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuUpdate);
        }

        #[unsafe(method(reloadSettings:))]
        fn reload_settings(&self, _sender: &AnyObject) {
            (self.ivars().sink)(TrayEvent::MenuReloadSettings);
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
    update_item: Retained<NSMenuItem>,
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
        let update_item;
        // SAFETY: `delegate` is a valid action target and outlives the menu
        // (owned by this MacTray); the selectors exist on it.
        unsafe {
            // Disabled version line (informational only): a nil action plus
            // an explicit setEnabled(false) keeps it grayed regardless of
            // the menu's autoenablesItems setting.
            let version = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&format!("SpotFreeze v{}", env!("CARGO_PKG_VERSION"))),
                None,
                &NSString::from_str(""),
            );
            version.setEnabled(false);
            menu.addItem(&version);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let spotlight = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Spotlight"),
                Some(sel!(spotlight:)),
                &NSString::from_str(""),
            );
            spotlight.setTarget(Some(target));
            menu.addItem(&spotlight);
            let screenshot = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Screenshot"),
                Some(sel!(screenshot:)),
                &NSString::from_str(""),
            );
            screenshot.setTarget(Some(target));
            menu.addItem(&screenshot);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let settings = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Settings…"),
                Some(sel!(editSettings:)),
                &NSString::from_str(""),
            );
            settings.setTarget(Some(target));
            menu.addItem(&settings);
            let open_folder = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Open Settings Folder"),
                Some(sel!(openSettingsFolder:)),
                &NSString::from_str(""),
            );
            open_folder.setTarget(Some(target));
            menu.addItem(&open_folder);
            let update = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Check for updates…"),
                Some(sel!(checkForUpdates:)),
                &NSString::from_str(""),
            );
            update.setTarget(Some(target));
            menu.addItem(&update);
            update_item = update;
            let reload = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Reload Settings"),
                Some(sel!(reloadSettings:)),
                &NSString::from_str(""),
            );
            reload.setTarget(Some(target));
            menu.addItem(&reload);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
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
            update_item,
            _delegate: delegate,
        })
    }

    /// Update the button tooltip (follows the live freeze binding).
    pub fn set_tooltip(&self, mtm: MainThreadMarker, tooltip: &str) {
        if let Some(button) = self.status_item.button(mtm) {
            button.setToolTip(Some(&NSString::from_str(tooltip)));
        }
    }

    /// Change the update action label and enabled state.
    pub fn set_update_state(&self, label: &str, enabled: bool) {
        self.update_item.setTitle(&NSString::from_str(label));
        self.update_item.setEnabled(enabled);
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

/// Template-image mask, `size`×`size` BGRA: opaque dark rounded square with
/// the spotlight circle, frost ring + ticks, and a small sparkle knocked out
/// (alpha is all a template image renders; the gray matches the Windows icon
/// for non-template contexts). Pure — headless-tested.
fn icon_mask(size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let coverage = mask_coverage(x, y, size);
            if coverage == 0.0 {
                continue;
            }
            let alpha = (coverage * 255.0).round() as u8;
            let offset = (y * size + x) * 4;
            // Channels equal, so BGRA vs RGBA order is irrelevant; the gray
            // is premultiplied against the coverage alpha.
            let gray = (32u32 * alpha as u32 / 255) as u8;
            out[offset] = gray;
            out[offset + 1] = gray;
            out[offset + 2] = gray;
            out[offset + 3] = alpha;
        }
    }
    out
}

/// Opaque coverage (0.0–1.0) of pixel (`x`, `y`), 4×4 supersampled: the
/// rounded square minus the knocked-out motif.
fn mask_coverage(x: usize, y: usize, size: usize) -> f64 {
    let mut hits = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            let px = x as f64 + (sx as f64 + 0.5) / 4.0;
            let py = y as f64 + (sy as f64 + 0.5) / 4.0;
            if mask_opaque_at(px, py, size as f64) {
                hits += 1;
            }
        }
    }
    hits as f64 / 16.0
}

/// `true` when the sample point is inside the rounded square and NOT inside
/// any knocked-out shape (circle, frost ring, ticks, sparkle).
fn mask_opaque_at(px: f64, py: f64, size: f64) -> bool {
    rounded_rect_contains(px, py, size, size * CORNER_RADIUS_FRAC) && !knocked_out_at(px, py, size)
}

/// `true` when the sample point is inside a knocked-out (transparent) shape.
fn knocked_out_at(px: f64, py: f64, size: f64) -> bool {
    let center = size / 2.0;
    let dx = px - center;
    let dy = py - center;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist <= size * CIRCLE_RADIUS_FRAC {
        return true; // spotlight hole
    }
    if (size * RING_INNER_FRAC..=size * RING_OUTER_FRAC).contains(&dist) {
        return true; // frost ring
    }
    // Frost ticks: 4 radial notches along the axes.
    let along = [dx, dy, -dx, -dy];
    let perp = [dy.abs(), dx.abs(), dy.abs(), dx.abs()];
    for i in 0..4 {
        if (size * TICK_INNER_FRAC..=size * TICK_OUTER_FRAC).contains(&along[i])
            && perp[i] <= size * TICK_HALF_WIDTH_FRAC
        {
            return true;
        }
    }
    // Sparkle: 4-point astroid at 45° upper-right.
    let diag = std::f64::consts::FRAC_1_SQRT_2;
    let sx = center + size * SPARKLE_DIST_FRAC * diag;
    let sy = center - size * SPARKLE_DIST_FRAC * diag;
    let a = size * SPARKLE_RADIUS_FRAC;
    let ux = ((px - sx).abs() / a).powf(2.0 / 3.0);
    let uy = ((py - sy).abs() / a).powf(2.0 / 3.0);
    ux + uy <= 1.0
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
        // Center of the icon: inside the spotlight hole → transparent.
        assert_eq!(alpha(&mask, size, size / 2, size / 2), 0);
        // Mid-edge points: inside the square, past the tick tips → opaque.
        assert_eq!(alpha(&mask, size, size / 2, 0), 255);
        assert_eq!(alpha(&mask, size, 0, size / 2), 255);
        // The fully opaque pixels are the dark gray of the Windows motif.
        let offset = (size / 2) * 4;
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
    fn frost_ring_is_knocked_out() {
        // Center 22.0 for size 44: the ring band spans radii
        // 0.33×44=14.52 .. 0.375×44=16.5, the opaque band between the circle
        // and the ring is fully dark.
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        // (34.5, 22.5): radius 12.5 — opaque band between circle and ring.
        assert_eq!(alpha(&mask, size, 34, 22), 255, "band must stay opaque");
        // (37.5, 22.5): radius 15.5 — inside the frost ring.
        assert_eq!(alpha(&mask, size, 37, 22), 0, "ring must be clear");
        // (9.5, 9.5): radius ~17-18 on the upper-left diagonal — past the
        // ring, off the tick axes, away from the sparkle.
        assert_eq!(alpha(&mask, size, 9, 9), 255, "diagonal must stay opaque");
    }

    #[test]
    fn frost_ticks_are_knocked_out_on_the_axes() {
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        // Tick band on the +x axis: radii 0.405×44=17.82 .. 0.46×44=20.24.
        assert_eq!(alpha(&mask, size, 41, 22), 0, "+x tick must be clear");
        assert_eq!(alpha(&mask, size, 3, 22), 0, "-x tick must be clear");
        assert_eq!(alpha(&mask, size, 22, 41), 0, "+y tick must be clear");
        assert_eq!(alpha(&mask, size, 22, 3), 0, "-y tick must be clear");
    }

    #[test]
    fn sparkle_sits_upper_right_only() {
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        // Sparkle center: (22, 22) + 0.46×44×(√2/2, -√2/2) ≈ (36.3, 7.7).
        assert_eq!(alpha(&mask, size, 36, 8), 0, "sparkle must be clear");
        // The mirrored spots stay opaque: the sparkle breaks the symmetry.
        assert_eq!(alpha(&mask, size, 7, 8), 255, "upper-left stays opaque");
        assert_eq!(alpha(&mask, size, 36, 36), 255, "lower-right stays opaque");
    }

    #[test]
    fn mask_edges_are_antialiased_and_premultiplied() {
        let size = ICON_PIXELS;
        let mask = icon_mask(size);
        // 4×4 supersampling must leave partially covered pixels on the shape
        // edges, and the gray must be premultiplied against the alpha.
        assert!(
            (0..size * size).any(|i| {
                let a = mask[i * 4 + 3];
                a > 0 && a < 255
            }),
            "expected partially covered edge pixels"
        );
        for i in 0..size * size {
            let a = mask[i * 4 + 3];
            let gray = (32 * a as u32 / 255) as u8;
            assert_eq!(mask[i * 4], gray, "B not premultiplied at pixel {i}");
            assert_eq!(mask[i * 4 + 1], gray, "G not premultiplied at pixel {i}");
            assert_eq!(mask[i * 4 + 2], gray, "R not premultiplied at pixel {i}");
        }
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
