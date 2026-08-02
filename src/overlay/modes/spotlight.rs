//! Spotlight LAYER: the frozen screen is darkened except a clear circle around
//! the cursor. Pure state machine — pixel work moved to
//! [`crate::overlay::composite::compose_frame`], which the controller feeds
//! with this layer's [`SpotlightMode::cursor`]/[`SpotlightMode::radius`] via
//! [`crate::overlay::modes::ModeStack::render_state`].

use super::ModeEffect;
use crate::geometry::{Point, Rect};

/// Smallest selectable spotlight radius (physical px).
const MIN_RADIUS: u32 = 10;
/// Largest selectable spotlight radius (physical px).
const MAX_RADIUS: u32 = 1000;
/// Radius change per `WHEEL_DELTA` (120) of wheel delta (physical px).
const RADIUS_STEP: i64 = 10;
/// Win32 `WHEEL_DELTA`: one wheel notch.
const WHEEL_DELTA: i64 = 120;

/// Axis-aligned bounding box of the spotlight circle in monitor-local pixels.
/// `+1` on each axis: the circle `dx^2 + dy^2 <= r^2` reaches `cx + r`
/// inclusive. Unclipped — dirty regions may extend past the monitor edge;
/// the controller clips them to the window.
fn circle_bbox(center: Point, radius: u32) -> Rect {
    let r = radius as i32;
    Rect::new(center.x - r, center.y - r, radius * 2 + 1, radius * 2 + 1)
}

/// Smallest rect covering both `a` and `b`; an empty rect contributes nothing.
///
/// Empty/right/bottom math is inlined from pub fields (per the `geometry`
/// contract: empty = either axis is 0) so this module stays independent of
/// the `Rect` helper methods.
fn rect_union(a: Rect, b: Rect) -> Rect {
    if a.width == 0 || a.height == 0 {
        return b;
    }
    if b.width == 0 || b.height == 0 {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width as i32).max(b.x + b.width as i32);
    let bottom = (a.y + a.height as i32).max(b.y + b.height as i32);
    Rect::new(x, y, (right - x) as u32, (bottom - y) as u32)
}

/// Repaint effect for a circle that moved/resized from `old` to `new`, each a
/// `(monitor, bbox)` pair. Cross-monitor moves repaint both monitors.
fn circle_repaint(old: (usize, Rect), new: (usize, Rect)) -> ModeEffect {
    let mut effect = ModeEffect::none();
    if old.0 == new.0 {
        effect.repaint.push((new.0, Some(rect_union(old.1, new.1))));
    } else {
        effect.repaint.push((old.0, Some(old.1)));
        effect.repaint.push((new.0, Some(new.1)));
    }
    effect
}

/// Spotlight layer state: cursor position + circle radius.
///
/// Wheel events resize the circle. The layer applies EVERY wheel event it is
/// offered — the [`super::ModeStack`] routes only PLAIN wheel events (no
/// modifiers held) here, so resizing needs no modifier and modifier chords
/// (e.g. the zoom-modifier wheel) never reach the layer.
///
/// Wheel deltas arrive in RAW Win32 units (one notch = [`WHEEL_DELTA`] = 120).
/// Smooth-scroll hardware (precision touchpads, high-resolution wheels) sends
/// sub-notch deltas (|delta| < 120) that pure per-event truncation would
/// silently drop, so they are banked in `wheel_accum`: every event consumes
/// only the delta its whole-pixel step accounts for and keeps the truncation
/// remainder for the next event (Bresenham-style), making resize responsive
/// at any scroll granularity without drift.
pub struct SpotlightMode {
    cursor: Point,
    cursor_monitor: usize,
    radius: u32,
    /// Unconsumed raw wheel delta (truncation remainder; |value| stays below
    /// `WHEEL_DELTA / RADIUS_STEP` after every applied resize).
    wheel_accum: i64,
}

impl SpotlightMode {
    /// `default_radius` in physical pixels (settings: `spotlight.default_radius`).
    ///
    /// The radius is clamped to `10..=1000` px, the same range wheel resizing
    /// is clamped to, so a rogue settings value cannot break the invariant.
    pub fn new(default_radius: u32) -> Self {
        Self {
            cursor: Point::default(),
            cursor_monitor: 0,
            radius: default_radius.clamp(MIN_RADIUS, MAX_RADIUS),
            wheel_accum: 0,
        }
    }

    /// Current circle radius in physical pixels (wheel-adjusted).
    pub fn radius(&self) -> u32 {
        self.radius
    }

