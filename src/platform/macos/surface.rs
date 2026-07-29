//! Overlay surfaces: one borderless `NSWindow` per screen presenting composed
//! [`DibBuffer`] frames from a persistent BGRA backing store, plus AppKit
//! input delivery as [`OverlayEvent`]s.
//!
//! Window configuration: `NSBorderlessWindowMask` content sized to the
//! screen's frame, `NSBackingStoreBuffered` (the retained mode the task plan
//! mentioned is deprecated in the SDK; buffered is the standard choice and
//! identical for us — the pixels live in OUR store, the window backing only
//! ever receives complete `drawRect:` passes), window level
//! `CGShieldingWindowLevel()` and collection behavior
//! `canJoinAllSpaces | stationary | fullScreenAuxiliary`.
//!
//! Level choice: the shielding level is what the system uses for its
//! whole-screen shield window — it covers every application window, the
//! Dock, and the menu bar. `NSScreenSaverWindowLevel` (1000) is the
//! alternative, but other apps can (and do) place windows at or above it;
//! nothing user-land puts windows above the shield.
//!
//! Rendering: `present()` memcpy's the full frame or just the dirty rows
//! (the per-mouse-move spotlight fast path) into a persistent
//! `width*height*4` BGRA buffer owned by the content view, invalidates the
//! matching view rect, and calls `displayIfNeeded()` so the draw happens
//! SYNCHRONOUSLY. The synchronous part is load-bearing: the controller's
//! border flash sleeps the main thread between presents, so a deferred
//! AppKit display cycle would collapse the flash into a single frame.
//! `drawRect:` wraps the store in a zero-copy `CGImage` (a `CGDataProvider`
//! over the live buffer — no pixel copy on the draw path) clipped to the
//! dirty rect. The view keeps the default (unflipped, y-up) coordinate
//! system: `CGContextDrawImage` draws top-down CGImages right-side up there,
//! and all y-flip math lives in [`crate::platform::macos::coords`]. The
//! buffer's `premultipliedFirst` flag is exact: A = 255 everywhere, so
//! premultiplied and non-premultiplied bytes are identical.
//!
//! Input: one local `NSEvent` monitor per surface (mouse moved/dragged,
//! left button down/up, scroll wheel, key down), filtered to events whose
//! window is this surface's window. Local monitors — instead of view
//! overrides — were chosen because they need no first-responder dance: a
//! borderless window refuses key status by default, and subclassing the view
//! alone does not fix that; with monitors only the WINDOW must become key
//! (see [`OverlayWindow`]) and all event kinds flow through one mechanism.
//! No tracking area either: `acceptsMouseMovedEvents = true` makes the
//! window server post `mouseMoved` for the window under the cursor, which
//! the monitor then observes — one less per-resize object to maintain.
//! Scroll events route by cursor location under AppKit, so per-window
//! filtering already delivers them to the right monitor (no focus-reroute
//! like Win32's `WM_MOUSEWHEEL`; the factory's `all_monitor_rects` argument
//! is therefore unused here). `LeftMouseDragged` is reported as
//! `MouseMove` — that is how a snip drag keeps tracking with the button
//! held. Key events are SWALLOWED (the monitor returns nil): the overlay is
//! modal while frozen.
//!
//! Keyboard layout limitation (v1): `CGKeyCode` is a physical-position code
//! of an ANSI QWERTY layout, and the `CGKeyCode`→VK table in
//! [`crate::hotkeys::keymap`] is positional too. On non-QWERTY layouts
//! (Dvorak, AZERTY, …) frozen-mode bindings therefore follow PHYSICAL keys,
//! not printed characters: a binding of `Z` fires on the key where Z sits on
//! ANSI QWERTY.
//!
//! Everything here is main-thread-only AppKit glue; the pure pieces (wheel
//! normalization, modifier mapping, dirty-rect clamping) are free functions
//! with headless unit tests.

