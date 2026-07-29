//! Spotlight mode: the frozen screen is darkened except a clear circle around
//! the cursor. Pure state machine — rendering goes through
//! [`crate::overlay::composite::spotlight_hole`].

use super::{ModeEffect, ModeKind, OverlayMode};
use crate::capture::DibBuffer;
use crate::hotkeys::gesture::Modifiers;
use crate::geometry::{Point, Rect};
use crate::overlay::composite::{darken, spotlight_hole};

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

/// Spotlight mode state: cursor position + circle radius.
///
/// Wheel events resize the circle ONLY while `radius_modifier` (settings:
/// `hotkeys.spotlight_radius_modifier`, default Ctrl) is held; other wheel
/// events are ignored (no-op effect).
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
    radius_modifier: Modifiers,
    /// Unconsumed raw wheel delta (truncation remainder; |value| stays below
    /// `WHEEL_DELTA / RADIUS_STEP` after every applied resize).
    wheel_accum: i64,
}

impl SpotlightMode {
    /// `default_radius` in physical pixels (settings: `spotlight.default_radius`);
    /// `radius_modifier` = modifier that must be HELD for wheel resize.
    ///
    /// The radius is clamped to `10..=1000` px, the same range wheel resizing
    /// is clamped to, so a rogue settings value cannot break the invariant.
    /// A `radius_modifier` of [`Modifiers::NONE`] means "no modifier required":
    /// every wheel event resizes (bitflags `contains(NONE)` is always true).
    pub fn new(default_radius: u32, radius_modifier: Modifiers) -> Self {
        Self {
            cursor: Point::default(),
            cursor_monitor: 0,
            radius: default_radius.clamp(MIN_RADIUS, MAX_RADIUS),
            radius_modifier,
            wheel_accum: 0,
        }
    }

    /// Current circle radius in physical pixels (wheel-adjusted).
    pub fn radius(&self) -> u32 {
        self.radius
    }
}

impl OverlayMode for SpotlightMode {
    fn kind(&self) -> ModeKind {
        ModeKind::Spotlight
    }

