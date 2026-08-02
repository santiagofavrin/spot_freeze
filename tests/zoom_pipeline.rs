//! Scenario (d): zoom pipeline.
//!
//! `zoom_resample` driven through `ZoomSettings`-style clamp math exactly the
//! way the controller applies it.
//!
//! REWORK NOTE (mode-redesign update): `ZoomMode` is now a pure LAYER (the
//! "zoom hold", toggled by the `zoom_hold` binding) — the `ModeStack` routing
//! matrix decides WHICH wheel events reach it (zoom modifier from any state,
//! plain wheel whenever the layer is active), so the layer's `on_wheel` takes
//! no `modifiers` argument and applies every wheel it receives. Rendering
//! moved out of the layer: the controller reads
//! [`ModeStack::render_state`] and hands the `RenderState` to
//! `composite::compose_frame` (pixel-exact compose coverage lives in
//! `composition_pipeline.rs`; the render_state contract is pinned here).
//!
//! ASSUMPTION (documented controller contract, pieced together from the frozen
//! docs): the overlay window reports wheel `delta` in raw Win32 units — one
//! notch = `WHEEL_DELTA` = 120, with sub-notch deltas possible from
//! smooth-scroll hardware — and `ZoomMode::on_wheel` applies
//! `zoom *= step_factor^(delta / 120)` clamped to `[zoom.min, zoom.max]`
//! (src/overlay/modes/zoom.rs). One notch = 120 delta units = one step_factor
//! multiplication. The resulting zoom (always > 0, >= min) is what the
//! pipeline passes to `zoom_resample` ("`zoom` must be > 0 — callers clamp it
//! to the settings min/max", src/overlay/composite.rs), with `focus` = cursor
//! monitor-local position and `viewport` = the monitor-local frame rect
//! (its x/y are ignored by the resampler).

mod common;

use common::buffer_with;
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::hotkeys::gesture::Modifiers;
use spotfreeze::overlay::composite::{ZoomFilter, zoom_resample};
use spotfreeze::overlay::modes::zoom::ZoomMode;
use spotfreeze::overlay::modes::{ModeKind, ModeParams, ModeStack};
use spotfreeze::settings::model::ZoomSettings;

/// One wheel notch in raw delta units (`WHEEL_DELTA`).
const NOTCH: i32 = 120;

fn assert_uniform(buf: &DibBuffer, want: [u8; 4], ctx: &str) {
    for y in 0..buf.height {
        for x in 0..buf.width {
            assert_eq!(buf.pixel(x, y).unwrap(), want, "{ctx} at ({x}, {y})");
        }
    }
}

/// Documented default zoom config (src/settings/model.rs field docs). Used as
/// explicit literals so these pipeline tests are decoupled from the settings
/// model; `zoom_settings_documented_defaults` verifies the model itself.
const DEFAULT_ZOOM: (f32, f32, f32) = (1.25, 1.0, 16.0);

/// ModeParams matching the documented defaults (spotlight fields irrelevant
/// here but present — the stack always starts with a spotlight layer).
fn default_params() -> ModeParams {
    ModeParams {
        spotlight_radius: 150,
        radius_modifier: Modifiers::CTRL,
        zoom_step: DEFAULT_ZOOM.0,
        zoom_min: DEFAULT_ZOOM.1,
        zoom_max: DEFAULT_ZOOM.2,
        zoom_modifier: Modifiers::SHIFT,
    }
}

#[test]
fn zoom_settings_documented_defaults() {
    let s = ZoomSettings::default();
    assert_eq!((s.step_factor, s.min, s.max), DEFAULT_ZOOM);
}

#[test]
fn wheel_applies_step_factor_per_notch_and_clamps_to_settings_bounds() {
    let (step_factor, min, max) = DEFAULT_ZOOM;
    let mut zm = ZoomMode::new(step_factor, min, max);
    assert_eq!(zm.zoom(), 1.0, "initial zoom is 1.0");

    // One notch up: * step_factor.
    let _ = zm.on_wheel(0, Point::new(5, 5), NOTCH);
    assert!((zm.zoom() - 1.25).abs() < 1e-6, "one notch = one step");

    // One notch down: back to 1.0.
    let _ = zm.on_wheel(0, Point::new(5, 5), -NOTCH);
    assert!((zm.zoom() - 1.0).abs() < 1e-6);

    // Two notches in one event: step^2 (raw-delta contract, see module docs).
    let _ = zm.on_wheel(0, Point::new(5, 5), 2 * NOTCH);
    assert!(
        (zm.zoom() - 1.5625).abs() < 1e-5,
        "delta 240 = two notches = step^2, got {}",
        zm.zoom()
    );

    // Far down: clamped exactly at min.
    let _ = zm.on_wheel(0, Point::new(5, 5), -NOTCH); // back to ~1.0
    for _ in 0..20 {
        let _ = zm.on_wheel(0, Point::new(5, 5), -NOTCH);
    }
    assert_eq!(zm.zoom(), min, "clamped at settings min");

    // Far up: clamped exactly at max (16.0 needs ~13 notches at 1.25).
    for _ in 0..50 {
        let _ = zm.on_wheel(0, Point::new(5, 5), NOTCH);
    }
    assert_eq!(zm.zoom(), max, "clamped at settings max");

    // Reset-view hotkey restores exactly 1.0.
    let _ = zm.reset_view();
    assert_eq!(zm.zoom(), 1.0, "reset_view restores 1.0");
}

