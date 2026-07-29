//! Snip mode: drag a rectangle on the frozen screen (Snip & Sketch style).
//! Pure state machine — the actual clipboard copy is done by the controller
//! via [`crate::overlay::composite::crop_normalized`] + Win32 clipboard.

use super::{ModeEffect, ModeKind, OverlayMode, SnipSelection};
use crate::capture::DibBuffer;
use crate::hotkeys::gesture::Modifiers;
use crate::geometry::{Point, Rect};
use crate::overlay::composite::darken;

/// Pixels the selection border extends OUTSIDE the selection rect. The full
/// border is `BORDER_OUT + 1` px (one more ring just inside the edge) = 2 px.
const BORDER_OUT: i32 = 1;

/// Normalized rectangle between two drag endpoints given in ANY direction
/// (handles "negative drags"). Mirrors the `geometry::Rect::from_points`
/// contract, inlined from pub fields so this module stays independent of the
/// `Rect` helper methods.
fn norm_rect(a: Point, b: Point) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(x, y, (a.x.max(b.x) - x) as u32, (a.y.max(b.y) - y) as u32)
}

/// Zero area on either axis (geometry contract, inlined).
fn rect_is_empty(r: Rect) -> bool {
    r.width == 0 || r.height == 0
}

/// Overlap of `r` with the buffer bounds `0,0,width,height`; `None` when the
/// overlap is empty (geometry `intersection` contract, inlined).
fn clip_to_buffer(r: Rect, width: u32, height: u32) -> Option<Rect> {
    let x = r.x.max(0);
    let y = r.y.max(0);
    let right = (r.x + r.width as i32).min(width as i32);
    let bottom = (r.y + r.height as i32).min(height as i32);
    if right <= x || bottom <= y {
        None
    } else {
        Some(Rect::new(x, y, (right - x) as u32, (bottom - y) as u32))
    }
}

/// Smallest rect covering both `a` and `b`; an empty rect contributes nothing
/// (an empty selection renders nothing, so it needs no repaint either).
fn rect_union(a: Rect, b: Rect) -> Rect {
    if rect_is_empty(a) {
        return b;
    }
    if rect_is_empty(b) {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width as i32).max(b.x + b.width as i32);
    let bottom = (a.y + a.height as i32).max(b.y + b.height as i32);
    Rect::new(x, y, (right - x) as u32, (bottom - y) as u32)
}

/// Dirty region for a selection rect: the rect itself plus the outer border
/// ring. Unclipped — the controller clips dirty regions to the window.
fn selection_dirty(r: Rect) -> Rect {
    Rect::new(
        r.x - BORDER_OUT,
        r.y - BORDER_OUT,
        r.width + 2 * BORDER_OUT as u32,
        r.height + 2 * BORDER_OUT as u32,
    )
}

/// Copy the ORIGINAL pixels of `r` back over the darkened frame (row-slice
/// memcpy per row — one bounds check per row).
fn restore_rect(out: &mut DibBuffer, original: &DibBuffer, r: Rect) {
    let stride = out.stride as usize;
    let row_bytes = r.width as usize * 4;
    for y in r.y..(r.y + r.height as i32) {
        let i = y as usize * stride + r.x as usize * 4;
        out.pixels[i..i + row_bytes].copy_from_slice(&original.pixels[i..i + row_bytes]);
    }
}

/// Invert B/G/R of the pixel span `x0..=x1` on row `y` (alpha untouched).
/// Caller guarantees `y` is in bounds and the span is already x-clipped.
fn invert_span(buf: &mut DibBuffer, y: i32, x0: i32, x1: i32) {
    let stride = buf.stride as usize;
    let row = y as usize * stride;
    for x in x0..=x1 {
        let i = row + x as usize * 4;
        buf.pixels[i] = !buf.pixels[i];
        buf.pixels[i + 1] = !buf.pixels[i + 1];
        buf.pixels[i + 2] = !buf.pixels[i + 2];
    }
}

