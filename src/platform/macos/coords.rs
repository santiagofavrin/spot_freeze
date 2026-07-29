//! Coordinate conversions between AppKit's coordinate spaces and the app's
//! crate-wide spaces.
//!
//! The two spaces and their differences:
//!
//! - **Cocoa global points** (`NSScreen.frame`, `NSEvent.mouseLocation`,
//!   `NSWindow` frames): origin at the PRIMARY screen's **bottom-left**
//!   corner, y increasing **up**, unit = points (1 point =
//!   `backingScaleFactor` physical pixels per screen).
//! - **App virtual-screen physical pixels** ([`MonitorInfo`].rect,
//!   [`OverlayEvent`] coordinates, [`DibBuffer`] pixels): origin at the
//!   primary screen's **top-left**, y increasing **down**, unit = physical
//!   pixels.
//!
//! Flipping y needs the primary screen's height in POINTS: the Cocoa global
//! space puts the primary screen at `y ∈ [0, primary_height]`, so a Cocoa y
//! maps to virtual `primary_height − y` (scaled). Screens above the primary
//! (Cocoa y > primary_height) land at negative virtual y, matching the
//! crate's virtual-screen contract.
//!
//! Pure module (no OS imports): `NSRect`/`NSPoint` fields are copied into the
//! plain `f64` structs below at the call sites, so all math is unit-testable
//! headless. Conversions round (`x.round()`); point values produced by the
//! window system are integral on exact scales (1.0, 2.0) and within half a
//! pixel otherwise.
//!
//! [`MonitorInfo`]: crate::capture::MonitorInfo
//! [`OverlayEvent`]: crate::overlay::events::OverlayEvent
//! [`DibBuffer`]: crate::capture::DibBuffer

use crate::geometry::{Point, Rect};

/// Rectangle in AppKit coordinates (points, bottom-left origin, y up).
/// Plain-data mirror of `NSRect` so this module stays OS-free.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CocoaRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CocoaRect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Inclusive of the left/bottom edges, exclusive of the right/top edges
    /// (same convention as [`Rect::contains`]).
    pub fn contains(&self, p: CocoaPoint) -> bool {
        p.x >= self.x && p.x < self.x + self.width && p.y >= self.y && p.y < self.y + self.height
    }
}

/// Point in AppKit coordinates (points). Plain-data mirror of `NSPoint`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CocoaPoint {
    pub x: f64,
    pub y: f64,
}

