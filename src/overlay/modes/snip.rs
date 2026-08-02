//! Snip LAYER: drag a rectangle on the frozen screen (Snip & Sketch style).
//! Pure state machine — the selection is painted by
//! [`crate::overlay::composite::compose_frame`] (which the controller feeds
//! with this layer's [`SnipMode::snip_selection`] via
//! [`crate::overlay::modes::ModeStack::render_state`]), and the actual
//! clipboard copy is done by the controller via
//! [`crate::overlay::composite::crop_normalized`] + Win32 clipboard.

use super::{ModeEffect, SnipSelection};
use crate::geometry::{Point, Rect};

/// Pixels the selection border extends OUTSIDE the selection rect when the
/// dirty region for a repaint is computed (must cover the painted border:
/// the composite draws 1 px outside + 1 px inside the rect edge).
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

/// Snip layer state: optional in-progress/finished drag.
///
/// Left button down starts a drag, move extends it, button up finishes it
/// (selection persists until a new drag starts, a full mode switch drops the
/// layer, or the overlay closes). The copy hotkey (default Ctrl+C) is handled
/// globally by the controller: it reads [`SnipMode::snip_selection`] — or,
/// when no selection exists, copies the whole frame of the monitor under the
/// cursor.
pub struct SnipMode {
    selection: Option<SnipSelection>,
    dragging: bool,
}

impl SnipMode {
    pub fn new() -> Self {
        Self {
            selection: None,
            dragging: false,
        }
    }

    /// The current selection, if any (monitor-local endpoints in any drag
    /// direction).
    pub fn snip_selection(&self) -> Option<SnipSelection> {
        self.selection
    }

    /// While dragging: extends the selection; requests repaint of the union of
    /// old + new selection regions. Otherwise a no-op effect.
    ///
    /// The drag stays on the monitor where the button went down
    /// ([`SnipSelection::monitor`]); moves on other monitors are ignored for
    /// the selection (a drag cannot span monitors).
    pub fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
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
        ModeEffect::repaint(
            monitor,
            Some(selection_dirty(rect_union(old_rect, new_rect))),
        )
    }

    /// Starts a new selection (replaces any existing one).
    ///
    /// The repaint covers the OLD selection's region: those pixels revert to
    /// their plain frame. The new selection starts zero-area (nothing to
    /// repaint yet).
    pub fn on_left_button_down(&mut self, monitor: usize, at: Point) -> ModeEffect {
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
    pub fn on_left_button_up(&mut self, monitor: usize, at: Point) -> ModeEffect {
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
            ModeEffect::repaint(
                monitor,
                Some(selection_dirty(rect_union(old_rect, new_rect))),
            )
        }
    }
}

impl Default for SnipMode {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: i32, y: i32) -> Point {
        Point::new(x, y)
    }

    // ---- state machine ---------------------------------------------------

    #[test]
    fn new_has_no_selection() {
        let m = SnipMode::new();
        assert_eq!(m.snip_selection(), None);
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
}
