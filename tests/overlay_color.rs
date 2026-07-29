//! Scenario (rework-a): configurable overlay veil color.
//!
//! `darken(buf, dim_alpha, color)` and `compose_frame(...)` with a non-black
//! veil (`#802020`) — EXACT channel math on synthetic buffers.
//!
//! SHARED API SPEC under test:
//! - `spotfreeze::settings::model::Rgb { r, g, b }` (serde "#RRGGBB").
//! - `composite::darken(buf: &mut DibBuffer, dim_alpha: u8, color: Rgb)`.
//! - `composite::compose_frame(original, out, viewport, &RenderState,
//!   dim_alpha, color)` — pipeline: zoom base -> colored darken -> spotlight
//!   hole (reveals zoomed base) -> snip selection copy+border.
//!
//! SPEC-ASSUMED formula (INTEGRATION FLAG, see tests/common/mod.rs):
//! `channel' = (channel * (255 - dim_alpha) + color_channel * dim_alpha) / 255`
//! single truncation. `color = black` reduces to the old black-veil formula;
//! `dim_alpha = 255` yields exactly the veil color.

mod common;

use common::{BLACK, buffer_with, darkened_pixel, dim_color_channel, dimmed_pixel_with, pattern_a};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{RenderState, compose_frame, darken};
use spotfreeze::settings::model::Rgb;

/// The non-black veil used throughout: #802020 (dark red).
const VEIL: Rgb = Rgb {
    r: 0x80,
    g: 0x20,
    b: 0x20,
};

const DIM: u8 = 160;

/// `RenderState` with every layer off (plain darkened frame).
const NO_LAYERS: RenderState = RenderState {
    zoom: None,
    spotlight: None,
    snip: None,
};

fn assert_uniform(buf: &DibBuffer, want: [u8; 4], ctx: &str) {
    for y in 0..buf.height {
        for x in 0..buf.width {
            assert_eq!(buf.pixel(x, y).unwrap(), want, "{ctx} at ({x}, {y})");
        }
    }
}

// ---- darken with color -----------------------------------------------------

#[test]
fn darken_with_color_exact_channel_math() {
    // Representative channel values × dim alphas, checked per BGRA channel
    // against the one-division blend formula.
    for &ch in &[0u8, 1, 2, 127, 128, 200, 254, 255] {
        for &a in &[0u8, 1, 63, 128, 160, 191, 254, 255] {
            let mut buf = buffer_with(1, 1, |_, _| [ch, ch, ch, 77]);
            darken(&mut buf, a, VEIL);
            let want = [
                dim_color_channel(ch, VEIL.b, a),
                dim_color_channel(ch, VEIL.g, a),
                dim_color_channel(ch, VEIL.r, a),
                77, // alpha untouched
            ];
            assert_eq!(buf.pixel(0, 0).unwrap(), want, "ch={ch} a={a}");
        }
    }
}

#[test]
fn darken_with_color_alpha_zero_is_identity() {
    let original = buffer_with(16, 16, pattern_a);
    let mut buf = original.clone();
    darken(&mut buf, 0, VEIL);
    assert_eq!(buf, original, "dim_alpha 0 = no change, regardless of color");
}

#[test]
fn darken_with_color_full_alpha_is_exactly_the_veil_color() {
    let mut buf = buffer_with(8, 8, |x, y| [x as u8, y as u8, 200, 123]);
    darken(&mut buf, 255, VEIL);
    // BGRA: veil.b -> B, veil.g -> G, veil.r -> R; alpha byte preserved.
    assert_uniform(&buf, [VEIL.b, VEIL.g, VEIL.r, 123], "dim_alpha 255 = solid veil");
}

#[test]
fn darken_with_black_color_matches_the_legacy_formula() {
    // Backward compat: color = black must reproduce the pre-rework math
    // (`channel * (255 - dim_alpha) / 255`) byte for byte.
    let original = buffer_with(24, 18, pattern_a);
    let mut buf = original.clone();
    darken(&mut buf, DIM, BLACK);
    for y in 0..original.height {
        for x in 0..original.width {
            assert_eq!(
                buf.pixel(x, y).unwrap(),
                darkened_pixel(pattern_a(x, y), DIM),
                "black veil == legacy darken at ({x}, {y})"
            );
        }
    }
}

