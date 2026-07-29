//! Scenario (rework-b): composable mode pipeline.
//!
//! `compose_frame(original, out, viewport, &RenderState, dim_alpha, color)`
//! with LAYERED modes (SHARED API SPEC):
//!
//! ```text
//! zoom base -> colored darken -> spotlight hole (reveals ZOOMED base)
//!            -> snip selection copy+border
//! ```
//!
//! Pins:
//! - zoom(2.0) + spotlight: pixels inside the circle equal the ZOOMED base
//!   (not the original); outside equal the colored-dimmed zoomed base.
//! - snip selection on the zoomed base: the selection interior shows the
//!   zoomed pixels (== `crop_normalized` of the zoomed base contents).
//!
//! SPEC ASSUMPTIONS (INTEGRATION FLAGS):
//! - The zoom base inside `compose_frame` uses `ZoomFilter::Nearest` — the
//!   same filter the pre-rework `ZoomMode::render` used ("Nearest is the
//!   default filter: zero interpolation cost"), consistent with the app's
//!   speed-first priority. If the landed pipeline uses Bilinear, recompute
//!   the reference `zoomed` buffers here with that filter instead.
//! - The snip border ring (color/thickness) is NOT pinned by the spec, so
//!   border pixels are excluded with a 3 px margin; interior and exterior
//!   are pinned exactly.

mod common;

use common::{BLACK, buffer_with, dimmed_pixel_with, pattern_a};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{
    RenderState, ZoomFilter, compose_frame, crop_normalized, zoom_resample,
};

const DIM: u8 = 160;
const W: u32 = 16;
const H: u32 = 16;

fn viewport() -> Rect {
    Rect::new(0, 0, W, H)
}

/// Reference zoomed base, computed with the same pure primitive the pipeline
/// is expected to use (`zoom_resample`, Nearest — see module docs).
fn zoomed_base(original: &DibBuffer, zoom: f32, focus: Point) -> DibBuffer {
    zoom_resample(original, viewport(), zoom, focus, ZoomFilter::Nearest)
}

#[test]
fn zoom_plus_spotlight_hole_reveals_zoomed_base_dimmed_zoomed_outside() {
    let original = buffer_with(W, H, pattern_a);
    let focus = Point::new(8, 8);
    let center = Point::new(8, 8);
    let radius = 3u32;
    let state = RenderState {
        zoom: Some((2.0, focus)),
        spotlight: Some((center, radius)),
        snip: None,
    };

    let zoomed = zoomed_base(&original, 2.0, focus);
    let mut out = DibBuffer::new(W, H);
    compose_frame(&original, &mut out, viewport(), &state, DIM, BLACK);

    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let dx = x - center.x;
            let dy = y - center.y;
            let inside = dx * dx + dy * dy <= (radius * radius) as i32;
            let zb = zoomed.pixel(x as u32, y as u32).unwrap();
            let want = if inside {
                zb // hole reveals the ZOOMED base
            } else {
                dimmed_pixel_with(zb, DIM, BLACK)
            };
            assert_eq!(
                out.pixel(x as u32, y as u32).unwrap(),
                want,
                "({x}, {y})"
            );
        }
    }

    // Discriminating checks: the hole shows the ZOOMED base, NOT the original.
    // (10,8): inside the circle (dx=2); zoomed(10,8) = original(9,8) ≠ original(10,8).
    assert_eq!(out.pixel(10, 8).unwrap(), original.pixel(9, 8).unwrap());
    assert_ne!(
        out.pixel(10, 8).unwrap(),
        original.pixel(10, 8).unwrap(),
        "hole must reveal the zoomed base, not the original"
    );
    // (0,0): outside; dimmed zoomed ≠ dimmed original (zoomed(0,0) = original(4,4)).
    assert_eq!(
        out.pixel(0, 0).unwrap(),
        dimmed_pixel_with(pattern_a(4, 4), DIM, BLACK)
    );
    assert_ne!(
        out.pixel(0, 0).unwrap(),
        dimmed_pixel_with(pattern_a(0, 0), DIM, BLACK),
        "darken must apply to the zoomed base, not the original"
    );
}

#[test]
fn zoom_only_dims_the_zoomed_base_everywhere() {
    // Pipeline order with no spotlight/snip layers: zoom base -> darken.
    let original = buffer_with(W, H, pattern_a);
    let focus = Point::new(8, 8);
    let state = RenderState {
        zoom: Some((2.0, focus)),
        spotlight: None,
        snip: None,
    };
    let zoomed = zoomed_base(&original, 2.0, focus);
    let mut out = DibBuffer::new(W, H);
    compose_frame(&original, &mut out, viewport(), &state, DIM, BLACK);
    for y in 0..H {
        for x in 0..W {
            assert_eq!(
                out.pixel(x, y).unwrap(),
                dimmed_pixel_with(zoomed.pixel(x, y).unwrap(), DIM, BLACK),
                "zoom-only frame at ({x}, {y})"
            );
        }
    }
}