/// Draw the selection border as a 2 px ring (1 px outside + 1 px inside the
/// rect edge) by INVERTING the current frame pixels. Inversion contrasts with
/// both the darkened background and the restored original on any content,
/// needs no settings color, and costs O(perimeter).
fn draw_border(buf: &mut DibBuffer, rc: Rect) {
    let bw = buf.width as i32;
    let bh = buf.height as i32;
    let rright = rc.x + rc.width as i32;
    let rbottom = rc.y + rc.height as i32;
    // Clipped outer-ring bounds, inclusive.
    let ox0 = (rc.x - BORDER_OUT).max(0);
    let oy0 = (rc.y - BORDER_OUT).max(0);
    let ox1 = (rright + BORDER_OUT - 1).min(bw - 1);
    let oy1 = (rbottom + BORDER_OUT - 1).min(bh - 1);
    if ox0 > ox1 || oy0 > oy1 {
        return;
    }
    for y in oy0..=oy1 {
        // The two edge rows on each side (outer + inner ring) span the full
        // width; middle rows touch only the two side columns. For 1-px-wide
        // selections the side columns overlap into a full-width span.
        if y <= rc.y || y >= rbottom - 1 || rc.width == 1 {
            invert_span(buf, y, ox0, ox1);
        } else {
            invert_span(buf, y, ox0, rc.x);
            invert_span(buf, y, rright - 1, ox1);
        }
    }
}

/// Snip mode state: optional in-progress/finished drag.
///
/// Left button down starts a drag, move extends it, button up finishes it
/// (selection persists until a new drag starts or the overlay closes).
/// The copy hotkey (default Ctrl+C) is handled globally by the controller:
/// it reads [`OverlayMode::snip_selection`] — or, when no selection exists,
/// copies the whole frame of the monitor under the cursor.
pub struct SnipMode {
    selection: Option<SnipSelection>,
    dragging: bool,
    // Cursor tracking: kept truthful for the controller but never read back
    // by the mode itself (snip rendering depends only on the selection).
    #[allow(dead_code)]
    cursor: Point,
    #[allow(dead_code)]
    cursor_monitor: usize,
}

impl SnipMode {
    pub fn new() -> Self {
        Self {
            selection: None,
            dragging: false,
            cursor: Point::default(),
            cursor_monitor: 0,
        }
    }
}

impl Default for SnipMode {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayMode for SnipMode {
    fn kind(&self) -> ModeKind {
        ModeKind::Snip
    }

    /// While dragging: extends the selection; requests repaint of the union of
    /// old + new selection regions.
    ///
    /// The drag stays on the monitor where the button went down
    /// ([`SnipSelection::monitor`]); moves on other monitors are ignored for
    /// the selection (a drag cannot span monitors).
    fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        self.cursor = at;
        self.cursor_monitor = monitor;
        if !self.dragging {
            return ModeEffect::none();
        }
        let Some(sel) = self.selection.as_mut() else {
            return ModeEffect::none();
        };
        if sel.monitor != monitor || sel.b == at {
            return ModeEffect::none();
        }
        let old_rect = norm_rect(sel.a, sel.b);
        sel.b = at;
        let new_rect = norm_rect(sel.a, at);
        ModeEffect::repaint(monitor, Some(selection_dirty(rect_union(old_rect, new_rect))))
    }

    fn on_wheel(
        &mut self,
        _monitor: usize,
        _at: Point,
        _delta: i32,
        _modifiers: Modifiers,
    ) -> ModeEffect {
        ModeEffect::none()
    }

    /// Starts a new selection (replaces any existing one).
    ///
    /// The repaint covers the OLD selection's region: those pixels revert to
    /// their darkened frame. The new selection starts zero-area (nothing to
    /// repaint yet).
    fn on_left_button_down(&mut self, monitor: usize, at: Point) -> ModeEffect {
        self.cursor = at;
        self.cursor_monitor = monitor;
        self.dragging = true;
        let old = self.selection.replace(SnipSelection {
            monitor,
            a: at,
            b: at,
        });
        match old {
            Some(o) => {
                let r = norm_rect(o.a, o.b);
                if rect_is_empty(r) {
                    ModeEffect::none()
                } else {
                    ModeEffect::repaint(o.monitor, Some(selection_dirty(r)))
                }
            }
            None => ModeEffect::none(),
        }
    }

