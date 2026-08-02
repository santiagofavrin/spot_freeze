//! Scenario (e): snip pipeline.
//!
//! `SnipMode` synthetic drag events => `snip_selection` => `crop_normalized`
//! => exact expected sub-rectangle pixels.
//!
//! REWORK NOTE (composable modes update): the snip LAYER no longer renders —
//! the controller hands `SnipSelection` endpoints to `composite::compose_frame`
//! via `ModeStack::render_state` (pipeline stage: "snip selection
//! copy+border"). The render test below drives `compose_frame` directly with
//! the layer-produced selection. The selection BORDER (color/thickness) is
//! not pinned by the SHARED API SPEC, so assertions keep a 2 px margin off
//! the ring; layered snip-on-zoomed-base pixels are covered in
//! `composition_pipeline.rs`.
//!
//! The controller contract being simulated (src/overlay/controller.rs,
//! `snip_copy_and_close`): Ctrl+C crops the selection from the composed BASE
//! of `SnipSelection.monitor` — the selection endpoints are monitor-local
//! pixels — and copies the crop to the clipboard.
//!
//! GAP (Win32/display-coupled, NOT covered headless — listed for Stage 3/4):
//! the final `capture::copy_dib_to_clipboard` step touches the real system
//! clipboard and is deliberately not exercised here (would clobber the user's
//! clipboard). Everything up to the clipboard boundary is covered exactly.

mod common;

use common::{BLACK, buffer_with, darkened_pixel, dimmed_pixel_with};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{RenderState, compose_frame, crop_normalized};
use spotfreeze::overlay::modes::SnipSelection;
use spotfreeze::overlay::modes::snip::SnipMode;
use spotfreeze::settings::model::Rgb;

/// Coordinate-encoding pattern: pixel (x, y) = [x, y, x^y, 255] (BGRA).
fn coord_pattern(x: u32, y: u32) -> [u8; 4] {
    [x as u8, y as u8, (x ^ y) as u8, 255]
}

/// Drive one complete drag: down at `a`, one extending move, up at `b`.
fn drag(mode: &mut SnipMode, monitor: usize, a: Point, b: Point) {
    let _ = mode.on_left_button_down(monitor, a);
    let _ = mode.on_mouse_move(monitor, b); // extend the in-progress selection
    let _ = mode.on_left_button_up(monitor, b);
}

#[test]
fn drag_produces_selection_and_crop_returns_exact_subrectangle() {
    let src = buffer_with(80, 60, coord_pattern);
    let mut snip = SnipMode::new();
    assert_eq!(snip.snip_selection(), None, "no selection initially");

    let a = Point::new(10, 10);
    let b = Point::new(34, 40);
    drag(&mut snip, 0, a, b);

    let sel = snip
        .snip_selection()
        .expect("selection after finished drag");
    assert_eq!(sel, SnipSelection { monitor: 0, a, b });

    // Controller contract: crop from the ORIGINAL capture of sel.monitor.
    let crop = crop_normalized(&src, sel.a, sel.b).expect("non-empty selection");
    assert_eq!((crop.width, crop.height), (24, 30), "normalized size");
    assert_eq!(crop.stride, 24 * 4, "tight stride");
    for y in 0..crop.height {
        for x in 0..crop.width {
            assert_eq!(
                crop.pixel(x, y).unwrap(),
                src.pixel(10 + x, 10 + y).unwrap(),
                "crop pixel ({x}, {y})"
            );
        }
    }
    // Absolute spot checks against the pattern formula: crop (1, 2) <=> src (11, 12).
    assert_eq!(crop.pixel(0, 0).unwrap(), [10, 10, 0, 255]);
    assert_eq!(crop.pixel(1, 2).unwrap(), [11, 12, 7, 255]);
    assert_eq!(crop.pixel(23, 29).unwrap(), [33, 39, 6, 255]);
}

#[test]
fn negative_drag_normalizes_inside_crop() {
    let src = buffer_with(80, 60, coord_pattern);
    let mut snip = SnipMode::new();

    // Drag up-left: a is below-right of b. Endpoints are stored AS DRAGGED;
    // normalization is `crop_normalized`'s job (SnipSelection docs).
    let a = Point::new(50, 45);
    let b = Point::new(20, 15);
    drag(&mut snip, 0, a, b);
    let sel = snip.snip_selection().expect("selection");
    assert_eq!((sel.a, sel.b), (a, b), "endpoints preserved as dragged");

    let crop = crop_normalized(&src, sel.a, sel.b).expect("non-empty");
    assert_eq!((crop.width, crop.height), (30, 30));
    for y in 0..crop.height {
        for x in 0..crop.width {
            assert_eq!(
                crop.pixel(x, y).unwrap(),
                src.pixel(20 + x, 15 + y).unwrap(),
                "negative-drag crop pixel ({x}, {y})"
            );
        }
    }
}

