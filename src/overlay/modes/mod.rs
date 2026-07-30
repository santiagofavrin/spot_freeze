//! Overlay modes (Spotlight on/off, Capture, and the zoom-hold LAYER) plus
//! the [`ModeStack`] that combines them. Every layer is a pure state machine —
//! pixel compositing lives in [`crate::overlay::composite::compose_frame`];
//! a layer only tracks state (cursor, radius, zoom factor, selection) and
//! reports dirty regions. No `windows` types anywhere in this module tree.
//!
//! # Mode model (product spec)
//!
//! - **Spotlight toggle (`S`) → [`ModeStack::toggle_mode`]**: the layer is
//!   added when inactive, REMOVED when active. Toggling the last layer off
//!   leaves the screen frozen but UNVEILED ([`ModeStack::any_active`] is
//!   false — the controller dims nothing). Spotlight is the default mode:
//!   freeze starts with the layer on.
//! - **Capture (`C`) → [`ModeStack::set_mode`]`(ModeKind::Snip)` →
//!   [`ModeStack::enter_capture`]**: the controller RE-BASES the freeze on the
//!   currently composited view (the spotlight/zoom effects active at that
//!   moment baked in); the stack STASHES the spotlight/zoom layers and
//!   activates a fresh snip layer for the drag-selection. Esc →
//!   [`ModeStack::exit_capture`]: the stashed layers come back exactly as
//!   they were (spotlight on/off state, zoom factor/focus) and the snip layer
//!   is dropped; the controller restores the pre-capture base.
//! - **Zoom hold (`F` toggle, or the zoom-modifier wheel chord from anywhere)
//!   → an effect LAYER, not a mode**: re-activation restores the LAST-USED
//!   zoom factor ([`ModeStack::last_zoom`], synced whenever the layer is
//!   toggled off); `0` ([`ModeStack::reset_view`]) returns the layer to 1.0.
//! - **Wheel routing matrix** ([`ModeStack::on_wheel`]):
//!   * spotlight is offered EVERY wheel event while active; the layer itself
//!     enforces its radius-modifier gate (default Ctrl) and keeps the
//!     sub-notch accumulator;
//!   * the configured zoom-modifier chord (default Shift+wheel) zooms from ANY
//!     state — IMPLICITLY ACTIVATING the zoom-hold layer at the last-used
//!     factor (additive, no border flash) when it isn't active yet;
//!   * the PLAIN wheel (no modifiers) zooms whenever the zoom layer is active;
//!   * both may respond to the same event (their effects merge).
//! - **Mouse move** feeds every active cursor-tracking layer (spotlight hole
//!   follows, zoom focus recenters, an in-progress snip drag extends).
//! - **Left drag** feeds the snip layer when active.
//! - **Rendering**: the controller asks [`ModeStack::render_state`] for the
//!   per-monitor [`crate::overlay::composite::RenderState`] and hands it to
//!   `compose_frame` — layers never touch pixels themselves.

use crate::geometry::{Point, Rect};
use crate::hotkeys::gesture::Modifiers;
use crate::overlay::composite::RenderState;

pub mod snip;
pub mod spotlight;
pub mod zoom;

pub use snip::SnipMode;
pub use spotlight::SpotlightMode;
pub use zoom::ZoomMode;

/// Which overlay mode (layer) is meant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ModeKind {
    Spotlight,
    Zoom,
    Snip,
}

/// What the controller must do after the mode stack handled an event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeEffect {
    /// `(monitor_index, dirty_region)` pairs to repaint. `dirty_region` is in
    /// monitor-local physical pixels; `None` = repaint the whole monitor.
    pub repaint: Vec<(usize, Option<Rect>)>,
    /// Reserved: a mode asks the controller to unfreeze (Esc and the copy
    /// hotkey are normally handled globally by the app/controller).
    pub exit: bool,
}

impl ModeEffect {
    /// No repaint, no exit.
    pub fn none() -> Self {
        Self::default()
    }

    /// Repaint one monitor (`None` dirty = full monitor), no exit.
    pub fn repaint(monitor: usize, dirty: Option<Rect>) -> Self {
        Self {
            repaint: vec![(monitor, dirty)],
            exit: false,
        }
    }

    /// Merge `other` into `self`: repaints append (in order), `exit` is sticky.
    /// Used to combine the effects of several active layers answering the
    /// same event.
    pub fn absorb(&mut self, other: ModeEffect) {
        self.repaint.extend(other.repaint);
        self.exit |= other.exit;
    }
}

/// A snip drag: two endpoints in MONITOR-LOCAL physical pixels on `monitor`,
/// in ANY drag direction — normalization happens in
/// [`crate::overlay::composite::crop_normalized`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SnipSelection {
    pub monitor: usize,
    pub a: Point,
    pub b: Point,
}

