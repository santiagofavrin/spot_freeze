//! Scenario (g): capture pipeline (mode-redesign update).
//!
//! Entering capture mode RE-BASES the freeze: the currently composited view
//! (zoom base → veil → spotlight hole) becomes the new frozen base, the snip
//! selection and the copy then work on the EFFECTED pixels (WYSIWYG), and the
//! frame carries the persistent capture indicator. Simulated here with the
//! same pure pieces the controller drives: `ModeStack` for the layer/capture
//! state and `compose_frame` for the pixels (the controller re-bases by
//! composing exactly this way; its Esc/copy routing and the re-base swap are
//! covered by the fake-surface tests in src/overlay/controller.rs).

mod common;

use common::{BLACK, buffer_with, pattern_a};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::hotkeys::gesture::Modifiers;
use spotfreeze::overlay::composite::{RenderState, compose_frame, crop_normalized};
use spotfreeze::overlay::modes::{ModeKind, ModeParams, ModeStack};

const DIM: u8 = 160;
const W: u32 = 24;
const H: u32 = 24;

fn viewport() -> Rect {
    Rect::new(0, 0, W, H)
}

/// ModeParams with a spotlight radius small enough to leave dimmed pixels on
/// the 24x24 buffer.
fn params() -> ModeParams {
    ModeParams {
        spotlight_radius: 5,
        zoom_step: 1.25,
        zoom_min: 1.0,
        zoom_max: 16.0,
        zoom_modifier: Modifiers::SHIFT,
    }
}

/// Compose `original` with the stack's render state for monitor 0, applying
/// the controller's dim rules: veil when any layer is active, NO veil in
/// capture mode (the re-based frame already carries the effects).
fn compose_with_stack(original: &DibBuffer, stack: &ModeStack) -> DibBuffer {
    let mut out = DibBuffer::new(original.width, original.height);
    let dim = if !stack.in_capture() && stack.any_active() {
        DIM
    } else {
        0
    };
    compose_frame(
        original,
        &mut out,
        viewport(),
        &stack.render_state(0),
        dim,
        BLACK,
    );
    out
}

/// The controller's capture-entry re-base: compose the CURRENT layers into a
/// new base (snip and the capture indicator excluded by construction). Must
/// be called BEFORE `set_mode(ModeKind::Snip)` stashes the layers — the
/// controller's exact order.
fn rebase(original: &DibBuffer, stack: &ModeStack) -> DibBuffer {
    let rs = RenderState {
        snip: None,
        capture: false,
        ..stack.render_state(0)
    };
    let mut out = DibBuffer::new(original.width, original.height);
    let dim = if stack.any_active() { DIM } else { 0 };
    compose_frame(original, &mut out, viewport(), &rs, dim, BLACK);
    out
}

#[test]
fn capture_rebakes_effects_and_indicator_wraps_an_undimmed_frame() {
    let original = buffer_with(W, H, pattern_a);
    let mut stack = ModeStack::new(params());
    stack.seed_cursor(0, Point::new(12, 12)); // spotlight hole at (12,12), r=5
    let pre_capture = compose_with_stack(&original, &stack);
    let base = rebase(&original, &stack); // BEFORE the switch stashes the layers

    stack.set_mode(ModeKind::Snip); // enter capture
    assert!(stack.in_capture());
    assert_eq!(
        base.pixels, pre_capture.pixels,
        "the re-frozen base IS the view at capture-entry time"
    );

    // The capture frame shows the effected base UNDIMMED (no double veil),
    // wrapped in a uniform indicator ring at the frame edge.
    assert!(stack.render_state(0).capture);
    let capture_frame = compose_with_stack(&base, &stack);
    for y in 0..H {
        for x in 0..W {
            let at_edge = x < 2 || x >= W - 2 || y < 2 || y >= H - 2;
            let got = capture_frame.pixel(x, y).unwrap();
            if at_edge {
                assert_eq!(
                    got,
                    capture_frame.pixel(0, 0).unwrap(),
                    "uniform indicator ring at ({x},{y})"
                );
                assert_ne!(
                    got,
                    base.pixel(x, y).unwrap(),
                    "the ring paints over the base at ({x},{y})"
                );
            } else {
                assert_eq!(
                    got,
                    base.pixel(x, y).unwrap(),
                    "interior shows the effected base at ({x},{y})"
                );
            }
        }
    }

    // Esc: capture flag and stash are gone; the frame is the pre-capture view.
    stack.exit_capture();
    assert!(!stack.in_capture());
    assert!(!stack.render_state(0).capture);
    let restored = compose_with_stack(&original, &stack);
    assert_eq!(
        restored.pixels, pre_capture.pixels,
        "the pre-capture view is restored exactly"
    );
}

