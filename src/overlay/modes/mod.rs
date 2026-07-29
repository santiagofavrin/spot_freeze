//! Overlay modes (Spotlight / Zoom / Snip) as COMPOSABLE LAYERS plus the
//! [`ModeStack`] that combines them. Every layer is a pure state machine —
//! pixel compositing lives in [`crate::overlay::composite::compose_frame`];
//! a layer only tracks state (cursor, radius, zoom factor, selection) and
//! reports dirty regions. No `windows` types anywhere in this module tree.
//!
//! # Composability contract (product spec)
//!
//! - **Toggle key (Spotlight's `S`) → [`ModeStack::toggle_mode`]**: the layer
//!   is added when inactive, REMOVED when active. Toggling the last layer off
//!   leaves the screen frozen but UNVEILED ([`ModeStack::any_active`] is
//!   false — the controller dims nothing).
//! - **Plain mode key (Snip's `C`) → [`ModeStack::set_mode`]**: FULL SWITCH —
//!   every layer is reset to fresh state (zoom 1.0, snip selection cleared,
//!   spotlight radius back to default, cursor re-seeded by the controller)
//!   and `kind` becomes the ONLY active layer.
//! - **Shift+mode key → [`ModeStack::add_mode`]**: ADDITIVE — `kind`'s layer is
//!   activated (fresh state) WITHOUT touching the existing layers; a no-op
//!   when that layer is already active.
//! - **Primary mode**: the last-activated layer (`None` when nothing is
//!   active). Only used by wheel routing: the plain (unmodified) wheel drives
//!   zoom when zoom is primary.
//! - **Wheel routing matrix** ([`ModeStack::on_wheel`]):
//!   * spotlight is offered EVERY wheel event while active; the layer itself
//!     enforces its radius-modifier gate (default Ctrl) and keeps the
//!     sub-notch accumulator;
//!   * the configured zoom-modifier chord (default Shift+wheel) zooms from ANY
//!     layer combination — IMPLICITLY ACTIVATING the zoom layer (additive, no
//!     border flash) when it isn't active yet — and the PLAIN wheel zooms when
//!     zoom is the primary mode;
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

/// The composable mode state of one freeze session: up to three active layers
/// (one per [`ModeKind`]) plus the PRIMARY (last-activated) kind — `None`
/// when every layer is toggled off (the screen stays frozen, unveiled).
///
/// Fresh layers are built from [`ModeParams`] on activation, so "reset ALL
/// mode state" is simply "drop every layer and rebuild the requested one".
pub struct ModeStack {
    params: ModeParams,
    spotlight: Option<SpotlightMode>,
    zoom: Option<ZoomMode>,
    snip: Option<SnipMode>,
    primary: Option<ModeKind>,
}

impl ModeStack {
    /// Freeze-time initial state: Spotlight is the only active layer (product
    /// spec) and the primary mode.
    pub fn new(params: ModeParams) -> Self {
        Self {
            spotlight: Some(SpotlightMode::new(
                params.spotlight_radius,
                params.radius_modifier,
            )),
            zoom: None,
            snip: None,
            primary: Some(ModeKind::Spotlight),
            params,
        }
    }

