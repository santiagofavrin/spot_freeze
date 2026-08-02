//! Overlay modes (Spotlight on/off, Capture, and the zoom LAYER) plus
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
//! - **Zoom (the zoom-modifier wheel chord from anywhere) → an IMPLICIT
//!   effect LAYER, not a mode, with no hotkey of its own**: the chord
//!   (default Shift+wheel) zooms from ANY state, IMPLICITLY ACTIVATING the
//!   layer at the last-used factor ([`ModeStack::last_zoom`]) when it isn't
//!   active yet. The layer exists only while actually magnified: zooming back
//!   to the configured minimum ([`ModeParams::zoom_min`], default 1.0)
//!   AUTO-DISMISSES it (banking the minimum as the last-used factor), and `0`
//!   ([`ModeStack::reset_view`]) drops it outright.
//! - **Wheel routing** ([`ModeStack::on_wheel`]):
//!   * the PLAIN wheel (no modifiers held) resizes the spotlight while its
//!     layer is active — wheel up makes it smaller, wheel down makes it
//!     bigger; no modifier is needed; the layer keeps the sub-notch
//!     accumulator. The plain wheel NEVER zooms;
//!   * the configured zoom-modifier chord (default Shift+wheel) zooms from ANY
//!     state — IMPLICITLY ACTIVATING the zoom layer at the last-used factor
//!     (additive, no transition animation) when it isn't active yet, and
//!     AUTO-DISMISSING it when the zoom returns to the minimum;
//!   * a chord matching several routes (possible only via a hand-edited empty
//!     zoom modifier) reaches every matching layer; their effects merge.
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

