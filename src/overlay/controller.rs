//! Overlay orchestration: freeze/unfreeze, mode switching, event routing,
//! clipboard copy. Win32-only module that drives the PURE modes.
//!
//! Implementation notes (contract clarifications — public API unchanged):
//! - **Default mode**: `freeze` enters Spotlight mode (product spec default).
//! - **Cursor seeding**: a freshly entered mode (`freeze` and `set_mode`)
//!   receives one synthetic mouse-move with the LIVE cursor position, so the
//!   first presented frame already has the spotlight hole / zoom center under
//!   the cursor instead of at `(0, 0)`. The mode's repaint effect is ignored
//!   because a full repaint always follows seeding.
//! - **Focused screen**: "monitor under the cursor" (the no-selection copy
//!   fallback) means the monitor whose virtual-screen bounds contain the live
//!   cursor position; falls back to monitor 0 when the cursor is outside every
//!   monitor (transient display-change states) so Ctrl+C always copies
//!   something.
//! - **Per-monitor selections**: snip drags are MONITOR-LOCAL and clamped at
//!   monitor edges by the mode; a selection never spans monitors. The copy
//!   path crops from that monitor's original frame only.

use crate::capture::{Capturer, DibBuffer, MonitorInfo, copy_dib_to_clipboard};
use crate::geometry::{Point, Rect};
use crate::overlay::composite::{crop_normalized, monitor_index_at, virtual_to_local};
use crate::overlay::modes::snip::SnipMode;
use crate::overlay::modes::spotlight::SpotlightMode;
use crate::overlay::modes::zoom::ZoomMode;
use crate::overlay::modes::{ModeKind, OverlayMode, SnipSelection};
use crate::overlay::window::{OverlayEvent, OverlayEventSink, OverlayWindow};
use crate::settings::model::AppSettings;
use anyhow::Result;
use std::cell::RefCell;
use std::rc::Rc;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// Everything that exists only while the screen is frozen.
///
/// `originals`, `frames`, `monitors`, and `windows` are parallel vectors in
/// [`Capturer::capture_all`] order; index = monitor index.
struct FreezeState {
    /// Per-monitor ORIGINAL (undarkened) captures — the clipboard copy source.
    originals: Vec<DibBuffer>,
    /// Per-monitor scratch frame the active mode renders into before
    /// `present`. Persistent across repaints so the per-mouse-move path never
    /// allocates; `OverlayMode::render` overwrites every pixel, so stale
    /// contents are impossible.
    frames: Vec<DibBuffer>,
    /// Monitor metadata (virtual-screen bounds + DPI), parallel to `originals`.
    monitors: Vec<MonitorInfo>,
    /// One overlay window per monitor, parallel to `originals`.
    windows: Vec<OverlayWindow>,
    /// Active mode state machine; rebuilt from `settings` on `set_mode`.
    mode: Box<dyn OverlayMode>,
    /// Freeze-time settings snapshot (dim opacity + mode parameters).
    settings: AppSettings,
}

/// Owns the frozen captures, one [`crate::overlay::window::OverlayWindow`] per
/// monitor, and the active mode state machine.
///
/// Single UI-thread object (not `Send`/`Sync`). The session state lives in a
/// shared cell so the overlay windows' event sink can route window events back
/// into the same event path as [`handle_overlay_event`](Self::handle_overlay_event)
/// without a message queue: the sink holds only a `Weak` (the state owns the
/// windows, which own the sink — a strong reference would cycle).
pub struct OverlayController {
    /// `Some` while frozen. `None` ⇒ not frozen (all state derived from this,
    /// so a sink-triggered exit can never desync a separate `frozen` flag).
    inner: Rc<RefCell<Option<FreezeState>>>,
    /// Active mode kind; reset to Spotlight on every freeze. Meaningless while
    /// unfrozen (returns the last used kind).
    active: ModeKind,
}

