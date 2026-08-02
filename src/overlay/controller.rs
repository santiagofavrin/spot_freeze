//! Overlay orchestration: freeze/unfreeze, COMPOSABLE mode layers, event
//! routing, freeze transitions, the mode legend, clipboard copy.
//! Platform-agnostic shell around the PURE [`ModeStack`] and
//! [`crate::overlay::composite`] pixel ops; the per-OS pieces (surfaces,
//! cursor, clipboard) go through the [`crate::platform`] seam.
//!
//! Implementation notes (contract clarifications — public API kept):
//! - **Default mode**: `freeze` enters Spotlight (product spec default) with
//!   the entry transition (see below).
//! - **Mode model**: Spotlight is a TOGGLE (`S`: layer on/off — with every
//!   layer off the screen stays frozen but UNVEILED); the zoom hold (`F` or
//!   the zoom-modifier wheel chord) is an effect LAYER re-activating at the
//!   last-used factor; Capture (`C`) RE-BASES the freeze (see below). Every
//!   key-driven mode change is SEAMLESS: no flash frames, no border rings.
//!   Spotlight toggles and full mode switches repaint once, instantly.
//! - **Transitions**: freeze/unfreeze runs on the pure step schedule in
//!   [`crate::overlay::fade`] (200 ms, ease-out cubic, <= 240 ms hard cap).
//!   Freeze fades the overlay IN while the spotlight circle expands from 60%
//!   to 100% of its radius; a full unfreeze mirrors (veil lifts, circle
//!   shrinks). Where the surface supports a constant window alpha
//!   ([`OverlaySurface::supports_alpha`] — Windows layered windows, macOS)
//!   the freeze/unfreeze fades are per-step alpha updates over frames
//!   re-composed with the shrinking/growing circle; elsewhere (Wayland) the
//!   entry presents re-composed frames whose veil ramps with the step alpha,
//!   and the exit blends toward the original capture
//!   ([`crate::overlay::composite::blend_frames`]) into the preallocated
//!   `fade_scratch` buffers through the normal present path (capped at 8
//!   full-frame steps). Esc from capture mode and the Ctrl+C close stay
//!   instant (no fade). Transitions are ATOMIC: they run synchronously on
//!   the UI thread; input pressed during one queues or drops per platform
//!   without ever corrupting the session (the interruption state machine is
//!   defined in the fade module docs).
//! - **Capture re-freeze**: entering capture mode composes every monitor's
//!   CURRENT view (zoom base → veil → spotlight hole — WITHOUT the mode
//!   legend) and swaps it in as the new ORIGINAL, stashing the pre-capture
//!   originals ([`rebase_freeze`]). The snip selection and the clipboard
//!   copy then operate on the EFFECTED pixels (WYSIWYG), and the frame gains
//!   the persistent accent capture indicator
//!   ([`crate::overlay::composite::compose_frame`] step 5). Capture mode has
//!   its own veil: the lighter, cooler snip veil (`overlay.snip_dim_opacity`
//!   / `overlay.snip_color`) replaces the spotlight veil, and the drawn
//!   rectangle stays COMPLETELY CLEAR (the un-dimmed base) behind a crisp
//!   two-tone border. Esc in capture mode exits back to the pre-capture
//!   view with the stashed spotlight/zoom state restored ([`exit_capture`]);
//!   Esc outside capture mode unfreezes.
//! - **Rendering**: every repaint composes the full frame via
//!   [`crate::overlay::composite::compose_frame`] with a
//!   [`crate::overlay::composite::RenderState`] built from the active layers
//!   ([`ModeStack::render_state`]) into the persistent per-monitor frame
//!   buffer — no per-frame allocations in the render path — then paints the
//!   mode legend ([`crate::overlay::legend`]) on top: a large translucent
//!   pill near the top-center listing the modes as tabs (active ones
//!   highlighted) with their freeze-time hotkey bindings. The legend never
//!   reaches the capture originals: `rebase_freeze` composes without it, so
//!   snip copies stay clean.
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
    RenderState, ZoomFilter, blend_frames, compose_frame, crop_normalized, monitor_index_at,
    virtual_to_local, zoom_resample,
};
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::overlay::fade::{FadeClock, FadeDirection, fade_step, spotlight_radius_scale};
use crate::overlay::legend::Legend;
use crate::overlay::modes::{ModeEffect, ModeKind, ModeParams, ModeStack, SnipSelection};
use crate::platform::{OverlaySurface, PlatformServices, SurfaceFactory};
use crate::settings::model::{AppSettings, Rgb};
use anyhow::Result;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

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
    /// Freeze-time settings snapshot (veil parameters + mode params + the
    /// hotkey bindings the legend shows).
    settings: AppSettings,
    /// The mode legend painted into every composed frame while frozen
    /// (tabs + freeze-time bindings); never baked into the capture originals.
    legend: Legend,
    /// Pre-capture per-monitor ORIGINALS, stashed while capture mode's
    /// re-frozen (effects-baked) base occupies `originals`; `None` outside
    /// capture mode. Invariant: `capture.is_some() == modes.in_capture()` —
    /// every capture transition (set_mode/add_mode/toggle_mode/unfreeze)
    /// moves this stash and the mode stack's layer stash together.
    capture: Option<Vec<DibBuffer>>,
    /// Per-monitor blend target for the pixel-blend fade (surfaces without
    /// [`OverlaySurface::supports_alpha`]), allocated ONCE at freeze time and
    /// reused by every freeze-fade step of the session — fades never allocate
    /// per step. EMPTY when every surface has a constant window alpha (the
    /// fade then only updates `set_alpha`).
    fade_scratch: Vec<DibBuffer>,
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
    /// Time source for the fade driver (system clock in production; tests
    /// inject a manual clock via [`OverlayController::with_fade_clock`]).
    clock: FadeClock,
}

impl OverlayController {
    pub fn new() -> Self {
        Self::with_fade_clock(FadeClock::system())
    }