#[test]
fn capture_copy_crops_effected_pixels_with_zoom_and_spotlight_baked_in() {
    let original = buffer_with(W, H, pattern_a);
    let mut stack = ModeStack::new(params());
    stack.seed_cursor(0, Point::new(12, 12));
    // Zoom hold via the wheel chord (implicit activation), one notch in.
    stack.on_wheel(0, Point::new(12, 12), 120, Modifiers::SHIFT);
    let pre_capture = compose_with_stack(&original, &stack);
    assert_ne!(
        pre_capture.pixels, original.pixels,
        "the view really is effected (zoom + veil + hole)"
    );

    let base = rebase(&original, &stack); // BEFORE the switch stashes the layers
    stack.set_mode(ModeKind::Snip);
    assert_eq!(base.pixels, pre_capture.pixels);

    // Snip drag + copy (controller contract): crop the re-frozen base.
    stack.on_left_button_down(0, Point::new(4, 16));
    stack.on_mouse_move(0, Point::new(12, 23));
    stack.on_left_button_up(0, Point::new(12, 23));
    let sel = stack.snip_selection().expect("selection after the drag");
    let crop = crop_normalized(&base, sel.a, sel.b).expect("non-empty selection");
    let expected = crop_normalized(&pre_capture, sel.a, sel.b).unwrap();
    assert_eq!(
        crop.pixels, expected.pixels,
        "the crop is the EFFECTED (zoom + spotlight baked in) pixels"
    );
    let raw = crop_normalized(&original, sel.a, sel.b).unwrap();
    assert_ne!(
        crop.pixels, raw.pixels,
        "NOT a crop of the undarkened, unzoomed original"
    );
}

#[test]
fn zoom_hold_layers_over_spotlight_and_restores_the_last_used_factor() {
    let original = buffer_with(W, H, pattern_a);
    let mut stack = ModeStack::new(params());
    stack.seed_cursor(0, Point::new(12, 12));

    // `F` toggles the hold on (1.0 initially), the wheel takes it to 1.5625.
    // The zoom chord is required here: with the spotlight layer also active,
    // the plain wheel resizes the spotlight instead (routing matrix).
    stack.toggle_mode(ModeKind::Zoom);
    stack.on_wheel(0, Point::new(12, 12), 240, Modifiers::SHIFT);
    let z = stack.zoom().expect("zoom hold active").zoom();
    assert!(
        (z - 1.5625).abs() < 1e-6,
        "the zoom chord zooms the active hold"
    );

    // Off and on again: the last-used factor is back (not a fresh 1.0).
    stack.toggle_mode(ModeKind::Zoom);
    stack.toggle_mode(ModeKind::Zoom);
    let z = stack.zoom().expect("zoom hold re-activated").zoom();
    assert!((z - 1.5625).abs() < 1e-6, "last-used factor restored");

    // The layer composes over the spotlight: hole = zoomed base, outside =
    // dimmed zoomed base (pipeline pinned in composition_pipeline.rs).
    let rs = stack.render_state(0);
    assert!(rs.zoom.is_some() && rs.spotlight.is_some() && !rs.capture);
    let frame = compose_with_stack(&original, &stack);
    assert_ne!(frame.pixels, original.pixels);

    // `0` resets the hold to 1.0.
    stack.reset_view();
    assert_eq!(stack.zoom().unwrap().zoom(), 1.0);
}