use crate::capture::DibBuffer;
use crate::geometry::Rect;
use crate::hotkeys::gesture::Modifiers;
use crate::hotkeys::keymap;
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::platform::OverlaySurface;
use crate::platform::macos::capture::{ScreenDesc, enumerate_screens, primary_height};
use crate::platform::macos::coords::{
    CocoaPoint, cocoa_rect_to_virtual, monitor_local_to_view_rect, view_point_to_monitor_local,
};
use anyhow::{Context, Result, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSGraphicsContext, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo, CGShieldingWindowLevel,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

/// The content view's state: the persistent frame store plus the
/// points-per-pixel scale needed to size the drawn image.
struct ViewIvars {
    store: RefCell<BackingStore>,
    scale: f64,
}

/// Persistent BGRA pixel store the composed frames are presented into.
/// Layout matches the [`DibBuffer`] contract (stride = width*4, top-down).
struct BackingStore {
    width: usize,
    height: usize,
    stride: usize,
    pixels: Vec<u8>,
}

define_class!(
    // SAFETY:
    // - The superclass NSView has no subclassing requirements; only
    //   `drawRect:` and `acceptsFirstResponder` are overridden.
    // - `OverlayView` does not implement `Drop`.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotFreezeOverlayView"]
    #[ivars = ViewIvars]
    struct OverlayView;

    impl OverlayView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty_rect: NSRect) {
            self.draw_store(dirty_rect);
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }
    }
);

impl OverlayView {
    /// Allocate and `initWithFrame:` a view with its backing store.
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        store: BackingStore,
        scale: f64,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            store: RefCell::new(store),
            scale,
        });
        // SAFETY: `this` is a freshly allocated OverlayView; initWithFrame:
        // is NSView's designated initializer for programmatic creation.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// `drawRect:` body: clip to the dirty region and draw the store's
    /// zero-copy wrapper image over the full view. The view is unflipped
    /// (y-up), where `CGContextDrawImage` renders top-down image data
    /// right-side up — matching the store's top-down rows.
    fn draw_store(&self, dirty_rect: NSRect) {
        let ivars = self.ivars();
        let store = ivars.store.borrow();
        let Some(context) = NSGraphicsContext::currentContext().map(|c| c.CGContext()) else {
            return;
        };
        let Some(color_space) = CGColorSpace::new_device_rgb() else {
            return;
        };
        // SAFETY: the provider wraps the store's live buffer with NO release
        // callback — the buffer is owned by `store`, which outlives both the
        // provider and the image (both die in this scope, before the borrow
        // ends). The draw consumes the bytes synchronously.
        let provider = unsafe {
            CGDataProvider::with_data(
                std::ptr::null_mut(),
                store.pixels.as_ptr().cast(),
                store.pixels.len(),
                None,
            )
        };
        let Some(provider) = provider else { return };
        // SAFETY: every argument is valid for the call; `decode` stays null
        // (no decode array) as allowed by CGImageCreate.
        let image = unsafe {
            CGImage::new(
                store.width,
                store.height,
                8,
                32,
                store.stride,
                Some(&color_space),
                CGBitmapInfo(
                    CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
                ),
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        };
        let Some(image) = image else { return };
        CGContext::clip_to_rect(Some(&context), dirty_rect);
        CGContext::draw_image(
            Some(&context),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: store.width as f64 / ivars.scale,
                    height: store.height as f64 / ivars.scale,
                },
            },
            Some(&image),
        );
    }
}

