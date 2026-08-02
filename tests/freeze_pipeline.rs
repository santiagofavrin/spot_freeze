//! Scenario (c): freeze pipeline.
//!
//! Two synthetic monitors (distinct pixel patterns, one at negative virtual x)
//! => darken => spotlight_hole => original pixels restored EXACTLY inside the
//! circle only.
//!
//! REWORK NOTE (composable modes update): `darken` now takes the veil color
//! (`darken(buf, dim_alpha, color)`, SHARED API SPEC). These scenario tests
//! keep the original black-veil behavior by passing `BLACK`, for which the
//! colored formula reduces exactly to the old one.
//!
//! The controller contract being simulated (src/overlay/controller.rs docs):
//! capture ONCE per monitor at freeze time, keep the originals, darken a copy
//! for display, and per mouse move restore the original pixels inside the
//! spotlight circle via `spotlight_hole` (buffer-local coordinates).
//!
//! GAP (Win32/display-coupled, NOT covered headless — listed for Stage 3/4):
//! `capture::GdiCapturer` / `enumerate_monitors` (real screen capture),
//! `overlay::window::OverlayWindow` (real layered windows), and
//! `OverlayController::freeze/unfreeze` itself (creates those windows) cannot
//! be exercised without touching the user's display. The pure pixel pipeline
//! they drive is fully covered here.

mod common;

use common::{BLACK, buffer_with, darkened_pixel, pattern_a, pattern_b};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{darken, monitor_index_at, spotlight_hole, virtual_to_local};

/// `overlay.dim_opacity` documented default.
const DIM: u8 = 160;

fn assert_fully_darkened(original: &DibBuffer, dark: &DibBuffer, dim: u8) {
    assert_eq!((dark.width, dark.height), (original.width, original.height));
    for y in 0..original.height {
        for x in 0..original.width {
            let o = original.pixel(x, y).unwrap();
            let d = dark.pixel(x, y).unwrap();
            assert_eq!(
                d,
                darkened_pixel(o, dim),
                "darkened pixel mismatch at ({x}, {y})"
            );
        }
    }
}

#[test]
fn darken_scales_bgr_toward_black_and_keeps_alpha() {
    let original = buffer_with(16, 12, pattern_a);
    let mut dark = original.clone();
    darken(&mut dark, DIM, BLACK);
    assert_fully_darkened(&original, &dark, DIM);
    // alpha untouched everywhere
    for y in 0..dark.height {
        for x in 0..dark.width {
            assert_eq!(dark.pixel(x, y).unwrap()[3], 255, "alpha at ({x}, {y})");
        }
    }
}

#[test]
fn darken_extremes_are_identity_and_black() {
    let original = buffer_with(8, 8, pattern_a);

    let mut same = original.clone();
    darken(&mut same, 0, BLACK);
    assert_eq!(same, original, "dim_alpha 0 = no change");

    let mut black = original.clone();
    darken(&mut black, 255, BLACK);
    for y in 0..black.height {
        for x in 0..black.width {
            assert_eq!(
                black.pixel(x, y).unwrap(),
                [0, 0, 0, 255],
                "dim_alpha 255 = fully black, alpha untouched, at ({x}, {y})"
            );
        }
    }
}

#[test]
fn freeze_darken_then_spotlight_restores_exactly_inside_circle() {
    // Two synthetic monitors: primary at origin, secondary at NEGATIVE virtual x.
    // (Buffers are monitor-local; the virtual-space side of this scenario is
    // exercised in the end-to-end test below and in monitor_mapping.rs.)
    let mon0 = Rect::new(0, 0, 64, 48);
    let mon1 = Rect::new(-64, 0, 64, 48);

    // Freeze: one capture per monitor, originals kept by the controller.
    let orig0 = buffer_with(mon0.width, mon0.height, pattern_a);
    let orig1 = buffer_with(mon1.width, mon1.height, pattern_b);
    assert_ne!(
        orig0.pixel(5, 5),
        orig1.pixel(5, 5),
        "distinct per-monitor patterns"
    );

    // Darken copies for display; originals stay intact.
    let mut dark0 = orig0.clone();
    let mut dark1 = orig1.clone();
    darken(&mut dark0, DIM, BLACK);
    darken(&mut dark1, DIM, BLACK);
    assert_fully_darkened(&orig0, &dark0, DIM);
    assert_fully_darkened(&orig1, &dark1, DIM);
    assert_eq!(
        orig0.pixel(3, 3).unwrap(),
        pattern_a(3, 3),
        "original intact"
    );

    // Mouse move on monitor 0: cut the spotlight hole from the ORIGINAL.
    let center = Point::new(20, 24);
    let radius = 12u32;
    let mut frame0 = dark0.clone();
    spotlight_hole(&mut frame0, &orig0, center, radius);

    let mut inside_count = 0u32;
    for y in 0..mon0.height {
        for x in 0..mon0.width {
            let dx = x as i64 - center.x as i64;
            let dy = y as i64 - center.y as i64;
            let inside = dx * dx + dy * dy <= radius as i64 * radius as i64;
            let got = frame0.pixel(x, y).unwrap();
            if inside {
                inside_count += 1;
                assert_eq!(
                    got,
                    orig0.pixel(x, y).unwrap(),
                    "inside circle must be EXACT original at ({x}, {y})"
                );
            } else {
                assert_eq!(
                    got,
                    dark0.pixel(x, y).unwrap(),
                    "outside circle must stay darkened at ({x}, {y})"
                );
            }
        }
    }
    assert!(inside_count > 0, "circle must cover some pixels");

    // Boundary is INCLUSIVE (`dx*dx + dy*dy <= radius*radius`): (32, 24) is
    // exactly on the circle (dx = 12, dy = 0); (33, 24) is just outside.
    assert_eq!(frame0.pixel(32, 24).unwrap(), orig0.pixel(32, 24).unwrap());
    assert_eq!(frame0.pixel(33, 24).unwrap(), dark0.pixel(33, 24).unwrap());

    // The hole on monitor 0 must not leak into monitor 1's frame, and monitor
    // 1's hole restores ITS OWN original pattern (buffer-local coordinates).
    let mut frame1 = dark1.clone();
    spotlight_hole(&mut frame1, &orig1, Point::new(10, 10), 8);
    assert_eq!(frame1.pixel(10, 10).unwrap(), pattern_b(10, 10));
    assert_eq!(
        frame1.pixel(18, 10).unwrap(),
        pattern_b(18, 10),
        "on-circle inclusive"
    );
    assert_eq!(
        frame1.pixel(40, 40).unwrap(),
        dark1.pixel(40, 40).unwrap(),
        "far outside stays darkened"
    );
}