#[test]
fn darken_with_color_empty_buffer_no_panic() {
    let mut buf = DibBuffer::default();
    darken(&mut buf, 128, VEIL);
    assert!(buf.pixels.is_empty());
}

// ---- compose_frame with color ----------------------------------------------

#[test]
fn compose_frame_no_layers_is_plain_colored_dim_of_original() {
    let original = buffer_with(24, 18, pattern_a);
    let mut out = DibBuffer::new(24, 18);
    compose_frame(
        &original,
        &mut out,
        Rect::new(0, 0, 24, 18),
        &NO_LAYERS,
        DIM,
        VEIL,
    );
    assert_eq!((out.width, out.height), (24, 18));
    for y in 0..18 {
        for x in 0..24 {
            assert_eq!(
                out.pixel(x, y).unwrap(),
                dimmed_pixel_with(pattern_a(x, y), DIM, VEIL),
                "no layers => colored dim at ({x}, {y})"
            );
        }
    }
    // The veil actually shows the configured color, not black: a bright pixel
    // keeps a strong red-veil tint (B and G channels pulled toward 0x20, R
    // toward 0x80).
    let p = out.pixel(23, 17).unwrap();
    let black_equiv = darkened_pixel(pattern_a(23, 17), DIM);
    assert_ne!(
        p, black_equiv,
        "colored veil must differ from the black-veil result"
    );
}

#[test]
fn compose_frame_spotlight_only_reveals_original_colored_dim_outside() {
    // Spotlight without zoom: base IS the original; the hole reveals original
    // pixels and everything outside is the colored-dimmed original.
    let original = buffer_with(32, 32, pattern_a);
    let center = Point::new(16, 16);
    let radius = 5u32;
    let state = RenderState {
        zoom: None,
        spotlight: Some((center, radius)),
        snip: None,
    };
    let mut out = DibBuffer::new(32, 32);
    compose_frame(
        &original,
        &mut out,
        Rect::new(0, 0, 32, 32),
        &state,
        DIM,
        VEIL,
    );
    for y in 0..32i32 {
        for x in 0..32i32 {
            let dx = x - center.x;
            let dy = y - center.y;
            let inside = dx * dx + dy * dy <= (radius * radius) as i32;
            let p = pattern_a(x as u32, y as u32);
            let want = if inside {
                p // exact original inside the hole
            } else {
                dimmed_pixel_with(p, DIM, VEIL)
            };
            assert_eq!(out.pixel(x as u32, y as u32).unwrap(), want, "({x}, {y})");
        }
    }
    // Boundary is inclusive: (21,16) on the circle (dx=5), (22,16) outside.
    assert_eq!(out.pixel(21, 16).unwrap(), pattern_a(21, 16));
    assert_eq!(
        out.pixel(22, 16).unwrap(),
        dimmed_pixel_with(pattern_a(22, 16), DIM, VEIL)
    );
}

#[test]
fn compose_frame_respects_dim_alpha_extremes_with_color() {
    let original = buffer_with(8, 8, pattern_a);
    // dim_alpha 0: veil invisible — the frame is the original even with a
    // non-black color configured.
    let mut out0 = DibBuffer::new(8, 8);
    compose_frame(
        &original,
        &mut out0,
        Rect::new(0, 0, 8, 8),
        &NO_LAYERS,
        0,
        VEIL,
    );
    assert_eq!(out0, original, "dim_alpha 0 => original frame");
    // dim_alpha 255: solid veil color, alpha bytes from the original (255).
    let mut out255 = DibBuffer::new(8, 8);
    compose_frame(
        &original,
        &mut out255,
        Rect::new(0, 0, 8, 8),
        &NO_LAYERS,
        255,
        VEIL,
    );
    assert_uniform(&out255, [VEIL.b, VEIL.g, VEIL.r, 255], "dim_alpha 255");
}