impl CocoaPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Round to the nearest physical pixel (half away from zero, like the
/// display hardware's own point→pixel mapping).
fn px(points: f64) -> i32 {
    points.round() as i32
}

/// A screen's frame in Cocoa global points → its virtual-screen rect in
/// physical pixels. `primary_height` is the PRIMARY screen's frame height in
/// points (the y-flip reference); `scale` is the screen's backingScaleFactor.
pub(crate) fn cocoa_rect_to_virtual(frame: CocoaRect, scale: f64, primary_height: f64) -> Rect {
    Rect {
        x: px(frame.x * scale),
        y: px((primary_height - (frame.y + frame.height)) * scale),
        width: px(frame.width * scale).max(0) as u32,
        height: px(frame.height * scale).max(0) as u32,
    }
}

/// A Cocoa global point (e.g. `NSEvent.mouseLocation`) → virtual-screen
/// physical pixels, using the scale of the screen the point sits on.
pub(crate) fn cocoa_point_to_virtual(p: CocoaPoint, scale: f64, primary_height: f64) -> Point {
    Point {
        x: px(p.x * scale),
        y: px((primary_height - p.y) * scale),
    }
}

/// A point in an UNFLIPPED view's local coordinates (bottom-left origin,
/// points — this is what `NSEvent.locationInWindow` yields for a content
/// view filling the window) → monitor-local physical pixels (top-left
/// origin), the [`OverlayEvent`] coordinate space.
///
/// [`OverlayEvent`]: crate::overlay::events::OverlayEvent
pub(crate) fn view_point_to_monitor_local(p: CocoaPoint, view_height: f64, scale: f64) -> Point {
    Point {
        x: px(p.x * scale),
        y: px((view_height - p.y) * scale),
    }
}

/// A monitor-local physical-pixel rect (e.g. a present() dirty region) →
/// the UNFLIPPED view points rect to pass to `setNeedsDisplayInRect:`.
/// Rows `[y, y+height)` measured from the top map to view y
/// `view_height − (y + height)/scale … view_height − y/scale`.
pub(crate) fn monitor_local_to_view_rect(local: Rect, view_height: f64, scale: f64) -> CocoaRect {
    CocoaRect {
        x: local.x as f64 / scale,
        y: view_height - (local.y + local.height as i32) as f64 / scale,
        width: local.width as f64 / scale,
        height: local.height as f64 / scale,
    }
}

#[cfg(test)]
mod tests {
    //! Headless: pure f64 math, no AppKit objects.
    use super::*;

    const H: f64 = 1080.0; // primary screen height in points

    // -- cocoa_rect_to_virtual ------------------------------------------------

    #[test]
    fn primary_screen_maps_to_origin() {
        let frame = CocoaRect::new(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(
            cocoa_rect_to_virtual(frame, 1.0, H),
            Rect::new(0, 0, 1920, 1080)
        );
    }

    #[test]
    fn retina_scale_doubles_pixels() {
        // 1440x900pt @2x = 2880x1800px.
        let frame = CocoaRect::new(0.0, 0.0, 1440.0, 900.0);
        assert_eq!(
            cocoa_rect_to_virtual(frame, 2.0, 900.0),
            Rect::new(0, 0, 2880, 1800)
        );
    }

    #[test]
    fn screen_left_of_primary_gets_negative_x() {
        let frame = CocoaRect::new(-1440.0, 0.0, 1440.0, 900.0);
        assert_eq!(
            cocoa_rect_to_virtual(frame, 2.0, 900.0),
            Rect::new(-2880, 0, 2880, 1800)
        );
    }

    #[test]
    fn screen_above_primary_gets_negative_y() {
        // Stacked on top of the primary: Cocoa y = primary height.
        let frame = CocoaRect::new(0.0, 1080.0, 1920.0, 1080.0);
        assert_eq!(
            cocoa_rect_to_virtual(frame, 1.0, H),
            Rect::new(0, -1080, 1920, 1080)
        );
    }

    #[test]
    fn screen_below_primary_gets_positive_y() {
        let frame = CocoaRect::new(0.0, -900.0, 1440.0, 900.0);
        assert_eq!(
            cocoa_rect_to_virtual(frame, 1.0, H),
            Rect::new(0, 1080, 1440, 900)
        );
    }

    #[test]
    fn rect_conversion_is_self_consistent_across_layouts() {
        // For these frames, converting to virtual and then applying the
        // documented inverse formula by hand lands back on the frame.
        for (frame, scale) in [
            (CocoaRect::new(0.0, 0.0, 1920.0, 1080.0), 1.0),
            (CocoaRect::new(-1440.0, 100.0, 1440.0, 900.0), 2.0),
            (CocoaRect::new(300.0, -500.0, 1024.0, 640.0), 1.5),
        ] {
            let h = 1080.0;
            let v = cocoa_rect_to_virtual(frame, scale, h);
            // Inverse of cocoa_rect_to_virtual, computed inline (kept in sync
            // with the module docs' formula).
            let back = CocoaRect::new(
                v.x as f64 / scale,
                h - (v.y + v.height as i32) as f64 / scale,
                v.width as f64 / scale,
                v.height as f64 / scale,
            );
            // Integral scales round-trip exactly; 1.5 stays within half a px.
            let tol = 0.5 / scale + 1e-9;
            assert!((back.x - frame.x).abs() <= tol, "x: {back:?} vs {frame:?}");
            assert!((back.y - frame.y).abs() <= tol, "y: {back:?} vs {frame:?}");
            assert!(
                (back.width - frame.width).abs() <= tol,
                "w: {back:?} vs {frame:?}"
            );
            assert!(
                (back.height - frame.height).abs() <= tol,
                "h: {back:?} vs {frame:?}"
            );
        }
    }

    // -- cocoa_point_to_virtual ------------------------------------------------

    #[test]
    fn point_conversion_flips_y() {
        assert_eq!(
            cocoa_point_to_virtual(CocoaPoint::new(10.0, 20.0), 1.0, H),
            Point::new(10, 1060)
        );
        // Primary screen's bottom-left corner is its virtual bottom edge.
        assert_eq!(
            cocoa_point_to_virtual(CocoaPoint::new(0.0, 0.0), 1.0, H),
            Point::new(0, 1080)
        );
        // Retina: points scale up.
        assert_eq!(
            cocoa_point_to_virtual(CocoaPoint::new(5.0, 899.0), 2.0, 900.0),
            Point::new(10, 2)
        );
    }

    // -- view_point_to_monitor_local -------------------------------------------

    #[test]
    fn view_point_flips_within_the_view() {
        // View bottom-left (0,0) is the monitor's bottom-left pixel corner.
        assert_eq!(
            view_point_to_monitor_local(CocoaPoint::new(0.0, 0.0), 900.0, 2.0),
            Point::new(0, 1800)
        );
        // View top-left is monitor-local (0, 0).
        assert_eq!(
            view_point_to_monitor_local(CocoaPoint::new(0.0, 900.0), 900.0, 2.0),
            Point::new(0, 0)
        );
        assert_eq!(
            view_point_to_monitor_local(CocoaPoint::new(10.0, 890.0), 900.0, 2.0),
            Point::new(20, 20)
        );
    }

    // -- monitor_local_to_view_rect ---------------------------------------------

    #[test]
    fn dirty_rect_maps_to_view_points_with_flipped_y() {
        // Monitor-local rows [20, 70) on a 900pt @2x view: view y range is
        // 900 − 35 = 865 up to 900 − 10 = 890.
        let local = Rect::new(10, 20, 100, 50);
        let view = monitor_local_to_view_rect(local, 900.0, 2.0);
        assert_eq!(view, CocoaRect::new(5.0, 865.0, 50.0, 25.0));
    }

    #[test]
    fn dirty_rect_and_view_point_are_consistent() {
        // The top-left corner of a dirty region, converted as a point at the
        // region's BOTTOM edge in view space, matches the rect's view origin.
        let local = Rect::new(10, 20, 100, 50);
        let view = monitor_local_to_view_rect(local, 900.0, 2.0);
        let corner =
            view_point_to_monitor_local(CocoaPoint::new(view.x, view.y + view.height), 900.0, 2.0);
        assert_eq!(corner, Point::new(local.x, local.y));
    }

    // -- CocoaRect::contains -----------------------------------------------------

    #[test]
    fn contains_edges() {
        let r = CocoaRect::new(10.0, 10.0, 100.0, 50.0);
        assert!(r.contains(CocoaPoint::new(10.0, 10.0))); // left/bottom inclusive
        assert!(r.contains(CocoaPoint::new(109.9, 59.9)));
        assert!(!r.contains(CocoaPoint::new(110.0, 30.0))); // right exclusive
        assert!(!r.contains(CocoaPoint::new(30.0, 60.0))); // top exclusive
        assert!(!r.contains(CocoaPoint::new(9.9, 30.0)));
    }
}