define_class!(
    // SAFETY:
    // - The superclass NSWindow has no subclassing requirements; only the
    //   two key/main-window eligibility overrides are added (a plain
    //   borderless NSWindow refuses key status, and key status is required
    //   for the app to receive keyboard events while frozen).
    // - `OverlayWindow` does not implement `Drop`.
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotFreezeOverlayWindow"]
    struct OverlayWindow;

    impl OverlayWindow {
        #[unsafe(method(canBecomeKey))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMain))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

/// One borderless overlay window + its event monitor.
pub struct MacOverlaySurface {
    window: Retained<OverlayWindow>,
    view: Retained<OverlayView>,
    /// The local event monitor registration; removed in `Drop`.
    event_monitor: Retained<AnyObject>,
    /// Physical-pixel size of the backing store.
    width: usize,
    height: usize,
    /// View height in points — the y-flip reference for event coordinates.
    view_height: f64,
    /// Points per physical pixel scale of the screen.
    scale: f64,
}

impl MacOverlaySurface {
    fn new(
        mtm: MainThreadMarker,
        monitor: usize,
        rect: Rect,
        screen: &ScreenDesc,
        sink: OverlayEventSink,
    ) -> Result<Self> {
        let width = rect.width as usize;
        let height = rect.height as usize;
        let scale = screen.scale;
        let view_height = screen.frame.height;

        let store = BackingStore {
            width,
            height,
            stride: width * 4,
            pixels: vec![0; width * 4 * height],
        };
        let view = OverlayView::new(
            mtm,
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: screen.frame.width,
                    height: screen.frame.height,
                },
            },
            store,
            scale,
        );

        let content_rect = NSRect {
            origin: NSPoint {
                x: screen.frame.x,
                y: screen.frame.y,
            },
            size: NSSize {
                width: screen.frame.width,
                height: screen.frame.height,
            },
        };
        // SAFETY: freshly allocated window; the initializer is the documented
        // one for programmatically created windows. `set_ivars(())` yields the
        // `PartialInit` a super-send init requires (no ivars on this class).
        let this = OverlayWindow::alloc(mtm).set_ivars(());
        let window: Retained<OverlayWindow> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: content_rect,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        // We own the Retained; the window must not release itself on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(true);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setLevel(CGShieldingWindowLevel() as _);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        window.setIgnoresMouseEvents(false);
        window.setAcceptsMouseMovedEvents(true);
        window.setContentView(Some(&view));

        let event_monitor = install_event_monitor(
            monitor,
            scale,
            view_height,
            objc2::rc::Weak::from(&window),
            sink,
        )?;

        // Show. The window under the cursor becomes key (keyboard input needs
        // a key window); the others are simply ordered in. The app activates
        // so its key window actually receives key events.
        let cursor = NSEvent::mouseLocation();
        if screen.frame.contains(CocoaPoint::new(cursor.x, cursor.y)) {
            NSApplication::sharedApplication(mtm).activate();
            window.makeKeyAndOrderFront(None);
        } else {
            window.orderFront(None);
        }

        Ok(Self {
            window,
            view,
            event_monitor,
            width,
            height,
            view_height,
            scale,
        })
    }
}

impl OverlaySurface for MacOverlaySurface {
    /// Copy `frame`'s full contents — or just the `dirty` monitor-local
    /// region — into the backing store, invalidate the matching view area,
    /// and draw synchronously (see module docs for why synchronous).
    fn present(&mut self, frame: &DibBuffer, dirty: Option<Rect>) -> Result<()> {
        if frame.width as usize != self.width
            || frame.height as usize != self.height
            || frame.stride != frame.width * 4
        {
            bail!(
                "frame layout does not match the surface: {}x{} stride {} vs {}x{}",
                frame.width,
                frame.height,
                frame.stride,
                self.width,
                self.height
            );
        }

        let clamped = dirty.and_then(|r| clamp_to_store(r, self.width, self.height));
        {
            let ivars = self.view.ivars();
            let mut store = ivars.store.borrow_mut();
            match clamped {
                Some(r) => {
                    let x0 = r.x as usize;
                    let cols = r.width as usize * 4;
                    for row in r.y as usize..r.bottom() as usize {
                        let off = row * self.width * 4 + x0 * 4;
                        store.pixels[off..off + cols]
                            .copy_from_slice(&frame.pixels[off..off + cols]);
                    }
                }
                None if dirty.is_none() => {
                    store.pixels.copy_from_slice(&frame.pixels);
                }
                None => {} // dirty region clipped to nothing
            }
        }
        // The borrow is released before displayIfNeeded() re-enters drawRect:.
        match clamped {
            Some(r) => self
                .view
                .setNeedsDisplayInRect(to_nsrect(monitor_local_to_view_rect(
                    r,
                    self.view_height,
                    self.scale,
                ))),
            None => self.view.setNeedsDisplay(true),
        }
        self.view.displayIfNeeded();
        Ok(())
    }
}