    /// Cursor position the circle is centered on (monitor-local px).
    pub fn cursor(&self) -> Point {
        self.cursor
    }

    /// Monitor the cursor (and therefore the hole) is on.
    pub fn cursor_monitor(&self) -> usize {
        self.cursor_monitor
    }

    /// Tracks the cursor; requests a repaint of the hole's old + new regions.
    pub fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        if monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old = (self.cursor_monitor, circle_bbox(self.cursor, self.radius));
        self.cursor = at;
        self.cursor_monitor = monitor;
        circle_repaint(old, (monitor, circle_bbox(at, self.radius)))
    }

    /// Resizes the radius by the raw wheel delta.
    ///
    /// `delta` is in RAW Win32 wheel units: `120` (wheel up) = `-10` px,
    /// proportionally (`60` = `-5`), clamped to `10..=1000`. Sub-notch deltas from
    /// smooth-scroll hardware are NOT dropped: they accumulate in
    /// `wheel_accum` and each event consumes only the delta its whole-pixel
    /// step accounts for, so a stream of tiny deltas (e.g. precision-touchpad
    /// `+6` ticks) still resizes once the banked delta reaches a whole pixel.
    /// The wheel's cursor position is tracked too, so a dirty region covers
    /// both the old and the new circle.
    pub fn on_wheel(&mut self, monitor: usize, at: Point, delta: i32) -> ModeEffect {
        // i64 math: delta * 10 fits easily. Bank the raw delta, then convert
        // the banked amount to whole pixels (truncating); the truncation
        // remainder stays banked for the next event. `WHEEL_DELTA /
        // RADIUS_STEP` == 12 exactly, so `step * 12` is exactly the delta the
        // applied step accounts for and the remainder is always < 12 raw
        // units — the accumulator can never grow unbounded or drift.
        self.wheel_accum += delta as i64;
        // Positive wheel deltas mean wheel up, which makes the spotlight
        // smaller. Negative deltas make it larger.
        let step = -(self.wheel_accum * RADIUS_STEP / WHEEL_DELTA) as i32;
        if step != 0 {
            self.wheel_accum -= step as i64 * (WHEEL_DELTA / RADIUS_STEP);
        }
        let new_radius =
            (self.radius as i32 + step).clamp(MIN_RADIUS as i32, MAX_RADIUS as i32) as u32;
        if new_radius == self.radius && monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old = (self.cursor_monitor, circle_bbox(self.cursor, self.radius));
        self.cursor = at;
        self.cursor_monitor = monitor;
        self.radius = new_radius;
        circle_repaint(old, (monitor, circle_bbox(at, new_radius)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- construction / state ------------------------------------------

    #[test]
    fn new_clamps_default_radius() {
        assert_eq!(SpotlightMode::new(5).radius(), MIN_RADIUS);
        assert_eq!(SpotlightMode::new(5000).radius(), MAX_RADIUS);
        assert_eq!(SpotlightMode::new(100).radius(), 100);
    }

    #[test]
    fn new_starts_cursor_at_origin_monitor_zero() {
        let m = SpotlightMode::new(100);
        assert_eq!(m.cursor(), Point::new(0, 0));
        assert_eq!(m.cursor_monitor(), 0);
    }

    // ---- mouse move ------------------------------------------------------

    #[test]
    fn mouse_move_same_position_is_noop() {
        let mut m = SpotlightMode::new(50);
        assert_eq!(m.on_mouse_move(0, Point::new(0, 0)), ModeEffect::none());
    }

    #[test]
    fn mouse_move_dirty_is_union_of_old_and_new_circle() {
        let mut m = SpotlightMode::new(50);
        // First move from the default (0,0) to (100,100): union of both
        // radius-50 circle bboxes = [-50,-50 .. 151,151).
        let e = m.on_mouse_move(0, Point::new(100, 100));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-50, -50, 201, 201)))],);
        // Second move to (110,100): union of circles at (100,100) and (110,100).
        let e = m.on_mouse_move(0, Point::new(110, 100));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(50, 50, 111, 101)))],);
    }

    #[test]
    fn mouse_move_to_other_monitor_repaints_both() {
        let mut m = SpotlightMode::new(20);
        m.on_mouse_move(0, Point::new(100, 100));
        let e = m.on_mouse_move(1, Point::new(30, 40));
        assert_eq!(
            e.repaint,
            vec![
                (0, Some(Rect::new(80, 80, 41, 41))),
                (1, Some(Rect::new(10, 20, 41, 41))),
            ],
        );
        assert_eq!(m.cursor_monitor(), 1);
        assert_eq!(m.cursor(), Point::new(30, 40));
    }

    // ---- wheel -----------------------------------------------------------

    #[test]
    fn wheel_resizes_120_delta_is_10px() {
        let mut m = SpotlightMode::new(100);
        // Union of the r=100 and r=90 circle bboxes at (0,0).
        let e = m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), 90);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-100, -100, 201, 201)))]);
        let e = m.on_wheel(0, Point::new(0, 0), -120);
        assert_eq!(m.radius(), 100);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-100, -100, 201, 201)))]);
    }

    #[test]
    fn wheel_multi_notch_and_fine_delta_scale_proportionally() {
        let mut m = SpotlightMode::new(100);
        m.on_wheel(0, Point::new(0, 0), 240);
        assert_eq!(m.radius(), 80);
        m.on_wheel(0, Point::new(0, 0), 60);
        assert_eq!(m.radius(), 75);
        m.on_wheel(0, Point::new(0, 0), -60);
        assert_eq!(m.radius(), 80);
    }

    #[test]
    fn wheel_sub_notch_deltas_still_resize() {
        // D2 regression: precision touchpads send sub-notch deltas (|delta| <
        // 120). Four +60 events MUST change the radius (-5 px each).
        let mut m = SpotlightMode::new(100);
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), 60);
        }
        assert_eq!(m.radius(), 80, "four +60 deltas = half a notch each pair");
        // And downwards.
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), -60);
        }
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_tiny_deltas_accumulate_to_whole_pixels() {
        // D2 regression: very fine deltas below one pixel per event (+6 raw =
        // 0.5 px) must NOT be dropped — they bank until a whole pixel exists.
        let mut m = SpotlightMode::new(100);
        m.on_wheel(0, Point::new(0, 0), 6);
        assert_eq!(m.radius(), 100, "first +6 banks 0.5 px: no change yet");
        m.on_wheel(0, Point::new(0, 0), 6);
        assert_eq!(m.radius(), 99, "two +6 events = one whole pixel smaller");
        // Twenty +6 events total = 120 raw = one notch = -10 px.
        for _ in 0..18 {
            m.on_wheel(0, Point::new(0, 0), 6);
        }
        assert_eq!(m.radius(), 90);
    }

    #[test]
    fn wheel_remainder_carries_across_events_without_drift() {
        // +130 twice = 260 raw = 21.67 px; Bresenham banking yields exactly
        // 21 px split 10 + 11 (the truncation remainder is never lost).
        let mut m = SpotlightMode::new(100);
        m.on_wheel(0, Point::new(0, 0), 130);
        assert_eq!(m.radius(), 90);
        m.on_wheel(0, Point::new(0, 0), 130);
        assert_eq!(m.radius(), 79);
        // A full notch immediately after still yields exactly +10 (no residue
        // distortion): 260 + 120 = 380 raw = 31.67 px → 131 total.
        m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), 69);
        // Direction reversal is symmetric: ±60 cancel exactly.
        let mut m = SpotlightMode::new(100);
        m.on_wheel(0, Point::new(0, 0), 60);
        m.on_wheel(0, Point::new(0, 0), -60);
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_clamps_at_min_and_max() {
        let mut m = SpotlightMode::new(MIN_RADIUS);
        let e = m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), MIN_RADIUS);
        assert_eq!(e, ModeEffect::none()); // clamped: nothing changed

        let mut m = SpotlightMode::new(MAX_RADIUS);
        let e = m.on_wheel(0, Point::new(0, 0), -120);
        assert_eq!(m.radius(), MAX_RADIUS);
        assert_eq!(e, ModeEffect::none());

        // A huge delta lands exactly on the clamp, not past it.
        let mut m = SpotlightMode::new(100);
        m.on_wheel(0, Point::new(0, 0), -120 * 1000);
        assert_eq!(m.radius(), MAX_RADIUS);
        m.on_wheel(0, Point::new(0, 0), 120 * 1000);
        assert_eq!(m.radius(), MIN_RADIUS);
    }

    #[test]
    fn wheel_tracks_cursor_and_covers_both_circles() {
        let mut m = SpotlightMode::new(50);
        m.on_mouse_move(0, Point::new(200, 200));
        // Wheel at a different position: cursor follows the wheel event.
        let e = m.on_wheel(0, Point::new(100, 100), 120);
        // Old: circle r=50 at (200,200); new: r=40 at (100,100).
        // Union: x/y from the new bbox (60,60), right/bottom from the old (251,251).
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(60, 60, 191, 191)))],);
    }
}
