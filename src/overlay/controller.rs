//! Overlay orchestration: freeze/unfreeze, COMPOSABLE mode layers, event
//! routing, border-flash feedback, clipboard copy. Platform-agnostic shell
//! around the PURE [`ModeStack`] and [`crate::overlay::composite`] pixel ops;
//! the per-OS pieces (surfaces, cursor, clipboard) go through the
//! [`crate::platform`] seam.
//!
//! Implementation notes (contract clarifications — public API kept):
//! - **Default mode**: `freeze` enters Spotlight (product spec default) and
//!   flashes the border ONCE ([`OverlayController::flash_count`]).
//! - **Mode model**: Spotlight is a TOGGLE (`S`: layer on/off — with every
//!   layer off the screen stays frozen but UNVEILED); the zoom hold (`F` or
//!   the zoom-modifier wheel chord) is an effect LAYER re-activating at the
//!   last-used factor; Capture (`C`) RE-BASES the freeze (see below). After
//!   every key-driven activation the border flashes `flash_count(kind)` times
//!   (S=1, F=2, C=3) — synchronous and brief by design (see `flash_border`);
//!   deactivating a layer does not flash.
//! - **Capture re-freeze**: entering capture mode composes every monitor's
//!   CURRENT view (zoom base → veil → spotlight hole) and swaps it in as the
//!   new ORIGINAL, stashing the pre-capture originals ([`rebase_freeze`]).
//!   The snip selection and the clipboard copy then operate on the EFFECTED
//!   pixels (WYSIWYG), and the frame gains the persistent accent capture
//!   indicator ([`crate::overlay::composite::compose_frame`] step 5) with no
//!   further dimming (the base already carries the effects). Esc in capture
//!   mode exits back to the pre-capture view with the stashed spotlight/zoom
//!   state restored ([`exit_capture`]); Esc outside capture mode unfreezes.
//! - **Rendering**: every repaint composes the full frame via
//!   [`crate::overlay::composite::compose_frame`] with a
//!   [`crate::overlay::composite::RenderState`] built from the active layers
//!   ([`ModeStack::render_state`]) into the persistent per-monitor frame
//!   buffer — no per-frame allocations in the render path.
//! - **Cursor seeding**: freshly activated layers receive one synthetic
//!   mouse-move with the LIVE cursor position, so the first presented frame
//!   already has the spotlight hole / zoom focus under the cursor instead of
//!   at `(0, 0)`.
//! - **Focused screen**: "monitor under the cursor" (the no-selection copy
//!   fallback) means the monitor whose virtual-screen bounds contain the live
//!   cursor position; falls back to monitor 0 when the cursor is outside every
//!   monitor (transient display-change states) so Ctrl+C always copies
//!   something.
//! - **Per-monitor selections**: snip drags are MONITOR-LOCAL and clamped at
//!   monitor edges by the layer; a selection never spans monitors. The copy
//!   path crops from that monitor's current base — in capture mode the
//!   re-frozen (effected) frame, otherwise the zoomed view when the zoom
//!   layer is active on it, else the original capture (WYSIWYG).
//! - **Keys**: modes never see key events — Esc / Ctrl+C / mode switches /
//!   reset-view are handled by the platform shell (global hotkeys on Windows,
//!   overlay key events matched against the frozen plan elsewhere), so the
//!   `KeyDown` arm of the event path is deliberately inert.

use crate::capture::{Capturer, DibBuffer, MonitorInfo};
use crate::geometry::{Point, Rect};
use crate::overlay::composite::{
    RenderState, ZoomFilter, compose_frame, crop_normalized, draw_border, monitor_index_at,
    virtual_to_local, zoom_resample,
};
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::overlay::modes::{ModeEffect, ModeKind, ModeParams, ModeStack, SnipSelection};
use crate::platform::{OverlaySurface, PlatformServices, SurfaceFactory};
use crate::settings::model::{AppSettings, Rgb};
use anyhow::Result;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Border-flash ON time per repetition (spec: 70 ms).
const FLASH_ON_MS: u64 = 70;
/// Border-flash OFF (normal frame) time per repetition (spec: 50 ms).
const FLASH_OFF_MS: u64 = 50;
/// Border-flash frame thickness in physical pixels (spec: 6).
const FLASH_THICKNESS: u32 = 6;
/// Border-flash frame color: white.
const FLASH_COLOR: Rgb = Rgb {
    r: 255,
    g: 255,
    b: 255,
};

/// A repaint deferred because its surface was busy: full frame or a
/// monitor-local region. Merging follows the damage contract: the union of
/// every deferred region covers all pixels that changed since the last
/// attached frame, so the eventual present is exact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PendingRepaint {
    Full,
    Region(Rect),
}

impl PendingRepaint {
    /// First deferred repaint from a `present` dirty argument (`None` = full).
    fn from_dirty(dirty: Option<Rect>) -> Self {
        match dirty {
            Some(r) => Self::Region(r),
            None => Self::Full,
        }
    }

    /// Fold a new `present` dirty argument into an existing deferred repaint.
    fn merge(self, dirty: Option<Rect>) -> Self {
        match (self, dirty) {
            (Self::Full, _) | (_, None) => Self::Full,
            (Self::Region(a), Some(b)) => Self::Region(a.union(&b)),
        }
    }

    /// Back to the `present` dirty argument (`Full` → `None`).
    fn as_present_arg(self) -> Option<Rect> {
        match self {
            Self::Full => None,
            Self::Region(r) => Some(r),
        }
    }
}

/// Everything that exists only while the screen is frozen.
///
/// `originals`, `frames`, `monitors`, and `windows` are parallel vectors in
/// [`Capturer::capture_all`] order; index = monitor index.
struct FreezeState {
    /// Per-monitor ORIGINAL (undarkened) captures — the base for compositing
    /// and (when no zoom layer is active on the monitor) the clipboard source.
    originals: Vec<DibBuffer>,
    /// Per-monitor scratch frame composited by `compose_frame` before
    /// `present`. Persistent across repaints so the render path never
    /// allocates; `compose_frame` overwrites every pixel, so stale contents
    /// are impossible.
    frames: Vec<DibBuffer>,
    /// Monitor metadata (virtual-screen bounds + DPI), parallel to `originals`.
    monitors: Vec<MonitorInfo>,
    /// One overlay surface per monitor, parallel to `originals`.
    windows: Vec<Box<dyn OverlaySurface>>,
    /// Per-monitor repaints DEFERRED because the surface was busy (Wayland
    /// buffer-slot pacing; surfaces with immediate presentation never defer).
    /// Drained by [`OverlayController::process_pending_repaints`], always
    /// presenting the freshest composed frame.
    pending_repaint: Vec<Option<PendingRepaint>>,
    /// Mode state (spotlight/zoom-hold/snip layers + the capture stash);
    /// layers are rebuilt from the freeze-time [`ModeParams`] on activation.
    modes: ModeStack,
    /// Freeze-time settings snapshot (dim opacity + veil color + mode params).
    settings: AppSettings,
    /// Pre-capture per-monitor ORIGINALS, stashed while capture mode's
    /// re-frozen (effects-baked) base occupies `originals`; `None` outside
    /// capture mode. Invariant: `capture.is_some() == modes.in_capture()` —
    /// every capture transition (set_mode/add_mode/toggle_mode/unfreeze)
    /// moves this stash and the mode stack's layer stash together.
    capture: Option<Vec<DibBuffer>>,
}