    /// Finishes the current drag. A zero-area drag clears the selection.
    fn on_left_button_up(&mut self, monitor: usize, at: Point) -> ModeEffect {
        self.cursor = at;
        self.cursor_monitor = monitor;
        if !self.dragging {
            return ModeEffect::none();
        }
        self.dragging = false;
        let Some(sel) = self.selection.as_mut() else {
            return ModeEffect::none();
        };
        if sel.monitor != monitor {
            // Released off the drag's monitor: finalize at the last tracked
            // point; discard if that left a zero-area selection.
            if rect_is_empty(norm_rect(sel.a, sel.b)) {
                self.selection = None;
            }
            return ModeEffect::none();
        }
        let old_rect = norm_rect(sel.a, sel.b);
        sel.b = at;
        let new_rect = norm_rect(sel.a, at);
        if rect_is_empty(new_rect) {
            // Zero-area drag (a plain click) clears the selection; repaint
            // whatever the previous frame of the drag had rendered.
            self.selection = None;
            if rect_is_empty(old_rect) {
                ModeEffect::none()
            } else {
                ModeEffect::repaint(monitor, Some(selection_dirty(old_rect)))
            }
        } else {
            ModeEffect::repaint(monitor, Some(selection_dirty(rect_union(old_rect, new_rect))))
        }
    }

    fn on_key(&mut self, _vk: u32, _modifiers: Modifiers) -> ModeEffect {
        ModeEffect::none()
    }

    fn snip_selection(&self) -> Option<SnipSelection> {
        self.selection
    }