#[test]
fn zoom_resample_output_has_viewport_dimensions() {
    let src = buffer_with(64, 64, pattern::gray);
    let viewport = Rect::new(7, 9, 32, 24); // x/y ignored per contract
    for filter in [ZoomFilter::Nearest, ZoomFilter::Bilinear] {
        let out = zoom_resample(&src, viewport, 2.0, Point::new(32, 32), filter);
        assert_eq!(
            (out.width, out.height),
            (32, 24),
            "{filter:?} viewport size"
        );
        assert_eq!(out.stride, 32 * 4, "{filter:?} tight stride");
        assert_eq!(out.pixels.len(), (32 * 24 * 4) as usize);
    }
}

#[test]
fn zoom_resample_uniform_source_stays_uniform_for_both_filters() {
    // Convention-robust: any sampler over a uniform field inside the source
    // returns the field color, so this pins the pipeline without depending on
    // the exact pixel-center convention.
    let field = [10, 20, 30, 255];
    let src = buffer_with(64, 64, |_, _| field);
    let viewport = Rect::new(0, 0, 32, 32);
    // zoom 2.0 => sampled region 16x16 centered on (32, 32) = src 24..40:
    // comfortably inside the 64x64 source for both filters.
    for filter in [ZoomFilter::Nearest, ZoomFilter::Bilinear] {
        let out = zoom_resample(&src, viewport, 2.0, Point::new(32, 32), filter);
        assert_uniform(&out, field, "uniform field must resample exactly");
    }
    // zoom 1.0, region = full 32x32 around center: still fully inside.
    let out = zoom_resample(&src, viewport, 1.0, Point::new(32, 32), ZoomFilter::Nearest);
    assert_uniform(&out, field, "zoom 1.0 uniform");
}

#[test]
fn zoom_resample_outside_source_replicates_edge_pixels() {
    // DEVIATION NOTE (flagged for Stage 3/4): the Stage-1 stub doc said
    // "Samples outside `src` are opaque black"; the landed composite
    // implementation (and its updated doc) CLAMPS to the nearest edge pixel
    // instead (edge pixels replicate outward) — better UX at screen borders.
    // This test pins the IMPLEMENTED behavior exactly.
    //
    // Mapping (composite doc): src = focus + (o + 0.5 - viewport/2)/zoom - 0.5.
    // With focus (0,0), viewport 32x32, zoom 1.0: src_coord(o) = o - 16,
    // so output columns/rows 0..16 sample the clamped edge pixel 0 and
    // o >= 16 sample src (o - 16) — identical for Nearest and Bilinear
    // (frac == 0 in the unclamped zone; both taps equal at the clamp).
    let src = buffer_with(64, 64, |x, y| [x as u8, y as u8, 200, 255]);
    let viewport = Rect::new(0, 0, 32, 32);
    for filter in [ZoomFilter::Nearest, ZoomFilter::Bilinear] {
        let out = zoom_resample(&src, viewport, 1.0, Point::new(0, 0), filter);
        for y in 0..32u32 {
            for x in 0..32u32 {
                let sx = x.saturating_sub(16) as u8;
                let sy = y.saturating_sub(16) as u8;
                assert_eq!(
                    out.pixel(x, y).unwrap(),
                    [sx, sy, 200, 255],
                    "{filter:?} at ({x}, {y})"
                );
            }
        }
        // The corner is the replicated edge pixel — NOT opaque black.
        assert_eq!(out.pixel(0, 0).unwrap(), [0, 0, 200, 255]);
        assert_ne!(
            out.pixel(0, 0).unwrap(),
            [0, 0, 0, 255],
            "{filter:?}: outside-source samples are edge-replicated, not black"
        );
    }
}