/// Owns the frozen captures, one [`OverlaySurface`] per monitor, and the
/// composable mode layers.
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
    /// Last-activated mode kind: set by key-driven layer activations and
    /// reset to Spotlight on every freeze and on Esc from capture mode.
    /// Meaningless while unfrozen (returns the last used kind).
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

    /// Border-flash repetitions for a mode activation: Spotlight = 1,
    /// Zoom = 2, Snip = 3 (product spec).
    pub fn flash_count(kind: ModeKind) -> u32 {
        match kind {
            ModeKind::Spotlight => 1,
            ModeKind::Zoom => 2,
            ModeKind::Snip => 3,
        }
    }

    /// Capture all monitors ONCE via `capturer`, create one overlay surface
    /// per monitor via `surfaces`, enter Spotlight mode, present the initial
    /// frames, and flash the border once. Cursor seeding and clipboard copies
    /// go through `services`.
    ///
    /// Settings are SNAPSHOT at freeze time: changing hotkeys/radius/zoom while
    /// frozen takes effect on the next freeze. No-op when already frozen.
    pub fn freeze(
        &mut self,
        capturer: &dyn Capturer,
        settings: &AppSettings,
        surfaces: &SurfaceFactory,
        services: &dyn PlatformServices,
    ) -> Result<()> {
        if self.is_frozen() {
            return Ok(());
        }

        // Single capture pass for the whole desktop.
        let captured = capturer.capture_all()?;
        let (monitors, originals): (Vec<MonitorInfo>, Vec<DibBuffer>) =
            captured.into_iter().unzip();

        // One overlay surface per monitor, all reporting to one shared sink
        // wired back into this controller. Every surface also gets the SHARED
        // monitor-rect list so it can reroute focus-delivered input (e.g.
        // `WM_MOUSEWHEEL` goes to the focus window, not the one under the
        // cursor) to the monitor actually containing the cursor. If creation
        // of surface N fails, the already-created surfaces close via Drop as
        // `windows` unwinds.
        let sink = self.make_sink();
        let monitor_rects = Rc::new(monitors.iter().map(|m| m.rect).collect::<Vec<_>>());
        let mut windows: Vec<Box<dyn OverlaySurface>> = Vec::with_capacity(monitors.len());
        for (index, monitor) in monitors.iter().enumerate() {
            windows.push(surfaces(
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
        let monitor_count = monitors.len();
        let mut state = FreezeState {
            originals,
            frames,
            monitors,
            windows,
            pending_repaint: vec![None; monitor_count],
            modes: ModeStack::new(mode_params(&settings)),
            settings,
            capture: None,
        };

        // Spotlight is the default mode (product spec). Seed the live cursor
        // position, present the initial frame on every monitor, then flash
        // the border once (freeze == spotlight activation).
        seed_cursor(&mut state, services);
        for m in 0..state.windows.len() {
            render_and_present(&mut state, m, None);
        }
        flash_border(&mut state, Self::flash_count(ModeKind::Spotlight));

        *self.inner.borrow_mut() = Some(state);
        self.active = ModeKind::Spotlight;
        Ok(())
    }

    /// Esc / cancel contract: in CAPTURE mode this only EXITS capture — the
    /// pre-capture originals and the stashed spotlight/zoom state are
    /// restored (the re-frozen base is dropped) and the session stays frozen;
    /// anywhere else it destroys all overlay windows and drops the captures.
    /// No-op when not frozen.
    pub fn unfreeze(&mut self) {
        {
            let mut slot = self.inner.borrow_mut();
            if let Some(state) = slot.as_mut()
                && state.capture.is_some()
            {
                exit_capture(state);
                self.active = ModeKind::Spotlight;
                return;
            }
        }
        // Take first, drop AFTER releasing the borrow: window teardown never
        // runs while the cell is mutably borrowed.
        let taken = self.inner.borrow_mut().take();
        drop(taken);
    }

    pub fn active_mode(&self) -> ModeKind {
        self.active
    }

    /// PLAIN mode key. Capture (`ModeKind::Snip`) ENTERS capture mode: the
    /// freeze is RE-BASED on the currently composited view (the spotlight
    /// and/or zoom effects active at that moment baked into the new base —
    /// [`rebase_freeze`]) and a fresh snip layer opens over it; re-pressing
    /// while already in capture only resets the selection (no second
    /// re-base). Every other kind is a FULL switch — reset ALL layers to
    /// fresh state (zoom back to 1.0, snip selection cleared, spotlight
    /// radius back to default, cursor re-seeded) and make `kind` the only
    /// active layer; switching directly OUT of capture mode this way drops
    /// the re-frozen base with the mode stack's layer stash, restoring the
    /// pre-capture originals. Full repaint and border flash follow either
    /// way. No-op when not frozen.
    pub fn set_mode(&mut self, kind: ModeKind, services: &dyn PlatformServices) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        // The re-base composes the CURRENT layers, before the stack stashes
        // them for capture mode.
        if kind == ModeKind::Snip {
            if !state.modes.in_capture() {
                rebase_freeze(state);
            }
        } else if let Some(pre) = state.capture.take() {
            // A full switch OUT of capture mode: the stack drops its layer
            // stash below, so the pre-capture originals go back with it —
            // the two capture stashes always move together.
            state.originals = pre;
        }
        // Layer parameters come from the freeze-time snapshot; live settings
        // edits therefore apply on the NEXT freeze, per the freeze contract.
        state.modes.set_mode(kind);
        seed_cursor(state, services);
        for m in 0..state.windows.len() {
            render_and_present(state, m, None);
        }
        self.active = kind;
        flash_border(state, Self::flash_count(kind));
    }

    /// ADD `kind`'s layer WITHOUT resetting the existing ones — the zoom-hold
    /// layer resumes at the last-used factor. `Snip` is capture mode, not an
    /// additive layer: entering it RE-BASES the freeze exactly like
    /// [`set_mode`](Self::set_mode). Full repaint + border flash. No-op when
    /// the layer is already active or when not frozen.
    pub fn add_mode(&mut self, kind: ModeKind, services: &dyn PlatformServices) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        if state.modes.is_active(kind) {
            return; // adding an active layer is a no-op: no reset, no flash
        }
        // Capture entry re-bases here too: the pixel stash must move in
        // lockstep with the layer stash the stack is about to take.
        if kind == ModeKind::Snip {
            rebase_freeze(state);
        }
        state.modes.add_mode(kind);
        seed_cursor(state, services);
        for m in 0..state.windows.len() {
            render_and_present(state, m, None);
        }
        self.active = kind;
        flash_border(state, Self::flash_count(kind));
    }

    /// TOGGLE key (spotlight's `S`, zoom hold's `F`): remove the layer when
    /// active (with no layers left the screen stays frozen but the overlay is
    /// UNVEILED), add it otherwise — the spotlight fresh, the zoom hold at
    /// the last-used factor. `Snip` toggles capture mode: ON re-bases the
    /// freeze like [`set_mode`](Self::set_mode), OFF exits capture, restoring
    /// the pre-capture originals and the stashed layers. Toggling ON re-seeds
    /// the cursor, full-repaints, and flashes `flash_count(kind)` times;
    /// toggling off only full-repaints (no flash). No-op when not frozen.
    pub fn toggle_mode(&mut self, kind: ModeKind, services: &dyn PlatformServices) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        let activating = !state.modes.is_active(kind);
        // `Snip` toggles capture mode: ON re-bases the freeze, OFF drops the
        // re-frozen base — the pixel stash moves with the stack's layer stash.
        if kind == ModeKind::Snip {
            if activating {
                rebase_freeze(state);
            } else if let Some(pre) = state.capture.take() {
                state.originals = pre;
            }
        }
        state.modes.toggle_mode(kind);
        if activating {
            seed_cursor(state, services);
            for m in 0..state.windows.len() {
                render_and_present(state, m, None);
            }
            self.active = kind;
            flash_border(state, Self::flash_count(kind));
        } else {
            for m in 0..state.windows.len() {
                render_and_present(state, m, None);
            }
        }
    }

    /// Route an overlay window event to the mode stack, then apply its
    /// [`crate::overlay::modes::ModeEffect`]: for each requested repaint,
    /// re-compose the frame and `present` it.
    ///
    /// The cancel (Esc), copy (Ctrl+C), mode-switch/toggle, and reset-zoom
    /// gestures are NOT handled here — the platform shell catches them (as
    /// global hotkeys on Windows, as overlay key events elsewhere) and calls
    /// [`unfreeze`](Self::unfreeze),
    /// [`snip_copy_and_close`](Self::snip_copy_and_close),
    /// [`set_mode`](Self::set_mode) / [`add_mode`](Self::add_mode) /
    /// [`toggle_mode`](Self::toggle_mode), or [`reset_view`](Self::reset_view).
    pub fn handle_overlay_event(&mut self, monitor: usize, event: OverlayEvent) {
        dispatch_event(&self.inner, monitor, event);
    }

    /// Reset-view hotkey path (default binding `0`): zoom back to 1.0 when the
    /// zoom layer is active and apply the repaint; a cheap no-op otherwise
    /// (and whenever not frozen).
    ///
    /// This is a DEDICATED entry point, deliberately not routed through
    /// [`handle_overlay_event`](Self::handle_overlay_event): layers never see
    /// key events (keys that matter are global hotkeys), so a synthesized key
    /// event would silently do nothing.
    pub fn reset_view(&mut self) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        let effect = state.modes.reset_view();
        for &(m, dirty) in &effect.repaint {
            render_and_present(state, m, dirty);
        }
    }

    /// Present every repaint that was deferred because its surface was busy
    /// (Wayland buffer-slot pacing; a no-op on platforms with immediate
    /// presents). Shells call this from their event loop; the pending repaint
    /// always carries the freshest composed frame, so the screen can never
    /// fall behind the cursor by more than the compositor's release cadence.
    pub fn process_pending_repaints(&mut self) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        drain_pending_repaints(state);
    }

    /// Ctrl+C contract: when a snip selection exists, crop it from that
    /// monitor's current base — in capture mode the re-frozen EFFECTED frame
    /// (spotlight/zoom baked in), otherwise the zoomed view when the zoom
    /// layer is active on that monitor, else the ORIGINAL capture — WYSIWYG
    /// with the presented frame — and copy it to the clipboard; otherwise
    /// copy the FULL current base of the monitor currently under the cursor
    /// ("focused screen"). Works from ANY mode combination. Then unfreeze.
    /// `Ok(())` no-op when not frozen.
    ///
    /// The selection is monitor-local (drags clamp at that monitor's edges);
    /// the crop normalizes any drag direction via [`crop_normalized`].
    pub fn snip_copy_and_close(&mut self, services: &dyn PlatformServices) -> Result<()> {
        // Closing is unconditional, so take the session state out up-front;
        // `None` (not frozen) is the documented no-op and never touches the
        // clipboard.
        let Some(state) = self.inner.borrow_mut().take() else {
            return Ok(());
        };

        let cursor = services.cursor_position_virtual().unwrap_or_default();
        let rects: Vec<Rect> = state.monitors.iter().map(|m| m.rect).collect();
        let plan = decide_copy_plan(state.modes.snip_selection(), cursor, &rects);

        let copy_result = match plan {
            Some(CopyPlan::Snip { monitor, a, b }) => {
                match copy_crop(&state.originals[monitor], state.modes.zoom_on(monitor), a, b) {
                    Some(snip) => services.copy_image_to_clipboard(&snip),
                    // Unreachable — decide_copy_plan pre-validated the clipped
                    // rect. Keep the "Ctrl+C always copies something" invariant.
                    None => services.copy_image_to_clipboard(&state.originals[monitor]),
                }
            }
            // Full current base: passed by reference, no buffer copy.
            Some(CopyPlan::FullMonitor { monitor }) => {
                services.copy_image_to_clipboard(&state.originals[monitor])
            }
            None => Ok(()), // zero monitors captured: nothing to copy
        };
        drop(state); // close every surface even when the copy failed
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

/// Freeze-time [`ModeParams`] snapshot from settings. The zoom triple is
/// sanitized so a hand-edited settings file can never break the layer
/// constructor.
fn mode_params(settings: &AppSettings) -> ModeParams {
    let (zoom_step, zoom_min, zoom_max) = sanitize_zoom_params(
        settings.zoom.step_factor,
        settings.zoom.min,
        settings.zoom.max,
    );
    ModeParams {
        spotlight_radius: settings.spotlight.default_radius,
        radius_modifier: settings.hotkeys.spotlight_radius_modifier,
        zoom_step,
        zoom_min,
        zoom_max,
        zoom_modifier: settings.hotkeys.zoom_modifier,
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

/// Feed `event` to the mode stack and apply the resulting `ModeEffect`'s
/// dirty-region repaints. Returns `true` when the stack requested exit.
fn apply_overlay_event(state: &mut FreezeState, monitor: usize, event: OverlayEvent) -> bool {
    if monitor >= state.windows.len() {
        return false; // stale event from an already-destroyed window
    }
    let effect = match event {
        OverlayEvent::MouseMove { at } => state.modes.on_mouse_move(monitor, at),
        OverlayEvent::MouseWheel {
            at,
            delta,
            modifiers,
        } => state.modes.on_wheel(monitor, at, delta, modifiers),
        OverlayEvent::LeftButtonDown { at } => state.modes.on_left_button_down(monitor, at),
        OverlayEvent::LeftButtonUp { at } => state.modes.on_left_button_up(monitor, at),
        // Keys never reach the layers: everything key-driven is handled by
        // the platform shell (documented in the module header).
        OverlayEvent::KeyDown { .. } => ModeEffect::none(),
    };
    for &(m, dirty) in &effect.repaint {
        render_and_present(state, m, dirty);
    }
    effect.exit
}

/// Re-compose monitor `m`'s full frame from the active layers and present it.
/// `dirty: Some(rect)` lets the window composite only that region (the
/// per-mouse-move fast path); the frame itself is always composed completely
/// (compose_frame overwrites every pixel). Present failures are best-effort
/// ignored: stale pixels until the next repaint beat a dead overlay.
fn render_and_present(state: &mut FreezeState, m: usize, dirty: Option<Rect>) {
    if m >= state.windows.len() {
        return; // defensive: a layer asked to repaint a nonexistent monitor
    }
    compose_frame_for(state, m);
    present_or_defer(state, m, dirty);
}

/// Present `frames[m]` now, merging any earlier deferred region into the
/// damage; when the surface is busy (Wayland buffer-slot pacing), defer to
/// `pending_repaint[m]` instead — [`OverlayController::process_pending_repaints`]
/// drains it with the freshest composed frame, so intermediate frames are
/// coalesced rather than queued behind the compositor's release cadence.
fn present_or_defer(state: &mut FreezeState, m: usize, dirty: Option<Rect>) {
    let FreezeState {
        frames,
        windows,
        pending_repaint,
        ..
    } = state;
    if windows[m].can_present() {
        let dirty = pending_repaint[m].map_or(dirty, |p| p.merge(dirty).as_present_arg());
        let _ = windows[m].present(&frames[m], dirty);
        pending_repaint[m] = None;
    } else {
        pending_repaint[m] = Some(
            pending_repaint[m]
                .map_or_else(|| PendingRepaint::from_dirty(dirty), |p| p.merge(dirty)),
        );
    }
}

/// Present every deferred repaint whose surface has since freed a slot.
fn drain_pending_repaints(state: &mut FreezeState) {
    let FreezeState {
        frames,
        windows,
        pending_repaint,
        ..
    } = state;
    for m in 0..windows.len() {
        let Some(pending) = pending_repaint[m] else {
            continue;
        };
        if windows[m].can_present() {
            let _ = windows[m].present(&frames[m], pending.as_present_arg());
            pending_repaint[m] = None;
        }
    }
}

/// Wait `ms`, draining deferred repaints in small slices so flash frames
/// deferred by buffer-slot pacing still reach the screen during the hold.
fn spin_drain(state: &mut FreezeState, ms: u64) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        drain_pending_repaints(state);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Compose monitor `m`'s frame: build the
/// [`crate::overlay::composite::RenderState`] from the active layers and run
/// the shared pipeline (zoom base → colored darken → spotlight hole → snip
/// selection → capture indicator) into the persistent frame buffer. With NO
/// active layer the veil is dropped entirely (dim opacity 0): the screen
/// stays frozen but shows the original capture. In CAPTURE mode the veil is
/// also dropped: the re-frozen base already carries the effects, so dimming
/// again would double-darken and break WYSIWYG with the copy.
fn compose_frame_for(state: &mut FreezeState, m: usize) {
    // Split borrows across disjoint fields: modes (read) builds the render
    // state, originals[m] (read) + frames[m] (write) are the pixel buffers,
    // settings (read) supplies the veil parameters.
    let FreezeState {
        originals,
        frames,
        modes,
        settings,
        ..
    } = state;
    let render_state = modes.render_state(m);
    let viewport = Rect::new(0, 0, originals[m].width, originals[m].height);
    let dim_opacity = if !modes.in_capture() && modes.any_active() {
        settings.overlay.dim_opacity
    } else {
        0
    };
    compose_frame(
        &originals[m],
        &mut frames[m],
        viewport,
        &render_state,
        dim_opacity,
        settings.overlay.color,
    );
}

/// Capture-mode entry: RE-BASE the freeze on the currently composited view.
/// Each monitor's frame is composed from the CURRENT layers (zoom base →
/// veil → spotlight hole — a snip layer cannot be stashed, and the capture
/// indicator is excluded by construction) and swapped in as the new ORIGINAL;
/// the pre-capture originals move into `state.capture` for
/// [`exit_capture`]. One-off allocation per monitor on a key press — never
/// on a repaint path.
fn rebase_freeze(state: &mut FreezeState) {
    let FreezeState {
        originals,
        modes,
        settings,
        capture,
        ..
    } = state;
    let dim_opacity = if modes.any_active() {
        settings.overlay.dim_opacity
    } else {
        0
    };
    let mut pre = Vec::with_capacity(originals.len());
    for m in 0..originals.len() {
        let render_state = RenderState {
            snip: None,
            capture: false,
            ..modes.render_state(m)
        };
        let viewport = Rect::new(0, 0, originals[m].width, originals[m].height);
        let mut base = DibBuffer::new(originals[m].width, originals[m].height);
        compose_frame(
            &originals[m],
            &mut base,
            viewport,
            &render_state,
            dim_opacity,
            settings.overlay.color,
        );
        pre.push(std::mem::replace(&mut originals[m], base));
    }
    *capture = Some(pre);
}

/// Esc from capture mode: drop the re-frozen base (the pre-capture originals
/// return), restore the stashed spotlight/zoom layers, and full-repaint every
/// monitor back to the pre-capture view.
fn exit_capture(state: &mut FreezeState) {
    if let Some(pre) = state.capture.take() {
        state.originals = pre;
    }
    state.modes.exit_capture();
    for m in 0..state.windows.len() {
        render_and_present(state, m, None);
    }
}

/// Synchronous border flash on EVERY monitor: compose the frame, draw a
/// white border ring, present, hold [`FLASH_ON_MS`]; re-compose (erasing the
/// border), present, hold [`FLASH_OFF_MS`] — repeated `count` times. Blocks
/// the UI thread for at most `count * (FLASH_ON_MS + FLASH_OFF_MS)` ms
/// (360 ms worst case for Snip) — deliberate, per the product spec: the
/// flash IS the mode-change feedback. Deferred presents are drained during
/// the holds (see [`spin_drain`]), so the flash survives buffer-slot pacing.
fn flash_border(state: &mut FreezeState, count: u32) {
    for _ in 0..count {
        for m in 0..state.windows.len() {
            compose_frame_for(state, m);
            draw_border(&mut state.frames[m], FLASH_COLOR, FLASH_THICKNESS);
            present_or_defer(state, m, None);
        }
        spin_drain(state, FLASH_ON_MS);
        for m in 0..state.windows.len() {
            compose_frame_for(state, m);
            present_or_defer(state, m, None);
        }
        spin_drain(state, FLASH_OFF_MS);
    }
}

/// Feed the live cursor position into every active layer (see module docs).
/// A full repaint always follows, so no repaint effect is needed.
fn seed_cursor(state: &mut FreezeState, services: &dyn PlatformServices) {
    if state.monitors.is_empty() {
        return;
    }
    let Some(cursor) = services.cursor_position_virtual() else {
        return;
    };
    let rects: Vec<Rect> = state.monitors.iter().map(|m| m.rect).collect();
    let Some(idx) = monitor_index_at(cursor, &rects) else {
        return; // cursor outside every monitor: leave the layers' default origin
    };
    state.modes.seed_cursor(idx, virtual_to_local(cursor, rects[idx]));
}

/// Clamp zoom settings to the `ZoomMode::new` contract (`step > 1.0`,
/// `min >= 1.0`, `max > min`) so a hand-edited settings file can never break
/// the layer constructor. Values already in range pass through untouched.
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
    /// from that monitor's current base (the re-frozen effected frame in
    /// capture mode, the zoomed view when zoom is active there, else the
    /// original frame).
    Snip { monitor: usize, a: Point, b: Point },
    /// Copy the focused monitor's full current base (effected in capture
    /// mode, ORIGINAL otherwise).
    FullMonitor { monitor: usize },
}

/// Pure copy-target decision, factored out for headless testing.
///
/// A selection whose normalized rect (clipped to that monitor's local bounds)
/// is non-empty ⇒ crop plan; every other case ⇒ full frame of the focused
/// monitor. (A selection can only EXIST while the snip layer is active, so
/// its presence is the whole gate.) Returns `None` only when `monitors` is
/// empty. Computed on raw geometry fields so the decision is identical in
/// tests.
fn decide_copy_plan(
    selection: Option<SnipSelection>,
    cursor_virtual: Point,
    monitors: &[Rect],
) -> Option<CopyPlan> {
    if monitors.is_empty() {
        return None;
    }
    if let Some(sel) = selection
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

/// Crop the rect between `a`/`b` from the monitor's COMPOSED BASE: when the
/// zoom layer is active on that monitor (`zoom` = `(factor, focus)`), the
/// base is the `zoom_resample`d view — exactly what the presented frame's
/// spotlight hole / selection shows (WYSIWYG) — otherwise the ORIGINAL
/// capture. Returns `None` for an empty/out-of-bounds rect (any drag
/// direction is normalized). One-off allocation on the copy path only —
/// never on a repaint path.
fn copy_crop(
    original: &DibBuffer,
    zoom: Option<(f32, Point)>,
    a: Point,
    b: Point,
) -> Option<DibBuffer> {
    match zoom {
        Some((factor, focus)) => {
            let viewport = Rect::new(0, 0, original.width, original.height);
            // Nearest is the render path's filter: zero interpolation cost,
            // and the copy must match the presented frame pixel-for-pixel.
            let base = zoom_resample(original, viewport, factor, focus, ZoomFilter::Nearest);
            crop_normalized(&base, a, b)
        }
        None => crop_normalized(original, a, b),
    }
}

/// `true` when the normalized drag rect, clipped to the monitor's local
/// bounds, has positive area. Mirrors `composite::crop_normalized`'s
/// normalize-then-clip semantics on raw fields (the decision must not depend
/// on pixel buffers). Endpoints may arrive in any drag direction; layers clamp
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
    //! copy DECISION logic and the zoomed-base crop are pure buffer math;
    //! `snip_copy_and_close` is only exercised while unfrozen (documented
    //! no-op, services untouched).
    use super::*;
    use crate::settings::model::AppSettings;

    /// [`PlatformServices`] double for the unfrozen-path tests: never
    /// consulted (every call would be a bug), so both methods panic.
    struct PanicServices;

    impl PlatformServices for PanicServices {
        fn cursor_position_virtual(&self) -> Option<Point> {
            panic!("services must not be consulted while unfrozen")
        }

        fn copy_image_to_clipboard(&self, _frame: &DibBuffer) -> Result<()> {
            panic!("services must not be consulted while unfrozen")
        }
    }

    // ---- frozen-session fakes (headless: in-memory capturer/surfaces) -------

    /// In-memory capturer: hands out clones of the configured captures.
    struct FakeCapturer {
        captured: Vec<(MonitorInfo, DibBuffer)>,
    }

    impl Capturer for FakeCapturer {
        fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
            Ok(self.captured.clone())
        }
    }

    /// Overlay surface recording every presented frame (shared with the test).
    struct FakeSurface {
        presents: Rc<RefCell<Vec<DibBuffer>>>,
    }

    impl OverlaySurface for FakeSurface {
        fn present(&mut self, frame: &DibBuffer, _dirty: Option<Rect>) -> Result<()> {
            self.presents.borrow_mut().push(frame.clone());
            Ok(())
        }
    }

    /// Services double: fixed cursor position, clipboard writes recorded.
    struct FakeServices {
        cursor: Point,
        copied: Rc<RefCell<Vec<DibBuffer>>>,
    }

    impl PlatformServices for FakeServices {
        fn cursor_position_virtual(&self) -> Option<Point> {
            Some(self.cursor)
        }

        fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
            self.copied.borrow_mut().push(frame.clone());
            Ok(())
        }
    }

    /// A frozen session over fake monitors plus the recording handles.
    struct FakeFreeze {
        controller: OverlayController,
        services: FakeServices,
        presents: Vec<Rc<RefCell<Vec<DibBuffer>>>>,
        copied: Rc<RefCell<Vec<DibBuffer>>>,
    }

    fn monitor_info(rect: Rect) -> MonitorInfo {
        MonitorInfo {
            rect,
            dpi_x: 96,
            dpi_y: 96,
            is_primary: rect.x == 0 && rect.y == 0,
            device_name: String::new(),
        }
    }

    /// Default settings with a spotlight radius small enough to leave dimmed
    /// pixels on the 32x32 fake monitors.
    fn fake_settings() -> AppSettings {
        let mut s = AppSettings::default();
        s.spotlight.default_radius = 6;
        s
    }

    /// M0 (origin) shows [`coord_pattern`], M1 (negative virtual x) its
    /// inverse. `cursor` is the fixed virtual-screen position the services
    /// double reports.
    fn freeze_fake(cursor: Point) -> FakeFreeze {
        let captured: Vec<(MonitorInfo, DibBuffer)> = [
            (Rect::new(0, 0, 32, 32), make_buf(32, 32, coord_pattern)),
            (
                Rect::new(-32, 0, 32, 32),
                make_buf(32, 32, |x, y| {
                    let [b, g, r, a] = coord_pattern(x, y);
                    [255 - b, 255 - g, 255 - r, a]
                }),
            ),
        ]
        .into_iter()
        .map(|(rect, buf)| (monitor_info(rect), buf))
        .collect();
        let presents: Vec<Rc<RefCell<Vec<DibBuffer>>>> = (0..captured.len())
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let factory_presents = presents.clone();
        let factory = move |index: usize,
                            _rect: Rect,
                            _rects: Rc<Vec<Rect>>,
                            _sink: OverlayEventSink|
              -> Result<Box<dyn OverlaySurface>> {
            Ok(Box::new(FakeSurface {
                presents: factory_presents[index].clone(),
            }))
        };
        let factory: &SurfaceFactory = &factory;
        let copied = Rc::new(RefCell::new(Vec::new()));
        let services = FakeServices {
            cursor,
            copied: copied.clone(),
        };
        let mut controller = OverlayController::new();
        controller
            .freeze(
                &FakeCapturer { captured },
                &fake_settings(),
                factory,
                &services,
            )
            .expect("freeze with fakes");
        FakeFreeze {
            controller,
            services,
            presents,
            copied,
        }
    }

    fn last_present(presents: &Rc<RefCell<Vec<DibBuffer>>>) -> DibBuffer {
        presents
            .borrow()
            .last()
            .expect("at least one present")
            .clone()
    }

    /// Every pixel at least `inset` px away from the frame edge must match.
    fn assert_interior_eq(a: &DibBuffer, b: &DibBuffer, inset: u32) {
        for y in inset..a.height - inset {
            for x in inset..a.width - inset {
                assert_eq!(px(a, x, y), px(b, x, y), "interior pixel ({x},{y})");
            }
        }
    }

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

    /// Synthetic buffer from a pixel generator (pub fields — no reliance on
    /// `DibBuffer::new`, which belongs to another module).
    fn make_buf(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> DibBuffer {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&f(x, y));
            }
        }
        DibBuffer {
            width: w,
            height: h,
            stride: w * 4,
            pixels,
        }
    }

    fn px(buf: &DibBuffer, x: u32, y: u32) -> [u8; 4] {
        let i = (y * buf.stride + x * 4) as usize;
        buf.pixels[i..i + 4].try_into().unwrap()
    }

    // ---- PendingRepaint ------------------------------------------------

    #[test]
    fn pending_from_dirty_maps_full_and_region() {
        assert_eq!(PendingRepaint::from_dirty(None), PendingRepaint::Full);
        let r = Rect::new(1, 2, 3, 4);
        assert_eq!(PendingRepaint::from_dirty(Some(r)), PendingRepaint::Region(r));
    }

    #[test]
    fn pending_merge_full_subsumes_everything() {
        let r = Rect::new(1, 2, 3, 4);
        assert_eq!(PendingRepaint::Full.merge(Some(r)), PendingRepaint::Full);
        assert_eq!(PendingRepaint::Full.merge(None), PendingRepaint::Full);
        assert_eq!(PendingRepaint::Region(r).merge(None), PendingRepaint::Full);
    }

    #[test]
    fn pending_merge_unions_regions() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(
            PendingRepaint::Region(a).merge(Some(b)),
            PendingRepaint::Region(Rect::new(0, 0, 15, 15))
        );
    }

    #[test]
    fn pending_as_present_arg_round_trips() {
        let r = Rect::new(1, 2, 3, 4);
        assert_eq!(PendingRepaint::Region(r).as_present_arg(), Some(r));
        assert_eq!(PendingRepaint::Full.as_present_arg(), None);
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
    fn set_mode_and_add_mode_when_unfrozen_are_noops() {
        let mut c = OverlayController::new();
        c.set_mode(ModeKind::Zoom, &PanicServices);
        c.add_mode(ModeKind::Snip, &PanicServices);
        c.add_mode(ModeKind::Spotlight, &PanicServices);
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

    #[test]
    fn snip_copy_when_unfrozen_is_ok_noop_and_touches_nothing() {
        let mut c = OverlayController::new();
        // Must return Ok WITHOUT consulting the platform services.
        assert!(c.snip_copy_and_close(&PanicServices).is_ok());
        assert!(!c.is_frozen());
    }

    // ---- flash_count (spec: S=1, Z=2, C=3) ----

    #[test]
    fn flash_count_maps_one_two_three() {
        assert_eq!(OverlayController::flash_count(ModeKind::Spotlight), 1);
        assert_eq!(OverlayController::flash_count(ModeKind::Zoom), 2);
        assert_eq!(OverlayController::flash_count(ModeKind::Snip), 3);
    }

    #[test]
    fn flash_timings_are_the_spec_values() {
        // Pinned so an accidental edit of the feedback cadence fails loudly.
        assert_eq!(FLASH_ON_MS, 70);
        assert_eq!(FLASH_OFF_MS, 50);
        assert_eq!(FLASH_THICKNESS, 6);
        assert_eq!(FLASH_COLOR, Rgb { r: 255, g: 255, b: 255 });
    }

    // ---- mode_params ----

    #[test]
    fn mode_params_snapshots_settings() {
        let s = AppSettings::default();
        let p = mode_params(&s);
        assert_eq!(p.spotlight_radius, s.spotlight.default_radius);
        assert_eq!(p.radius_modifier, s.hotkeys.spotlight_radius_modifier);
        assert_eq!(p.zoom_modifier, s.hotkeys.zoom_modifier);
        assert_eq!(p.zoom_step, s.zoom.step_factor);
        assert_eq!(p.zoom_min, s.zoom.min);
        assert_eq!(p.zoom_max, s.zoom.max);
    }

    #[test]
    fn mode_params_sanitizes_zoom() {
        let mut s = AppSettings::default();
        s.zoom.step_factor = 0.5;
        s.zoom.min = 0.25;
        s.zoom.max = f32::NAN;
        let p = mode_params(&s);
        assert_eq!(p.zoom_step, 1.25);
        assert_eq!(p.zoom_min, 1.0);
        assert!(p.zoom_max > p.zoom_min);
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
    fn plan_is_snip_for_nonempty_selection() {
        let m = two_monitors();
        let plan = decide_copy_plan(snip(0, 10, 10, 110, 60), Point::new(5, 5), &m);
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
        let plan = decide_copy_plan(snip(0, 110, 60, 10, 10), Point::new(5, 5), &m);
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
        let plan = decide_copy_plan(snip(1, 0, 0, 500, 500), Point::new(5, 5), &m);
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
        let plan = decide_copy_plan(snip(0, 50, 50, 50, 50), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_falls_back_for_fully_outside_selection() {
        let m = two_monitors();
        let plan = decide_copy_plan(snip(0, -500, 0, -400, 100), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_falls_back_for_invalid_selection_monitor() {
        let m = two_monitors();
        let plan = decide_copy_plan(snip(99, 0, 0, 100, 100), Point::new(7, 7), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_full_frame_uses_focused_monitor() {
        let m = two_monitors();
        let plan = decide_copy_plan(None, Point::new(-100, 200), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 1 }));
        // Cursor outside all monitors: fallback monitor 0.
        let plan = decide_copy_plan(None, Point::new(9999, 0), &m);
        assert_eq!(plan, Some(CopyPlan::FullMonitor { monitor: 0 }));
    }

    #[test]
    fn plan_is_none_without_monitors() {
        let plan = decide_copy_plan(snip(0, 0, 0, 10, 10), Point::new(0, 0), &[]);
        assert_eq!(plan, None);
    }

    // ---- copy_crop: snip from the composed (zoomed) base ----

    /// Coordinate-encoding pattern: pixel (x, y) = [x, y, x^y, 255] (BGRA).
    fn coord_pattern(x: u32, y: u32) -> [u8; 4] {
        [x as u8, y as u8, (x ^ y) as u8, 255]
    }

    #[test]
    fn copy_crop_without_zoom_crops_the_original() {
        let src = make_buf(32, 32, coord_pattern);
        let crop = copy_crop(&src, None, Point::new(4, 6), Point::new(12, 10)).unwrap();
        assert_eq!((crop.width, crop.height), (8, 4));
        for y in 0..crop.height {
            for x in 0..crop.width {
                assert_eq!(px(&crop, x, y), coord_pattern(x + 4, y + 6));
            }
        }
    }

    #[test]
    fn copy_crop_with_zoom_crops_the_zoomed_base_not_the_original() {
        // 32x32 original, zoom 2.0 around focus (16,16): output pixel o
        // samples src 16 + (o + 0.5 - 16)/2 - 0.5 (composite mapping,
        // nearest). The selection is in OUTPUT (screen) coordinates, so the
        // crop must come from the resampled view — WYSIWYG.
        let src = make_buf(32, 32, coord_pattern);
        let a = Point::new(8, 8);
        let b = Point::new(24, 24);
        let zoomed = copy_crop(&src, Some((2.0, Point::new(16, 16))), a, b).unwrap();
        let plain = copy_crop(&src, None, a, b).unwrap();
        assert_eq!((zoomed.width, zoomed.height), (16, 16));
        assert_ne!(
            zoomed.pixels, plain.pixels,
            "zoomed crop must differ from the unzoomed one"
        );
        // Exact mapping check for every cropped pixel.
        let src_of = |o: u32| (16.0f32 + (o as f32 + 0.5 - 16.0) / 2.0 - 0.5).round() as u32;
        for y in 0..16u32 {
            for x in 0..16u32 {
                let expect = coord_pattern(src_of(x + 8), src_of(y + 8));
                assert_eq!(px(&zoomed, x, y), expect, "zoomed crop pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn copy_crop_zoomed_base_matches_compose_input_contract() {
        // The crop must be pixel-identical to cropping the buffer the render
        // path would have built (same resample call with the same viewport).
        let src = make_buf(32, 32, coord_pattern);
        let a = Point::new(3, 5);
        let b = Point::new(29, 20);
        let (factor, focus) = (1.5, Point::new(10, 22));
        let crop = copy_crop(&src, Some((factor, focus)), a, b).unwrap();
        let base = zoom_resample(
            &src,
            Rect::new(0, 0, 32, 32),
            factor,
            focus,
            ZoomFilter::Nearest,
        );
        let expect = crop_normalized(&base, a, b).unwrap();
        assert_eq!(crop.pixels, expect.pixels);
    }

    #[test]
    fn copy_crop_degenerate_rect_is_none_in_both_bases() {
        let src = make_buf(16, 16, coord_pattern);
        assert!(copy_crop(&src, None, Point::new(4, 4), Point::new(4, 4)).is_none());
        assert!(
            copy_crop(&src, Some((2.0, Point::new(8, 8))), Point::new(4, 4), Point::new(4, 4))
                .is_none()
        );
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

    // ---- frozen sessions: Esc routing, capture re-freeze, effected copies ----

    #[test]
    fn esc_in_spotlight_mode_fully_unfreezes() {
        let mut f = freeze_fake(Point::new(16, 16));
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
    }

    #[test]
    fn esc_in_spotlight_off_fully_unfreezes() {
        let mut f = freeze_fake(Point::new(16, 16));
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services); // spotlight off
        assert!(f.controller.is_frozen());
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
    }

    #[test]
    fn esc_in_capture_exits_capture_and_restores_the_pre_capture_view() {
        let mut f = freeze_fake(Point::new(16, 16));
        let pre_capture = last_present(&f.presents[0]); // dimmed, hole at (16,16)

        f.controller.set_mode(ModeKind::Snip, &f.services);
        assert!(
            f.controller.is_frozen(),
            "capture entry keeps the session frozen"
        );
        let capture_frame = last_present(&f.presents[0]);
        assert_interior_eq(&capture_frame, &pre_capture, 2);
        assert_ne!(
            px(&capture_frame, 0, 0),
            px(&pre_capture, 0, 0),
            "the capture indicator repaints the frame edge"
        );
        assert_eq!(
            px(&capture_frame, 0, 0),
            px(&capture_frame, 31, 31),
            "one uniform indicator ring around the frame"
        );

        f.controller.unfreeze(); // Esc: exit capture, stay frozen
        assert!(
            f.controller.is_frozen(),
            "Esc in capture must NOT unfreeze"
        );
        assert_eq!(f.controller.active_mode(), ModeKind::Spotlight);
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            pre_capture.pixels,
            "the pre-capture view is restored exactly"
        );

        f.controller.unfreeze(); // second Esc: unfreeze for real
        assert!(!f.controller.is_frozen());
    }

    #[test]
    fn capture_copy_crops_the_effected_pixels_not_the_original() {
        let mut f = freeze_fake(Point::new(2, 2)); // hole parked in the corner
        let pre_capture = last_present(&f.presents[0]);

        f.controller.set_mode(ModeKind::Snip, &f.services);
        // Drag a selection in the dimmed area, away from the spotlight hole.
        for event in [
            OverlayEvent::LeftButtonDown {
                at: Point::new(10, 10),
            },
            OverlayEvent::MouseMove {
                at: Point::new(26, 26),
            },
            OverlayEvent::LeftButtonUp {
                at: Point::new(26, 26),
            },
        ] {
            f.controller.handle_overlay_event(0, event);
        }
        f.controller
            .snip_copy_and_close(&f.services)
            .expect("copy");
        assert!(!f.controller.is_frozen(), "Ctrl+C closes the session");

        let copied = f.copied.borrow();
        let crop = copied.last().expect("one clipboard write");
        assert_eq!((crop.width, crop.height), (16, 16));
        let expected = crop_normalized(&pre_capture, Point::new(10, 10), Point::new(26, 26))
            .expect("non-empty selection");
        assert_eq!(
            crop.pixels, expected.pixels,
            "the crop comes from the re-frozen (effected) base"
        );
        let raw = crop_normalized(
            &make_buf(32, 32, coord_pattern),
            Point::new(10, 10),
            Point::new(26, 26),
        )
        .unwrap();
        assert_ne!(
            crop.pixels, raw.pixels,
            "effected (dimmed) pixels, NOT the undarkened original"
        );
    }

    #[test]
    fn capture_copy_crops_the_right_monitors_effected_base() {
        let mut f = freeze_fake(Point::new(-16, 16)); // cursor on M1 (negative x)
        let pre1 = last_present(&f.presents[1]);

        f.controller.set_mode(ModeKind::Snip, &f.services);
        // Drag on monitor 1 (selection endpoints are monitor-local).
        for event in [
            OverlayEvent::LeftButtonDown {
                at: Point::new(10, 10),
            },
            OverlayEvent::MouseMove {
                at: Point::new(20, 20),
            },
            OverlayEvent::LeftButtonUp {
                at: Point::new(20, 20),
            },
        ] {
            f.controller.handle_overlay_event(1, event);
        }
        f.controller
            .snip_copy_and_close(&f.services)
            .expect("copy");

        let copied = f.copied.borrow();
        let crop = copied.last().expect("one clipboard write");
        let expected = crop_normalized(&pre1, Point::new(10, 10), Point::new(20, 20)).unwrap();
        assert_eq!(
            crop.pixels, expected.pixels,
            "the crop comes from monitor 1's effected base"
        );
        let pre0 = last_present(&f.presents[0]);
        let other = crop_normalized(&pre0, Point::new(10, 10), Point::new(20, 20)).unwrap();
        assert_ne!(
            crop.pixels, other.pixels,
            "monitor mapping: not monitor 0's pixels"
        );
    }

    #[test]
    fn capture_rebases_with_zoom_baked_in_full_monitor_copy_is_effected() {
        let mut f = freeze_fake(Point::new(16, 16));
        // Zoom hold via the wheel chord (implicit activation), one notch in.
        f.controller.handle_overlay_event(
            0,
            OverlayEvent::MouseWheel {
                at: Point::new(16, 16),
                delta: 120,
                modifiers: crate::hotkeys::gesture::Modifiers::SHIFT,
            },
        );
        let pre_capture = last_present(&f.presents[0]); // zoomed + dimmed + hole

        f.controller.set_mode(ModeKind::Snip, &f.services);
        // No selection: the full-monitor copy is the re-frozen effected view.
        f.controller
            .snip_copy_and_close(&f.services)
            .expect("copy");

        let copied = f.copied.borrow();
        let frame = copied.last().expect("one clipboard write");
        assert_eq!(
            frame.pixels, pre_capture.pixels,
            "full copy == the re-frozen (zoom + spotlight baked in) view"
        );
        assert_ne!(
            frame.pixels,
            make_buf(32, 32, coord_pattern).pixels,
            "NOT the plain original"
        );
    }

    #[test]
    fn pressing_capture_again_clears_the_selection_without_rebasing() {
        let mut f = freeze_fake(Point::new(16, 16));
        let pre_capture = last_present(&f.presents[0]);

        f.controller.set_mode(ModeKind::Snip, &f.services);
        for event in [
            OverlayEvent::LeftButtonDown {
                at: Point::new(8, 8),
            },
            OverlayEvent::MouseMove {
                at: Point::new(20, 20),
            },
            OverlayEvent::LeftButtonUp {
                at: Point::new(20, 20),
            },
        ] {
            f.controller.handle_overlay_event(0, event);
        }
        f.controller.set_mode(ModeKind::Snip, &f.services); // again: reset, no re-bake
        // Selection cleared: the copy falls back to the full-monitor base.
        f.controller
            .snip_copy_and_close(&f.services)
            .expect("copy");

        let copied = f.copied.borrow();
        let frame = copied.last().expect("one clipboard write");
        assert_eq!(
            frame.pixels, pre_capture.pixels,
            "base untouched by the second press (no indicator/selection baked in)"
        );
    }

    #[test]
    fn esc_in_capture_restores_spotlight_off_state() {
        let mut f = freeze_fake(Point::new(16, 16));
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services); // off: unveiled
        let unveiled = last_present(&f.presents[0]);
        assert_eq!(unveiled.pixels, make_buf(32, 32, coord_pattern).pixels);

        f.controller.set_mode(ModeKind::Snip, &f.services);
        f.controller.unfreeze(); // exit capture
        assert!(f.controller.is_frozen());
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            unveiled.pixels,
            "spotlight stays off after Esc from capture"
        );
        // And it toggles back on normally.
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
        assert_ne!(last_present(&f.presents[0]).pixels, unveiled.pixels);
    }

    #[test]
    fn set_mode_spotlight_out_of_capture_restores_the_pre_capture_view() {
        let mut f = freeze_fake(Point::new(16, 16));
        let pre_capture = last_present(&f.presents[0]); // dimmed, hole at (16,16)

        f.controller.set_mode(ModeKind::Snip, &f.services);
        f.controller.set_mode(ModeKind::Spotlight, &f.services);
        assert!(f.controller.is_frozen());
        assert_eq!(f.controller.active_mode(), ModeKind::Spotlight);
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            pre_capture.pixels,
            "the re-frozen base is dropped, not composed under a fresh layer"
        );

        // The pixel stash left with the mode switch: Esc now unfreezes for
        // real instead of exiting a capture the stack already forgot.
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
    }

    #[test]
    fn add_mode_snip_enters_capture_with_rebase_and_indicator() {
        let mut f = freeze_fake(Point::new(16, 16));
        let pre_capture = last_present(&f.presents[0]);

        f.controller.add_mode(ModeKind::Snip, &f.services);
        assert!(f.controller.is_frozen());
        let capture_frame = last_present(&f.presents[0]);
        assert_interior_eq(&capture_frame, &pre_capture, 2);
        assert_ne!(
            px(&capture_frame, 0, 0),
            px(&pre_capture, 0, 0),
            "the capture indicator marks the re-based session"
        );

        // Esc exits capture (the session stays frozen); a second Esc
        // unfreezes for real.
        f.controller.unfreeze();
        assert!(f.controller.is_frozen());
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            pre_capture.pixels,
            "the pre-capture view is restored exactly"
        );
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
    }

    #[test]
    fn toggle_mode_snip_enters_and_exits_capture() {
        let mut f = freeze_fake(Point::new(16, 16));
        let pre_capture = last_present(&f.presents[0]);

        f.controller.toggle_mode(ModeKind::Snip, &f.services);
        assert!(f.controller.is_frozen());
        let capture_frame = last_present(&f.presents[0]);
        assert_ne!(
            px(&capture_frame, 0, 0),
            px(&pre_capture, 0, 0),
            "toggling on enters capture (indicator ring)"
        );

        f.controller.toggle_mode(ModeKind::Snip, &f.services);
        assert!(
            f.controller.is_frozen(),
            "toggling off exits capture, the session stays frozen"
        );
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            pre_capture.pixels,
            "the pre-capture view is restored exactly"
        );
    }
}