/// The mode state of one freeze session: the spotlight layer, the zoom
/// layer, the snip (capture) layer, the zoom factor the layer re-activates
/// with, and — while capture mode is active — the stashed spotlight/zoom
/// layers it was entered from.
///
/// Fresh layers are built from [`ModeParams`] on activation, so "reset ALL
/// mode state" is simply "drop every layer and rebuild the requested one".
pub struct ModeStack {
    params: ModeParams,
    spotlight: Option<SpotlightMode>,
    zoom: Option<ZoomMode>,
    snip: Option<SnipMode>,
    /// Factor the zoom layer re-activates with; synced from the layer every
    /// time it is dismissed (the "last-used zoom level", in practice always
    /// the configured minimum once the layer auto-dismisses at baseline).
    last_zoom: f32,
    /// Spotlight/zoom layers stashed while capture mode re-bases the freeze;
    /// `None` outside capture mode. Invariant: `saved.is_some() ==
    /// snip.is_some()` — the snip layer exists only inside capture mode and
    /// every transition moves both together; the controller pairs this stash
    /// with its own pre-capture pixel stash (`FreezeState::capture`).
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
    /// spec) and the last-used zoom factor starts at 1.0.
    pub fn new(params: ModeParams) -> Self {
        Self {
            spotlight: Some(SpotlightMode::new(params.spotlight_radius)),
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

    /// Per-layer active flags in the legend's tab order (Spotlight, Zoom,
    /// Snip) — the controller paints the mode legend with them.
    pub fn layers_active(&self) -> [bool; 3] {
        [
            self.spotlight.is_some(),
            self.zoom.is_some(),
            self.snip.is_some(),
        ]
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
    /// reset ALL mode state (zoom layer dropped, snip selection cleared,
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

    /// ADD `kind`'s layer WITHOUT touching the existing ones (the zoom
    /// layer comes back at the last-used factor). `Snip` is capture mode, not
    /// an additive layer: it enters capture, stashing the existing layers
    /// (see [`ModeStack::enter_capture`]). No-op when the layer is already
    /// active.
    pub fn add_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            return;
        }
        self.activate(kind);
    }

    /// TOGGLE key (spotlight's `S`): remove `kind`'s layer when active, add
    /// it when not. Removing the zoom layer banks its factor as the last-used
    /// level; re-activating restores it. Toggling the last spotlight layer
    /// off leaves the screen frozen but unveiled; toggling it back on
    /// re-activates with fresh state (radius back to default). `Snip` toggles
    /// capture mode: ON enters via [`ModeStack::enter_capture`], OFF exits
    /// via [`ModeStack::exit_capture`] (the stashed layers come back).
    ///
    /// `ModeKind::Zoom` is not reachable from any hotkey (zoom is the
    /// implicit wheel chord); the arm remains for completeness and tests.
    pub fn toggle_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            match kind {
                ModeKind::Spotlight => self.spotlight = None,
                ModeKind::Zoom => {
                    if let Some(zoom) = self.zoom.take() {
                        self.last_zoom = zoom.zoom();
                    }
                }
                ModeKind::Snip => self.exit_capture(),
            }
            return;
        }
        self.activate(kind);
    }

    /// Activate `kind`'s layer: fresh state for spotlight, the last-used
    /// factor for the zoom layer, capture entry for the snip layer (the only
    /// way a snip layer may come into existence — see the `saved` invariant).
    fn activate(&mut self, kind: ModeKind) {
        match kind {
            ModeKind::Spotlight => {
                self.spotlight = Some(SpotlightMode::new(self.params.spotlight_radius));
            }
            ModeKind::Zoom => {
                self.zoom = Some(ZoomMode::with_zoom(
                    self.last_zoom,
                    self.params.zoom_step,
                    self.params.zoom_min,
                    self.params.zoom_max,
                ));
            }
            ModeKind::Snip => self.enter_capture(),
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

    /// Wheel routing (see module docs):
    ///
    /// - the PLAIN wheel (no modifiers held) resizes the spotlight while its
    ///   layer is active — and NEVER zooms;
    /// - the configured zoom-modifier chord (default Shift+wheel) reaches the
    ///   zoom layer from ANY state — IMPLICITLY ACTIVATING it at the last-used
    ///   factor when it isn't active yet — and never resizes the spotlight.
    ///
    /// Several layers may answer the same event only when the zoom chord
    /// matches the plain wheel (a hand-edited empty zoom modifier); their
    /// repaint effects merge.
    pub fn on_wheel(
        &mut self,
        monitor: usize,
        at: Point,
        delta: i32,
        modifiers: Modifiers,
    ) -> ModeEffect {
        let mut effect = ModeEffect::none();
        let plain = modifiers.is_empty();
        // The plain wheel resizes the spotlight only; it never touches zoom.
        if plain && let Some(spot) = self.spotlight.as_mut() {
            effect.absorb(spot.on_wheel(monitor, at, delta));
        }
        let zoom_chord = modifiers.contains(self.params.zoom_modifier);
        if zoom_chord {
            // Implicit activation: the zoom-modifier chord adds the zoom layer
            // when it isn't active yet — product spec: the chord zooms
            // straight out of the pristine spotlight-only state. This is
            // ADDITIVE (existing layers untouched) and plays no transition
            // animation: the zoom arrives with the scroll itself, one frame
            // per wheel event. Zooming IN from nothing activates at the
            // last-used factor; zooming OUT from nothing is a no-op (the zoom
            // can never go below the baseline). No cursor seeding needed here
            // — ZoomMode::on_wheel makes the wheel position the new focus.
            if self.zoom.is_none() && delta > 0 {
                self.add_mode(ModeKind::Zoom);
            }
            if let Some(zoom) = self.zoom.as_mut() {
                effect.absorb(zoom.on_wheel(monitor, at, delta));
            }
            // Auto-dismiss: back at the configured minimum (no magnification)
            // the layer is dropped, banking the minimum as the last-used
            // factor. The zoom layer thus exists only while actually
            // magnified.
            let min = self.params.zoom_min;
            if self.zoom.as_ref().is_some_and(|z| z.zoom() <= min) {
                self.last_zoom = min;
                self.zoom = None;
            }
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

    /// Reset-view hotkey (default binding `0`): DISMISS the zoom layer
    /// entirely (back to the un-zoomed view), repainting the monitor it was
    /// on, and bank the configured minimum as the last-used factor. A no-op
    /// effect when no zoom layer is active.
    pub fn reset_view(&mut self) -> ModeEffect {
        match self.zoom.take() {
            Some(zoom) => {
                self.last_zoom = self.params.zoom_min;
                ModeEffect::repaint(zoom.cursor_monitor(), None)
            }
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

    /// Default-like params: radius 100, Shift zoom modifier, zoom step 1.25
    /// in [1.0, 100.0].
    fn params() -> ModeParams {
        ModeParams {
            spotlight_radius: 100,
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
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // implicit zoom 1.25
        stack.set_mode(ModeKind::Snip); // capture: spotlight/zoom stashed
        stack.on_left_button_down(0, pt(2, 2));
        stack.on_mouse_move(0, pt(8, 8));
        stack.on_left_button_up(0, pt(8, 8));
        assert!(stack.snip_selection().is_some());
        assert!(stack.in_capture());

        stack.set_mode(ModeKind::Spotlight);
        assert!(!stack.is_active(ModeKind::Zoom), "zoom dropped");
        assert!(!stack.is_active(ModeKind::Snip), "snip dropped");
        assert!(!stack.in_capture(), "capture stash dropped");
        assert!(stack.is_active(ModeKind::Spotlight));
        assert_eq!(stack.snip_selection(), None, "selection cleared");
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            100,
            "spotlight rebuilt fresh at the default radius"
        );
    }

    #[test]
    fn set_mode_same_kind_still_resets_state() {
        // Spec: a plain press is a FULL SWITCH — no same-kind exemption.
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(0, 0), 120, Modifiers::NONE); // radius 90
        stack.set_mode(ModeKind::Spotlight);
        assert_eq!(stack.spotlight().unwrap().radius(), 100, "radius reset");
    }

    #[test]
    fn add_mode_preserves_existing_layers() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90

        stack.add_mode(ModeKind::Zoom);
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            90,
            "additive activation does NOT reset the spotlight"
        );
        assert_eq!(
            stack.zoom().unwrap().zoom(),
            1.0,
            "zoom layer starts at 1.0"
        );

        // Snip is capture mode, not an additive layer: the existing layers
        // are STASHED (exit_capture restores them), not combined with snip.
        stack.add_mode(ModeKind::Snip);
        assert!(stack.is_active(ModeKind::Snip));
        assert!(stack.in_capture());
        assert!(!stack.is_active(ModeKind::Spotlight), "stashed for capture");
        assert!(!stack.is_active(ModeKind::Zoom), "stashed for capture");
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
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90
        stack.toggle_mode(ModeKind::Spotlight);
        stack.toggle_mode(ModeKind::Spotlight);
        assert!(stack.is_active(ModeKind::Spotlight));
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            100,
            "fresh default state"
        );
    }

    #[test]
    fn zoom_toggle_banks_and_restores_the_last_used_factor() {
        // `toggle_mode(ModeKind::Zoom)` is unreachable from any hotkey (zoom
        // is the implicit wheel chord); this pins the method's bank/restore
        // contract for the internal activation path.
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // layer on at 1.0
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.5625

        stack.toggle_mode(ModeKind::Zoom); // off: factor banked
        assert!(!stack.is_active(ModeKind::Zoom));
        assert!(stack.is_active(ModeKind::Spotlight), "spotlight untouched");

        stack.toggle_mode(ModeKind::Zoom); // on: last-used factor back
        assert_zoom_near(&stack, 1.5625);
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
    fn wheel_spotlight_only_plain_wheel_resizes_modifier_chords_stay_inert() {
        let mut stack = ModeStack::new(params());
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(stack.spotlight().unwrap().radius(), 90);
        assert!(!e.repaint.is_empty(), "resize reports a repaint");
        assert!(
            !stack.is_active(ModeKind::Zoom),
            "the plain wheel must NOT activate the zoom layer"
        );

        // A chord that is neither plain nor the zoom modifier does nothing.
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(e, ModeEffect::none(), "Ctrl+wheel does not resize");
        assert_eq!(stack.spotlight().unwrap().radius(), 90);
        assert!(!stack.is_active(ModeKind::Zoom));
    }

    #[test]
    fn wheel_spotlight_only_shift_wheel_implicitly_activates_zoom() {
        // Pristine spotlight-only + the zoom-modifier chord (default
        // Shift+wheel) ADDITIVELY activates the zoom layer (at the
        // last-used factor, 1.0 here) and zooms in the same event — no
        // dedicated zoom hotkey needed first. (No transition animation is
        // involved at this level: animations live in the controller's
        // key-driven paths.)
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90 first
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
            90,
            "additive: spotlight preserved (the chord bypasses the resize)"
        );
        // The wheel event's position becomes the fresh layer's focus.
        let zoom = stack.zoom().unwrap();
        assert_eq!((zoom.cursor_monitor(), zoom.cursor()), (0, pt(10, 10)));
    }

    #[test]
    fn wheel_after_implicit_zoom_activation_plain_wheel_resizes_the_spotlight() {
        // Once the chord implicitly added the zoom layer, BOTH layers are
        // active: the plain wheel belongs to the spotlight, the chord keeps
        // zooming.
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // activates + 1.25
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert!(!e.repaint.is_empty(), "plain wheel reaches the spotlight");
        assert_eq!(stack.spotlight().unwrap().radius(), 90);
        assert_zoom_near(&stack, 1.25);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(!e.repaint.is_empty(), "zoom chord keeps zooming");
        assert_zoom_near(&stack, 1.5625);
        assert_eq!(stack.spotlight().unwrap().radius(), 90);
    }

    #[test]
    fn wheel_zoom_active_spotlight_off_plain_wheel_never_zooms() {
        // With the spotlight off the zoom layer is the only one active, but
        // the PLAIN wheel still never zooms — zoom is exclusively the
        // zoom-modifier chord.
        let mut stack = ModeStack::new(params());
        stack.toggle_mode(ModeKind::Spotlight); // spotlight off
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // implicit zoom 1.25
        assert_zoom_near(&stack, 1.25);

        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(
            e,
            ModeEffect::none(),
            "plain wheel is inert with no spotlight to resize"
        );
        assert_zoom_near(&stack, 1.25);
        // ...and the zoom chord still zooms, from the same state.
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(!e.repaint.is_empty(), "zoom chord always reaches zoom");
        assert_zoom_near(&stack, 1.5625);
    }

    #[test]
    fn wheel_both_layers_active_plain_wheel_resizes_spotlight_only() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // both layers active
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(stack.spotlight().unwrap().radius(), 90);
        assert_eq!(
            stack.zoom().unwrap().zoom(),
            1.0,
            "an active spotlight owns the plain wheel"
        );
        assert!(!e.repaint.is_empty());
    }

    #[test]
    fn wheel_chord_with_extra_modifiers_zooms_and_never_resizes() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(stack.spotlight().unwrap().radius(), 100, "no resize");
        assert_zoom_near(&stack, 1.25);
        // Only the zoom layer answered: one full repaint.
        assert_eq!(e.repaint, vec![(0, None)], "zoom full repaint only");
    }

    #[test]
    fn wheel_zoom_active_unbound_chord_does_nothing() {
        let mut stack = ModeStack::new(params());
        stack.toggle_mode(ModeKind::Spotlight); // spotlight off
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // implicit zoom 1.25
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL);
        assert_eq!(e, ModeEffect::none(), "Ctrl matches no route");
        assert_zoom_near(&stack, 1.25);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(
            !e.repaint.is_empty(),
            "zoom modifier reaches the zoom layer"
        );
        assert_zoom_near(&stack, 1.5625);
    }

    #[test]
    fn wheel_sub_notch_accumulators_survive_routing() {
        // D2 regression at stack level: four +60 plain events resize by +20
        // in total (spotlight accumulator), four +60 Shift events zoom by
        // step^2 (zoom fractional exponent).
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        for _ in 0..4 {
            stack.on_wheel(0, pt(10, 10), 60, Modifiers::NONE);
        }
        assert_eq!(stack.spotlight().unwrap().radius(), 80);
        for _ in 0..4 {
            stack.on_wheel(0, pt(10, 10), 60, Modifiers::SHIFT);
        }
        assert_zoom_near(&stack, 1.5625);
        // Chord deltas must NOT bank into the spotlight accumulator.
        stack.on_wheel(0, pt(10, 10), 60, Modifiers::SHIFT); // (zooms, not banking radius)
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(stack.spotlight().unwrap().radius(), 70);
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
    fn reset_view_drops_the_zoom_layer_repainting_its_monitor() {
        let mut stack = ModeStack::new(params());
        assert_eq!(stack.reset_view(), ModeEffect::none(), "no zoom layer");

        stack.on_wheel(1, pt(5, 5), 120, Modifiers::SHIFT); // implicit zoom on monitor 1
        assert!(stack.is_active(ModeKind::Zoom));
        let e = stack.reset_view();
        assert!(!stack.is_active(ModeKind::Zoom), "zoom layer dropped");
        assert_eq!(e.repaint, vec![(1, None)], "repaints the cursor monitor");
        assert!(!e.exit);
        // Spotlight state is not touched by reset_view.
        assert!(stack.spotlight().is_some());
        // The minimum is banked as the last-used factor: re-activating starts
        // from the baseline again.
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert_zoom_near(&stack, 1.25);
    }

    // ---- implicit zoom activation / auto-dismiss ----------------------------

    #[test]
    fn zoom_chord_zoom_out_from_no_layer_is_a_noop() {
        // Zooming OUT with no zoom layer can never go below the baseline:
        // nothing activates, nothing changes.
        let mut stack = ModeStack::new(params());
        let e = stack.on_wheel(0, pt(10, 10), -120, Modifiers::SHIFT);
        assert_eq!(e, ModeEffect::none());
        assert!(!stack.is_active(ModeKind::Zoom));
        assert_eq!(stack.spotlight().unwrap().radius(), 100);
    }

    #[test]
    fn zoom_chord_auto_dismisses_the_layer_back_at_the_minimum() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // implicit zoom 1.25
        assert!(stack.is_active(ModeKind::Zoom));

        // Zooming back out to the baseline drops the layer entirely.
        let e = stack.on_wheel(0, pt(10, 10), -120, Modifiers::SHIFT);
        assert!(
            !stack.is_active(ModeKind::Zoom),
            "back at min: layer dropped"
        );
        assert!(!e.repaint.is_empty(), "the dismissal repaints");
        assert!(stack.is_active(ModeKind::Spotlight), "spotlight untouched");

        // The minimum is banked as the last-used factor: re-activating starts
        // from the baseline, not from a stale magnified level.
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert_zoom_near(&stack, 1.25);
    }

    #[test]
    fn zoom_chord_zoom_out_past_the_minimum_clamps_then_dismisses() {
        // A multi-notch zoom-out that would overshoot the baseline clamps at
        // the minimum and dismisses in the same event.
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // implicit zoom 1.25
        assert!(stack.is_active(ModeKind::Zoom));
        stack.on_wheel(0, pt(10, 10), -480, Modifiers::SHIFT); // far out
        assert!(!stack.is_active(ModeKind::Zoom), "clamped at min: dropped");
    }

    // ---- capture mode ---------------------------------------------------------

    #[test]
    fn set_mode_snip_enters_capture_stashing_spotlight_and_zoom() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom layer on, 1.25

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
        stack.on_wheel(0, pt(30, 40), 120, Modifiers::NONE); // radius 90
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
        assert_eq!(spot.radius(), 90, "spotlight state survives the round-trip");
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

    #[test]
    fn add_mode_snip_enters_capture_and_exit_restores_the_stash() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom layer on, 1.25

        stack.add_mode(ModeKind::Snip);
        assert!(stack.in_capture(), "add_mode(Snip) enters capture mode");
        assert!(stack.is_active(ModeKind::Snip));
        assert!(!stack.is_active(ModeKind::Spotlight), "stashed");
        assert!(!stack.is_active(ModeKind::Zoom), "stashed");

        stack.exit_capture();
        assert!(!stack.in_capture());
        assert!(!stack.is_active(ModeKind::Snip));
        assert_eq!(stack.spotlight().unwrap().radius(), 90, "stash restored");
        assert_zoom_near(&stack, 1.25);
    }

    #[test]
    fn toggle_mode_snip_toggles_capture_in_and_out() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE); // radius 90

        stack.toggle_mode(ModeKind::Snip); // ON: enter capture
        assert!(stack.in_capture());
        assert!(stack.is_active(ModeKind::Snip));
        assert!(!stack.is_active(ModeKind::Spotlight), "stashed");

        stack.toggle_mode(ModeKind::Snip); // OFF: exit capture
        assert!(!stack.in_capture());
        assert!(!stack.is_active(ModeKind::Snip));
        assert_eq!(stack.spotlight().unwrap().radius(), 90, "stash restored");
    }

    #[test]
    fn set_mode_out_of_capture_drops_the_stash() {
        let mut stack = ModeStack::new(params());
        stack.set_mode(ModeKind::Snip);
        assert!(stack.in_capture());

        stack.set_mode(ModeKind::Spotlight);
        assert!(!stack.in_capture(), "a full switch drops the capture stash");
        assert!(!stack.is_active(ModeKind::Snip));
        assert!(stack.is_active(ModeKind::Spotlight));
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
        stack.on_mouse_move(0, pt(9, 9));

        let rs = stack.render_state(0);
        assert_eq!(
            rs.spotlight,
            Some((pt(9, 9), 100)),
            "cursor followed the move"
        );
        let (z, focus) = rs.zoom.expect("zoom on monitor 0");
        assert!((z - 1.25).abs() < 1e-6);
        assert_eq!(focus, pt(9, 9));
        assert_eq!(rs.snip, None);
        assert!(!rs.capture);

        let rs1 = stack.render_state(1);
        assert_eq!(rs1.spotlight, None);
        assert_eq!(rs1.zoom, None);
        assert_eq!(rs1.snip, None);
    }

    #[test]
    fn zoom_on_reports_factor_and_focus_per_monitor() {
        let mut stack = ModeStack::new(params());
        assert_eq!(stack.zoom_on(0), None, "no zoom layer yet");
        stack.add_mode(ModeKind::Zoom);
        stack.seed_cursor(1, pt(7, 7));
        stack.on_wheel(1, pt(7, 7), 120, Modifiers::SHIFT);
        assert_eq!(
            stack.zoom_on(1).map(|(z, p)| ((z * 100.0) as i32, p)),
            Some((125, pt(7, 7)))
        );
        assert_eq!(stack.zoom_on(0), None, "focus is on monitor 1");
    }
}