/// Construction parameters for a [`ModeStack`], snapshotted from settings at
/// freeze time (live settings edits apply on the NEXT freeze). The controller
/// sanitizes the zoom triple before filling this in.
#[derive(Clone, Copy, Debug)]
pub struct ModeParams {
    /// Spotlight circle radius at activation (settings: `spotlight.default_radius`).
    pub spotlight_radius: u32,
    /// Modifier held while scrolling to resize the spotlight circle
    /// (settings: `hotkeys.spotlight_radius_modifier`, default Ctrl).
    pub radius_modifier: Modifiers,
    /// Zoom wheel step factor (> 1.0; settings: `zoom.step_factor`).
    pub zoom_step: f32,
    /// Minimum zoom (>= 1.0; settings: `zoom.min`).
    pub zoom_min: f32,
    /// Maximum zoom (> min; settings: `zoom.max`).
    pub zoom_max: f32,
    /// Modifier held while scrolling to zoom from ANY mode combination
    /// (settings: `hotkeys.zoom_modifier`, default Shift).
    pub zoom_modifier: Modifiers,
}

/// The mode state of one freeze session: the spotlight layer, the zoom-hold
/// layer, the snip (capture) layer, the zoom factor the hold layer
/// re-activates with, and — while capture mode is active — the stashed
/// spotlight/zoom layers it was entered from.
///
/// Fresh layers are built from [`ModeParams`] on activation, so "reset ALL
/// mode state" is simply "drop every layer and rebuild the requested one".
pub struct ModeStack {
    params: ModeParams,
    spotlight: Option<SpotlightMode>,
    zoom: Option<ZoomMode>,
    snip: Option<SnipMode>,
    /// Factor the zoom-hold layer re-activates with; synced from the layer
    /// every time it is toggled off (the "last-used zoom level").
    last_zoom: f32,
    /// Spotlight/zoom layers stashed while capture mode re-bases the freeze;
    /// `None` outside capture mode.
    saved: Option<SavedLayers>,
}

/// Layers set aside by [`ModeStack::enter_capture`] and restored untouched by
/// [`ModeStack::exit_capture`].
struct SavedLayers {
    spotlight: Option<SpotlightMode>,
    zoom: Option<ZoomMode>,
}

impl ModeStack {
    /// Freeze-time initial state: Spotlight is the only active layer (product
    /// spec) and the zoom hold starts at 1.0.
    pub fn new(params: ModeParams) -> Self {
        Self {
            spotlight: Some(SpotlightMode::new(
                params.spotlight_radius,
                params.radius_modifier,
            )),
            zoom: None,
            snip: None,
            last_zoom: 1.0,
            saved: None,
            params,
        }
    }

    /// `true` while `kind`'s layer is active.
    pub fn is_active(&self, kind: ModeKind) -> bool {
        match kind {
            ModeKind::Spotlight => self.spotlight.is_some(),
            ModeKind::Zoom => self.zoom.is_some(),
            ModeKind::Snip => self.snip.is_some(),
        }
    }

    /// `true` while ANY layer is active. When false the screen is still
    /// frozen but the overlay is unveiled (the controller dims nothing).
    pub fn any_active(&self) -> bool {
        self.spotlight.is_some() || self.zoom.is_some() || self.snip.is_some()
    }

    /// Read access to the layers (state inspection, tests, copy planning).
    pub fn spotlight(&self) -> Option<&SpotlightMode> {
        self.spotlight.as_ref()
    }

    pub fn zoom(&self) -> Option<&ZoomMode> {
        self.zoom.as_ref()
    }

    pub fn snip(&self) -> Option<&SnipMode> {
        self.snip.as_ref()
    }

    /// PLAIN mode key. Capture (`ModeKind::Snip`) ENTERS capture mode (see
    /// [`ModeStack::enter_capture`]); every other kind is a FULL SWITCH —
    /// reset ALL mode state (zoom hold back to 1.0, snip selection cleared,
    /// spotlight radius back to default, any capture stash dropped) and make
    /// `kind` the only active layer.
    pub fn set_mode(&mut self, kind: ModeKind) {
        if kind == ModeKind::Snip {
            self.enter_capture();
            return;
        }
        self.spotlight = None;
        self.zoom = None;
        self.snip = None;
        self.last_zoom = 1.0;
        self.saved = None;
        self.activate(kind);
    }