    /// The primary (last-activated) mode, `None` when no layer is active.
    /// Drives the plain-wheel zoom rule.
    pub fn primary(&self) -> Option<ModeKind> {
        self.primary
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

    /// PLAIN mode key: full switch — reset ALL layers to fresh state and make
    /// `kind` the only active one (radius back to default, zoom back to 1.0,
    /// snip selection cleared). Always resets, even when `kind` is already the
    /// only active layer (spec: a plain press is a full switch).
    pub fn set_mode(&mut self, kind: ModeKind) {
        self.spotlight = None;
        self.zoom = None;
        self.snip = None;
        self.activate(kind);
    }

    /// SHIFT+mode key: add `kind`'s layer (fresh state) WITHOUT touching the
    /// existing layers. No-op when the layer is already active.
    pub fn add_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            return;
        }
        self.activate(kind);
    }

    /// TOGGLE key (Spotlight's `S`): remove `kind`'s layer when active, add it
    /// (fresh state) when not. Toggling the last layer off leaves the screen
    /// frozen but unveiled; toggling back on re-activates with fresh state
    /// (spotlight radius back to default).
    pub fn toggle_mode(&mut self, kind: ModeKind) {
        if self.is_active(kind) {
            match kind {
                ModeKind::Spotlight => self.spotlight = None,
                ModeKind::Zoom => self.zoom = None,
                ModeKind::Snip => self.snip = None,
            }
            if self.primary == Some(kind) {
                // Fall back to any remaining layer (arbitrary but stable
                // order); None when nothing is left.
                self.primary = [ModeKind::Zoom, ModeKind::Snip, ModeKind::Spotlight]
                    .into_iter()
                    .find(|&k| self.is_active(k));
            }
            return;
        }
        self.activate(kind);
    }

    /// Activate `kind`'s layer with fresh state and make it primary.
    fn activate(&mut self, kind: ModeKind) {
        match kind {
            ModeKind::Spotlight => {
                self.spotlight = Some(SpotlightMode::new(
                    self.params.spotlight_radius,
                    self.params.radius_modifier,
                ));
            }
            ModeKind::Zoom => {
                self.zoom = Some(ZoomMode::new(
                    self.params.zoom_step,
                    self.params.zoom_min,
                    self.params.zoom_max,
                ));
            }
            ModeKind::Snip => {
                self.snip = Some(SnipMode::new());
            }
        }
        self.primary = Some(kind);
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
    /// - the configured zoom-modifier chord (default Shift+wheel) reaches zoom
    ///   from ANY layer combination — IMPLICITLY ACTIVATING the zoom layer
    ///   first when it isn't active yet — and the PLAIN wheel (no modifiers)
    ///   reaches zoom when zoom is the primary mode.
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
        // Implicit activation (D3): the zoom-modifier chord adds the zoom
        // layer when it isn't active yet — product spec: the chord zooms
        // straight out of the pristine spotlight-only state. This is ADDITIVE
        // (fresh zoom state, existing layers untouched — same as Shift+Z) and
        // deliberately does NOT flash the border: the flash lives in the
        // controller's key-driven set_mode/add_mode path, and flashing on
        // every scroll would be wrong. No cursor seeding needed here —
        // ZoomMode::on_wheel makes the wheel position the new focus itself.
        if zoom_chord && self.zoom.is_none() {
            self.add_mode(ModeKind::Zoom);
        }
        let plain_zoom = modifiers.is_empty() && self.primary == Some(ModeKind::Zoom);
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
    /// follow the cursor monitor, snip its drag monitor).
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
    fn new_starts_spotlight_only_and_primary() {
        let stack = ModeStack::new(params());
        assert_eq!(stack.primary(), Some(ModeKind::Spotlight));
        assert!(stack.is_active(ModeKind::Spotlight));
        assert!(!stack.is_active(ModeKind::Zoom));
        assert!(!stack.is_active(ModeKind::Snip));
        assert!(stack.spotlight().is_some());
        assert!(stack.zoom().is_none());
        assert!(stack.snip().is_none());
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
        assert_eq!(stack.primary(), Some(ModeKind::Zoom));
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
        assert_eq!(stack.primary(), Some(ModeKind::Spotlight));
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
    fn add_mode_preserves_existing_layers_and_makes_kind_primary() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110

        stack.add_mode(ModeKind::Zoom);
        assert_eq!(stack.primary(), Some(ModeKind::Zoom));
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            110,
            "additive activation does NOT reset the spotlight"
        );
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0, "new layer starts fresh");

        stack.add_mode(ModeKind::Snip);
        assert!(stack.is_active(ModeKind::Spotlight));
        assert!(stack.is_active(ModeKind::Zoom));
        assert!(stack.is_active(ModeKind::Snip));
        assert_eq!(stack.primary(), Some(ModeKind::Snip));
    }

    #[test]
    fn add_mode_already_active_is_a_noop() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // primary Zoom
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25
        stack.add_mode(ModeKind::Zoom); // no-op: layer already active
        assert_zoom_near(&stack, 1.25);
        assert_eq!(stack.primary(), Some(ModeKind::Zoom), "primary untouched");
        // Same for the freeze-default spotlight layer.
        stack.add_mode(ModeKind::Spotlight);
        assert_eq!(stack.spotlight().unwrap().radius(), 100);
    }

    // ---- toggle_mode ---------------------------------------------------------

    #[test]
    fn toggle_off_removes_the_layer_and_clears_primary_when_none_left() {
        let mut stack = ModeStack::new(params());
        assert!(stack.any_active());
        stack.toggle_mode(ModeKind::Spotlight);
        assert!(!stack.is_active(ModeKind::Spotlight));
        assert!(!stack.any_active(), "no layers left: frozen but unveiled");
        assert_eq!(stack.primary(), None);
    }

    #[test]
    fn toggle_on_reactivates_with_fresh_state_and_primary() {
        let mut stack = ModeStack::new(params());
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::CTRL); // radius 110
        stack.toggle_mode(ModeKind::Spotlight);
        stack.toggle_mode(ModeKind::Spotlight);
        assert!(stack.is_active(ModeKind::Spotlight));
        assert_eq!(stack.spotlight().unwrap().radius(), 100, "fresh default state");
        assert_eq!(stack.primary(), Some(ModeKind::Spotlight));
    }

    #[test]
    fn toggle_off_primary_falls_back_to_a_remaining_layer() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // primary Zoom
        stack.toggle_mode(ModeKind::Zoom);
        assert!(!stack.is_active(ModeKind::Zoom));
        assert_eq!(stack.primary(), Some(ModeKind::Spotlight));
        // Zoom state is rebuilt fresh on re-activation.
        stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT); // zoom 1.25 again
        stack.toggle_mode(ModeKind::Zoom);
        stack.add_mode(ModeKind::Zoom);
        assert_eq!(stack.zoom().unwrap().zoom(), 1.0, "fresh zoom state");
    }

    #[test]
    fn plain_wheel_is_inert_with_no_layers_active() {
        let mut stack = ModeStack::new(params());
        stack.toggle_mode(ModeKind::Spotlight);
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none(), "no zoom layer and no primary");
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
        // D3 semantics: pristine spotlight-only + the zoom-modifier chord
        // (default Shift+wheel) ADDITIVELY activates the zoom layer and zooms
        // in the same event — no explicit Shift+Z needed first. (No border
        // flash is involved at this level: flashing lives in the controller's
        // key-driven activation path.)
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
        assert_eq!(
            stack.primary(),
            Some(ModeKind::Zoom),
            "implicit activation makes zoom the primary mode"
        );
        // The wheel event's position becomes the fresh layer's focus.
        let zoom = stack.zoom().unwrap();
        assert_eq!((zoom.cursor_monitor(), zoom.cursor()), (0, pt(10, 10)));
    }

    #[test]
    fn wheel_implicit_zoom_activation_then_plain_wheel_zooms() {
        // Follow-up of the D3 rule: after the implicit activation zoom is
        // PRIMARY, so the plain wheel now zooms (same as after Shift+Z).
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
    fn wheel_zoom_primary_plain_wheel_zooms() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // primary becomes Zoom
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert!(!e.repaint.is_empty());
        assert_zoom_near(&stack, 1.25);
        assert_eq!(
            stack.spotlight().unwrap().radius(),
            100,
            "spotlight untouched by the plain wheel"
        );
    }

    #[test]
    fn wheel_zoom_modifier_zooms_whenever_zoom_is_active() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom);
        stack.primary = Some(ModeKind::Spotlight); // zoom active but NOT primary
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::SHIFT);
        assert!(!e.repaint.is_empty(), "zoom modifier always reaches zoom");
        assert_zoom_near(&stack, 1.25);
        // ...while the PLAIN wheel does nothing when zoom is not primary.
        let e = stack.on_wheel(0, pt(10, 10), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none());
        assert_zoom_near(&stack, 1.25);
    }

    #[test]
    fn wheel_ctrl_with_zoom_active_only_resizes_spotlight() {
        let mut stack = ModeStack::new(params());
        stack.add_mode(ModeKind::Zoom); // both active, primary Zoom
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
        stack.set_mode(ModeKind::Zoom); // zoom only, primary
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

    // ---- render_state / zoom_on ----------------------------------------------

    #[test]
    fn render_state_spotlight_only_on_cursor_monitor() {
        let mut stack = ModeStack::new(params());
        stack.seed_cursor(0, pt(30, 40));
        let rs = stack.render_state(0);
        assert_eq!(rs.spotlight, Some((pt(30, 40), 100)));
        assert_eq!(rs.zoom, None);
        assert_eq!(rs.snip, None);
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