impl Drop for MacOverlaySurface {
    /// Remove the event monitor and close the window. `releasedWhenClosed`
    /// is false, so closing only hides the window; our `Retained`s release
    /// the objects afterwards.
    fn drop(&mut self) {
        // SAFETY: `event_monitor` is the object returned by the matching
        // addLocalMonitor call and is removed at most once (here).
        unsafe { NSEvent::removeMonitor(&self.event_monitor) };
        self.window.close();
    }
}

/// [`SurfaceFactory`](crate::platform::SurfaceFactory) implementation: one
/// [`MacOverlaySurface`] per monitor, placed on the `NSScreen` whose frame
/// converts to the given virtual rect.
///
/// `_all_monitor_rects` is unused: scroll events route by cursor location
/// under AppKit (unlike Win32's focus-window wheel delivery), so no
/// focus-reroute table is needed (see module docs).
pub fn create_overlay_surface(
    monitor: usize,
    rect: Rect,
    _all_monitor_rects: Rc<Vec<Rect>>,
    sink: OverlayEventSink,
) -> Result<Box<dyn OverlaySurface>> {
    let mtm = MainThreadMarker::new()
        .context("overlay surfaces must be created on the application's main thread")?;
    let screens = enumerate_screens(mtm);
    let primary_height = primary_height(&screens);
    let screen = screens
        .iter()
        .find(|s| cocoa_rect_to_virtual(s.frame, s.scale, primary_height) == rect)
        .context("no NSScreen matches the captured monitor rect")?;
    let surface = MacOverlaySurface::new(mtm, monitor, rect, screen, sink)?;
    Ok(Box::new(surface))
}

/// Install the surface's local event monitor (see module docs). The handler
/// captures the window as a `Weak` so a late event after teardown is a no-op,
/// plus the sink and the wheel accumulator — no reference cycles anywhere.
fn install_event_monitor(
    monitor: usize,
    scale: f64,
    view_height: f64,
    window: objc2::rc::Weak<OverlayWindow>,
    sink: OverlayEventSink,
) -> Result<Retained<AnyObject>> {
    let wheel = Rc::new(RefCell::new(WheelNorm::new()));
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let Some(window) = window.load() else {
            return event.as_ptr();
        };
        // SAFETY: the event is valid for the duration of the callback.
        let event = unsafe { event.as_ref() };
        // Local monitors fire on the main thread, where the marker exists.
        let Some(mtm) = MainThreadMarker::new() else {
            return std::ptr::null_mut();
        };
        // Only this surface's own window: every surface installs a monitor,
        // and an event belongs to the window it was dispatched to.
        let ours: &NSWindow = &window;
        let is_ours = event.window(mtm).is_some_and(|w| std::ptr::eq(&*w, ours));
        if !is_ours {
            return NonNull::from(event).as_ptr();
        }

        let local = || {
            let p = event.locationInWindow();
            view_point_to_monitor_local(CocoaPoint::new(p.x, p.y), view_height, scale)
        };
        match event.r#type() {
            // A left drag IS a move for the snip selection (the cursor moves
            // with the button held; plain `mouseMoved` stops arriving).
            NSEventType::MouseMoved | NSEventType::LeftMouseDragged => {
                sink(monitor, OverlayEvent::MouseMove { at: local() });
            }
            NSEventType::LeftMouseDown => {
                sink(monitor, OverlayEvent::LeftButtonDown { at: local() });
            }
            NSEventType::LeftMouseUp => {
                sink(monitor, OverlayEvent::LeftButtonUp { at: local() });
            }
            NSEventType::ScrollWheel => {
                let delta = if event.hasPreciseScrollingDeltas() {
                    wheel.borrow_mut().precise(event.scrollingDeltaY())
                } else {
                    wheel.borrow_mut().lines(event.scrollingDeltaY())
                };
                sink(
                    monitor,
                    OverlayEvent::MouseWheel {
                        at: local(),
                        delta,
                        modifiers: modifiers_from(event.modifierFlags()),
                    },
                );
            }
            NSEventType::KeyDown => {
                if let Some(vk) = keymap::cg_keycode_to_vk(event.keyCode()) {
                    sink(
                        monitor,
                        OverlayEvent::KeyDown {
                            vk,
                            modifiers: modifiers_from(event.modifierFlags()),
                        },
                    );
                }
                // Swallow: the frozen overlay is modal; nothing else may act
                // on the keystroke (returning nil consumes the event).
                return std::ptr::null_mut();
            }
            _ => {}
        }
        NonNull::from(event).as_ptr()
    });

    let mask = NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDragged
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::ScrollWheel
        | NSEventMask::KeyDown;
    // SAFETY: the block matches the documented handler signature; AppKit
    // retains it until the monitor is removed (Drop does that).
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
        .context("addLocalMonitorForEventsMatchingMask failed")
}