#[test]
fn zero_area_drag_clears_selection_and_crop_returns_none() {
    let mut snip = SnipMode::new();
    drag(&mut snip, 0, Point::new(10, 10), Point::new(30, 30));
    assert!(snip.snip_selection().is_some());

    // A click without movement = zero-area drag => selection cleared
    // (SnipMode docs: "A zero-area drag clears the selection").
    drag(&mut snip, 0, Point::new(25, 25), Point::new(25, 25));
    assert_eq!(snip.snip_selection(), None, "zero-area drag clears");

    // crop_normalized agrees for degenerate rectangles.
    let src = buffer_with(16, 16, coord_pattern);
    assert!(crop_normalized(&src, Point::new(4, 4), Point::new(4, 4)).is_none());
    assert!(
        crop_normalized(&src, Point::new(4, 4), Point::new(9, 4)).is_none(),
        "zero height"
    );
    assert!(
        crop_normalized(&src, Point::new(4, 4), Point::new(4, 9)).is_none(),
        "zero width"
    );
}

#[test]
fn new_drag_replaces_previous_selection_and_tracks_monitor() {
    let mut snip = SnipMode::new();
    drag(&mut snip, 0, Point::new(5, 5), Point::new(15, 15));
    let first = snip.snip_selection().expect("first selection");
    assert_eq!(first.monitor, 0);

    // New drag on ANOTHER monitor replaces the old selection wholesale —
    // the controller will crop from that monitor's composed base.
    let _ = snip.on_left_button_down(1, Point::new(2, 3));
    let _ = snip.on_left_button_up(1, Point::new(20, 30));
    let sel = snip.snip_selection().expect("replaced selection");
    assert_eq!(
        sel,
        SnipSelection {
            monitor: 1,
            a: Point::new(2, 3),
            b: Point::new(20, 30)
        }
    );
    assert_ne!(sel, first);
}

#[test]
fn crop_clips_to_buffer_bounds() {
    let src = buffer_with(16, 16, coord_pattern);

    // Drag starting OUTSIDE the top-left corner: clipped to the buffer.
    let crop =
        crop_normalized(&src, Point::new(-10, -10), Point::new(5, 5)).expect("overlapping drag");
    assert_eq!((crop.width, crop.height), (5, 5), "clipped to x/y in 0..5");
    for y in 0..crop.height {
        for x in 0..crop.width {
            assert_eq!(crop.pixel(x, y).unwrap(), src.pixel(x, y).unwrap());
        }
    }

    // Drag hanging off the bottom-right corner.
    let crop =
        crop_normalized(&src, Point::new(12, 12), Point::new(40, 40)).expect("overlapping drag");
    assert_eq!(
        (crop.width, crop.height),
        (4, 4),
        "clipped to x/y in 12..16"
    );
    for y in 0..crop.height {
        for x in 0..crop.width {
            assert_eq!(
                crop.pixel(x, y).unwrap(),
                src.pixel(12 + x, 12 + y).unwrap()
            );
        }
    }

    // Fully outside => None (right/bottom edges are exclusive).
    assert!(crop_normalized(&src, Point::new(-50, -50), Point::new(-20, -20)).is_none());
    assert!(crop_normalized(&src, Point::new(16, 0), Point::new(30, 10)).is_none());
    assert!(crop_normalized(&src, Point::new(0, 16), Point::new(10, 30)).is_none());
}

#[test]
fn compose_frame_snip_only_shows_original_inside_dimmed_outside() {
    // Rework render contract: RenderState.snip = Some((a, b)) with no zoom
    // layer => the selection shows the ORIGINAL pixels (base IS the original),
    // everything outside stays darkened. The layer produces the endpoints;
    // compose_frame does the pixels. Border ring: margin-safe (module docs).
    let original = buffer_with(40, 30, coord_pattern);
    let mut snip = SnipMode::new();
    drag(&mut snip, 0, Point::new(8, 6), Point::new(20, 18)); // rect x 8..20, y 6..18
    let sel = snip.snip_selection().expect("selection");

    let state = RenderState {
        zoom: None,
        spotlight: None,
        snip: Some((sel.a, sel.b)),
        capture: false,
    };
    let mut out = DibBuffer::new(40, 30);
    compose_frame(
        &original,
        &mut out,
        Rect::new(0, 0, 40, 30),
        &state,
        160,
        BLACK,
    );

    // Interior, >= 2 px off every rect edge (rect pixels x 8..=19, y 6..=17):
    // EXACT original.
    for y in 8..=15i32 {
        for x in 10..=17i32 {
            assert_eq!(
                out.pixel(x as u32, y as u32).unwrap(),
                original.pixel(x as u32, y as u32).unwrap(),
                "selection interior ({x}, {y})"
            );
        }
    }
    // Exterior, >= 3 px away from the rect: EXACT darkened original.
    for (x, y) in [
        (0u32, 0u32),
        (39, 29),
        (0, 29),
        (39, 0),
        (2, 15),
        (25, 2),
        (25, 25),
        (4, 3),
    ] {
        assert_eq!(
            out.pixel(x, y).unwrap(),
            darkened_pixel(original.pixel(x, y).unwrap(), 160),
            "outside selection ({x}, {y})"
        );
    }
    // Deep-interior / deep-exterior invariants.
    assert_eq!(out.pixel(14, 12).unwrap(), original.pixel(14, 12).unwrap());
    assert_eq!(
        out.pixel(0, 0).unwrap(),
        darkened_pixel(original.pixel(0, 0).unwrap(), 160)
    );

    // No selection (snip: None) => uniformly darkened frame.
    let plain = RenderState {
        zoom: None,
        spotlight: None,
        snip: None,
        capture: false,
    };
    let mut out2 = DibBuffer::new(40, 30);
    compose_frame(
        &original,
        &mut out2,
        Rect::new(0, 0, 40, 30),
        &plain,
        160,
        BLACK,
    );
    for y in 0..30u32 {
        for x in 0..40u32 {
            assert_eq!(
                out2.pixel(x, y).unwrap(),
                darkened_pixel(original.pixel(x, y).unwrap(), 160),
                "no selection => uniformly darkened at ({x}, {y})"
            );
        }
    }
}

