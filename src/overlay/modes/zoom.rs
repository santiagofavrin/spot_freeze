//! Zoom mode: magnify the frozen screen around the cursor. Pure state machine —
//! resampling goes through [`crate::overlay::composite::zoom_resample`].

use super::{ModeEffect, ModeKind, OverlayMode};
use crate::capture::DibBuffer;
use crate::hotkeys::gesture::Modifiers;
use crate::geometry::{Point, Rect};
use crate::overlay::composite::{darken, zoom_resample, ZoomFilter};

/// Win32 `WHEEL_DELTA`: one wheel notch.
const WHEEL_DELTA: f32 = 120.0;
/// Fallback when settings pass a `step_factor <= 1.0` (config bug guard).
const DEFAULT_STEP_FACTOR: f32 = 1.25;

/// Zoom mode state: cursor position + current magnification.
///
/// Wheel events multiply/divide the zoom by `step_factor` (settings:
/// `zoom.step_factor`), clamped to `[min, max]`. The reset-view hotkey restores
/// 1.0. Monitors other than the cursor's monitor keep their plain darkened
/// frame.
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
/// its darkened frame, the new one becomes the zoomed view).
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

/// Copy `src` into `dst`. Dimensions match by contract; on mismatch (caller
/// bug) copy the common rows instead of panicking in release builds.
fn copy_into(src: &DibBuffer, dst: &mut DibBuffer) {
    if src.width == dst.width
        && src.height == dst.height
        && src.stride == dst.stride
        && src.pixels.len() == dst.pixels.len()
    {
        dst.pixels.copy_from_slice(&src.pixels);
    } else {
        let rows = src.height.min(dst.height) as usize;
        let row_bytes = (src.stride.min(dst.stride)) as usize;
        for y in 0..rows {
            let s = y * src.stride as usize;
            let d = y * dst.stride as usize;
            dst.pixels[d..d + row_bytes].copy_from_slice(&src.pixels[s..s + row_bytes]);
        }
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
}

impl OverlayMode for ZoomMode {
    fn kind(&self) -> ModeKind {
        ModeKind::Zoom
    }

    /// Tracks the cursor; the zoomed view recenters on it.
    fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
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
    /// looking at. `modifiers` are ignored — zooming needs no chord.
    fn on_wheel(
        &mut self,
        monitor: usize,
        at: Point,
        delta: i32,
        _modifiers: Modifiers,
    ) -> ModeEffect {
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

    fn on_left_button_down(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
        ModeEffect::none()
    }

    fn on_left_button_up(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
        ModeEffect::none()
    }

    fn on_key(&mut self, _vk: u32, _modifiers: Modifiers) -> ModeEffect {
        ModeEffect::none()
    }

    /// Zoom = 1.0 again; repaints the cursor monitor.
    fn reset_view(&mut self) -> ModeEffect {
        self.zoom = 1.0;
        ModeEffect::repaint(self.cursor_monitor, None)
    }

    /// Cursor monitor: `zoom_resample` of the original (undarkened) around the
    /// cursor; other monitors: plain darkened original.
    fn render(&self, monitor: usize, original: &DibBuffer, out: &mut DibBuffer, dim_alpha: u8) {
        if monitor == self.cursor_monitor {
            let viewport = Rect::new(0, 0, original.width, original.height);
            // Nearest is the default filter: zero interpolation cost.
            let resampled =
                zoom_resample(original, viewport, self.zoom, self.cursor, ZoomFilter::Nearest);
            copy_into(&resampled, out);
        } else {
            copy_into(original, out);
            darken(out, dim_alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -------------------------------------------------------

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

    fn assert_zoom_near(mode: &ZoomMode, expected: f32) {
        assert!(
            (mode.zoom() - expected).abs() < 1e-6,
            "zoom {} vs expected {expected}",
            mode.zoom()
        );
    }

    // ---- construction ----------------------------------------------------

    #[test]
    fn new_starts_at_1x() {
        let m = ZoomMode::new(1.25, 1.0, 10.0);
        assert_eq!(m.zoom(), 1.0);
        assert_eq!(m.kind(), ModeKind::Zoom);
    }

    #[test]
    fn new_normalizes_invalid_config() {
        // step_factor <= 1.0 falls back so the wheel can only zoom IN per notch.
        let mut m = ZoomMode::new(0.5, 1.0, 10.0);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        assert!(m.zoom() > 1.0);
        // max < min is repaired instead of panicking in clamp.
        let mut m = ZoomMode::new(1.5, 4.0, 2.0);
        for _ in 0..50 {
            m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        }
        assert!(m.zoom() >= 4.0);
        // min < 1.0 is floored to 1.0: zooming out stops at 1x.
        let mut m = ZoomMode::new(1.5, 0.5, 10.0);
        for _ in 0..50 {
            m.on_wheel(0, Point::new(0, 0), -120, Modifiers::NONE);
        }
        assert_eq!(m.zoom(), 1.0);
    }

    // ---- wheel zoom -------------------------------------------------------

    #[test]
    fn wheel_multiplies_and_divides_by_step_factor() {
        let mut m = ZoomMode::new(1.25, 1.0, 100.0);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        assert_zoom_near(&m, 1.25);
        m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        assert_zoom_near(&m, 1.5625);
        m.on_wheel(0, Point::new(0, 0), -120, Modifiers::NONE);
        assert_zoom_near(&m, 1.25);
        m.on_wheel(0, Point::new(0, 0), -120, Modifiers::NONE);
        assert_zoom_near(&m, 1.0);
    }

    #[test]
    fn wheel_two_notches_apply_step_squared() {
        let mut m = ZoomMode::new(2.0, 1.0, 100.0);
        m.on_wheel(0, Point::new(0, 0), 240, Modifiers::NONE);
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
            m.on_wheel(0, Point::new(0, 0), 60, Modifiers::NONE);
            assert!(m.zoom() > prev, "every +60 event must zoom in");
            prev = m.zoom();
        }
        assert_zoom_near(&m, 1.5625); // 1.25^2
        // Same smoothness on the way out.
        m.on_wheel(0, Point::new(0, 0), -60, Modifiers::NONE);
        assert!(m.zoom() < prev);
    }

    #[test]
    fn wheel_clamps_at_min_and_max() {
        let mut m = ZoomMode::new(1.5, 1.0, 3.0);
        for _ in 0..20 {
            m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        }
        assert_eq!(m.zoom(), 3.0);
        // Further wheel-up at the clamp is a no-op effect.
        let e = m.on_wheel(0, Point::new(0, 0), 120, Modifiers::NONE);
        assert_eq!(e, ModeEffect::none());
        for _ in 0..20 {
            m.on_wheel(0, Point::new(0, 0), -120, Modifiers::NONE);
        }
        assert_eq!(m.zoom(), 1.0);
    }

    #[test]
    fn wheel_reports_full_repaint_of_cursor_monitor() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(30, 30));
        let e = m.on_wheel(0, Point::new(30, 30), 120, Modifiers::NONE);
        assert_eq!(e.repaint, vec![(0, None)]);
    }

    #[test]
    fn wheel_on_other_monitor_moves_focus_and_repaints_both() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(10, 10));
        let e = m.on_wheel(1, Point::new(50, 50), 120, Modifiers::NONE);
        assert_eq!(e.repaint, vec![(0, None), (1, None)]);
        // Rendering now follows monitor 1.
        let original = make_buf(4, 4, |_, _| COLOR);
        let mut out = make_buf(4, 4, |_, _| [0, 0, 0, 0]);
        m.render(0, &original, &mut out, 128);
        assert_eq!(px(&out, 0, 0), dimmed(COLOR, 128)); // monitor 0: darkened
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

    // ---- reset / misc events ----------------------------------------------

    #[test]
    fn reset_view_restores_1x_and_repaints_cursor_monitor() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(1, Point::new(5, 5));
        m.on_wheel(1, Point::new(5, 5), 240, Modifiers::NONE);
        assert!(m.zoom() > 1.0);
        let e = m.reset_view();
        assert_eq!(m.zoom(), 1.0);
        assert_eq!(e.repaint, vec![(1, None)]);
    }

    #[test]
    fn reset_goes_through_reset_view_not_key_events() {
        // D1 regression: the reset-zoom hotkey must reach the mode via
        // `reset_view`. The OLD app path synthesized a KeyDown with the
        // freeze-time binding (default `0`) — and `on_key` is a documented
        // no-op, so the hotkey was dead. Pin both halves of that contract:
        // the synthesized-key path stays inert, and `reset_view` actually
        // resets and yields the repaint the controller presents.
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(7, 7));
        m.on_wheel(0, Point::new(7, 7), 120, Modifiers::NONE);
        assert!(m.zoom() > 1.0);
        let via_key = m.on_key(b'0' as u32, Modifiers::NONE);
        assert_eq!(via_key, ModeEffect::none());
        assert!(m.zoom() > 1.0, "a synthesized key event must NOT reset zoom");
        let via_reset = m.reset_view();
        assert_eq!(m.zoom(), 1.0, "reset_view is the live reset path");
        assert_eq!(via_reset.repaint, vec![(0, None)]);
    }

    #[test]
    fn buttons_and_keys_are_noops() {
        let mut m = ZoomMode::new(1.25, 1.0, 10.0);
        assert_eq!(m.on_left_button_down(0, Point::new(1, 1)), ModeEffect::none());
        assert_eq!(m.on_left_button_up(0, Point::new(1, 1)), ModeEffect::none());
        assert_eq!(m.on_key(b'0' as u32, Modifiers::NONE), ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
    }

    // ---- render -----------------------------------------------------------

    #[test]
    fn render_cursor_monitor_is_undarkened_resample() {
        // Uniform content: every sampling kernel/mapping yields the same
        // color, so this holds regardless of composite rounding details.
        let original = make_buf(16, 16, |_, _| COLOR);
        let mut out = make_buf(16, 16, |_, _| [0, 0, 0, 0]);
        let mut m = ZoomMode::new(2.0, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(8, 8));
        m.on_wheel(0, Point::new(8, 8), 120, Modifiers::NONE); // zoom = 2.0
        m.render(0, &original, &mut out, 200); // dim_alpha ignored on cursor monitor
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(px(&out, x, y), COLOR, "({x},{y}) — must NOT be darkened");
            }
        }
    }

    #[test]
    fn render_at_1x_with_centered_cursor_is_pixel_exact() {
        // Even dims + focus at exact center + zoom 1.0: composite guarantees
        // an identity copy — and it must be UNDARKENED.
        let pattern = |x: u32, y: u32| [(x * 7 + y) as u8, (y * 5 + x) as u8, ((x + y) * 3) as u8, 255];
        let original = make_buf(8, 6, pattern);
        let mut out = make_buf(8, 6, |_, _| [9, 9, 9, 9]);
        let mut m = ZoomMode::new(1.5, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(4, 3)); // exact center of 8x6
        m.render(0, &original, &mut out, 128);
        assert_eq!(out.pixels, original.pixels);
    }

    #[test]
    fn render_2x_nearest_matches_composite_mapping() {
        // 8x8 original, focus (4,4), zoom 2.0 -> source region [2,6) x [2,6),
        // each source pixel -> 2x2 output block (composite mapping:
        // src = focus + (o + 0.5 - viewport/2)/zoom - 0.5, rounded).
        let pattern = |x: u32, y: u32| [(x * 30) as u8, (y * 60) as u8, (x * 3 + y) as u8, 255];
        let original = make_buf(8, 8, pattern);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let mut m = ZoomMode::new(2.0, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(4, 4));
        m.on_wheel(0, Point::new(4, 4), 120, Modifiers::NONE);
        m.render(0, &original, &mut out, 0);
        for oy in 0..8u32 {
            for ox in 0..8u32 {
                let sx = (4.0f32 + (ox as f32 + 0.5 - 4.0) / 2.0 - 0.5).round() as u32;
                let sy = (4.0f32 + (oy as f32 + 0.5 - 4.0) / 2.0 - 0.5).round() as u32;
                assert_eq!(px(&out, ox, oy), pattern(sx, sy), "({ox},{oy})");
            }
        }
    }

    #[test]
    fn render_other_monitors_darkened_only() {
        let original = make_buf(8, 8, |_, _| COLOR);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let mut m = ZoomMode::new(2.0, 1.0, 10.0);
        m.on_mouse_move(0, Point::new(4, 4));
        m.on_wheel(0, Point::new(4, 4), 120, Modifiers::NONE);
        m.render(1, &original, &mut out, 128); // monitor 1 is not the cursor monitor
        let dim = dimmed(COLOR, 128);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&out, x, y), dim, "({x},{y})");
            }
        }
    }
}