impl OverlayController {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(None)),
            active: ModeKind::Spotlight,
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.inner.borrow().is_some()
    }

    /// Number of overlay windows/monitors while frozen; 0 otherwise.
    pub fn monitor_count(&self) -> usize {
        self.inner.borrow().as_ref().map_or(0, |s| s.windows.len())
    }

    /// Capture all monitors ONCE via `capturer`, create one overlay window per
    /// monitor presenting the darkened capture, and enter Spotlight mode.
    ///
    /// Settings are SNAPSHOT at freeze time: changing hotkeys/radius/zoom while
    /// frozen takes effect on the next freeze. No-op when already frozen.
    pub fn freeze(&mut self, capturer: &dyn Capturer, settings: &AppSettings) -> Result<()> {
        if self.is_frozen() {
            return Ok(());
        }

        // Single capture pass for the whole desktop.
        let captured = capturer.capture_all()?;
        let (monitors, originals): (Vec<MonitorInfo>, Vec<DibBuffer>) =
            captured.into_iter().unzip();

        // One layered window per monitor, all reporting to one shared sink
        // wired back into this controller. Every window also gets the SHARED
        // monitor-rect list so its wheel handler can reroute `WM_MOUSEWHEEL`
        // (delivered to the focus window, not the one under the cursor) to
        // the monitor actually containing the cursor. If creation of window N
        // fails, the already-created windows close via Drop as `windows`
        // unwinds.
        let sink = self.make_sink();
        let monitor_rects = Rc::new(monitors.iter().map(|m| m.rect).collect::<Vec<_>>());
        let mut windows = Vec::with_capacity(monitors.len());
        for (index, monitor) in monitors.iter().enumerate() {
            windows.push(OverlayWindow::create(
                index,
                monitor.rect,
                monitor_rects.clone(),
                sink.clone(),
            )?);
        }

        // Persistent per-monitor render targets: no allocation on repaints.
        let frames = originals
            .iter()
            .map(|o| DibBuffer::new(o.width, o.height))
            .collect();

        let settings = settings.clone();
        let mut state = FreezeState {
            originals,
            frames,
            monitors,
            windows,
            mode: build_mode(ModeKind::Spotlight, &settings),
            settings,
        };

        // Spotlight is the default mode (product spec). Seed the live cursor
        // position, then present the initial full frame on every monitor.
        seed_cursor_position(&mut *state.mode, &state.monitors);
        for m in 0..state.windows.len() {
            render_and_present(&mut state, m, None);
        }

        *self.inner.borrow_mut() = Some(state);
        self.active = ModeKind::Spotlight;
        Ok(())
    }

    /// Destroy all overlay windows and drop the captures. No-op when not frozen.
    pub fn unfreeze(&mut self) {
        // Take first, drop AFTER releasing the borrow: window teardown never
        // runs while the cell is mutably borrowed.
        let taken = self.inner.borrow_mut().take();
        drop(taken);
    }

    pub fn active_mode(&self) -> ModeKind {
        self.active
    }

    /// Switch the active mode (rebuilds the mode state machine from the freeze
    /// snapshot, full repaint). No-op when not frozen.
    ///
    /// Also a no-op when `kind` is already active, so re-pressing the current
    /// mode key does not reset mode state (spotlight radius, zoom factor,
    /// in-progress snip).
    pub fn set_mode(&mut self, kind: ModeKind) {
        if kind == self.active {
            return;
        }
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        // Mode parameters come from the freeze-time snapshot; live settings
        // edits therefore apply on the NEXT freeze, per the freeze contract.
        state.mode = build_mode(kind, &state.settings);
        seed_cursor_position(&mut *state.mode, &state.monitors);
        for m in 0..state.windows.len() {
            render_and_present(state, m, None);
        }
        self.active = kind;
    }

    /// Route an overlay window event to the active mode, then apply its
    /// [`crate::overlay::modes::ModeEffect`]: for each requested repaint, render
    /// the mode frame and `present` it.
    ///
    /// The cancel (Esc), copy (Ctrl+C), mode-switch, and reset-zoom gestures are
    /// NOT handled here — the app catches them as global hotkeys and calls
    /// [`unfreeze`](Self::unfreeze), [`snip_copy_and_close`](Self::snip_copy_and_close),
    /// [`set_mode`](Self::set_mode), or [`reset_view`](Self::reset_view).
    pub fn handle_overlay_event(&mut self, monitor: usize, event: OverlayEvent) {
        dispatch_event(&self.inner, monitor, event);
    }

    /// Reset-view hotkey path (default binding `0`): call the active mode's
    /// [`crate::overlay::modes::OverlayMode::reset_view`] and apply the
    /// returned [`crate::overlay::modes::ModeEffect`]'s repaints via
    /// `present`. Only Zoom overrides `reset_view` today (restores 1.0 and
    /// repaints the cursor monitor); every other mode returns an empty
    /// effect, making this a cheap no-op. No-op when not frozen.
    ///
    /// This is a DEDICATED entry point, deliberately not routed through
    /// [`handle_overlay_event`](Self::handle_overlay_event): modes treat
    /// `on_key` as a no-op (keys that matter are global hotkeys), so a
    /// synthesized key event would silently do nothing.
    pub fn reset_view(&mut self) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        let effect = state.mode.reset_view();
        // `ModeEffect::exit` is reserved for the event path; `reset_view`
        // implementations never request exit, so only repaints are applied.
        for &(m, dirty) in &effect.repaint {
            render_and_present(state, m, dirty);
        }
    }

    /// Ctrl+C contract: when a snip selection exists, crop it from the ORIGINAL
    /// (undarkened) capture and copy it to the clipboard; otherwise copy the
    /// FULL frame of the monitor currently under the cursor ("focused screen").
    /// Then unfreeze. `Ok(())` no-op when not frozen.
    ///
    /// The selection is monitor-local (drags clamp at that monitor's edges);
    /// the crop comes from that monitor's original frame via
    /// [`crop_normalized`], which normalizes any drag direction.
    pub fn snip_copy_and_close(&mut self) -> Result<()> {
        // Closing is unconditional, so take the session state out up-front;
        // `None` (not frozen) is the documented no-op and never touches the
        // clipboard.
        let Some(state) = self.inner.borrow_mut().take() else {
            return Ok(());
        };

        let cursor = cursor_position_virtual().unwrap_or_default();
        let rects: Vec<Rect> = state.monitors.iter().map(|m| m.rect).collect();
        let plan = decide_copy_plan(
            state.mode.kind(),
            state.mode.snip_selection(),
            cursor,
            &rects,
        );

        let copy_result = match plan {
            Some(CopyPlan::Snip { monitor, a, b }) => {
                match crop_normalized(&state.originals[monitor], a, b) {
                    Some(snip) => copy_dib_to_clipboard(&snip),
                    // Unreachable — decide_copy_plan pre-validated the clipped
                    // rect. Keep the "Ctrl+C always copies something" invariant.
                    None => copy_dib_to_clipboard(&state.originals[monitor]),
                }
            }
            // Full original frame: passed by reference, no buffer copy.
            Some(CopyPlan::FullMonitor { monitor }) => {
                copy_dib_to_clipboard(&state.originals[monitor])
            }
            None => Ok(()), // zero monitors captured: nothing to copy
        };
        drop(state); // close every window even when the copy failed
        copy_result
    }

    /// Shared sink for all overlay windows. The closure owns only a `Weak` to
    /// the session cell; events arriving after unfreeze (or mid-teardown)
    /// no-op.
    fn make_sink(&self) -> OverlayEventSink {
        let weak = Rc::downgrade(&self.inner);
        Rc::new(move |monitor, event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            dispatch_event(&inner, monitor, event);
        })
    }
}