    /// Tracks the cursor; requests a repaint of the hole's old + new regions.
    fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        if monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old = (self.cursor_monitor, circle_bbox(self.cursor, self.radius));
        self.cursor = at;
        self.cursor_monitor = monitor;
        circle_repaint(old, (monitor, circle_bbox(at, self.radius)))
    }

    /// Resizes the radius only while `radius_modifier` is held.
    ///
    /// `delta` is in RAW Win32 wheel units: `120` = `+10` px, proportionally
    /// (`60` = `+5`), clamped to `10..=1000`. Sub-notch deltas from
    /// smooth-scroll hardware are NOT dropped: they accumulate in
    /// `wheel_accum` and each event consumes only the delta its whole-pixel
    /// step accounts for, so a stream of tiny deltas (e.g. precision-touchpad
    /// `+6` ticks) still resizes once the banked delta reaches a whole pixel.
    /// The wheel's cursor position is tracked too, so a dirty region covers
    /// both the old and the new circle.
    fn on_wheel(
        &mut self,
        monitor: usize,
        at: Point,
        delta: i32,
        modifiers: Modifiers,
    ) -> ModeEffect {
        if !modifiers.contains(self.radius_modifier) {
            return ModeEffect::none();
        }
        // i64 math: delta * 10 fits easily. Bank the raw delta, then convert
        // the banked amount to whole pixels (truncating); the truncation
        // remainder stays banked for the next event. `WHEEL_DELTA /
        // RADIUS_STEP` == 12 exactly, so `step * 12` is exactly the delta the
        // applied step accounts for and the remainder is always < 12 raw
        // units — the accumulator can never grow unbounded or drift.
        self.wheel_accum += delta as i64;
        let step = (self.wheel_accum * RADIUS_STEP / WHEEL_DELTA) as i32;
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

    fn on_left_button_down(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
        ModeEffect::none()
    }

    fn on_left_button_up(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
        ModeEffect::none()
    }

    fn on_key(&mut self, _vk: u32, _modifiers: Modifiers) -> ModeEffect {
        ModeEffect::none()
    }

    /// Darken the original, then cut the original pixels back in inside the
    /// circle (only on the monitor where the cursor currently is).
    fn render(&self, monitor: usize, original: &DibBuffer, out: &mut DibBuffer, dim_alpha: u8) {
        // Contract: same dimensions. Guarded (not asserted) so a buggy caller
        // degrades to a plain darkened frame instead of panicking in release.
        let same_dims = original.width == out.width
            && original.height == out.height
            && original.stride == out.stride
            && original.pixels.len() == out.pixels.len();
        if same_dims {
            out.pixels.copy_from_slice(&original.pixels);
        }
        darken(out, dim_alpha);
        if same_dims && monitor == self.cursor_monitor {
            spotlight_hole(out, original, self.cursor, self.radius);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -------------------------------------------------------

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

    /// Darken contract math (mirrors composite::darken docs).
    fn dimmed(c: [u8; 4], dim_alpha: u8) -> [u8; 4] {
        let keep = 255 - dim_alpha as u32;
        [
            (c[0] as u32 * keep / 255) as u8,
            (c[1] as u32 * keep / 255) as u8,
            (c[2] as u32 * keep / 255) as u8,
            c[3],
        ]
    }

    const COLOR: [u8; 4] = [200, 100, 50, 255];

    // ---- construction / state ------------------------------------------

    #[test]
    fn new_clamps_default_radius() {
        assert_eq!(SpotlightMode::new(5, Modifiers::CTRL).radius(), MIN_RADIUS);
        assert_eq!(SpotlightMode::new(5000, Modifiers::CTRL).radius(), MAX_RADIUS);
        assert_eq!(SpotlightMode::new(100, Modifiers::CTRL).radius(), 100);
    }

    #[test]
    fn kind_is_spotlight() {
        assert_eq!(SpotlightMode::new(100, Modifiers::CTRL).kind(), ModeKind::Spotlight);
    }

    #[test]
    fn buttons_and_keys_are_noops() {
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        assert_eq!(m.on_left_button_down(0, Point::new(5, 5)), ModeEffect::none());
        assert_eq!(m.on_left_button_up(0, Point::new(5, 5)), ModeEffect::none());
        assert_eq!(m.on_key(0x1B, Modifiers::NONE), ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
        assert_eq!(m.reset_view(), ModeEffect::none());
    }

    // ---- mouse move ------------------------------------------------------

    #[test]
    fn mouse_move_same_position_is_noop() {
        let mut m = SpotlightMode::new(50, Modifiers::CTRL);
        assert_eq!(m.on_mouse_move(0, Point::new(0, 0)), ModeEffect::none());
    }

    #[test]
    fn mouse_move_dirty_is_union_of_old_and_new_circle() {
        let mut m = SpotlightMode::new(50, Modifiers::CTRL);
        // First move from the default (0,0) to (100,100): union of both
        // radius-50 circle bboxes = [-50,-50 .. 151,151).
        let e = m.on_mouse_move(0, Point::new(100, 100));
        assert_eq!(
            e.repaint,
            vec![(0, Some(Rect::new(-50, -50, 201, 201)))],
        );
        // Second move to (110,100): union of circles at (100,100) and (110,100).
        let e = m.on_mouse_move(0, Point::new(110, 100));
        assert_eq!(
            e.repaint,
            vec![(0, Some(Rect::new(50, 50, 111, 101)))],
        );
    }

    #[test]
    fn mouse_move_to_other_monitor_repaints_both() {
        let mut m = SpotlightMode::new(20, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(100, 100));
        let e = m.on_mouse_move(1, Point::new(30, 40));
        assert_eq!(
            e.repaint,
            vec![
                (0, Some(Rect::new(80, 80, 41, 41))),
                (1, Some(Rect::new(10, 20, 41, 41))),
            ],
        );
    }

    // ---- wheel -----------------------------------------------------------

    #[test]
    fn wheel_without_modifier_is_ignored() {
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(10, 10));
        let e = m.on_wheel(0, Point::new(10, 10), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none());
        assert_eq!(m.radius(), 100);
        // Shift is not Ctrl either.
        let e = m.on_wheel(0, Point::new(10, 10), 120, Modifiers::SHIFT);
        assert_eq!(e, ModeEffect::none());
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_with_modifier_resizes_120_delta_is_10px() {
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        // Union of the r=100 and r=110 circle bboxes at (0,0).
        let e = m.on_wheel(0, Point::new(0, 0), 120, Modifiers::CTRL);
        assert_eq!(m.radius(), 110);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-110, -110, 221, 221)))]);
        let e = m.on_wheel(0, Point::new(0, 0), -120, Modifiers::CTRL);
        assert_eq!(m.radius(), 100);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-110, -110, 221, 221)))]);
    }

    #[test]
    fn wheel_multi_notch_and_fine_delta_scale_proportionally() {
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 240, Modifiers::CTRL);
        assert_eq!(m.radius(), 120);
        m.on_wheel(0, Point::new(0, 0), 60, Modifiers::CTRL);
        assert_eq!(m.radius(), 125);
        m.on_wheel(0, Point::new(0, 0), -60, Modifiers::CTRL);
        assert_eq!(m.radius(), 120);
    }

    #[test]
    fn wheel_sub_notch_deltas_still_resize() {
        // D2 regression: precision touchpads send sub-notch deltas (|delta| <
        // 120). Four +60 events MUST change the radius (+5 px each).
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), 60, Modifiers::CTRL);
        }
        assert_eq!(m.radius(), 120, "four +60 deltas = half a notch each pair");
        // And downwards.
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), -60, Modifiers::CTRL);
        }
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_tiny_deltas_accumulate_to_whole_pixels() {
        // D2 regression: very fine deltas below one pixel per event (+6 raw =
        // 0.5 px) must NOT be dropped — they bank until a whole pixel exists.
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 6, Modifiers::CTRL);
        assert_eq!(m.radius(), 100, "first +6 banks 0.5 px: no change yet");
        m.on_wheel(0, Point::new(0, 0), 6, Modifiers::CTRL);
        assert_eq!(m.radius(), 101, "two +6 events = one whole pixel");
        // Twenty +6 events total = 120 raw = one notch = +10 px.
        for _ in 0..18 {
            m.on_wheel(0, Point::new(0, 0), 6, Modifiers::CTRL);
        }
        assert_eq!(m.radius(), 110);
    }

    #[test]
    fn wheel_remainder_carries_across_events_without_drift() {
        // +130 twice = 260 raw = 21.67 px; Bresenham banking yields exactly
        // 21 px split 10 + 11 (the truncation remainder is never lost).
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 130, Modifiers::CTRL);
        assert_eq!(m.radius(), 110);
        m.on_wheel(0, Point::new(0, 0), 130, Modifiers::CTRL);
        assert_eq!(m.radius(), 121);
        // A full notch immediately after still yields exactly +10 (no residue
        // distortion): 260 + 120 = 380 raw = 31.67 px → 131 total.
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::CTRL);
        assert_eq!(m.radius(), 131);
        // Direction reversal is symmetric: ±60 cancel exactly.
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 60, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), -60, Modifiers::CTRL);
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_without_modifier_does_not_bank_delta() {
        // Deltas arriving while the modifier is NOT held are ignored entirely
        // — they must not lurk in the accumulator for a later held event.
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 60, Modifiers::NONE);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::CTRL);
        assert_eq!(m.radius(), 110, "unheld +60 must not contribute");
    }

    #[test]
    fn wheel_clamps_at_min_and_max() {
        let mut m = SpotlightMode::new(MIN_RADIUS, Modifiers::CTRL);
        let e = m.on_wheel(0, Point::new(0, 0), -120, Modifiers::CTRL);
        assert_eq!(m.radius(), MIN_RADIUS);
        assert_eq!(e, ModeEffect::none()); // clamped: nothing changed

        let mut m = SpotlightMode::new(MAX_RADIUS, Modifiers::CTRL);
        let e = m.on_wheel(0, Point::new(0, 0), 120, Modifiers::CTRL);
        assert_eq!(m.radius(), MAX_RADIUS);
        assert_eq!(e, ModeEffect::none());

        // A huge delta lands exactly on the clamp, not past it.
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 120 * 1000, Modifiers::CTRL);
        assert_eq!(m.radius(), MAX_RADIUS);
        m.on_wheel(0, Point::new(0, 0), -120 * 1000, Modifiers::CTRL);
        assert_eq!(m.radius(), MIN_RADIUS);
    }

    #[test]
    fn wheel_with_extra_modifiers_still_resizes() {
        // Ctrl+Shift held while binding is Ctrl: contains() is a subset test.
        let mut m = SpotlightMode::new(100, Modifiers::CTRL);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(m.radius(), 110);
    }

    #[test]
    fn wheel_with_none_modifier_always_resizes() {
        let mut m = SpotlightMode::new(100, Modifiers::NONE);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        assert_eq!(m.radius(), 110);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::SHIFT);
        assert_eq!(m.radius(), 120);
    }

    #[test]
    fn wheel_tracks_cursor_and_covers_both_circles() {
        let mut m = SpotlightMode::new(50, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(200, 200));
        // Wheel at a different position: cursor follows the wheel event.
        let e = m.on_wheel(0, Point::new(100, 100), 120, Modifiers::CTRL);
        // Old: circle r=50 at (200,200); new: r=60 at (100,100).
        // Union: x/y from the new bbox (40,40), right/bottom from the old (251,251).
        assert_eq!(
            e.repaint,
            vec![(0, Some(Rect::new(40, 40, 211, 211)))],
        );
    }

    // ---- render ----------------------------------------------------------

    #[test]
    fn render_darkens_everywhere_except_circle() {
        let original = make_buf(40, 40, |_, _| COLOR);
        let mut out = make_buf(40, 40, |_, _| [0, 0, 0, 0]);
        let mut m = SpotlightMode::new(10, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(20, 20));
        m.render(0, &original, &mut out, 128);

        let dim = dimmed(COLOR, 128);
        for y in 0..40i32 {
            for x in 0..40i32 {
                let dx = x - 20;
                let dy = y - 20;
                let inside = dx * dx + dy * dy <= 100;
                let expect = if inside { COLOR } else { dim };
                assert_eq!(px(&out, x as u32, y as u32), expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_other_monitors_are_fully_darkened() {
        let original = make_buf(16, 16, |_, _| COLOR);
        let mut out = make_buf(16, 16, |_, _| [0, 0, 0, 0]);
        let mut m = SpotlightMode::new(10, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(8, 8));
        m.render(1, &original, &mut out, 128); // monitor 1: cursor is on 0
        let dim = dimmed(COLOR, 128);
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(px(&out, x, y), dim, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_pattern_preserves_exact_pixels_inside_hole() {
        // Non-uniform pattern: the hole must show the ORIGINAL bytes, and the
        // dimmed region must be the darkened pattern (not a flat fill).
        let pattern = |x: u32, y: u32| [(x * 3) as u8, (y * 5) as u8, (x + y) as u8, 255];
        let original = make_buf(30, 30, pattern);
        let mut out = make_buf(30, 30, |_, _| [0, 0, 0, 0]);
        let mut m = SpotlightMode::new(12, Modifiers::CTRL);
        m.on_mouse_move(0, Point::new(15, 15));
        m.render(0, &original, &mut out, 100);
        for y in 0..30i32 {
            for x in 0..30i32 {
                let dx = x - 15;
                let dy = y - 15;
                let c = pattern(x as u32, y as u32);
                let expect = if dx * dx + dy * dy <= 144 {
                    c
                } else {
                    dimmed(c, 100)
                };
                assert_eq!(px(&out, x as u32, y as u32), expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_dim_alpha_zero_leaves_original() {
        let original = make_buf(10, 10, |x, y| [x as u8, y as u8, 7, 255]);
        let mut out = make_buf(10, 10, |_, _| [1, 2, 3, 4]);
        let m = SpotlightMode::new(10, Modifiers::CTRL);
        m.render(0, &original, &mut out, 0);
        assert_eq!(out.pixels, original.pixels);
    }
}