    /// Controller driving its fades on `clock` (tests: a manual clock, so
    /// fades walk the full step schedule in zero wall-clock time).
    pub fn with_fade_clock(clock: FadeClock) -> Self {
        Self {
            inner: Rc::new(RefCell::new(None)),
            active: ModeKind::Spotlight,
            clock,
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.inner.borrow().is_some()
    }

    /// Number of overlay windows/monitors while frozen; 0 otherwise.
    pub fn monitor_count(&self) -> usize {
        self.inner.borrow().as_ref().map_or(0, |s| s.windows.len())
    }

    /// Capture all monitors ONCE via `capturer`, create one overlay surface
    /// per monitor via `surfaces`, enter Spotlight mode, and run the entry
    /// transition (see [`crate::overlay::fade`]): the veil eases in while the
    /// spotlight circle expands from 60% to its full radius. Cursor seeding
    /// and clipboard copies go through `services`.
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
        // The pixel-blend fade's scratch targets, preallocated for the whole
        // session (constant-alpha surfaces fade without them).
        let fade_scratch = if windows.iter().all(|w| w.supports_alpha()) {
            Vec::new()
        } else {
            originals
                .iter()
                .map(|o| DibBuffer::new(o.width, o.height))
                .collect()
        };

        let settings = settings.clone();
        let monitor_count = monitors.len();
        let mut state = FreezeState {
            originals,
            frames,
            monitors,
            windows,
            pending_repaint: vec![None; monitor_count],
            modes: ModeStack::new(mode_params(&settings)),
            legend: Legend::from_hotkeys(&settings.hotkeys),
            settings,
            capture: None,
            fade_scratch,
        };

        // Spotlight is the default mode (product spec). Seed the live cursor
        // position, then run the entry transition: the veil eases in and the
        // circle expands to its full radius (freeze == spotlight activation).
        seed_cursor(&mut state, services);
        fade_in(&mut state, &self.clock);

        *self.inner.borrow_mut() = Some(state);
        self.active = ModeKind::Spotlight;
        Ok(())
    }

