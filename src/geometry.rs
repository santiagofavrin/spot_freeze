//! Pure 2-D geometry shared by capture, overlay compositing, and modes.
//!
//! No `windows` imports — these types are constructed freely in headless tests.
//! All units are physical pixels. Whether a [`Point`]/[`Rect`] value is expressed
//! in virtual-screen or monitor-local coordinates is defined (and documented) by
//! the function that consumes it.

/// A 2-D point in physical pixels. Coordinate space is consumer-defined.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle: top-left corner + size.
///
/// `x`/`y` may be negative (virtual-screen coordinates of non-primary monitors
/// can be negative); `width`/`height` are unsigned physical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Build the normalized rectangle between two drag endpoints given in ANY
    /// order/direction — handles "negative drags" (`a` may be below or right of
    /// `b`). A zero-length axis yields a zero size on that axis.
    pub fn from_points(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self {
            x,
            y,
            width: a.x.abs_diff(b.x),
            height: a.y.abs_diff(b.y),
        }
    }

    /// Right edge, exclusive: `x + width`.
    pub fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// Bottom edge, exclusive: `y + height`.
    pub fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    /// `true` when the rect has zero area (`width == 0 || height == 0`).
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Inclusive of the left/top edges, exclusive of the right/bottom edges.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Overlap of two rects; `None` when they do not intersect. Touching edges
    /// count as empty overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        })
    }

    /// Smallest rect containing both rects. Coordinates may overflow only on
    /// absurd inputs; real monitor/dirty rects are far below the limits.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (headless-safe)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_spans_both_rects() {
        let a = Rect::new(-10, 5, 20, 10);
        let b = Rect::new(0, -5, 30, 10);
        assert_eq!(a.union(&b), Rect::new(-10, -5, 40, 20));
        assert_eq!(b.union(&a), a.union(&b), "commutative");
        assert_eq!(a.union(&a), a, "idempotent");
    }

    #[test]
    fn union_with_contained_rect_is_the_outer_rect() {
        let outer = Rect::new(0, 0, 100, 100);
        let inner = Rect::new(10, 10, 5, 5);
        assert_eq!(outer.union(&inner), outer);
        assert_eq!(inner.union(&outer), outer);
    }

    // -- from_points -----------------------------------------------------------

    #[test]
    fn from_points_normalizes_any_drag_direction() {
        let a = Point::new(10, 20);
        let b = Point::new(30, 50);
        let want = Rect::new(10, 20, 20, 30);
        assert_eq!(Rect::from_points(a, b), want);
        assert_eq!(Rect::from_points(b, a), want, "reversed drag");
        // Mixed directions: a is right of / below b on one axis only.
        assert_eq!(
            Rect::from_points(Point::new(30, 20), Point::new(10, 50)),
            want
        );
        assert_eq!(
            Rect::from_points(Point::new(10, 50), Point::new(30, 20)),
            want
        );
    }

    #[test]
    fn from_points_negative_coords_and_zero_axes() {
        // Negative drag endpoints (non-primary monitor space).
        assert_eq!(
            Rect::from_points(Point::new(-30, -20), Point::new(-10, -5)),
            Rect::new(-30, -20, 20, 15)
        );
        // Zero-length axis yields zero size on that axis.
        assert_eq!(
            Rect::from_points(Point::new(5, 5), Point::new(5, 9)),
            Rect::new(5, 5, 0, 4)
        );
        assert_eq!(
            Rect::from_points(Point::new(7, 3), Point::new(1, 3)),
            Rect::new(1, 3, 6, 0)
        );
        // Identical points → empty rect at that point.
        assert_eq!(
            Rect::from_points(Point::new(4, 4), Point::new(4, 4)),
            Rect::new(4, 4, 0, 0)
        );
    }

    // -- right / bottom (exclusive) ---------------------------------------------

    #[test]
    fn right_and_bottom_are_exclusive_edges() {
        let r = Rect::new(10, 20, 30, 40);
        assert_eq!(r.right(), 40);
        assert_eq!(r.bottom(), 60);
        // Zero-size rect: right == x, bottom == y.
        let z = Rect::new(10, 20, 0, 0);
        assert_eq!(z.right(), 10);
        assert_eq!(z.bottom(), 20);
    }

    #[test]
    fn right_and_bottom_negative_coord_safe() {
        let r = Rect::new(-100, -50, 60, 30);
        assert_eq!(r.right(), -40);
        assert_eq!(r.bottom(), -20);
        // Rect entirely in negative space.
        let n = Rect::new(-100, -50, 20, 20);
        assert_eq!(n.right(), -80);
        assert_eq!(n.bottom(), -30);
    }

    // -- is_empty ----------------------------------------------------------------

    #[test]
    fn is_empty_means_zero_area() {
        assert!(Rect::new(0, 0, 0, 0).is_empty());
        assert!(Rect::new(5, 5, 0, 10).is_empty());
        assert!(Rect::new(5, 5, 10, 0).is_empty());
        assert!(!Rect::new(5, 5, 1, 1).is_empty());
        assert!(!Rect::new(-5, -5, 10, 10).is_empty());
    }

    // -- contains ------------------------------------------------------------------

    #[test]
    fn contains_is_left_top_inclusive_right_bottom_exclusive() {
        let r = Rect::new(10, 20, 30, 40); // [10,40) x [20,60)
        assert!(r.contains(Point::new(10, 20)), "top-left corner inclusive");
        assert!(r.contains(Point::new(39, 59)), "last inside pixel");
        assert!(!r.contains(Point::new(40, 60)), "right/bottom exclusive");
        assert!(!r.contains(Point::new(40, 20)), "right edge exclusive");
        assert!(!r.contains(Point::new(10, 60)), "bottom edge exclusive");
        assert!(!r.contains(Point::new(9, 20)), "left of rect");
        assert!(!r.contains(Point::new(10, 19)), "above rect");
        // Empty rect contains nothing, not even its own origin.
        assert!(!Rect::new(10, 20, 0, 0).contains(Point::new(10, 20)));
    }

    #[test]
    fn contains_negative_coord_safe() {
        let r = Rect::new(-50, -50, 20, 20); // [-50,-30) x [-50,-30)
        assert!(r.contains(Point::new(-50, -50)));
        assert!(r.contains(Point::new(-31, -31)));
        assert!(!r.contains(Point::new(-30, -50)));
        assert!(!r.contains(Point::new(0, 0)));
    }

    // -- intersection --------------------------------------------------------------

    #[test]
    fn intersection_of_overlapping_rects() {
        let a = Rect::new(0, 0, 20, 20);
        let b = Rect::new(10, 5, 20, 20);
        assert_eq!(a.intersection(&b), Some(Rect::new(10, 5, 10, 15)));
        // Commutative.
        assert_eq!(b.intersection(&a), Some(Rect::new(10, 5, 10, 15)));
        // Full containment.
        let inner = Rect::new(5, 5, 4, 4);
        assert_eq!(a.intersection(&inner), Some(inner));
        assert_eq!(a.intersection(&a), Some(a));
    }

    #[test]
    fn intersection_none_when_disjoint_or_touching() {
        let a = Rect::new(0, 0, 10, 10);
        // Disjoint on x and on y.
        assert_eq!(a.intersection(&Rect::new(20, 0, 5, 5)), None);
        assert_eq!(a.intersection(&Rect::new(0, 20, 5, 5)), None);
        // Touching edges count as empty overlap.
        assert_eq!(
            a.intersection(&Rect::new(10, 0, 5, 5)),
            None,
            "touch right edge"
        );
        assert_eq!(
            a.intersection(&Rect::new(0, 10, 5, 5)),
            None,
            "touch bottom edge"
        );
        assert_eq!(
            a.intersection(&Rect::new(-5, 0, 5, 5)),
            None,
            "touch left edge"
        );
        // Empty rects intersect with nothing.
        assert_eq!(a.intersection(&Rect::new(2, 2, 0, 0)), None);
        assert_eq!(Rect::new(2, 2, 0, 0).intersection(&a), None);
    }

    #[test]
    fn intersection_negative_coord_safe() {
        // Monitor left of primary: x in [-1920, 0).
        let mon = Rect::new(-1920, 0, 1920, 1080);
        let partial = Rect::new(-100, 500, 200, 200); // straddles x = 0
        assert_eq!(
            mon.intersection(&partial),
            Some(Rect::new(-100, 500, 100, 200))
        );
        let fully_inside = Rect::new(-1000, 100, 50, 50);
        assert_eq!(mon.intersection(&fully_inside), Some(fully_inside));
        assert_eq!(
            mon.intersection(&Rect::new(0, 0, 100, 100)),
            None,
            "touch at x=0"
        );
    }
}
