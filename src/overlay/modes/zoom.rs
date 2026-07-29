//! Zoom LAYER: magnify the frozen screen around the cursor. Pure state
//! machine — resampling moved to [`crate::overlay::composite::compose_frame`],
//! which the controller feeds with this layer's [`ZoomMode::zoom`]/
//! [`ZoomMode::cursor`] via [`crate::overlay::modes::ModeStack::render_state`].

use super::ModeEffect;
use crate::geometry::Point;

/// Win32 `WHEEL_DELTA`: one wheel notch.
const WHEEL_DELTA: f32 = 120.0;
/// Fallback when settings pass a `step_factor <= 1.0` (config bug guard).
const DEFAULT_STEP_FACTOR: f32 = 1.25;

/// Zoom layer state: cursor position + current magnification.
///
/// Wheel events multiply/divide the zoom by `step_factor` (settings:
/// `zoom.step_factor`), clamped to `[min, max]`. The reset-view hotkey restores
/// 1.0. WHICH wheel events reach the layer is decided by the
/// [`super::ModeStack`] routing matrix (zoom modifier whenever active, plain
/// wheel when zoom is primary) — the layer itself applies every wheel it gets.
pub struct ZoomMode {
    cursor: Point,
    cursor_monitor: usize,
    zoom: f32,
    step_factor: f32,
    min: f32,
    max: f32,
}

/// Full-monitor repaint after a cursor/focus change; when the cursor crossed
/// to another monitor, BOTH monitors change appearance (the old one reverts to
/// its plain frame, the new one shows the zoomed base).
fn full_repaint(old_monitor: usize, new_monitor: usize) -> ModeEffect {
    if old_monitor == new_monitor {
        ModeEffect::repaint(new_monitor, None)
    } else {
        let mut effect = ModeEffect::none();
        effect.repaint.push((old_monitor, None));
        effect.repaint.push((new_monitor, None));
        effect
    }
}

impl ZoomMode {
    /// Config from settings: `step_factor` (> 1.0), `min` (>= 1.0), `max` (> min).
    /// Initial zoom is 1.0.
    ///
    /// Defensive normalization: a `step_factor <= 1.0` falls back to 1.25,
    /// `min` is floored at 1.0, and `max` is raised to at least `min`, so a
    /// bad config can never invert the wheel direction or panic `f32::clamp`.
    pub fn new(step_factor: f32, min: f32, max: f32) -> Self {
        let step_factor = if step_factor > 1.0 {
            step_factor
        } else {
            DEFAULT_STEP_FACTOR
        };
        let min = min.max(1.0);
        let max = max.max(min);
        Self {
            cursor: Point::default(),
            cursor_monitor: 0,
            zoom: 1.0,
            step_factor,
            min,
            max,
        }
    }

    /// Current magnification (1.0 = none).
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Focus of the magnified view (monitor-local px) — the cursor position.
    pub fn cursor(&self) -> Point {
        self.cursor
    }

    /// Monitor the zoomed view (focus) is on.
    pub fn cursor_monitor(&self) -> usize {
        self.cursor_monitor
    }