    /// Esc / cancel contract: in CAPTURE mode this only EXITS capture — the
    /// pre-capture originals and the stashed spotlight/zoom state are
    /// restored (the re-frozen base is dropped) and the session stays frozen
    /// (deliberately INSTANT: no fade there); anywhere else it fades the
    /// overlay OUT (see [`crate::overlay::fade`]), then destroys all overlay
    /// windows and drops the captures. No-op when not frozen.
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
        // runs while the cell is mutably borrowed. The fade runs on the taken
        // state — sink events arriving mid-fade already see `None` and no-op.
        let mut taken = self.inner.borrow_mut().take();
        if let Some(state) = taken.as_mut() {
            fade_out(state, &self.clock);
        }
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
    /// pre-capture originals. One full repaint follows either way — mode
    /// switches are INSTANT by design (only freeze/unfreeze fades animate;
    /// see the module docs).
    /// No-op when not frozen.
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
    }

    /// ADD `kind`'s layer WITHOUT resetting the existing ones — the zoom-hold
    /// layer resumes at the last-used factor. `Snip` is capture mode, not an
    /// additive layer: entering it RE-BASES the freeze exactly like
    /// [`set_mode`](Self::set_mode). Every kind repaints once. No-op when the
    /// layer is already active or when not frozen.
    pub fn add_mode(&mut self, kind: ModeKind, services: &dyn PlatformServices) {
        let mut slot = self.inner.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        if state.modes.is_active(kind) {
            return; // adding an active layer is a no-op: no reset, no repaint
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
    }

    /// TOGGLE key (spotlight's `S`, zoom hold's `F`): remove the layer when
    /// active (with no layers left the screen stays frozen but the overlay is
    /// UNVEILED), add it otherwise — the spotlight fresh, the zoom hold at
    /// the last-used factor. `Snip` toggles capture mode: ON re-bases the
    /// freeze like [`set_mode`](Self::set_mode), OFF exits capture, restoring
    /// the pre-capture originals and the stashed layers. Every toggle repaints
    /// once, instantly. No-op when not frozen.
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
        match (kind, activating) {
            (ModeKind::Spotlight, true) => {
                state.modes.toggle_mode(kind);
                seed_cursor(state, services);
                self.active = kind;
            }
            (ModeKind::Spotlight, false) => {
                state.modes.toggle_mode(kind);
            }
            _ => {
                state.modes.toggle_mode(kind);
                if activating {
                    seed_cursor(state, services);
                    self.active = kind;
                }
            }
        }
        for m in 0..state.windows.len() {
            render_and_present(state, m, None);
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

/// Wait `ms`, draining deferred repaints in small slices so frames deferred
/// by buffer-slot pacing still reach the screen during the hold.
fn wait_draining(state: &mut FreezeState, wait: Duration, clock: &FadeClock) {
    let start = clock.now();
    loop {
        let remaining = wait.saturating_sub(clock.now().saturating_sub(start));
        if remaining.is_zero() {
            break;
        }
        drain_pending_repaints(state);
        clock.sleep(remaining.min(Duration::from_millis(2)));
    }
}

/// How one transition step reaches the screen (per surface kind, per
/// transition — see the [`crate::overlay::fade`] module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Carrier {
    /// Constant-alpha surfaces (Windows, macOS): the frame carries the FULL
    /// veil; the step alpha goes to `set_alpha` (a true crossfade against the
    /// live desktop). Used by the freeze/unfreeze fades only.
    WindowAlpha,
    /// The composed frame carries a veil RAMP (dim lerped by the step alpha)
    /// and is presented directly. Freeze entry on blend surfaces (alpha 0 ==
    /// the original capture == the live screen, so the overlay appears
    /// seamlessly).
    VeilRamp,
    /// Blend surfaces fading OUT: the frame carries the full veil and is
    /// blended toward the original capture by the step alpha into
    /// `fade_scratch` — a veil ramp cannot crossfade a zoom base away.
    BlendToOriginal,
}

/// Per-transition compose/veil schedule shared by [`fade_in`] and
/// [`fade_out`].
struct TransitionPlan {
    /// Veil alpha at step alpha 0 and at step alpha 255 (lerped per step).
    /// Equal values = a constant veil (the window-alpha and blend carriers).
    dim_at_0: u8,
    dim_at_255: u8,
    /// Scale the spotlight circle 60%<->100% with the step alpha
    /// ([`spotlight_radius_scale`]).
    animate_radius: bool,
    /// Scale the legend by the step alpha too (freeze entry on blend
    /// surfaces, so the pill fades in with the veil instead of popping).
    fade_legend: bool,
    carrier: Carrier,
}

/// Shared step loop of every transition: sample the clock, compose each
/// monitor's frame with the step's veil/radius values, push it through the
/// plan's carrier, then wait to the next nominal boundary draining deferred
/// repaints. A busy blend surface simply skips its step — the next one
/// carries a newer alpha, so the transition degrades and never stalls (see
/// the [`crate::overlay::fade`] module docs). The caller applies the exact
/// endpoint after the loop.
fn run_transition(
    state: &mut FreezeState,
    direction: FadeDirection,
    plan: &TransitionPlan,
    clock: &FadeClock,
) {
    let start = clock.now();
    while let Some(step) = fade_step(clock.now().saturating_sub(start), direction) {
        let a = step.alpha;
        let dim = lerp_u8(plan.dim_at_0, plan.dim_at_255, a);
        let scale = if plan.animate_radius {
            spotlight_radius_scale(a)
        } else {
            1000
        };
        let legend_alpha = if plan.fade_legend { a } else { 255 };
        for m in 0..state.windows.len() {
            match plan.carrier {
                Carrier::WindowAlpha => {
                    compose_frame_anim(state, m, Some(dim), scale, legend_alpha);
                    let _ = state.windows[m].set_alpha(a);
                    present_or_defer(state, m, None);
                }
                Carrier::VeilRamp => {
                    if state.windows[m].can_present() {
                        compose_frame_anim(state, m, Some(dim), scale, legend_alpha);
                        present_or_defer(state, m, None);
                    }
                }
                Carrier::BlendToOriginal => {
                    if state.windows[m].can_present() {
                        compose_frame_anim(state, m, Some(dim), scale, legend_alpha);
                        blend_frames(
                            &state.originals[m],
                            &state.frames[m],
                            &mut state.fade_scratch[m],
                            a,
                        );
                        let _ = state.windows[m].present(&state.fade_scratch[m], None);
                    }
                }
            }
        }
        wait_draining(state, step.wait, clock);
    }
}

/// Freeze entry transition: the veil eases in while the spotlight circle
/// expands from 60% to 100% of its radius (freeze == spotlight activation).
/// Constant-alpha surfaces start fully transparent and crossfade via
/// `set_alpha` over frames re-composed with the growing circle; blend
/// surfaces present re-composed frames whose veil ramps with the step alpha
/// (at alpha 0 the frame IS the original capture — what the live screen just
/// showed — so the overlay appears seamlessly either way).
fn fade_in(state: &mut FreezeState, clock: &FadeClock) {
    let use_alpha = state.windows.iter().all(|w| w.supports_alpha());
    let dim = state.settings.overlay.dim_opacity;
    let plan = if use_alpha {
        // Transparent BEFORE the first present: the initial frame must never
        // appear at full opacity ahead of the transition.
        for w in &mut state.windows {
            let _ = w.set_alpha(0);
        }
        TransitionPlan {
            dim_at_0: dim,
            dim_at_255: dim,
            animate_radius: true,
            fade_legend: false,
            carrier: Carrier::WindowAlpha,
        }
    } else {
        TransitionPlan {
            dim_at_0: 0,
            dim_at_255: dim,
            animate_radius: true,
            fade_legend: true,
            carrier: Carrier::VeilRamp,
        }
    };
    run_transition(state, FadeDirection::In, &plan, clock);
    // The exact endpoint: the settled frame at full opacity on every monitor.
    if use_alpha {
        for w in &mut state.windows {
            let _ = w.set_alpha(255);
        }
    }
    for m in 0..state.windows.len() {
        compose_frame_for(state, m);
        present_or_defer(state, m, None);
    }
}

/// Full-unfreeze fade-OUT on the taken state (the windows die right after):
/// the mirror of [`fade_in`] — the veil lifts while the circle shrinks back
/// to 60% — down to the exact transparent/original endpoint. The blend path's
/// full-frame presents supersede every repaint deferred so far, so the
/// pending slots are dropped up front.
fn fade_out(state: &mut FreezeState, clock: &FadeClock) {
    let use_alpha = state.windows.iter().all(|w| w.supports_alpha());
    let animate_radius = state.modes.is_active(ModeKind::Spotlight);
    let (dim, _) = veil_for(
        state.modes.in_capture(),
        state.modes.any_active(),
        &state.settings,
    );
    let plan = TransitionPlan {
        dim_at_0: dim,
        dim_at_255: dim,
        animate_radius,
        fade_legend: false,
        carrier: if use_alpha {
            Carrier::WindowAlpha
        } else {
            Carrier::BlendToOriginal
        },
    };
    if !use_alpha {
        for pending in &mut state.pending_repaint {
            *pending = None;
        }
    }
    run_transition(state, FadeDirection::Out, &plan, clock);
    if use_alpha {
        for w in &mut state.windows {
            let _ = w.set_alpha(0);
        }
    } else {
        // The alpha-0 blend IS the original capture: present it directly.
        for m in 0..state.windows.len() {
            if state.windows[m].can_present() {
                let _ = state.windows[m].present(&state.originals[m], None);
            }
        }
    }
}

/// The veil (dim alpha, color) for a layer-set snapshot: capture mode gets
/// the lighter, cooler snip veil (`overlay.snip_dim_opacity` /
/// `overlay.snip_color`); any other active layer set gets the spotlight veil;
/// no layers means no veil at all (the frozen screen shows the original
/// capture — dim alpha 0).
fn veil_for(in_capture: bool, any_active: bool, settings: &AppSettings) -> (u8, Rgb) {
    if in_capture {
        (
            settings.overlay.snip_dim_opacity,
            settings.overlay.snip_color,
        )
    } else if any_active {
        (settings.overlay.dim_opacity, settings.overlay.color)
    } else {
        (0, settings.overlay.color)
    }
}

/// Veil alpha between two endpoint veils at the eased progress byte `alpha`
/// (`from` at alpha 0, `to` at alpha 255): exact at both ends, rounded to
/// nearest in between — the same one-division blend family as
/// [`crate::overlay::composite::darken`].
fn lerp_u8(from: u8, to: u8, alpha: u8) -> u8 {
    ((from as u32 * (255 - alpha as u32) + to as u32 * alpha as u32 + 127) / 255) as u8
}

/// Spotlight radius at `scale_pm` permille of the settled radius, rounded to
/// the nearest pixel.
fn scale_radius(radius: u32, scale_pm: u32) -> u32 {
    ((radius as u64 * scale_pm as u64 + 500) / 1000) as u32
}

/// Compose monitor `m`'s frame at its settled state (no transition
/// overrides): [`compose_frame_anim`] with the current veil, full radius,
/// full-strength legend.
fn compose_frame_for(state: &mut FreezeState, m: usize) {
    compose_frame_anim(state, m, None, 1000, 255);
}

/// Compose monitor `m`'s frame: build the
/// [`crate::overlay::composite::RenderState`] from the active layers, apply
/// the transition overrides (`dim` forces the veil alpha, `radius_scale_pm`
/// scales the spotlight circle, `legend_alpha` scales the legend), run the
/// shared pipeline (zoom base → colored darken → spotlight hole → snip
/// selection → capture indicator) into the persistent frame buffer, and paint
/// the mode legend on top. The veil comes from [`veil_for`]: capture mode
/// gets the snip veil, other active layer sets the spotlight veil, and with
/// NO active layer the veil is dropped entirely (the frozen screen shows the
/// original capture).
fn compose_frame_anim(state: &mut FreezeState, m: usize, dim: Option<u8>, radius_scale_pm: u32, legend_alpha: u8) {
    // Split borrows across disjoint fields: modes (read) builds the render
    // state, originals[m] (read) + frames[m] (write) are the pixel buffers,
    // settings (read) supplies the veil parameters, legend (read) the pill.
    let FreezeState {
        originals,
        frames,
        modes,
        settings,
        legend,
        ..
    } = state;
    let mut render_state = modes.render_state(m);
    if radius_scale_pm < 1000
        && let Some((center, radius)) = render_state.spotlight
    {
        render_state.spotlight = Some((center, scale_radius(radius, radius_scale_pm)));
    }
    let viewport = Rect::new(0, 0, originals[m].width, originals[m].height);
    let (base_dim, veil_color) = veil_for(modes.in_capture(), modes.any_active(), settings);
    compose_frame(
        &originals[m],
        &mut frames[m],
        viewport,
        &render_state,
        dim.unwrap_or(base_dim),
        veil_color,
    );
    legend.paint(&mut frames[m], &modes.layers_active(), legend_alpha);
}

/// Capture-mode entry: RE-BASE the freeze on the currently composited view.
/// Each monitor's frame is composed from the CURRENT layers (zoom base →
/// veil → spotlight hole — a snip layer cannot be stashed, and the capture
/// indicator is excluded by construction) and swapped in as the new ORIGINAL;
/// the pre-capture originals move into `state.capture` for
/// [`exit_capture`]. The mode legend is NOT composed here (it is painted by
/// [`compose_frame_anim`] only), so it can never be baked into the base and
/// leak into a copy. One-off allocation per monitor on a key press — never
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
    use crate::overlay::composite::darken;
    use crate::overlay::fade::{FADE_STEP_MS, FADE_STEPS, fade_alpha};
    use crate::settings::model::AppSettings;
    use std::cell::Cell;

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

    /// Overlay surface recording every presented frame (shared with the
    /// test). No constant-alpha support ⇒ the controller exercises the
    /// pixel-blend fade path with it.
    struct FakeSurface {
        presents: Rc<RefCell<Vec<DibBuffer>>>,
    }

    impl OverlaySurface for FakeSurface {
        fn present(&mut self, frame: &DibBuffer, _dirty: Option<Rect>) -> Result<()> {
            self.presents.borrow_mut().push(frame.clone());
            Ok(())
        }
    }

    /// Constant-alpha surface (the Windows/macOS path): records the applied
    /// alpha sequence alongside the presented frames.
    struct AlphaSurface {
        presents: Rc<RefCell<Vec<DibBuffer>>>,
        alphas: Rc<RefCell<Vec<u8>>>,
    }

    impl OverlaySurface for AlphaSurface {
        fn present(&mut self, frame: &DibBuffer, _dirty: Option<Rect>) -> Result<()> {
            self.presents.borrow_mut().push(frame.clone());
            Ok(())
        }

        fn supports_alpha(&self) -> bool {
            true
        }

        fn set_alpha(&mut self, alpha: u8) -> Result<()> {
            self.alphas.borrow_mut().push(alpha);
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
        captured: Vec<(MonitorInfo, DibBuffer)>,
        alpha_surfaces: bool,
        presents: Vec<Rc<RefCell<Vec<DibBuffer>>>>,
        alphas: Vec<Rc<RefCell<Vec<u8>>>>,
        copied: Rc<RefCell<Vec<DibBuffer>>>,
    }

    impl FakeFreeze {
        /// Freeze again over the same fake monitors (recordings accumulate:
        /// the new surfaces share the handles). A no-op while already frozen.
        fn refreeze(&mut self) {
            let alpha_surfaces = self.alpha_surfaces;
            let factory_presents = self.presents.clone();
            let factory_alphas = self.alphas.clone();
            let factory = move |index: usize,
                                _rect: Rect,
                                _rects: Rc<Vec<Rect>>,
                                _sink: OverlayEventSink|
                  -> Result<Box<dyn OverlaySurface>> {
                Ok(if alpha_surfaces {
                    Box::new(AlphaSurface {
                        presents: factory_presents[index].clone(),
                        alphas: factory_alphas[index].clone(),
                    })
                } else {
                    Box::new(FakeSurface {
                        presents: factory_presents[index].clone(),
                    })
                })
            };
            let factory: &SurfaceFactory = &factory;
            self.controller
                .freeze(
                    &FakeCapturer {
                        captured: self.captured.clone(),
                    },
                    &fake_settings(),
                    factory,
                    &self.services,
                )
                .expect("refreeze with fakes");
        }
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
    /// pixels on the 32x32 fake monitors. The layer clamps the radius to its
    /// 10 px minimum ([`SpotlightMode::new`]), so the settled fake radius is
    /// 10 wherever tests compute expected frames.
    fn fake_settings() -> AppSettings {
        let mut s = AppSettings::default();
        s.spotlight.default_radius = 6;
        s
    }

    /// Manual fade clock: sleeps advance time instantly by the exact request,
    /// so fades walk the full nominal step schedule in zero wall-clock time.
    fn manual_fade_clock() -> FadeClock {
        FadeClock::manual(Rc::new(Cell::new(Duration::ZERO)))
    }

    /// M0 (origin) shows [`coord_pattern`], M1 (negative virtual x) its
    /// inverse. Blend-path surfaces, manual fade clock.
    fn freeze_fake(cursor: Point) -> FakeFreeze {
        freeze_fake_ex(cursor, false, manual_fade_clock())
    }

    /// M0 (origin) shows [`coord_pattern`], M1 (negative virtual x) its
    /// inverse.
    fn two_small_monitors() -> Vec<(MonitorInfo, DibBuffer)> {
        [
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
        .collect()
    }

    /// One 800x160 monitor at the origin — big enough for the legend pill
    /// (the 32x32 monitors of [`two_small_monitors`] skip it).
    fn big_monitor() -> Vec<(MonitorInfo, DibBuffer)> {
        vec![(
            monitor_info(Rect::new(0, 0, 800, 160)),
            make_buf(800, 160, coord_pattern),
        )]
    }

    /// `cursor` is the fixed virtual-screen position the services double
    /// reports; `alpha_surfaces` picks the constant-alpha surface fake (the
    /// Windows/macOS fade path) over the blend-path one.
    fn freeze_fake_ex(cursor: Point, alpha_surfaces: bool, clock: FadeClock) -> FakeFreeze {
        freeze_fake_with(two_small_monitors(), cursor, alpha_surfaces, clock)
    }

    /// The [`freeze_fake_ex`] core over arbitrary fake monitors.
    fn freeze_fake_with(
        captured: Vec<(MonitorInfo, DibBuffer)>,
        cursor: Point,
        alpha_surfaces: bool,
        clock: FadeClock,
    ) -> FakeFreeze {
        let presents: Vec<Rc<RefCell<Vec<DibBuffer>>>> = (0..captured.len())
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let alphas: Vec<Rc<RefCell<Vec<u8>>>> = (0..captured.len())
            .map(|_| Rc::new(RefCell::new(Vec::new())))
            .collect();
        let copied = Rc::new(RefCell::new(Vec::new()));
        let services = FakeServices {
            cursor,
            copied: copied.clone(),
        };
        let controller = OverlayController::with_fade_clock(clock);
        let mut session = FakeFreeze {
            controller,
            services,
            captured,
            alpha_surfaces,
            presents,
            alphas,
            copied,
        };
        session.refreeze();
        session
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

    // ---- transition helpers (veil endpoints, lerp, radius scale) ----

    #[test]
    fn veil_for_selects_snip_spotlight_or_no_veil() {
        let s = AppSettings::default();
        // Capture mode: the snip veil (lighter, cooler).
        assert_eq!(
            veil_for(true, true, &s),
            (s.overlay.snip_dim_opacity, s.overlay.snip_color)
        );
        // Active layers outside capture: the spotlight veil.
        assert_eq!(
            veil_for(false, true, &s),
            (s.overlay.dim_opacity, s.overlay.color)
        );
        // No layers: no veil (the frozen screen shows the original capture).
        assert_eq!(veil_for(false, false, &s), (0, s.overlay.color));
    }

    #[test]
    fn lerp_u8_hits_exact_endpoints_and_rounds_to_nearest() {
        assert_eq!(lerp_u8(0, 160, 0), 0);
        assert_eq!(lerp_u8(0, 160, 255), 160);
        assert_eq!(lerp_u8(160, 0, 255), 0);
        assert_eq!(lerp_u8(160, 0, 0), 160);
        assert_eq!(lerp_u8(90, 90, 128), 90, "constant veil at any alpha");
        assert_eq!(
            lerp_u8(0, 160, 128),
            ((160u32 * 128 + 127) / 255) as u8,
            "one-division blend family"
        );
    }

    #[test]
    fn scale_radius_rounds_to_nearest_pixel() {
        assert_eq!(scale_radius(150, 1000), 150);
        assert_eq!(scale_radius(150, 600), 90);
        assert_eq!(scale_radius(150, 0), 0);
        assert_eq!(scale_radius(5, 500), 3, "2.5 rounds to nearest");
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
        // The snip veil replaces the spotlight veil: the re-frozen base (==
        // the pre-capture frame) dimmed with the snip veil parameters.
        let defaults = AppSettings::default();
        let mut snip_veiled = pre_capture.clone();
        darken(
            &mut snip_veiled,
            defaults.overlay.snip_dim_opacity,
            defaults.overlay.snip_color,
        );
        assert_interior_eq(&capture_frame, &snip_veiled, 2);
        assert_ne!(
            px(&capture_frame, 0, 0),
            px(&snip_veiled, 0, 0),
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
        let defaults = AppSettings::default();
        let mut snip_veiled = pre_capture.clone();
        darken(
            &mut snip_veiled,
            defaults.overlay.snip_dim_opacity,
            defaults.overlay.snip_color,
        );
        assert_interior_eq(&capture_frame, &snip_veiled, 2);
        assert_ne!(
            px(&capture_frame, 0, 0),
            px(&snip_veiled, 0, 0),
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

    // ---- freeze/unfreeze fades ----------------------------------------------
    //
    // The interruption state machine (src/overlay/fade.rs docs): transitions
    // are atomic on the UI thread, so every rapid-toggle case resolves by
    // serialization — these tests pin the sequences and the exact endpoints.
    // The 32x32 fake monitors are smaller than the legend pill, so the
    // legend never paints in them (legend coverage is at the bottom).

    /// Expected present count of one transition: the schedule's nominal
    /// steps plus the exact settled endpoint applied after the last step.
    const TRANSITION_PRESENTS: usize = FADE_STEPS as usize + 1;

    /// The settled spotlight frame for the 32x32 fake: full-strength veil,
    /// hole of `radius` at the cursor (16,16). No legend (monitor too small).
    fn settled_frame(radius: u32) -> DibBuffer {
        let original = make_buf(32, 32, coord_pattern);
        let mut out = DibBuffer::new(32, 32);
        let state = RenderState {
            spotlight: Some((Point::new(16, 16), radius)),
            ..RenderState::default()
        };
        compose_frame(
            &original,
            &mut out,
            Rect::new(0, 0, 32, 32),
            &state,
            160,
            Rgb::BLACK,
        );
        out
    }

    /// Compose the 32x32 frame for one transition step: veil `dim`, circle
    /// `radius` at the cursor.
    fn step_frame(dim: u8, radius: u32) -> DibBuffer {
        let original = make_buf(32, 32, coord_pattern);
        let mut out = DibBuffer::new(32, 32);
        let state = RenderState {
            spotlight: Some((Point::new(16, 16), radius)),
            ..RenderState::default()
        };
        compose_frame(&original, &mut out, Rect::new(0, 0, 32, 32), &state, dim, Rgb::BLACK);
        out
    }

    /// `true` when the frame's 6 px border band is ENTIRELY white — the
    /// removed mode-change flash's signature. Any single non-white band
    /// pixel clears the frame (the amber capture indicator, the two-tone
    /// snip ring and the legend never fill the whole band).
    fn has_white_border_band(buf: &DibBuffer) -> bool {
        const BAND: u32 = 6;
        for y in 0..buf.height {
            for x in 0..buf.width {
                let in_band =
                    x < BAND || y < BAND || x + BAND >= buf.width || y + BAND >= buf.height;
                if in_band && px(buf, x, y) != [255, 255, 255, 255] {
                    return false;
                }
            }
        }
        !buf.pixels.is_empty()
    }


    #[test]
    fn freeze_fades_in_from_the_original_capture() {
        let f = freeze_fake(Point::new(16, 16));
        let p = f.presents[0].borrow();
        assert_eq!(
            p.len(),
            TRANSITION_PRESENTS,
            "steps + exact endpoint, no extra frames"
        );
        let original = make_buf(32, 32, coord_pattern);
        assert_eq!(
            p[0].pixels, original.pixels,
            "alpha 0 == the original capture (the live screen's pixels)"
        );
        // Each step composes the veil ramp + growing circle for the
        // schedule's alpha.
        for k in 1..FADE_STEPS {
            let a = fade_alpha(k * FADE_STEP_MS, FadeDirection::In);
            let expect = step_frame(lerp_u8(0, 160, a), scale_radius(10, spotlight_radius_scale(a)));
            assert_eq!(p[k as usize].pixels, expect.pixels, "fade step {k}");
        }
        // The endpoint is the settled frame (full veil, full radius).
        assert_eq!(p[p.len() - 1].pixels, settled_frame(10).pixels);
    }

    #[test]
    fn freeze_fade_in_drives_constant_alpha_surfaces() {
        let f = freeze_fake_ex(Point::new(16, 16), true, manual_fade_clock());
        // Transparent BEFORE the first present, the schedule's alphas, then
        // the exact opaque endpoint.
        let mut expect = vec![0u8];
        for k in 0..FADE_STEPS {
            expect.push(fade_alpha(k * FADE_STEP_MS, FadeDirection::In));
        }
        expect.push(255);
        assert_eq!(*f.alphas[0].borrow(), expect);
        assert_eq!(
            *f.alphas[1].borrow(),
            expect,
            "every monitor fades in lockstep"
        );
        // The alpha path never blends pixels: one present per step (full
        // veil, growing circle) plus the settled endpoint.
        let p = f.presents[0].borrow();
        assert_eq!(p.len(), TRANSITION_PRESENTS);
        assert_eq!(
            p[0].pixels,
            settled_frame(scale_radius(10, spotlight_radius_scale(0))).pixels,
            "step 0: full veil, 60% circle"
        );
        assert_eq!(p[p.len() - 1].pixels, settled_frame(10).pixels);
    }

    #[test]
    fn unfreeze_fades_out_to_the_original_capture() {
        let mut f = freeze_fake(Point::new(16, 16));
        let before = f.presents[0].borrow().len();
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
        let p = f.presents[0].borrow();
        let fade = &p[before..];
        assert_eq!(fade.len(), TRANSITION_PRESENTS, "steps + exact endpoint");
        let original = make_buf(32, 32, coord_pattern);
        // Each step blends the original toward the frame composed with the
        // shrinking circle at the schedule's alpha.
        for k in 0..FADE_STEPS {
            let a = fade_alpha(k * FADE_STEP_MS, FadeDirection::Out);
            let target = step_frame(160, scale_radius(10, spotlight_radius_scale(a)));
            let mut expect = DibBuffer::new(32, 32);
            blend_frames(&original, &target, &mut expect, a);
            assert_eq!(fade[k as usize].pixels, expect.pixels, "fade-out step {k}");
        }
        assert_eq!(
            fade.last().unwrap().pixels,
            original.pixels,
            "the fade ends exactly on the original capture"
        );
    }

    #[test]
    fn unfreeze_fade_out_ends_fully_transparent_on_alpha_surfaces() {
        let mut f = freeze_fake_ex(Point::new(16, 16), true, manual_fade_clock());
        f.alphas[0].borrow_mut().clear();
        let before = f.presents[0].borrow().len();
        f.controller.unfreeze();
        let mut expect: Vec<u8> = (0..FADE_STEPS)
            .map(|k| fade_alpha(k * FADE_STEP_MS, FadeDirection::Out))
            .collect();
        expect.push(0); // exact transparent endpoint before teardown
        assert_eq!(*f.alphas[0].borrow(), expect);
        assert!(!f.controller.is_frozen());
        // One present per step (the shrinking circle is re-composed); no
        // endpoint present — the windows die right after.
        assert_eq!(
            f.presents[0].borrow().len() - before,
            FADE_STEPS as usize
        );
    }

    #[test]
    fn a_stalled_step_skips_ahead_instead_of_stalling() {
        // Sleep advances the clock by 50 ms regardless of the request: the
        // nominal 25 ms steps are missed, and the fade lands on the CURRENT
        // alpha each time instead of queueing the missed ones.
        let cell = Rc::new(Cell::new(Duration::ZERO));
        let clock = FadeClock::custom(
            {
                let cell = cell.clone();
                Rc::new(move || cell.get())
            },
            Rc::new(move |_d| cell.set(cell.get() + Duration::from_millis(50))),
        );
        let f = freeze_fake_ex(Point::new(16, 16), true, clock);
        assert_eq!(
            *f.alphas[0].borrow(),
            vec![
                0,
                fade_alpha(0, FadeDirection::In),
                fade_alpha(50, FadeDirection::In),
                fade_alpha(100, FadeDirection::In),
                fade_alpha(150, FadeDirection::In),
                255,
            ]
        );
    }

    #[test]
    fn esc_in_capture_exits_without_fading() {
        let mut f = freeze_fake(Point::new(16, 16));
        f.controller.set_mode(ModeKind::Snip, &f.services);
        let before = f.presents[0].borrow().len();
        // Esc in capture only exits capture: the exit repaint, no fade steps.
        f.controller.unfreeze();
        assert!(f.controller.is_frozen());
        assert_eq!(
            f.presents[0].borrow().len(),
            before + 1,
            "only the exit-capture repaint — no fade"
        );
        // The real unfreeze right after DOES fade (steps + endpoint).
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
        assert_eq!(
            f.presents[0].borrow().len(),
            before + 1 + TRANSITION_PRESENTS,
        );
    }

    #[test]
    fn rapid_toggles_serialize_and_every_transition_lands_exact() {
        let mut f = freeze_fake(Point::new(16, 16));
        // Unfreeze right after the freeze (the "Esc during fade-in" case:
        // serialized behind the atomic fade-in) — full fade-out, then gone.
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
        // Toggle straight back on (the "freeze during fade-out" case): a
        // FRESH session fading in from its own original capture.
        let before = f.presents[0].borrow().len();
        f.refreeze();
        assert!(f.controller.is_frozen());
        assert_eq!(f.controller.active_mode(), ModeKind::Spotlight);
        let p = f.presents[0].borrow();
        assert_eq!(
            p[before].pixels,
            make_buf(32, 32, coord_pattern).pixels,
            "the new session fades in from its original capture"
        );
        // And one more full cycle lands exactly on the original again.
        drop(p);
        f.controller.unfreeze();
        assert!(!f.controller.is_frozen());
        assert_eq!(
            last_present(&f.presents[0]).pixels,
            make_buf(32, 32, coord_pattern).pixels,
        );
    }

    #[test]
    fn freeze_while_frozen_does_not_reanimate() {
        let mut f = freeze_fake(Point::new(16, 16));
        let before = f.presents[0].borrow().len();
        f.refreeze(); // documented no-op: already frozen
        assert!(f.controller.is_frozen());
        assert_eq!(
            f.presents[0].borrow().len(),
            before,
            "no second fade-in while frozen"
        );
    }

    // ---- instant spotlight toggles ------------------------------------------

    #[test]
    fn spotlight_toggle_off_repaints_once_to_the_unveiled_endpoint() {
        let mut f = freeze_fake(Point::new(16, 16));
        let before = f.presents[0].borrow().len();
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
        assert!(f.controller.is_frozen(), "toggling off stays frozen");
        let p = f.presents[0].borrow();
        assert_eq!(p.len(), before + 1, "one immediate settled repaint");
        assert_eq!(
            p.last().unwrap().pixels,
            make_buf(32, 32, coord_pattern).pixels
        );
    }

    #[test]
    fn spotlight_toggle_on_repaints_once_to_the_settled_endpoint() {
        let mut f = freeze_fake(Point::new(16, 16));
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services); // off
        let before = f.presents[0].borrow().len();
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services); // on again
        assert_eq!(f.controller.active_mode(), ModeKind::Spotlight);
        let p = f.presents[0].borrow();
        assert_eq!(p.len(), before + 1, "one immediate settled repaint");
        assert_eq!(p.last().unwrap().pixels, settled_frame(10).pixels);
    }

    #[test]
    fn spotlight_toggle_off_with_zoom_repaints_once_and_keeps_the_veil() {
        let mut f = freeze_fake(Point::new(16, 16));
        // Zoom hold on via the wheel chord: both layers active at (16,16).
        f.controller.handle_overlay_event(
            0,
            OverlayEvent::MouseWheel {
                at: Point::new(16, 16),
                delta: 120,
                modifiers: crate::hotkeys::gesture::Modifiers::SHIFT,
            },
        );
        let before = f.presents[0].borrow().len();
        f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
        let p = f.presents[0].borrow();
        assert_eq!(p.len(), before + 1, "one immediate settled repaint");
        // The zoom layer keeps the veil at full strength after the spotlight
        // hole disappears.
        let original = make_buf(32, 32, coord_pattern);
        let zoomed = zoom_resample(
            &original,
            Rect::new(0, 0, 32, 32),
            1.25,
            Point::new(16, 16),
            ZoomFilter::Nearest,
        );
        let mut zoom_veil = zoomed.clone();
        darken(&mut zoom_veil, 160, Rgb::BLACK);
        assert_eq!(p.last().unwrap().pixels, zoom_veil.pixels);
    }

    #[test]
    fn mode_switches_and_capture_entry_are_instant_single_repaints() {
        let mut f = freeze_fake(Point::new(16, 16));
        let before = f.presents[0].borrow().len();
        // Capture entry: exactly one repaint per monitor (no transition).
        f.controller.set_mode(ModeKind::Snip, &f.services);
        assert_eq!(f.presents[0].borrow().len(), before + 1);
        // Esc back out of capture: one repaint.
        f.controller.unfreeze();
        assert!(f.controller.is_frozen());
        assert_eq!(f.presents[0].borrow().len(), before + 2);
        // Full mode switches: one repaint each.
        f.controller.set_mode(ModeKind::Zoom, &f.services);
        assert_eq!(f.presents[0].borrow().len(), before + 3);
        f.controller.set_mode(ModeKind::Spotlight, &f.services);
        assert_eq!(f.presents[0].borrow().len(), before + 4);
        // Zoom toggle: one repaint, like every in-session toggle.
        f.controller.toggle_mode(ModeKind::Zoom, &f.services);
        assert_eq!(f.presents[0].borrow().len(), before + 5);
    }

    #[test]
    fn no_presented_frame_ever_has_a_white_border_band() {
        // The whole key-driven journey on BOTH surface kinds; every recorded
        // frame must be flash-free.
        for alpha_surfaces in [false, true] {
            let mut f = freeze_fake_ex(Point::new(16, 16), alpha_surfaces, manual_fade_clock());
            f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
            f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
            f.controller.set_mode(ModeKind::Snip, &f.services);
            f.controller.unfreeze(); // exit capture
            f.controller.set_mode(ModeKind::Zoom, &f.services);
            f.controller.set_mode(ModeKind::Spotlight, &f.services);
            f.controller.unfreeze(); // fade out
            for (i, presents) in f.presents.iter().enumerate() {
                for (j, frame) in presents.borrow().iter().enumerate() {
                    assert!(
                        !has_white_border_band(frame),
                        "white flash band on monitor {i}, frame {j} (alpha={alpha_surfaces})"
                    );
                }
            }
        }
    }

    // ---- mode legend ------------------------------------------------------------

    #[test]
    fn legend_is_painted_while_frozen_and_fades_in_with_the_veil() {
        let f = freeze_fake_with(big_monitor(), Point::new(400, 100), false, manual_fade_clock());
        let p = f.presents[0].borrow();
        assert_eq!(p.len(), TRANSITION_PRESENTS);
        let original = make_buf(800, 160, coord_pattern);
        assert_eq!(p[0].pixels, original.pixels, "step 0: no pill yet (alpha 0)");
        // Every step: veil ramp + growing circle + the pill at the step's
        // alpha (it fades in WITH the veil, never pops).
        let legend = Legend::from_hotkeys(&AppSettings::default().hotkeys);
        for k in 1..FADE_STEPS {
            let a = fade_alpha(k * FADE_STEP_MS, FadeDirection::In);
            let mut expect = DibBuffer::new(800, 160);
            let state = RenderState {
                spotlight: Some((Point::new(400, 100), scale_radius(10, spotlight_radius_scale(a)))),
                ..RenderState::default()
            };
            compose_frame(
                &original,
                &mut expect,
                Rect::new(0, 0, 800, 160),
                &state,
                lerp_u8(0, 160, a),
                Rgb::BLACK,
            );
            legend.paint(&mut expect, &[true, false, false], a);
            assert_eq!(p[k as usize].pixels, expect.pixels, "fade step {k}");
        }
        // The settled frame carries the pill at full strength.
        let mut settled = DibBuffer::new(800, 160);
        let state = RenderState {
            spotlight: Some((Point::new(400, 100), 10)),
            ..RenderState::default()
        };
        compose_frame(
            &original,
            &mut settled,
            Rect::new(0, 0, 800, 160),
            &state,
            160,
            Rgb::BLACK,
        );
        legend.paint(&mut settled, &[true, false, false], 255);
        assert_eq!(p[p.len() - 1].pixels, settled.pixels);
    }

    #[test]
    fn legend_never_reaches_the_rebased_base_or_the_clipboard() {
        let mut f = freeze_fake_with(big_monitor(), Point::new(400, 100), false, manual_fade_clock());
        f.controller.set_mode(ModeKind::Snip, &f.services);
        // Drag a selection over the pill zone (near the top-center).
        for event in [
            OverlayEvent::LeftButtonDown {
                at: Point::new(80, 40),
            },
            OverlayEvent::MouseMove {
                at: Point::new(720, 100),
            },
            OverlayEvent::LeftButtonUp {
                at: Point::new(720, 100),
            },
        ] {
            f.controller.handle_overlay_event(0, event);
        }
        f.controller
            .snip_copy_and_close(&f.services)
            .expect("copy");
        let copied = f.copied.borrow();
        let crop = copied.last().expect("one clipboard write");
        // The crop comes from the re-frozen base, composed WITHOUT the
        // legend — recompute it exactly.
        let original = make_buf(800, 160, coord_pattern);
        let mut base = DibBuffer::new(800, 160);
        let state = RenderState {
            spotlight: Some((Point::new(400, 100), 10)),
            ..RenderState::default()
        };
        compose_frame(
            &original,
            &mut base,
            Rect::new(0, 0, 800, 160),
            &state,
            160,
            Rgb::BLACK,
        );
        let expect = crop_normalized(&base, Point::new(80, 40), Point::new(720, 100)).unwrap();
        assert_eq!(crop.pixels, expect.pixels, "no legend pixels in the copy");
        // Discriminator: had the legend been baked into the base, the crop
        // would contain pill pixels.
        let legend = Legend::from_hotkeys(&AppSettings::default().hotkeys);
        legend.paint(&mut base, &[true, false, false], 255);
        let baked = crop_normalized(&base, Point::new(80, 40), Point::new(720, 100)).unwrap();
        assert_ne!(
            crop.pixels, baked.pixels,
            "the pill zone proves the legend was excluded from the base"
        );
    }
}