#[test]
fn snip_selection_on_zoomed_base_shows_the_zoomed_pixels() {
    let original = buffer_with(W, H, pattern_a);
    let focus = Point::new(8, 8);
    let a = Point::new(4, 4);
    let b = Point::new(12, 10); // selection rect x 4..12, y 4..10
    let state = RenderState {
        zoom: Some((2.0, focus)),
        spotlight: None,
        snip: Some((a, b)),
    };

    let zoomed = zoomed_base(&original, 2.0, focus);
    let mut out = DibBuffer::new(W, H);
    compose_frame(&original, &mut out, viewport(), &state, DIM, BLACK);

    // Interior (>= 2 px off every rect edge, safe for any border ring up to
    // 2 px — the ring is spec-unpinned, see module docs): the selection shows
    // the ZOOMED base pixels, undarkened. Rect x 4..12, y 4..10 (right/bottom
    // exclusive) => safe probes x in 6..=9, y in 6..=7.
    for y in 6..=7u32 {
        for x in 6..=9u32 {
            assert_eq!(
                out.pixel(x, y).unwrap(),
                zoomed.pixel(x, y).unwrap(),
                "selection interior ({x}, {y}) = zoomed base"
            );
        }
    }
    // Exterior far from the rect: colored-dimmed zoomed base.
    for (x, y) in [(0u32, 0u32), (15, 15), (0, 15), (15, 0)] {
        assert_eq!(
            out.pixel(x, y).unwrap(),
            dimmed_pixel_with(zoomed.pixel(x, y).unwrap(), DIM, BLACK),
            "outside selection ({x}, {y})"
        );
    }
    // Discriminator: interior pixel (6,6) is the zoomed pixel orig(7,7)
    // (src = 8 + (6.5-8)/2 - 0.5 = 6.75 -> 7), which differs from the
    // UNzoomed original(6,6) — the interior comes from the zoomed base.
    assert_eq!(out.pixel(6, 6).unwrap(), original.pixel(7, 7).unwrap());
    assert_ne!(
        out.pixel(6, 6).unwrap(),
        original.pixel(6, 6).unwrap(),
        "selection interior must come from the zoomed base"
    );

    // "Crops the zoomed pixels": cropping the ZOOMED base at the selection
    // yields exactly the pixels the frame shows inside (margin-safe probes:
    // crop (2,2)/(5,3)/(3,2) <=> frame (6,6)/(9,7)/(7,6)).
    let crop = crop_normalized(&zoomed, a, b).expect("non-empty selection");
    assert_eq!((crop.width, crop.height), (8, 6));
    for (cx, cy) in [(2u32, 2u32), (5, 3), (3, 2)] {
        assert_eq!(
            crop.pixel(cx, cy).unwrap(),
            out.pixel(4 + cx, 4 + cy).unwrap(),
            "crop({cx},{cy}) == frame interior"
        );
    }
    // And the crop contents are zoomed pixels (coordinate-encoded pattern).
    assert_eq!(crop.pixel(0, 0).unwrap(), zoomed.pixel(4, 4).unwrap());
    assert_eq!(crop.pixel(7, 5).unwrap(), zoomed.pixel(11, 9).unwrap());
}

#[test]
fn zoom_spotlight_snip_all_layers_compose_in_spec_order() {
    // All three layers: snip interior (margin-safe) = zoomed base; spotlight
    // hole outside the snip = zoomed base; elsewhere = dimmed zoomed base.
    let original = buffer_with(W, H, pattern_a);
    let focus = Point::new(8, 8);
    let center = Point::new(2, 13);
    let radius = 2u32;
    let state = RenderState {
        zoom: Some((2.0, focus)),
        spotlight: Some((center, radius)),
        snip: Some((Point::new(4, 4), Point::new(12, 10))),
    };
    let zoomed = zoomed_base(&original, 2.0, focus);
    let mut out = DibBuffer::new(W, H);
    compose_frame(&original, &mut out, viewport(), &state, DIM, BLACK);

    // Inside the spotlight circle (and well outside the snip rect/border):
    // hole reveals the zoomed base.
    assert_eq!(out.pixel(2, 13).unwrap(), zoomed.pixel(2, 13).unwrap());
    // Snip interior (margin-safe) = zoomed base.
    assert_eq!(out.pixel(8, 7).unwrap(), zoomed.pixel(8, 7).unwrap());
    // Far from both: dimmed zoomed base.
    assert_eq!(
        out.pixel(15, 15).unwrap(),
        dimmed_pixel_with(zoomed.pixel(15, 15).unwrap(), DIM, BLACK)
    );
    assert_eq!(
        out.pixel(15, 0).unwrap(),
        dimmed_pixel_with(zoomed.pixel(15, 0).unwrap(), DIM, BLACK)
    );
}