impl Default for OverlayController {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared event path for the window sink and
/// [`OverlayController::handle_overlay_event`]. Handles the reserved
/// `ModeEffect::exit` by tearing the session down; teardown runs AFTER the
/// borrow is released so window destruction never re-enters the cell.
fn dispatch_event(inner: &Rc<RefCell<Option<FreezeState>>>, monitor: usize, event: OverlayEvent) {
    let mut slot = inner.borrow_mut();
    let exit = slot
        .as_mut()
        .is_some_and(|state| apply_overlay_event(state, monitor, event));
    let taken = if exit { slot.take() } else { None };
    drop(slot);
    drop(taken);
}

/// Feed `event` to the active mode and apply the resulting `ModeEffect`'s
/// dirty-region repaints. Returns `true` when the mode requested exit.
fn apply_overlay_event(state: &mut FreezeState, monitor: usize, event: OverlayEvent) -> bool {
    if monitor >= state.windows.len() {
        return false; // stale event from an already-destroyed window
    }
    let effect = match event {
        OverlayEvent::MouseMove { at } => state.mode.on_mouse_move(monitor, at),
        OverlayEvent::MouseWheel {
            at,
            delta,
            modifiers,
        } => state.mode.on_wheel(monitor, at, delta, modifiers),
        OverlayEvent::LeftButtonDown { at } => state.mode.on_left_button_down(monitor, at),
        OverlayEvent::LeftButtonUp { at } => state.mode.on_left_button_up(monitor, at),
        OverlayEvent::KeyDown { vk, modifiers } => state.mode.on_key(vk, modifiers),
    };
    for &(m, dirty) in &effect.repaint {
        render_and_present(state, m, dirty);
    }
    effect.exit
}

/// Re-render monitor `m`'s full frame through the active mode and present it.
/// `dirty: Some(rect)` lets the window composite only that region (the
/// spotlight per-mouse-move fast path); the mode still renders the complete
/// frame per its contract. Present failures are best-effort ignored: stale
/// pixels until the next repaint beat a dead overlay.
fn render_and_present(state: &mut FreezeState, m: usize, dirty: Option<Rect>) {
    if m >= state.windows.len() {
        return; // defensive: a mode asked to repaint a nonexistent monitor
    }
    // Split borrows across disjoint fields: mode (read) renders from
    // originals[m] (read) into frames[m] (write), then windows[m] presents.
    let FreezeState {
        originals,
        frames,
        windows,
        mode,
        settings,
        ..
    } = state;
    mode.render(m, &originals[m], &mut frames[m], settings.overlay.dim_opacity);
    let _ = windows[m].present(&frames[m], dirty);
}

/// Build a fresh mode state machine from the freeze-time settings snapshot.
fn build_mode(kind: ModeKind, settings: &AppSettings) -> Box<dyn OverlayMode> {
    match kind {
        ModeKind::Spotlight => Box::new(SpotlightMode::new(
            settings.spotlight.default_radius,
            settings.hotkeys.spotlight_radius_modifier,
        )),
        ModeKind::Zoom => {
            let (step, min, max) = sanitize_zoom_params(
                settings.zoom.step_factor,
                settings.zoom.min,
                settings.zoom.max,
            );
            Box::new(ZoomMode::new(step, min, max))
        }
        ModeKind::Snip => Box::new(SnipMode::new()),
    }
}

/// Feed the live cursor position into a freshly built mode (see module docs).
/// The repaint effect is discarded — callers always full-repaint right after.
fn seed_cursor_position(mode: &mut dyn OverlayMode, monitors: &[MonitorInfo]) {
    if monitors.is_empty() {
        return;
    }
    let Some(cursor) = cursor_position_virtual() else {
        return;
    };
    let rects: Vec<Rect> = monitors.iter().map(|m| m.rect).collect();
    let Some(idx) = monitor_index_at(cursor, &rects) else {
        return; // cursor outside every monitor: leave the mode's default origin
    };
    let _ = mode.on_mouse_move(idx, virtual_to_local(cursor, rects[idx]));
}

/// Current cursor position in virtual-screen coordinates; `None` on failure.
fn cursor_position_virtual() -> Option<Point> {
    let mut pt = POINT::default();
    // SAFETY: read-only query writing to a caller-provided POINT; touches no
    // window, hook, clipboard, or input state. Never called from tests.
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    Some(Point::new(pt.x, pt.y))
}

/// Clamp zoom settings to the `ZoomMode::new` contract (`step > 1.0`,
/// `min >= 1.0`, `max > min`) so a hand-edited settings file can never break
/// the mode constructor. Values already in range pass through untouched.
fn sanitize_zoom_params(step: f32, min: f32, max: f32) -> (f32, f32, f32) {
    let step = if step.is_finite() && step > 1.0 {
        step
    } else {
        1.25
    };
    let min = if min.is_finite() && min >= 1.0 {
        min
    } else {
        1.0
    };
    // min >= 1.0 here, so min * 2.0 is exact and strictly greater than min
    // (or +inf for astronomically large min — still > min).
    let max = if max.is_finite() && max > min {
        max
    } else {
        min * 2.0
    };
    (step, min, max)
}

/// What [`OverlayController::snip_copy_and_close`] puts on the clipboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyPlan {
    /// Crop the snip selection (monitor-local endpoints, any drag direction)
    /// from that monitor's ORIGINAL frame.
    Snip { monitor: usize, a: Point, b: Point },
    /// Copy the focused monitor's full ORIGINAL frame.
    FullMonitor { monitor: usize },
}