    /// Darken everywhere EXCEPT the selected rectangle (selection shows the
    /// original pixels); draws nothing while no selection exists.
    ///
    /// A 2 px inversion border marks the selection edge: 1 px OUTSIDE the
    /// rect (inverted darkened pixels) + 1 px INSIDE (inverted original
    /// pixels) — see [`draw_border`] and [`BORDER_OUT`].
    fn render(&self, monitor: usize, original: &DibBuffer, out: &mut DibBuffer, dim_alpha: u8) {
        let same_dims = original.width == out.width
            && original.height == out.height
            && original.stride == out.stride
            && original.pixels.len() == out.pixels.len();
        if same_dims {
            out.pixels.copy_from_slice(&original.pixels);
        }
        darken(out, dim_alpha);
        let Some(sel) = self.selection else {
            return;
        };
        if sel.monitor != monitor || !same_dims {
            return;
        }
        let Some(rc) = clip_to_buffer(norm_rect(sel.a, sel.b), out.width, out.height) else {
            return; // empty or fully outside this monitor's buffer
        };
        restore_rect(out, original, rc);
        draw_border(out, rc);
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

    fn inverted(c: [u8; 4]) -> [u8; 4] {
        [!c[0], !c[1], !c[2], c[3]]
    }

    const COLOR: [u8; 4] = [200, 100, 50, 255];

    fn pt(x: i32, y: i32) -> Point {
        Point::new(x, y)
    }

    // ---- state machine ---------------------------------------------------

    #[test]
    fn new_has_no_selection() {
        let m = SnipMode::new();
        assert_eq!(m.snip_selection(), None);
        assert_eq!(m.kind(), ModeKind::Snip);
        let m2 = SnipMode::default();
        assert_eq!(m2.snip_selection(), None);
    }

    #[test]
    fn button_down_starts_zero_area_selection_without_repaint() {
        let mut m = SnipMode::new();
        let e = m.on_left_button_down(0, pt(3, 4));
        assert_eq!(e, ModeEffect::none());
        assert_eq!(
            m.snip_selection(),
            Some(SnipSelection {
                monitor: 0,
                a: pt(3, 4),
                b: pt(3, 4),
            })
        );
    }

    #[test]
    fn drag_extends_selection_and_unions_dirty_rects() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(4, 4));
        // First extension: old rect empty, new rect (4,4,2,2) + border.
        let e = m.on_mouse_move(0, pt(6, 6));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(3, 3, 4, 4)))]);
        // Second extension: union of (4,4,2,2) and (4,4,6,6) + border.
        let e = m.on_mouse_move(0, pt(10, 10));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(3, 3, 8, 8)))]);
        // Same point again: no-op.
        assert_eq!(m.on_mouse_move(0, pt(10, 10)), ModeEffect::none());
    }

    #[test]
    fn negative_drag_normalizes_endpoints() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(8, 8));
        m.on_mouse_move(0, pt(2, 3));
        m.on_left_button_up(0, pt(2, 3));
        let sel = m.snip_selection().unwrap();
        assert_eq!(sel.a, pt(8, 8));
        assert_eq!(sel.b, pt(2, 3));
        // Normalized rect: (2,3,6,5).
        assert_eq!(norm_rect(sel.a, sel.b), Rect::new(2, 3, 6, 5));
    }

    #[test]
    fn button_up_finishes_drag_selection_persists() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(1, 1));
        m.on_mouse_move(0, pt(5, 5));
        let e = m.on_left_button_up(0, pt(5, 5));
        // old (1,1,4,4) union new (1,1,4,4) + border.
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(0, 0, 6, 6)))]);
        assert!(m.snip_selection().is_some());
        // Moves after the drag no longer change anything.
        assert_eq!(m.on_mouse_move(0, pt(9, 9)), ModeEffect::none());
        assert_eq!(
            m.snip_selection(),
            Some(SnipSelection {
                monitor: 0,
                a: pt(1, 1),
                b: pt(5, 5),
            })
        );
    }

    #[test]
    fn zero_area_click_clears_selection() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(3, 3));
        let e = m.on_left_button_up(0, pt(3, 3));
        assert_eq!(e, ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
    }

    #[test]
    fn zero_area_click_clears_previous_selection_and_repaints_it() {
        let mut m = SnipMode::new();
        // Make a real selection first.
        m.on_left_button_down(0, pt(1, 1));
        m.on_mouse_move(0, pt(7, 7));
        m.on_left_button_up(0, pt(7, 7));
        assert!(m.snip_selection().is_some());
        // New button-down replaces it: repaint the old rect + border.
        let e = m.on_left_button_down(0, pt(20, 20));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(0, 0, 8, 8)))]);
        // Click (no drag) clears: repaint the zero-area frame -> nothing rendered.
        let e = m.on_left_button_up(0, pt(20, 20));
        assert_eq!(e, ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
    }

    #[test]
    fn drag_cannot_cross_monitors() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(2, 2));
        // Move on ANOTHER monitor while dragging: selection untouched.
        let e = m.on_mouse_move(1, pt(50, 50));
        assert_eq!(e, ModeEffect::none());
        assert_eq!(
            m.snip_selection(),
            Some(SnipSelection {
                monitor: 0,
                a: pt(2, 2),
                b: pt(2, 2),
            })
        );
        // Release on the other monitor: finalizes (zero-area -> discarded).
        let e = m.on_left_button_up(1, pt(50, 50));
        assert_eq!(e, ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
    }

    #[test]
    fn drag_released_off_monitor_keeps_nonzero_selection() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(2, 2));
        m.on_mouse_move(0, pt(6, 6));
        let e = m.on_left_button_up(1, pt(99, 99));
        assert_eq!(e, ModeEffect::none());
        // Selection finalizes at the last on-monitor point.
        assert_eq!(
            m.snip_selection(),
            Some(SnipSelection {
                monitor: 0,
                a: pt(2, 2),
                b: pt(6, 6),
            })
        );
    }

    #[test]
    fn new_drag_on_other_monitor_repaints_old_monitor() {
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(1, 1));
        m.on_mouse_move(0, pt(5, 5));
        m.on_left_button_up(0, pt(5, 5));
        let e = m.on_left_button_down(1, pt(3, 3));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(0, 0, 6, 6)))]);
        assert_eq!(m.snip_selection().unwrap().monitor, 1);
    }

    #[test]
    fn button_up_without_drag_is_noop() {
        let mut m = SnipMode::new();
        assert_eq!(m.on_left_button_up(0, pt(5, 5)), ModeEffect::none());
        assert_eq!(m.snip_selection(), None);
    }

    #[test]
    fn wheel_and_keys_are_noops() {
        let mut m = SnipMode::new();
        assert_eq!(m.on_wheel(0, pt(1, 1), 120, Modifiers::CTRL), ModeEffect::none());
        assert_eq!(m.on_key(0x1B, Modifiers::NONE), ModeEffect::none());
        assert_eq!(m.reset_view(), ModeEffect::none());
    }

    // ---- render ----------------------------------------------------------

    #[test]
    fn render_without_selection_darkens_everything() {
        let original = make_buf(8, 8, |_, _| COLOR);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let m = SnipMode::new();
        m.render(0, &original, &mut out, 128);
        let dim = dimmed(COLOR, 128);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&out, x, y), dim, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_with_selection_restores_inside_and_darkens_outside() {
        let original = make_buf(10, 10, |_, _| COLOR);
        let mut out = make_buf(10, 10, |_, _| [0, 0, 0, 0]);
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(2, 2));
        m.on_mouse_move(0, pt(6, 6));
        m.on_left_button_up(0, pt(6, 6));
        m.render(0, &original, &mut out, 128);

        let dim = dimmed(COLOR, 128);
        let inv_dim = inverted(dim);
        let inv_orig = inverted(COLOR);
        for y in 0..10i32 {
            for x in 0..10i32 {
                let got = px(&out, x as u32, y as u32);
                // Selection rect (2,2,4,4): pixels x,y in 2..6.
                let in_x = (2..6).contains(&x);
                let in_y = (2..6).contains(&y);
                // Inner border ring: outermost pixel layer of the rect.
                let on_inner_ring = in_x && in_y && (x == 2 || x == 5 || y == 2 || y == 5);
                // Outer border ring: 1 px frame around the rect.
                let on_outer_ring = (1..=6).contains(&x)
                    && (1..=6).contains(&y)
                    && !(in_x && in_y);
                let expect = if on_inner_ring {
                    inv_orig
                } else if on_outer_ring {
                    inv_dim
                } else if in_x && in_y {
                    COLOR
                } else {
                    dim
                };
                assert_eq!(got, expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_selection_on_other_monitor_leaves_this_one_darkened() {
        let original = make_buf(8, 8, |_, _| COLOR);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(1, 1));
        m.on_mouse_move(0, pt(5, 5));
        m.on_left_button_up(0, pt(5, 5));
        m.render(1, &original, &mut out, 128); // selection is on monitor 0
        let dim = dimmed(COLOR, 128);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&out, x, y), dim, "({x},{y})");
            }
        }
    }

    #[test]
    fn render_clips_selection_to_buffer_edge() {
        // Drag endpoints outside the buffer (mouse capture can deliver those):
        // selection (-3,-3)..(4,4) on a 8x8 buffer clips to (0,0,4,4).
        let original = make_buf(8, 8, |_, _| COLOR);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(-3, -3));
        m.on_mouse_move(0, pt(4, 4));
        m.on_left_button_up(0, pt(4, 4));
        m.render(0, &original, &mut out, 128);

        let dim = dimmed(COLOR, 128);
        // (0,0) is the clipped rect's corner -> inner border ring -> inverted original.
        assert_eq!(px(&out, 0, 0), inverted(COLOR));
        // (2,2) is interior -> original.
        assert_eq!(px(&out, 2, 2), COLOR);
        // (6,6) is outside -> darkened.
        assert_eq!(px(&out, 6, 6), dim);
        // No panic, every pixel written.
        assert_eq!(out.pixels.len(), 8 * 8 * 4);
    }

    #[test]
    fn render_degenerate_line_selection_draws_nothing() {
        // a and b share y: zero-height rect renders nothing (all darkened).
        let original = make_buf(8, 8, |_, _| COLOR);
        let mut out = make_buf(8, 8, |_, _| [0, 0, 0, 0]);
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(2, 4));
        m.on_mouse_move(0, pt(6, 4));
        // Line selections are non-empty while DRAGGING (norm_rect gives
        // zero height -> rect_is_empty -> dirty none) but finish as zero-area,
        // which clears on button up. Keep the drag unfinished to inspect.
        m.render(0, &original, &mut out, 128);
        let dim = dimmed(COLOR, 128);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&out, x, y), dim, "({x},{y})");
            }
        }
        // ...and finishing the drag clears it.
        m.on_left_button_up(0, pt(6, 4));
        assert_eq!(m.snip_selection(), None);
    }

    #[test]
    fn render_1px_selection_is_all_border() {
        // 1x1 selection: everything is border ring; must not panic or smear.
        let original = make_buf(6, 6, |_, _| COLOR);
        let mut out = make_buf(6, 6, |_, _| [0, 0, 0, 0]);
        let mut m = SnipMode::new();
        m.on_left_button_down(0, pt(2, 2));
        m.on_mouse_move(0, pt(3, 3));
        m.on_left_button_up(0, pt(3, 3));
        m.render(0, &original, &mut out, 128);
        // Center pixel: inner ring -> inverted original.
        assert_eq!(px(&out, 2, 2), inverted(COLOR));
        // Neighbor outside ring -> darkened.
        assert_eq!(px(&out, 0, 0), dimmed(COLOR, 128));
        // Ring pixel (1,2): outer ring -> inverted darkened.
        assert_eq!(px(&out, 1, 2), inverted(dimmed(COLOR, 128)));
    }
}