    /// ADD `kind`'s layer WITHOUT touching the existing ones (the zoom-hold
    /// layer comes back at the last-used factor). No-op when the layer is
    /// already active.
    pub fn add_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            return;
        }
        self.activate(kind);
    }

    /// TOGGLE key (spotlight's `S`, zoom hold's `F`): remove `kind`'s layer
    /// when active, add it when not. Removing the zoom-hold layer banks its
    /// factor as the last-used level; re-activating restores it. Toggling the
    /// last spotlight layer off leaves the screen frozen but unveiled;
    /// toggling it back on re-activates with fresh state (radius back to
    /// default).
    pub fn toggle_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            match kind {
                ModeKind::Spotlight => self.spotlight = None,
                ModeKind::Zoom => {
                    if let Some(zoom) = self.zoom.take() {
                        self.last_zoom = zoom.zoom();
                    }
                }
                ModeKind::Snip => self.snip = None,
            }
            return;
        }
        self.activate(kind);
    }

    /// Activate `kind`'s layer: fresh state for spotlight/snip, the last-used
    /// factor for the zoom hold.
    fn activate(&mut self, kind: ModeKind) {
        match kind {
            ModeKind::Spotlight => {
                self.spotlight = Some(SpotlightMode::new(
                    self.params.spotlight_radius,
                    self.params.radius_modifier,
                ));
            }
            ModeKind::Zoom => {
                self.zoom = Some(ZoomMode::with_zoom(
                    self.last_zoom,
                    self.params.zoom_step,
                    self.params.zoom_min,
                    self.params.zoom_max,
                ));
            }
            ModeKind::Snip => {
                self.snip = Some(SnipMode::new());
            }
        }
    }

    /// Enter capture mode: stash the spotlight/zoom layers (the controller
    /// bakes them into the re-frozen base) and activate a FRESH snip layer —
    /// any in-progress selection is cleared. Re-entering while already in
    /// capture only resets the snip layer; the stash is kept.
    pub fn enter_capture(&mut self) {
        if self.saved.is_none() {
            self.saved = Some(SavedLayers {
                spotlight: self.spotlight.take(),
                zoom: self.zoom.take(),
            });
        }
        self.snip = Some(SnipMode::new());
    }

    /// Esc from capture mode: restore the stashed spotlight/zoom layers
    /// exactly as they were (spotlight on/off state, zoom factor/focus) and
    /// drop the snip layer (the selection goes with it).
    pub fn exit_capture(&mut self) {
        if let Some(saved) = self.saved.take() {
            self.spotlight = saved.spotlight;
            self.zoom = saved.zoom;
        }
        self.snip = None;
    }

    /// `true` while capture mode is active (the freeze is re-based and the
    /// pre-capture layers are stashed).
    pub fn in_capture(&self) -> bool {
        self.saved.is_some()
    }

    /// Seed the live cursor into every active cursor-tracking layer after
    /// activation. The controller full-repaints right after, so the layers'
    /// repaint effects are discarded here.
    pub fn seed_cursor(&mut self, monitor: usize, at: Point) {
        if let Some(spot) = self.spotlight.as_mut() {
            let _ = spot.on_mouse_move(monitor, at);
        }
        if let Some(zoom) = self.zoom.as_mut() {
            let _ = zoom.on_mouse_move(monitor, at);
        }
        if let Some(snip) = self.snip.as_mut() {
            let _ = snip.on_mouse_move(monitor, at);
        }
    }

    /// Mouse move feeds EVERY active cursor-tracking layer; effects merge.
    pub fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        let mut effect = ModeEffect::none();
        if let Some(spot) = self.spotlight.as_mut() {
            effect.absorb(spot.on_mouse_move(monitor, at));
        }
        if let Some(zoom) = self.zoom.as_mut() {
            effect.absorb(zoom.on_mouse_move(monitor, at));
        }
        if let Some(snip) = self.snip.as_mut() {
            effect.absorb(snip.on_mouse_move(monitor, at));
        }
        effect
    }

    /// Wheel routing matrix (see module docs):
    ///
    /// - spotlight is offered every wheel event while active — the layer
    ///   enforces the radius-modifier gate itself (and only banks sub-notch
    ///   deltas while the modifier is held);
    /// - the configured zoom-modifier chord (default Shift+wheel) reaches the
    ///   zoom-hold layer from ANY state — IMPLICITLY ACTIVATING it at the
    ///   last-used factor when it isn't active yet;
    /// - the PLAIN wheel (no modifiers) reaches the zoom layer whenever it is
    ///   active.
    ///
    /// Both layers may answer the same event (e.g. Ctrl+Shift with default
    /// bindings); their repaint effects merge.
    pub fn on_wheel(
        &mut self,
        monitor: usize,
        at: Point,
        delta: i32,
        modifiers: Modifiers,
    ) -> ModeEffect {
        let mut effect = ModeEffect::none();
        if let Some(spot) = self.spotlight.as_mut() {
            effect.absorb(spot.on_wheel(monitor, at, delta, modifiers));
        }
        let zoom_chord = modifiers.contains(self.params.zoom_modifier);
        // Implicit activation: the zoom-modifier chord adds the zoom-hold
        // layer when it isn't active yet — product spec: the chord zooms
        // straight out of the pristine spotlight-only state. This is ADDITIVE
        // (existing layers untouched) and deliberately does NOT flash the
        // border: the flash lives in the controller's key-driven toggle path,
        // and flashing on every scroll would be wrong. No cursor seeding
        // needed here — ZoomMode::on_wheel makes the wheel position the new
        // focus itself.
        if zoom_chord && self.zoom.is_none() {
            self.add_mode(ModeKind::Zoom);
        }
        let plain_zoom = modifiers.is_empty() && self.zoom.is_some();
        if let Some(zoom) = self.zoom.as_mut()
            && (zoom_chord || plain_zoom)
        {
            effect.absorb(zoom.on_wheel(monitor, at, delta));
        }
        effect
    }

    /// Left button down feeds the snip layer when active (drag start).
    pub fn on_left_button_down(&mut self, monitor: usize, at: Point) -> ModeEffect {
        match self.snip.as_mut() {
            Some(snip) => snip.on_left_button_down(monitor, at),
            None => ModeEffect::none(),
        }
    }

    /// Left button up feeds the snip layer when active (drag finish).
    pub fn on_left_button_up(&mut self, monitor: usize, at: Point) -> ModeEffect {
        match self.snip.as_mut() {
            Some(snip) => snip.on_left_button_up(monitor, at),
            None => ModeEffect::none(),
        }
    }

    /// Reset-view hotkey (default binding `0`): zoom back to 1.0 when the zoom
    /// layer is active; a no-op effect otherwise.
    pub fn reset_view(&mut self) -> ModeEffect {
        match self.zoom.as_mut() {
            Some(zoom) => zoom.reset_view(),
            None => ModeEffect::none(),
        }
    }

    /// The current snip selection, when the snip layer is active and has one.
    pub fn snip_selection(&self) -> Option<SnipSelection> {
        self.snip.as_ref().and_then(SnipMode::snip_selection)
    }

    /// `(zoom_factor, focus)` when the zoom layer is active ON `monitor` —
    /// the composed BASE the snip copy crops from (WYSIWYG with the presented
    /// frame); `None` when zoom is inactive or focused on another monitor.
    pub fn zoom_on(&self, monitor: usize) -> Option<(f32, Point)> {
        self.zoom
            .as_ref()
            .filter(|z| z.cursor_monitor() == monitor)
            .map(|z| (z.zoom(), z.cursor()))
    }

    /// The per-monitor [`RenderState`] for `compose_frame`: each active layer
    /// contributes only on the monitor its state lives on (spotlight/zoom
    /// follow the cursor monitor, snip its drag monitor); `capture` flags
    /// capture mode for the indicator frame border.
    pub fn render_state(&self, monitor: usize) -> RenderState {
        let spotlight = self
            .spotlight
            .as_ref()
            .filter(|s| s.cursor_monitor() == monitor)
            .map(|s| (s.cursor(), s.radius()));
        let zoom = self
            .zoom
            .as_ref()
            .filter(|z| z.cursor_monitor() == monitor)
            .map(|z| (z.zoom(), z.cursor()));
        let snip = self
            .snip
            .as_ref()
            .and_then(SnipMode::snip_selection)
            .filter(|sel| sel.monitor == monitor)
            .map(|sel| (sel.a, sel.b));
        RenderState {
            zoom,
            spotlight,
            snip,
            capture: self.in_capture(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default-like params: radius 100, Ctrl radius modifier, Shift zoom
    /// modifier, zoom step 1.25 in [1.0, 100.0].
    fn params() -> ModeParams {
        ModeParams {
            spotlight_radius: 100,
            radius_modifier: Modifiers::CTRL,
            zoom_step: 1.25,
            zoom_min: 1.0,
            zoom_max: 100.0,
            zoom_modifier: Modifiers::SHIFT,
        }
    }

    fn pt(x: i32, y: i32) -> Point {
        Point::new(x, y)
    }

    fn assert_zoom_near(stack: &ModeStack, expected: f32) {
        let z = stack.zoom().expect("zoom layer active").zoom();
        assert!(
            (z - expected).abs() < 1e-6,
            "zoom {z} vs expected {expected}"
        );
    }

    // ---- ModeEffect ------------------------------------------------------

    #[test]
    fn mode_effect_none_is_empty() {
        let e = ModeEffect::none();
        assert!(e.repaint.is_empty());
        assert!(!e.exit);
        assert_eq!(e, ModeEffect::default());
    }

    #[test]
    fn mode_effect_repaint_single_monitor() {
        let dirty = Rect::new(3, -4, 10, 20);
        let e = ModeEffect::repaint(2, Some(dirty));
        assert_eq!(e.repaint, vec![(2, Some(dirty))]);
        assert!(!e.exit);
    }

    #[test]
    fn mode_effect_absorb_merges_in_order_and_exit_is_sticky() {
        let mut a = ModeEffect::repaint(0, Some(Rect::new(0, 0, 5, 5)));
        a.absorb(ModeEffect::repaint(1, None));
        assert_eq!(
            a.repaint,
            vec![(0, Some(Rect::new(0, 0, 5, 5))), (1, None)],
            "repaints append in order"
        );
        assert!(!a.exit);
        a.absorb(ModeEffect {
            repaint: vec![],
            exit: true,
        });
        assert!(a.exit, "exit is sticky");
        a.absorb(ModeEffect::none());
        assert!(a.exit, "never cleared by a later empty effect");
    }

    // ---- construction / activation ----------------------------------------

    #[test]
    fn new_starts_spotlight_only_not_in_capture() {
        let stack = ModeStack::new(params());
        assert!(stack.is_active(ModeKind::Spotlight));
        assert!(!stack.is_active(ModeKind::Zoom));
        assert!(!stack.is_active(ModeKind::Snip));
        assert!(stack.spotlight().is_some());
        assert!(stack.zoom().is_none());
        assert!(stack.snip().is_none());
        assert!(!stack.in_capture());
        assert_eq!(stack.spotlight().unwrap().radius(), 100);
    }

    #[test]
    fn set_mode_resets_all_layers_and_makes_kind_the_only_active_one() {
        let mut stack = ModeStack::new(params());
        // Dirty every layer: radius changed, zoom engaged, selection drawn.
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110
        stack.add_mode(ModeKind::Zoom);
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25
        stack.add_mode(ModeKind::Snip);
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(8, 8));
        stack.on_left_button_up(0, pt(8, 8));
        assert!(stack.snip_selection().is_some());
        assert_zoom_near(&stack, 1.25);
        assert_eq!(stack.spotlight().unwrap().radius(), 110);

        stack.set_mode(ModeKind::Zoom);
        assert!(!stack.is_active(ModeKind::Spotlight), "spotlight dropped");
        assert!(!stack.is_active(ModeKind::Snip), "snip dropped");
        assert!(stack.is_active(ModeKind::Zoom));
        assert_eq!(
            stack.zoom().unwrap().zoom(),
            1.0,
            "zoom layer rebuilt fresh at 1.0"
        );
        assert_eq!(stack.snip_selection(), None, "selection cleared");

        // Switching BACK to spotlight yields a fresh default radius again.
        stack.set_mode(ModeKind::Spotlight);
        assert_eq!(stack.spotlight().unwrap().radius(), 100);
        assert!(stack.zoom().is_none());
    }

    #[test]
    fn set_mode_same_kind_still_resets_state() {
        // Spec: a plain press is a FULL SWITCH — no same-kind exemption.
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(0, 0), 120, Modifiers::CTRL); // radius 110
        stack.set_mode(ModeKind::Spotlight);
        assert_eq!(stack.spotlight().unwrap().radius(), 100, "radius reset");
    }

    #[test]
    fn add_mode_preserves_existing_layers() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110

        stack.add_mode(ModeKind::Zoom);
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            110,
            "additive activation does NOT reset the spotlight"
        );
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0, "zoom hold starts at 1.0");

        stack.add_mode(ModeKind::Snip);
        assert!(stack.is_active(ModeKind::Spotlight));
        assert!(stack.is_active(ModeKind::Zoom));
        assert!(stack.is_active(ModeKind::Snip));
    }

    #[test]
    fn add_mode_already_active_is_a_noop() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25
        stack.add_mode(ModeKind::Zoom); // no-op: layer already active
        assert_zoom_near(&stack, 1.25);
        // Same for the freeze-default spotlight layer.
        stack.add_mode(ModeKind::Spotlight);
        assert_eq!(stack.spotlight().unwrap().radius(), 100);
    }

    // ---- toggle_mode ---------------------------------------------------------

    #[test]
    fn toggle_off_removes_the_layer_and_leaves_nothing_active() {
        let mut stack = ModeStack::new(params());
        assert!(stack.any_active());
        stack.toggle_mode(ModeKind::Spotlight);
        assert!(!stack.is_active(ModeKind::Spotlight));
        assert!(!stack.any_active(), "no layers left: frozen but unveiled");
    }

    #[test]
    fn toggle_on_reactivates_spotlight_with_fresh_state() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110
        stack.toggle_mode(ModeKind::Spotlight);
        stack.toggle_mode(ModeKind::Spotlight);
        assert!(stack.is_active(ModeKind::Spotlight));
        assert_eq!(stack.spotlight().unwrap().radius(), 100, "fresh default state");
    }

    #[test]
    fn zoom_hold_toggle_banks_and_restores_the_last_used_factor() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // hold on at 1.0
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.5625

        stack.toggle_mode(ModeKind::Zoom); // hold off: factor banked
        assert!(!stack.is_active(ModeKind::Zoom));
        assert!(stack.is_active(ModeKind::Spotlight), "spotlight untouched");

        stack.toggle_mode(ModeKind::Zoom); // hold on: last-used factor back
        assert_zoom_near(&stack, 1.5625);
        // Toggling off at 1.0 (after `0`) banks 1.0 again.
        stack.reset_view();
        stack.toggle_mode(ModeKind::Zoom);
        stack.toggle_mode(ModeKind::Zoom);
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0);
    }

    #[test]
    fn implicit_wheel_activation_applies_the_last_used_factor() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 240, Modifiers::SHIFT); // activates at 1.0, zooms 1.5625
        stack.toggle_mode(ModeKind::Zoom); // bank 1.5625
        assert!(!stack.is_active(ModeKind::Zoom));

        // The chord implicitly re-activates at the banked factor, then wheels.
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(stack.is_active(ModeKind::Zoom));
        assert_zoom_near(&stack, 1.953125); // 1.5625 * 1.25
    }

    #[test]
    fn plain_wheel_is_inert_with_no_layers_active() {
        let mut stack = ModeStack::new(params());
        stack.toggle_mode(ModeKind::Spotlight);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none(), "no zoom layer active");
        assert!(!stack.is_active(ModeKind::Zoom));
    }

    // ---- seed_cursor -------------------------------------------------------

    #[test]
    fn seed_cursor_feeds_every_active_layer() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        stack.seed_cursor(1, pt(30, 40));
        let spot = stack.spotlight().unwrap();
        assert_eq!((spot.cursor_monitor(), spot.cursor()), (1, pt(30, 40)));
        let zoom = stack.zoom().unwrap();
        assert_eq!((zoom.cursor_monitor(), zoom.cursor()), (1, pt(30, 40)));
    }

    // ---- wheel routing matrix ----------------------------------------------
    // (active layers) x (held modifiers) -> which layer responds.

    #[test]
    fn wheel_spotlight_only_ctrl_resizes_plain_wheel_stays_inert() {
        let mut stack = ModeStack::new(params());
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(stack.spotlight().unwrap().radius(), 110);
        assert!(!e.repaint.is_empty(), "resize reports a repaint");

        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none(), "plain wheel does not resize");
        assert_eq!(stack.spotlight().unwrap().radius(), 110);
        assert!(
            !stack.is_active(ModeKind::Zoom),
            "plain wheel must NOT activate the zoom layer"
        );
    }

    #[test]
    fn wheel_spotlight_only_shift_wheel_implicitly_activates_zoom() {
        // Pristine spotlight-only + the zoom-modifier chord (default
        // Shift+wheel) ADDITIVELY activates the zoom-hold layer (at the
        // last-used factor, 1.0 here) and zooms in the same event — no `F`
        // press needed first. (No border flash is involved at this level:
        // flashing lives in the controller's key-driven activation path.)
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110 first
        assert!(!stack.is_active(ModeKind::Zoom), "pristine: no zoom layer");

        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(
            stack.is_active(ModeKind::Zoom),
            "zoom layer implicitly activated"
        );
        assert_zoom_near(&stack, 1.25);
        assert!(!e.repaint.is_empty(), "the implicit zoom repaints");
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            110,
            "additive: spotlight preserved (Shift is not the radius modifier)"
        );
        // The wheel event's position becomes the fresh layer's focus.
        let zoom = stack.zoom().unwrap();
        assert_eq!((zoom.cursor_monitor(), zoom.cursor()), (0, pt(10, 10)));
    }

    #[test]
    fn wheel_implicit_zoom_activation_then_plain_wheel_zooms() {
        // Follow-up of the implicit-activation rule: once the zoom layer is
        // active, the plain wheel zooms.
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // activates + 1.25
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert!(!e.repaint.is_empty(), "plain wheel now reaches zoom");
        assert_zoom_near(&stack, 1.5625);
        // ...and Ctrl+wheel still drives the spotlight radius only.
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(stack.spotlight().unwrap().radius(), 110);
        assert_zoom_near(&stack, 1.5625);
        assert!(!e.repaint.is_empty());
    }

    #[test]
    fn wheel_zoom_active_plain_wheel_and_chord_both_zoom() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // zoom hold active
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert!(
            !e.repaint.is_empty(),
            "plain wheel reaches the active zoom layer"
        );
        assert_zoom_near(&stack, 1.25);
        // ...and the zoom chord reaches it too, from the same state.
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(!e.repaint.is_empty(), "zoom chord always reaches zoom");
        assert_zoom_near(&stack, 1.5625);
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            100,
            "spotlight untouched by both wheels"
        );
    }

    #[test]
    fn wheel_ctrl_with_zoom_active_only_resizes_spotlight() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // both layers active
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(stack.spotlight().unwrap().radius(), 110);
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0, "Ctrl is not the zoom modifier");
        assert!(!e.repaint.is_empty());
    }

    #[test]
    fn wheel_ctrl_shift_reaches_both_layers() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(stack.spotlight().unwrap().radius(), 110, "Ctrl gate passed");
        assert_zoom_near(&stack, 1.25);
        // Spotlight's circle repaint (dirty) then zoom's full repaint, merged.
        assert_eq!(e.repaint.len(), 2, "both layers answered the same event");
        assert!(e.repaint[0].1.is_some(), "spotlight dirty region first");
        assert_eq!(e.repaint[1], (0, None), "zoom full repaint second");
    }

    #[test]
    fn wheel_zoom_only_switch_ctrl_does_nothing() {
        let mut stack = ModeStack::new(params());
        stack.set_mode(ModeKind::Zoom); // zoom only
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(e, ModeEffect::none(), "no spotlight layer to resize");
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(!e.repaint.is_empty(), "zoom modifier reaches the zoom layer");
        assert_zoom_near(&stack, 1.25);
    }

    #[test]
    fn wheel_sub_notch_accumulators_survive_routing() {
        // D2 regression at stack level: four +60 Ctrl events resize by +20 in
        // total (spotlight accumulator), four +60 Shift events zoom by step^2
        // (zoom fractional exponent).
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        for _ in 0..4 {
            stack.on_wheel(0, pt(10, 10), 60, Modifiers::CTRL);
        }
        assert_eq!(stack.spotlight().unwrap().radius(), 120);
        for _ in 0..4 {
            stack.on_wheel(0, pt(10, 10), 60, Modifiers::SHIFT);
        }
        assert_zoom_near(&stack, 1.5625);
        // Unheld deltas must NOT bank into the spotlight accumulator.
        stack.on_wheel(0, pt(10, 10), 60, Modifiers::NONE);
        stack.on_wheel(0, pt(10, 10), 60, Modifiers::SHIFT); // (zooms, not banking radius)
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(stack.spotlight().unwrap().radius(), 130);
    }

    // ---- mouse move / drag routing ------------------------------------------

    #[test]
    fn mouse_move_feeds_all_active_layers() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        let e = stack.on_mouse_move(0, pt(50, 60));
        // Spotlight circle repaint (dirty) + zoom full repaint, merged.
        assert_eq!(e.repaint.len(), 2);
        assert_eq!(stack.spotlight().unwrap().cursor(), pt(50, 60));
        assert_eq!(stack.zoom().unwrap().cursor(), pt(50, 60));
    }

    #[test]
    fn left_drag_feeds_snip_only_when_snip_active() {
        let mut stack = ModeStack::new(params());
        // No snip layer: buttons are inert.
        assert_eq!(stack.on_left_button_down(0, pt(2, 2)), ModeEffect::none());
        assert_eq!(stack.on_left_button_up(0, pt(9, 9)), ModeEffect::none());

        stack.add_mode(ModeKind::Snip);
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(9, 9));
        stack.on_left_button_up(0, pt(9, 9));
        assert_eq!(
            stack.snip_selection(),
            Some(SnipSelection {
                monitor: 0,
                a: pt(2, 2),
                b: pt(9, 9),
            })
        );
    }

    // ---- reset_view ---------------------------------------------------------

    #[test]
    fn reset_view_resets_zoom_only_when_zoom_active() {
        let mut stack = ModeStack::new(params());
        assert_eq!(stack.reset_view(), ModeEffect::none(), "no zoom layer");

        stack.add_mode(ModeKind::Zoom);
        stack.seed_cursor(1, pt(5, 5));
        stack.on_wheel(1, pt(5, 5), 120, Modifiers::SHIFT);
        assert!(stack.zoom().unwrap().zoom() > 1.0);
        let e = stack.reset_view();
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0);
        assert_eq!(e.repaint, vec![(1, None)], "repaints the cursor monitor");
        assert!(!e.exit);
        // Spotlight state is not touched by reset_view.
        assert!(stack.spotlight().is_some());
    }

    // ---- capture mode ---------------------------------------------------------

    #[test]
    fn set_mode_snip_enters_capture_stashing_spotlight_and_zoom() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom hold on, 1.25

        stack.set_mode(ModeKind::Snip);
        assert!(stack.in_capture());
        assert!(!stack.is_active(ModeKind::Spotlight), "stashed, not active");
        assert!(!stack.is_active(ModeKind::Zoom), "stashed, not active");
        assert!(stack.is_active(ModeKind::Snip), "fresh snip layer active");
        let rs = stack.render_state(0);
        assert!(rs.capture, "capture indicator flag set");
        assert_eq!(rs.zoom, None);
        assert_eq!(rs.spotlight, None);
    }

    #[test]
    fn exit_capture_restores_the_stashed_layers_exactly() {
        let mut stack = ModeStack::new(params());
        stack.seed_cursor(0, pt(30, 40));
        stack.on_wheel(0, pt(30, 40), 120, Modifiers::CTRL); // radius 110
        stack.on_wheel(0, pt(30, 40), 120, Modifiers::SHIFT); // zoom 1.25 at (30,40)
        stack.set_mode(ModeKind::Snip);
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(8, 8));
        stack.on_left_button_up(0, pt(8, 8));
        assert!(stack.snip_selection().is_some());

        stack.exit_capture();
        assert!(!stack.in_capture());
        assert!(!stack.is_active(ModeKind::Snip));
        assert_eq!(
            stack.snip_selection(),
            None,
            "selection dropped with the snip layer"
        );
        let spot = stack.spotlight().expect("spotlight restored");
        assert_eq!(spot.radius(), 110, "spotlight state survives the round-trip");
        assert_eq!((spot.cursor_monitor(), spot.cursor()), (0, pt(30, 40)));
        let zoom = stack.zoom().expect("zoom restored");
        assert!((zoom.zoom() - 1.25).abs() < 1e-6, "zoom factor restored");
        assert_eq!(
            (zoom.cursor_monitor(), zoom.cursor()),
            (0, pt(30, 40)),
            "zoom focus restored"
        );
        assert!(!stack.render_state(0).capture);
    }

    #[test]
    fn exit_capture_restores_spotlight_off_state() {
        let mut stack = ModeStack::new(params());
        stack.toggle_mode(ModeKind::Spotlight); // spotlight OFF
        stack.set_mode(ModeKind::Snip);
        stack.exit_capture();
        assert!(!stack.is_active(ModeKind::Spotlight), "stays off");
        assert!(!stack.any_active(), "back to frozen-but-unveiled");
    }

    #[test]
    fn reentering_capture_clears_the_selection_but_keeps_the_stash() {
        let mut stack = ModeStack::new(params());
        stack.set_mode(ModeKind::Snip);
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(9, 9));
        stack.on_left_button_up(0, pt(9, 9));
        assert!(stack.snip_selection().is_some());

        stack.set_mode(ModeKind::Snip); // plain press again: reset, no re-stash
        assert!(stack.in_capture());
        assert_eq!(stack.snip_selection(), None, "selection cleared");
        stack.exit_capture();
        assert!(
            stack.is_active(ModeKind::Spotlight),
            "the original stash is restored, not a double-stash"
        );
    }

    // ---- render_state / zoom_on ----------------------------------------------

    #[test]
    fn render_state_spotlight_only_on_cursor_monitor() {
        let mut stack = ModeStack::new(params());
        stack.seed_cursor(0, pt(30, 40));
        let rs = stack.render_state(0);
        assert_eq!(rs.spotlight, Some((pt(30, 40), 100)));
        assert_eq!(rs.zoom, None);
        assert_eq!(rs.snip, None);
        assert!(!rs.capture);
        let rs1 = stack.render_state(1);
        assert_eq!(rs1.spotlight, None, "cursor is on monitor 0");
    }

    #[test]
    fn render_state_combines_active_layers_on_their_own_monitors() {
        let mut stack = ModeStack::new(params());
        stack.seed_cursor(0, pt(30, 40));
        stack.add_mode(ModeKind::Zoom); // cursor seeded again below
        stack.seed_cursor(0, pt(30, 40));
        stack.on_wheel(0, pt(30, 40), 120, Modifiers::SHIFT); // zoom 1.25
        stack.add_mode(ModeKind::Snip);
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(9, 9));
        stack.on_left_button_up(0, pt(9, 9));

        let rs = stack.render_state(0);
        assert_eq!(rs.spotlight, Some((pt(9, 9), 100)), "cursor followed the drag");
        let (z, focus) = rs.zoom.expect("zoom on monitor 0");
        assert!((z - 1.25).abs() < 1e-6);
        assert_eq!(focus, pt(9, 9));
        assert_eq!(rs.snip, Some((pt(2, 2), pt(9, 9))));

        let rs1 = stack.render_state(1);
        assert_eq!(rs1.spotlight, None);
        assert_eq!(rs1.zoom, None);
        assert_eq!(rs1.snip, None, "selection lives on monitor 0");
    }

    #[test]
    fn zoom_on_reports_factor_and_focus_per_monitor() {
        let mut stack = ModeStack::new(params());
        assert_eq!(stack.zoom_on(0), None, "no zoom layer yet");
        stack.add_mode(ModeKind::Zoom);
        stack.seed_cursor(1, pt(7, 7));
        stack.on_wheel(1, pt(7, 7), 120, Modifiers::SHIFT);
        assert_eq!(stack.zoom_on(1).map(|(z, p)| ((z * 100.0) as i32, p)), Some((125, pt(7, 7))));
        assert_eq!(stack.zoom_on(0), None, "focus is on monitor 1");
    }
}