/// Pure copy-target decision, factored out for headless testing.
///
/// Snip mode + a selection whose normalized rect (clipped to that monitor's
/// local bounds) is non-empty ⇒ crop plan; every other case ⇒ full frame of
/// the focused monitor. Returns `None` only when `monitors` is empty.
/// Computed on raw geometry fields so the decision is identical in tests.
fn decide_copy_plan(
    mode: ModeKind,
    selection: Option<SnipSelection>,
    cursor_virtual: Point,
    monitors: &[Rect],
) -> Option<CopyPlan> {
    if monitors.is_empty() {
        return None;
    }
    if mode == ModeKind::Snip
        && let Some(sel) = selection
        && sel.monitor < monitors.len()
        && snip_rect_is_copyable(sel.a, sel.b, monitors[sel.monitor])
    {
        return Some(CopyPlan::Snip {
            monitor: sel.monitor,
            a: sel.a,
            b: sel.b,
        });
    }
    Some(CopyPlan::FullMonitor {
        monitor: focus_monitor_index(cursor_virtual, monitors),
    })
}

/// `true` when the normalized drag rect, clipped to the monitor's local
/// bounds, has positive area. Mirrors `composite::crop_normalized`'s
/// normalize-then-clip semantics on raw fields (the decision must not depend
/// on pixel buffers). Endpoints may arrive in any drag direction; modes clamp
/// drags at monitor edges, so out-of-bounds points are only defensive input.
fn snip_rect_is_copyable(a: Point, b: Point, monitor: Rect) -> bool {
    let x0 = a.x.min(b.x).max(0);
    let y0 = a.y.min(b.y).max(0);
    let x1 = a.x.max(b.x).min(monitor.width as i32);
    let y1 = a.y.max(b.y).min(monitor.height as i32);
    x1 > x0 && y1 > y0
}