/// `NSEventModifierFlags` → the crate's [`Modifiers`] bits. Command maps to
/// `WIN` (the "Win" slot of the cross-platform modifier vocabulary).
fn modifiers_from(flags: NSEventModifierFlags) -> Modifiers {
    let mut out = Modifiers::NONE;
    if flags.contains(NSEventModifierFlags::Shift) {
        out = out | Modifiers::SHIFT;
    }
    if flags.contains(NSEventModifierFlags::Control) {
        out = out | Modifiers::CTRL;
    }
    if flags.contains(NSEventModifierFlags::Option) {
        out = out | Modifiers::ALT;
    }
    if flags.contains(NSEventModifierFlags::Command) {
        out = out | Modifiers::WIN;
    }
    out
}

/// Scroll-delta normalization to the [`OverlayEvent::MouseWheel`] contract
/// (one notch = 120 units, sub-notch values allowed and significant).
///
/// Precise deltas (trackpads, smooth wheels) arrive in point-ish units where
/// ~10pt ≈ one line ≈ one notch, so they are scaled by 12; the fractional
/// remainder carries across events so slow scrolling never stalls at zero.
/// Non-precise deltas arrive in LINES and are simply ×120.
struct WheelNorm {
    remainder: f64,
}

impl WheelNorm {
    fn new() -> Self {
        Self { remainder: 0.0 }
    }

    /// ~10pt per notch → 12 wheel units per point. Emits the rounded
    /// accumulation and carries the sub-unit remainder forward, so deltas
    /// are conserved exactly over a scroll sequence.
    fn precise(&mut self, points: f64) -> i32 {
        let total = self.remainder + points * 12.0;
        let whole = total.round();
        self.remainder = total - whole;
        whole as i32
    }

    /// One line = one notch.
    fn lines(&mut self, lines: f64) -> i32 {
        (lines * 120.0).round() as i32
    }
}

/// Clamp a monitor-local dirty rect to the store bounds; `None` when nothing
/// is left (the controller may produce slightly out-of-range regions at the
/// edges of zoomed views).
fn clamp_to_store(r: Rect, width: usize, height: usize) -> Option<Rect> {
    let bounds = Rect::new(0, 0, width as u32, height as u32);
    r.intersection(&bounds)
}