#[test]
fn zoom_resample_magnifies_around_focus() {
    // Quadrant-colored 64x64 source; focus exactly on the quadrant junction.
    // zoom 2.0 over a 64x64 viewport samples the inner 32x32 (src 16..48),
    // so each output quadrant shows one source quadrant's color.
    // Probe points are >= 8 output px (>= 4 src px) away from every border —
    // outside any bilinear kernel's reach — so both filters must agree exactly.
    let quadrant = |x: u32, y: u32| -> [u8; 4] {
        match (x < 32, y < 32) {
            (true, true) => [200, 10, 10, 255], // top-left: blue-heavy (BGRA)
            (false, true) => [10, 200, 10, 255], // top-right: green
            (true, false) => [10, 10, 200, 255], // bottom-left: red
            (false, false) => [200, 200, 200, 255], // bottom-right: light gray
        }
    };
    let src = buffer_with(64, 64, quadrant);
    let viewport = Rect::new(0, 0, 64, 64);

    for filter in [ZoomFilter::Nearest, ZoomFilter::Bilinear] {
        let out = zoom_resample(&src, viewport, 2.0, Point::new(32, 32), filter);
        assert_eq!(
            out.pixel(8, 8).unwrap(),
            [200, 10, 10, 255],
            "{filter:?} TL"
        );
        assert_eq!(
            out.pixel(55, 8).unwrap(),
            [10, 200, 10, 255],
            "{filter:?} TR"
        );
        assert_eq!(
            out.pixel(8, 55).unwrap(),
            [10, 10, 200, 255],
            "{filter:?} BL"
        );
        assert_eq!(
            out.pixel(55, 55).unwrap(),
            [200, 200, 200, 255],
            "{filter:?} BR"
        );
    }
}

#[test]
fn controller_style_flow_zoom_state_feeds_resample() {
    // Drive the layer like the ModeStack does, then hand its zoom to the
    // resampler with the cursor as focus (the compose path's contract).
    let (step_factor, min, max) = DEFAULT_ZOOM;
    let mut zm = ZoomMode::new(step_factor, min, max);
    let cursor = Point::new(32, 32);
    let _ = zm.on_mouse_move(0, cursor);
    let _ = zm.on_wheel(0, cursor, NOTCH); // zoom = 1.25
    let zoom = zm.zoom();
    assert!(zoom > 1.0 && zoom <= max, "clamped, resamplable zoom");

    let src = buffer_with(64, 64, |_, _| [50, 60, 70, 255]);
    let out = zoom_resample(
        &src,
        Rect::new(0, 0, 64, 64),
        zoom,
        cursor,
        ZoomFilter::Nearest,
    );
    assert_eq!((out.width, out.height), (64, 64));
    // 64/1.25 = 51.2px region around (32,32) => fully inside the source.
    assert_uniform(&out, [50, 60, 70, 255], "zoomed uniform field");
}

#[test]
fn render_state_carries_zoom_only_on_the_cursor_monitor() {
    // Rework render-path contract (replaces the old ZoomMode::render pin):
    // the layer contributes to `ModeStack::render_state` — and thereby to
    // `compose_frame` — ONLY on the monitor its cursor is on; every other
    // monitor gets a layer-free RenderState (plain darkened frame).
    let mut stack = ModeStack::new(default_params());
    stack.set_mode(ModeKind::Zoom); // full switch: zoom is the only layer
    let cursor = Point::new(16, 16);
    let _ = stack.on_mouse_move(0, cursor);
    // Plain wheel reaches zoom because the layer is active (routing matrix).
    let _ = stack.on_wheel(0, cursor, NOTCH, Modifiers::NONE);
    let zoom = stack.zoom().expect("zoom layer active").zoom();
    assert!((zoom - 1.25).abs() < 1e-6);

    let rs0 = stack.render_state(0);
    assert_eq!(
        rs0.zoom,
        Some((zoom, cursor)),
        "zoom layer on cursor monitor"
    );
    assert!(
        rs0.spotlight.is_none(),
        "set_mode dropped the spotlight layer"
    );
    assert!(rs0.snip.is_none());

    let rs1 = stack.render_state(1);
    assert_eq!(
        rs1.zoom, None,
        "no zoom contribution off the cursor monitor"
    );
    assert!(rs1.spotlight.is_none());
    assert!(rs1.snip.is_none());
}

#[test]
fn render_state_zoom_modifier_wheel_zooms_when_layer_active() {
    // Shift+wheel (the default zoom_modifier) reaches the zoom layer from any
    // state — here right after an additive activation.
    let mut stack = ModeStack::new(default_params()); // starts: spotlight only
    stack.add_mode(ModeKind::Zoom); // layer added at the last-used factor (1.0)
    let _ = stack.on_mouse_move(0, Point::new(8, 8));
    let _ = stack.on_wheel(0, Point::new(8, 8), NOTCH, Modifiers::SHIFT);
    let zoom = stack.zoom().expect("zoom layer").zoom();
    assert!(
        (zoom - 1.25).abs() < 1e-6,
        "Shift+wheel zooms while the zoom layer is active, got {zoom}"
    );
    let rs = stack.render_state(0);
    assert_eq!(rs.zoom, Some((zoom, Point::new(8, 8))));
    // Spotlight is still active too (add_mode is additive): the hole follows
    // the same cursor.
    assert_eq!(rs.spotlight, Some((Point::new(8, 8), 150)));
}

/// Small namespace so the uniform-source tests read cleanly.
mod pattern {
    pub fn gray(_x: u32, _y: u32) -> [u8; 4] {
        [128, 128, 128, 255]
    }
}