/// Index of the monitor containing `cursor_virtual` (virtual-screen
/// coordinates; edges inclusive top/left, exclusive bottom/right, matching
/// `composite::monitor_index_at` semantics). Falls back to monitor 0 when the
/// cursor is outside every monitor so the caller always gets a valid index
/// for a non-empty slice.
fn focus_monitor_index(cursor_virtual: Point, monitors: &[Rect]) -> usize {
    monitors
        .iter()
        .position(|r| {
            cursor_virtual.x >= r.x
                && cursor_virtual.x < r.x + r.width as i32
                && cursor_virtual.y >= r.y
                && cursor_virtual.y < r.y + r.height as i32
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! Headless-safe: no windows, no hotkeys, no clipboard, no capture. The
    //! copy DECISION logic is pure field math; `snip_copy_and_close` is only
    //! exercised while unfrozen (documented no-op, clipboard untouched).
    use super::*;

    /// Primary 1920x1080 at (0,0) + secondary 2560x1440 LEFT of it (negative x).
    fn two_monitors() -> Vec<Rect> {
        vec![Rect::new(0, 0, 1920, 1080), Rect::new(-2560, 0, 2560, 1440)]
    }

    fn snip(monitor: usize, ax: i32, ay: i32, bx: i32, by: i32) -> Option<SnipSelection> {
        Some(SnipSelection {
            monitor,
            a: Point::new(ax, ay),
            b: Point::new(bx, by),
        })
    }

    // ---- controller state machine (unfrozen paths only) ----

    #[test]
    fn new_controller_is_unfrozen_spotlight() {
        let c = OverlayController::new();
        assert!(!c.is_frozen());
        assert_eq!(c.monitor_count(), 0);
        assert_eq!(c.active_mode(), ModeKind::Spotlight);
    }

    #[test]
    fn default_matches_new() {
        let c = OverlayController::default();
        assert!(!c.is_frozen());
        assert_eq!(c.active_mode(), ModeKind::Spotlight);
    }

    #[test]
    fn unfreeze_when_unfrozen_is_noop() {
        let mut c = OverlayController::new();
        c.unfreeze();
        assert!(!c.is_frozen());
        assert_eq!(c.monitor_count(), 0);
    }

    #[test]
    fn set_mode_when_unfrozen_is_noop() {
        let mut c = OverlayController::new();
        c.set_mode(ModeKind::Zoom);
        c.set_mode(ModeKind::Snip);
        assert!(!c.is_frozen());
        assert_eq!(c.active_mode(), ModeKind::Spotlight);
    }

    #[test]
    fn handle_event_when_unfrozen_is_noop() {
        let mut c = OverlayController::new();
        for event in [
            OverlayEvent::MouseMove {
                at: Point::new(5, 5),
            },
            OverlayEvent::LeftButtonDown {
                at: Point::new(5, 5),
            },
            OverlayEvent::KeyDown {
                vk: 0x1B,
                modifiers: crate::hotkeys::gesture::Modifiers::NONE,
            },
        ] {
            c.handle_overlay_event(0, event);
        }
        assert!(!c.is_frozen());
    }

    #[test]
    fn reset_view_when_unfrozen_is_noop() {
        let mut c = OverlayController::new();
        c.reset_view(); // must not panic or create state
        assert!(!c.is_frozen());
        assert_eq!(c.monitor_count(), 0);
    }

    // ---- reset_view plumbing (D1 regression) ----

    #[test]
    fn reset_view_plumbing_reaches_mode_and_returns_repaint() {
        // D1 regression: the frozen reset-zoom path is
        // `app → OverlayController::reset_view → mode.reset_view → apply the
        // effect's repaints via render_and_present`. The window presents are
        // Win32-only, but the controller's half of the plumbing — invoking
        // `reset_view` on the active mode THROUGH THE TRAIT OBJECT and
        // consuming its effect exactly the way `OverlayController::reset_view`
        // does — is pure. Drive a ZoomMode behind `Box<dyn OverlayMode>` (the
        // exact type the controller holds) through the same two steps.
        let mut mode: Box<dyn OverlayMode> =
            Box::new(ZoomMode::new(1.25, 1.0, 16.0));
        mode.on_mouse_move(0, Point::new(10, 10));
        mode.on_wheel(0, Point::new(10, 10), 120, crate::hotkeys::gesture::Modifiers::NONE);
        let effect = mode.reset_view();
        // The effect the controller would present: full repaint of the cursor
        // monitor, and (verified in zoom.rs) zoom is back to 1.0.
        assert_eq!(effect.repaint, vec![(0, None)]);
        assert!(!effect.exit);
        // Modes that don't override reset_view (the controller's Spotlight /
        // Snip states) yield an empty effect — reset_view is a safe no-op.
        let mut spotlight: Box<dyn OverlayMode> =
            Box::new(crate::overlay::modes::spotlight::SpotlightMode::new(
                100,
                crate::hotkeys::gesture::Modifiers::CTRL,
            ));
        assert_eq!(spotlight.reset_view(), crate::overlay::modes::ModeEffect::none());
    }

    #[test]
    fn snip_copy_when_unfrozen_is_ok_noop_and_touches_nothing() {
        let mut c = OverlayController::new();
        // Must return Ok WITHOUT calling GetCursorPos/copy_dib_to_clipboard.
        assert!(c.snip_copy_and_close().is_ok());
        assert!(!c.is_frozen());
    }

    // ---- focus_monitor_index ----

    #[test]
    fn focus_is_primary_for_origin_area() {
        let m = two_monitors();
        assert_eq!(focus_monitor_index(Point::new(0, 0), &m), 0);
        assert_eq!(focus_monitor_index(Point::new(1919, 1079), &m), 0);
    }

    #[test]
    fn focus_edges_are_inclusive_topleft_exclusive_bottomright() {
        let m = two_monitors();
        // Primary right/bottom edges are exclusive: outside both monitors here.
        assert_eq!(focus_monitor_index(Point::new(1920, 540), &m), 0); // fallback
        assert_eq!(focus_monitor_index(Point::new(960, 1080), &m), 0); // fallback
        // Secondary left edge inclusive, right edge (0) exclusive.
        assert_eq!(focus_monitor_index(Point::new(-2560, 0), &m), 1);
        assert_eq!(focus_monitor_index(Point::new(-1, 1439), &m), 1);
    }

    #[test]
    fn focus_handles_negative_virtual_coords() {
        let m = two_monitors();
        assert_eq!(focus_monitor_index(Point::new(-100, 100), &m), 1);
        assert_eq!(focus_monitor_index(Point::new(-2560, 1439), &m), 1);
    }

    #[test]
    fn focus_outside_all_monitors_falls_back_to_zero() {
        let m = two_monitors();
        assert_eq!(focus_monitor_index(Point::new(9999, 9999), &m), 0);
        assert_eq!(focus_monitor_index(Point::new(-9999, 0), &m), 0);
    }

    // ---- snip_rect_is_copyable ----

    #[test]
    fn snip_rect_copyable_for_forward_and_negative_drags() {
        let mon = Rect::new(0, 0, 1920, 1080);
        assert!(snip_rect_is_copyable(Point::new(10, 10), Point::new(110, 60), mon));
        assert!(snip_rect_is_copyable(Point::new(110, 60), Point::new(10, 10), mon));
        assert!(snip_rect_is_copyable(Point::new(110, 10), Point::new(10, 60), mon));
    }

    #[test]
    fn snip_rect_zero_area_is_not_copyable() {
        let mon = Rect::new(0, 0, 1920, 1080);
        assert!(!snip_rect_is_copyable(Point::new(50, 50), Point::new(50, 50), mon));
        assert!(!snip_rect_is_copyable(Point::new(50, 10), Point::new(50, 100), mon)); // w=0
        assert!(!snip_rect_is_copyable(Point::new(10, 50), Point::new(100, 50), mon)); // h=0
    }

    #[test]
    fn snip_rect_clips_to_monitor_bounds() {
        let mon = Rect::new(0, 0, 1920, 1080);
        // Partially outside: clipped remainder is non-empty.
        assert!(snip_rect_is_copyable(Point::new(-50, 10), Point::new(100, 60), mon));
        // Fully outside: clipped to nothing.
        assert!(!snip_rect_is_copyable(Point::new(-500, 10), Point::new(-400, 60), mon));
        assert!(!snip_rect_is_copyable(Point::new(0, 2000), Point::new(100, 2100), mon));
    }

    // ---- decide_copy_plan ----

    #[test]
    fn plan_is_snip_for_nonempty_selection_in_snip_mode() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, snip(0, 10, 10, 110, 60), Point::new(5, 5), &m);
        assert_eq!(
            plan,
            Some(CopyPlan::Snip {
                monitor: 0,
                a: Point::new(10, 10),
                b: Point::new(110, 60),
            })
        );
    }

    #[test]
    fn plan_is_snip_for_negative_drag() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, snip(0, 110, 60, 10, 10), Point::new(5, 5), &m);
        assert_eq!(
            plan,
            Some(CopyPlan::Snip {
                monitor: 0,
                a: Point::new(110, 60),
                b: Point::new(10, 10),
            })
        );
    }

    #[test]
    fn plan_is_snip_on_secondary_monitor_with_local_coords() {
        let m = two_monitors();
        // Selection coords are MONITOR-LOCAL: monitor 1's negative virtual
        // origin is irrelevant here; its width/height bound the clip.
        let plan = decide_copy_plan(ModeKind::Snip, snip(1, 0, 0, 500, 500), Point::new(5, 5), &m);
        assert_eq!(
            plan,
            Some(CopyPlan::Snip {
                monitor: 1,
                a: Point::new(0, 0),
                b: Point::new(500, 500),
            })
        );
    }

    #[test]
    fn plan_falls_back_to_full_frame_for_zero_area_selection() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, snip(0, 50, 50, 50, 50), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_falls_back_for_fully_outside_selection() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, snip(0, -500, 0, -400, 100), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_falls_back_for_invalid_selection_monitor() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, snip(99, 0, 0, 100, 100), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_ignores_selection_outside_snip_mode() {
        let m = two_monitors();
        // A selection can only exist in Snip mode, but the mode gate is explicit.
        for kind in [ModeKind::Spotlight, ModeKind::Zoom] {
            let plan = decide_copy_plan(kind, snip(0, 10, 10, 110, 60), Point::new(7, 7), &m);
            assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
        }
    }

    #[test]
    fn plan_full_frame_uses_focused_monitor() {
        let m = two_monitors();
        let plan = decide_copy_plan(ModeKind::Snip, None, Point::new(-100, 200), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 1 }));
        // Cursor outside all monitors: fallback monitor 0.
        let plan = decide_copy_plan(ModeKind::Zoom, None, Point::new(9999, 0), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_is_none_without_monitors() {
        let plan = decide_copy_plan(ModeKind::Snip, snip(0, 0, 0, 10, 10), Point::new(0, 0), &[]);
        assert_eq!(plan, None);
    }

    // ---- sanitize_zoom_params ----

    #[test]
    fn zoom_params_in_range_pass_through() {
        assert_eq!(sanitize_zoom_params(1.25, 1.0, 16.0), (1.25, 1.0, 16.0));
    }

    #[test]
    fn zoom_params_repair_bad_step() {
        assert_eq!(sanitize_zoom_params(0.5, 1.0, 16.0).0, 1.25);
        assert_eq!(sanitize_zoom_params(f32::NAN, 1.0, 16.0).0, 1.25);
        assert_eq!(sanitize_zoom_params(f32::INFINITY, 1.0, 16.0).0, 1.25);
        assert_eq!(sanitize_zoom_params(1.0, 1.0, 16.0).0, 1.25);
    }

    #[test]
    fn zoom_params_repair_bad_min() {
        assert_eq!(sanitize_zoom_params(1.25, 0.5, 16.0).1, 1.0);
        assert_eq!(sanitize_zoom_params(1.25, f32::NAN, 16.0).1, 1.0);
    }

    #[test]
    fn zoom_params_repair_max_not_above_min() {
        let (_, min, max) = sanitize_zoom_params(1.25, 2.0, 2.0);
        assert!(max > min);
        assert_eq!(max, 4.0);
        let (_, min, max) = sanitize_zoom_params(1.25, 3.0, f32::NAN);
        assert!(max > min);
        assert_eq!(max, 6.0);
    }
}