/// Plain-struct mirror → `NSRect` at the AppKit boundary.
fn to_nsrect(r: crate::platform::macos::coords::CocoaRect) -> NSRect {
    NSRect {
        origin: NSPoint { x: r.x, y: r.y },
        size: NSSize {
            width: r.width,
            height: r.height,
        },
    }
}

#[cfg(test)]
mod tests {
    //! Headless-safe: pure helpers only. No windows, no views, no events —
    //! bitflags and plain structs constructed by hand.
    use super::*;

    // -- WheelNorm ------------------------------------------------------------

    #[test]
    fn precise_deltas_scale_ten_points_per_notch() {
        let mut w = WheelNorm::new();
        assert_eq!(w.precise(10.0), 120); // one notch
        assert_eq!(w.precise(-10.0), -120);
    }

    #[test]
    fn precise_small_deltas_pass_through_fractionally() {
        let mut w = WheelNorm::new();
        // A 1pt trackpad nudge is a sub-notch delta: 12 units, NOT zero.
        assert_eq!(w.precise(1.0), 12);
        assert_eq!(w.precise(0.25), 3);
    }

    #[test]
    fn precise_remainder_is_conserved_across_events() {
        let mut w = WheelNorm::new();
        let mut total = 0;
        for _ in 0..10 {
            total += w.precise(0.05); // 0.6 units each — sub-unit per event
        }
        assert_eq!(total, 6);
    }

    #[test]
    fn precise_direction_reversal_uses_the_remainder() {
        let mut w = WheelNorm::new();
        assert_eq!(w.precise(0.09), 1); // 1.08 → 1, remainder 0.08
        assert_eq!(w.precise(-0.01), 0); // -0.04 → 0, remainder stays
        assert_eq!(w.precise(-0.09), -1); // remainder drains before sign flips
    }

    #[test]
    fn lines_are_one_notch_each() {
        let mut w = WheelNorm::new();
        assert_eq!(w.lines(1.0), 120);
        assert_eq!(w.lines(-3.0), -360);
        // Wheels that report fractional lines (0.1 per detent on some mice).
        assert_eq!(w.lines(0.1), 12);
    }

    // -- modifiers_from ---------------------------------------------------------

    #[test]
    fn modifier_flags_map_to_the_crate_bits() {
        assert_eq!(modifiers_from(NSEventModifierFlags(0)), Modifiers::NONE);
        assert_eq!(
            modifiers_from(NSEventModifierFlags::Shift),
            Modifiers::SHIFT
        );
        assert_eq!(
            modifiers_from(NSEventModifierFlags::Control),
            Modifiers::CTRL
        );
        assert_eq!(modifiers_from(NSEventModifierFlags::Option), Modifiers::ALT);
        assert_eq!(
            modifiers_from(NSEventModifierFlags::Command),
            Modifiers::WIN
        );
    }

    #[test]
    fn modifier_flags_combine() {
        let flags = NSEventModifierFlags::Shift
            | NSEventModifierFlags::Command
            | NSEventModifierFlags::CapsLock
            | NSEventModifierFlags::NumericPad;
        assert_eq!(modifiers_from(flags), Modifiers::SHIFT | Modifiers::WIN);
    }

    // -- clamp_to_store -----------------------------------------------------------

    #[test]
    fn clamp_keeps_fully_inside_rects() {
        let r = Rect::new(10, 20, 100, 50);
        assert_eq!(clamp_to_store(r, 200, 200), Some(r));
    }

    #[test]
    fn clamp_crops_partially_outside_rects() {
        assert_eq!(
            clamp_to_store(Rect::new(-10, 90, 50, 30), 100, 100),
            Some(Rect::new(0, 90, 40, 10))
        );
    }

    #[test]
    fn clamp_drops_fully_outside_rects() {
        assert_eq!(clamp_to_store(Rect::new(0, 200, 10, 10), 100, 100), None);
        assert_eq!(clamp_to_store(Rect::new(-50, 0, 10, 10), 100, 100), None);
    }
}