#[test]
fn spotlight_hole_clips_when_center_is_outside_buffer() {
    let orig = buffer_with(32, 32, pattern_a);
    let mut dark = orig.clone();
    darken(&mut dark, DIM, BLACK);

    // Center beyond the top-left corner; only the overlapping arc is restored.
    let mut frame = dark.clone();
    spotlight_hole(&mut frame, &orig, Point::new(-6, -6), 10);
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as i64 + 6;
            let dy = y as i64 + 6;
            let inside = dx * dx + dy * dy <= 100;
            let want = if inside {
                orig.pixel(x, y).unwrap()
            } else {
                dark.pixel(x, y).unwrap()
            };
            assert_eq!(
                frame.pixel(x, y).unwrap(),
                want,
                "clipped hole at ({x}, {y})"
            );
        }
    }
    // spot checks: (2, 0) is exactly on the clipped circle (dx=8, dy=6: 64+36=100);
    // (3, 0) is just outside (dx=9, dy=6: 81+36=117).
    assert_eq!(frame.pixel(2, 0).unwrap(), orig.pixel(2, 0).unwrap());
    assert_eq!(frame.pixel(3, 0).unwrap(), dark.pixel(3, 0).unwrap());
    // and (0, 2) is exactly on it too (dx=6, dy=8: 36+64=100)
    assert_eq!(frame.pixel(0, 2).unwrap(), orig.pixel(0, 2).unwrap());
}

#[test]
fn end_to_end_negative_virtual_x_cursor_maps_to_monitor_local_hole() {
    // Full controller-style flow for the negative-virtual-x monitor:
    // virtual cursor position => monitor lookup => buffer-local center => hole.
    let monitors = [Rect::new(0, 0, 64, 48), Rect::new(-64, 0, 64, 48)];
    let orig0 = buffer_with(64, 48, pattern_a);
    let orig1 = buffer_with(64, 48, pattern_b);
    let originals = [&orig0, &orig1];

    let mut dark0 = orig0.clone();
    let mut dark1 = orig1.clone();
    darken(&mut dark0, DIM, BLACK);
    darken(&mut dark1, DIM, BLACK);
    let mut frames = [dark0, dark1];

    // Cursor at virtual (-30, 20): on the LEFT monitor (index 1, negative x).
    let cursor_virtual = Point::new(-30, 20);
    let idx = monitor_index_at(cursor_virtual, &monitors).expect("cursor on a monitor");
    assert_eq!(idx, 1, "negative-virtual-x monitor");
    let center = virtual_to_local(cursor_virtual, monitors[idx]);
    assert_eq!(center, Point::new(34, 20));

    spotlight_hole(&mut frames[idx], originals[idx], center, 10);

    // Restored pixels come from monitor 1's OWN pattern at the mapped center.
    assert_eq!(frames[idx].pixel(34, 20).unwrap(), pattern_b(34, 20));
    // Outside the circle on monitor 1 stays darkened pattern B.
    assert_eq!(
        frames[idx].pixel(0, 0).unwrap(),
        darkened_pixel(pattern_b(0, 0), DIM)
    );
    // Monitor 0's frame is untouched by monitor 1's spotlight.
    assert_eq!(
        frames[0].pixel(34, 20).unwrap(),
        darkened_pixel(pattern_a(34, 20), DIM),
        "other monitor stays fully darkened"
    );
}