    /// Tracks the cursor; the zoomed view recenters on it.
    pub fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        if monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old_monitor = self.cursor_monitor;
        self.cursor = at;
        self.cursor_monitor = monitor;
        full_repaint(old_monitor, monitor)
    }

    /// Multiplies zoom by `step_factor^(delta / 120)`, clamped to [min, max].
    ///
    /// `delta` is in RAW Win32 wheel units (one notch = 120). The exponent is
    /// deliberately FRACTIONAL: smooth-scroll hardware (precision touchpads,
    /// high-resolution wheels) sends sub-notch deltas and gets true smooth
    /// zoom (e.g. four +60 events = step² in total, applied progressively as
    /// step^0.5 per event). The wheel's cursor position becomes the new
    /// focus, so zooming happens around the point the user is actually
    /// looking at.
    pub fn on_wheel(&mut self, monitor: usize, at: Point, delta: i32) -> ModeEffect {
        let notches = delta as f32 / WHEEL_DELTA;
        let new_zoom = (self.zoom * self.step_factor.powf(notches)).clamp(self.min, self.max);
        if new_zoom == self.zoom && monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old_monitor = self.cursor_monitor;
        self.zoom = new_zoom;
        self.cursor = at;
        self.cursor_monitor = monitor;
        full_repaint(old_monitor, monitor)
    }

    /// Reset-view hotkey: zoom = 1.0 again; repaints the cursor monitor.
    pub fn reset_view(&mut self) -> ModeEffect {
        self.zoom = 1.0;
        ModeEffect::repaint(self.cursor_monitor, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_zoom_near(mode: &ZoomMode, expected: f32) {
        assert!(
            (mode.zoom() - expected).abs() < 1e-6,
            "zoom {} vs expected {expected}",
            mode.zoom()
        );
    }

    // ---- construction ----------------------------------------------------

    #[test]
    fn new_starts_at_1x_cursor_at_origin() {
        let m = ZoomMode::new(1.25, 1.0, 10.0);
        assert_eq!(m.zoom(), 1.0);
        assert_eq!(m.cursor(), Point::new(0, 0));
        assert_eq!(m.cursor_monitor(), 0);
    }

    #[test]
    fn new_normalizes_invalid_config() {
        // step_factor <= 1.0 falls back so the wheel can only zoom IN per notch.
        let mut m = ZoomMode::new(0.5, 1.0, 10.0);
        m.on_wheel(0, Point::new(0, 0), 120);
        assert!(m.zoom() > 1.0);
        // max < min is repaired instead of panicking in clamp.
        let mut m = ZoomMode::new(1.5, 4.0, 2.0);
        for _ in 0..50 {
            m.on_wheel(0, Point::new(0, 0), 120);
        }
        assert!(m.zoom() >= 4.0);
        // min < 1.0 is floored to 1.0: zooming out stops at 1x.
        let mut m = ZoomMode::new(1.5, 0.5, 10.0);
        for _ in 0..50 {
            m.on_wheel(0, Point::new(0, 0), -120);
        }
        assert_eq!(m.zoom(), 1.0);
    }

    // ---- wheel zoom -------------------------------------------------------

    #[test]
    fn wheel_multiplies_and_divides_by_step_factor() {
        let mut m = ZoomMode::new(1.25, 1.0, 100.0);
        m.on_wheel(0, Point::new(0, 0), 120);
        assert_zoom_near(&m, 1.25);
        m.on_wheel(0, Point::new(0, 0), 120);
        assert_zoom_near(&m, 1.5625);
        m.on_wheel(0, Point::new(0, 0), -120);
        assert_zoom_near(&m, 1.25);
        m.on_wheel(0, Point::new(0, 0), -120);
        assert_zoom_near(&m, 1.0);
    }

    #[test]
    fn wheel_two_notches_apply_step_squared() {
        let mut m = ZoomMode::new(2.0, 1.0, 100.0);
        m.on_wheel(0, Point::new(0, 0), 240);
        assert_zoom_near(&m, 4.0);
    }

    #[test]
    fn wheel_sub_notch_deltas_zoom_smoothly() {
        // D2 regression: raw deltas below one notch (precision touchpads)
        // must change the zoom, not truncate to zero. Four +60 events total
        // step^(0.5*4) = step², each event making progress.
        let mut m = ZoomMode::new(1.25, 1.0, 100.0);
        let mut prev = m.zoom();
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), 60);
            assert!(m.zoom() > prev, "every +60 event must zoom in");
            prev = m.zoom();
        }
        assert_zoom_near(&m, 1.5625); // 1.25^2
        // Same smoothness on the way out.
        m.on_wheel(0, Point::new(0, 0), -60);
        assert!(m.zoom() < prev);
    }

    #[test]
    fn wheel_clamps_at_min_and_max() {
        let mut m = ZoomMode::new(1.5, 1.0, 3.0);
        for _ in 0..20 {
            m.on_wheel(0, Point::new(0, 0), 120);
        }
        assert_eq!(m.zoom(), 3.0);
        // Further wheel-up at the clamp is a no-op effect.
        let e = m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(e, ModeEffect::none());
        for _ in 0..20 {
            m.on_wheel(0, Point::new(0, 0), -120);
        }
        assert_eq!(m.zoom(), 1.0);
    }

    #[test]
    fn wheel_reports_full_repaint_of_cursor_monitor() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(30, 30));
        let e = m.on_wheel(0, Point::new(30, 30), 120);
        assert_eq!(e.repaint, vec![(0, None)]);
    }

    #[test]
    fn wheel_on_other_monitor_moves_focus_and_repaints_both() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(10, 10));
        let e = m.on_wheel(1, Point::new(50, 50), 120);
        assert_eq!(e.repaint, vec![(0, None), (1, None)]);
        assert_eq!((m.cursor_monitor(), m.cursor()), (1, Point::new(50, 50)));
    }

    // ---- mouse move -------------------------------------------------------

    #[test]
    fn mouse_move_same_position_is_noop() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        assert_eq!(m.on_mouse_move(0, Point::new(0, 0)), ModeEffect::none());
    }

    #[test]
    fn mouse_move_repaints_full_monitor() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        let e = m.on_mouse_move(0, Point::new(7, 9));
        assert_eq!(e.repaint, vec![(0, None)]);
    }

    #[test]
    fn mouse_move_across_monitors_repaints_both() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(7, 9));
        let e = m.on_mouse_move(2, Point::new(1, 1));
        assert_eq!(e.repaint, vec![(0, None), (2, None)]);
    }

    // ---- reset ------------------------------------------------------------

    #[test]
    fn reset_view_restores_1x_and_repaints_cursor_monitor() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(1, Point::new(5, 5));
        m.on_wheel(1, Point::new(5, 5), 240);
        assert!(m.zoom() > 1.0);
        let e = m.reset_view();
        assert_eq!(m.zoom(), 1.0);
        assert_eq!(e.repaint, vec![(1, None)]);
    }

    #[test]
    fn reset_view_is_the_only_reset_path() {
        // D1 regression, updated for the layered design: the OLD app path
        // synthesized a KeyDown into the mode — dead code, because `on_key`
        // was a documented no-op. Key events no longer reach layers AT ALL
        // (the ModeStack has no key entry point; the controller's KeyDown arm
        // is inert), so `reset_view` is structurally the only reset path.
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(7, 7));
        m.on_wheel(0, Point::new(7, 7), 120);
        assert!(m.zoom() > 1.0);
        let via_reset = m.reset_view();
        assert_eq!(m.zoom(), 1.0, "reset_view is the live reset path");
        assert_eq!(via_reset.repaint, vec![(0, None)]);
    }
}