// ---- capture-mode look (rework: snip veil + crisp border) ---------------------

/// The documented snip veil defaults (`overlay.snip_dim_opacity` /
/// `overlay.snip_color`): much lighter and cooler than the spotlight veil.
const SNIP_DIM: u8 = 90;
const SNIP_VEIL: Rgb = Rgb {
    r: 0x16,
    g: 0x28,
    b: 0x3A,
};

#[test]
fn snip_veil_defaults_are_lighter_and_a_different_color_than_the_spotlight_veil() {
    let s = spotfreeze::settings::model::OverlaySettings::default();
    assert_eq!(s.snip_dim_opacity, SNIP_DIM);
    assert!(
        s.snip_dim_opacity <= 100,
        "much lower opacity than the spotlight veil's {}",
        s.dim_opacity
    );
    assert_eq!(s.snip_color, SNIP_VEIL);
    assert_ne!(s.snip_color, s.color, "different from the spotlight veil");
}

#[test]
fn compose_capture_frame_dims_with_the_snip_veil_and_keeps_the_selection_clear() {
    // Capture mode: the frame composes the re-frozen base under the SNIP
    // veil (dim + cool color), not the spotlight veil; the drawn rectangle
    // stays COMPLETELY CLEAR (the raw base pixels, zero dimming) behind the
    // two-tone border.
    let original = buffer_with(40, 30, coord_pattern);
    let (a, b) = (Point::new(8, 6), Point::new(20, 18));
    let state = RenderState {
        zoom: None,
        spotlight: None,
        snip: Some((a, b)),
        capture: true,
    };
    let mut out = DibBuffer::new(40, 30);
    compose_frame(
        &original,
        &mut out,
        Rect::new(0, 0, 40, 30),
        &state,
        SNIP_DIM,
        SNIP_VEIL,
    );

    // Exterior: dimmed with the SNIP veil (the colored formula), and visibly
    // different from the spotlight-veil result. (Probes stay 2 px off the
    // frame edge, clear of the capture indicator ring.)
    for (x, y) in [(2u32, 2u32), (37, 27), (30, 2), (2, 25)] {
        assert_eq!(
            out.pixel(x, y).unwrap(),
            dimmed_pixel_with(original.pixel(x, y).unwrap(), SNIP_DIM, SNIP_VEIL),
            "snip veil at ({x}, {y})"
        );
        assert_ne!(
            out.pixel(x, y).unwrap(),
            darkened_pixel(original.pixel(x, y).unwrap(), 160),
            "not the spotlight veil at ({x}, {y})"
        );
    }
    // Interior (margin-safe): EXACT base pixels — zero dimming.
    for y in 8..=15u32 {
        for x in 10..=17u32 {
            assert_eq!(
                out.pixel(x, y).unwrap(),
                original.pixel(x, y).unwrap(),
                "clear selection at ({x}, {y})"
            );
        }
    }
    // The border ring is crisp and two-tone: white OUTER line (over the
    // veil), black INNER line (over the clear selection).
    assert_eq!(out.pixel(8, 5).unwrap(), [255, 255, 255, 255], "outer top");
    assert_eq!(
        out.pixel(7, 10).unwrap(),
        [255, 255, 255, 255],
        "outer left"
    );
    assert_eq!(out.pixel(8, 6).unwrap(), [0, 0, 0, 255], "inner top-left");
    assert_eq!(
        out.pixel(19, 17).unwrap(),
        [0, 0, 0, 255],
        "inner bottom-right"
    );
    assert_eq!(
        out.pixel(20, 18).unwrap(),
        [255, 255, 255, 255],
        "outer bottom-right"
    );
}
