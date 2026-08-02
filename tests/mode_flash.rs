//! Scenario (rework): mode changes are SEAMLESS — the white border-flash
//! feedback is gone — and freeze/unfreeze are INSTANT.
//!
//! The pre-rework app flashed a white 6 px border ring 1-3 times on every
//! key-driven mode activation (S=1, F=2, C=3, freeze=1). That feedback is
//! REMOVED: activation into spotlight and every other mode change must be
//! seamless — no flash frames, no white repaint pops. What remains:
//!
//! - freeze entry presents each monitor's settled frame EXACTLY ONCE (no
//!   fade, no animation); unfreeze presents nothing — the overlay windows
//!   are simply destroyed;
//! - spotlight toggles, full mode switches (`set_mode`), capture entry (`C`),
//!   and Esc from capture repaint exactly ONCE — instant by design;
//! - NO frame presented on any monitor, at any point of the journey, ever
//!   carries an entirely white 6 px border band.
//!
//! Drives the real [`OverlayController`] over the shared in-memory fakes
//! (tests/common), headless.

mod common;

use common::{FakeFreeze, buffer_with, has_white_border_band, monitor_info};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::modes::ModeKind;
use spotfreeze::settings::model::AppSettings;

/// Coordinate-encoding pattern: pixel (x, y) = [x, y, x^y, 255] (BGRA).
fn coord_pattern(x: u32, y: u32) -> [u8; 4] {
    [x as u8, y as u8, (x ^ y) as u8, 255]
}

/// Two 32x32 fake monitors (the second at negative virtual x), too small for
/// the legend pill — these tests exercise the instant freeze/unfreeze paths.
fn small_settings() -> AppSettings {
    let mut s = AppSettings::default();
    s.spotlight.default_radius = 6; // clamped to the layer's 10 px minimum
    s
}

fn freeze(cursor: Point) -> FakeFreeze {
    let captured = vec![
        (
            monitor_info(Rect::new(0, 0, 32, 32)),
            buffer_with(32, 32, coord_pattern),
        ),
        (
            monitor_info(Rect::new(-32, 0, 32, 32)),
            buffer_with(32, 32, |x, y| {
                let [b, g, r, a] = coord_pattern(x, y);
                [255 - b, 255 - g, 255 - r, a]
            }),
        ),
    ];
    FakeFreeze::new(captured, &small_settings(), cursor)
}

fn original0() -> DibBuffer {
    buffer_with(32, 32, coord_pattern)
}

fn assert_flash_free(f: &FakeFreeze, ctx: &str) {
    for (i, presents) in f.presents.iter().enumerate() {
        for (j, frame) in presents.borrow().iter().enumerate() {
            assert!(
                !has_white_border_band(frame),
                "{ctx}: white flash band on monitor {i}, frame {j}"
            );
        }
    }
}

#[test]
fn freeze_entry_presents_the_settled_frame_once_and_seamless() {
    let f = freeze(Point::new(16, 16));
    for (m, presents) in f.presents.iter().enumerate() {
        assert_eq!(
            presents.borrow().len(),
            1,
            "monitor {m}: instant freeze, exactly one present"
        );
    }
    let p = f.presents[0].borrow();
    assert_ne!(
        p[0].pixels,
        original0().pixels,
        "the single frame is the settled veiled view, not the bare original"
    );
    assert_flash_free(&f, "freeze entry");
}

#[test]
fn spotlight_toggle_repaints_once_both_ways_without_flashing() {
    let mut f = freeze(Point::new(16, 16));
    // Off: one settled repaint, session stays frozen.
    let before = f.presents[0].borrow().len();
    f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
    assert!(f.controller.is_frozen());
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 1,
        "toggle-off: one settled repaint, no flash frames"
    );
    assert_eq!(
        f.last_present(0).pixels,
        original0().pixels,
        "toggle-off ends exactly on the unveiled original"
    );
    // On: one settled repaint.
    let before = f.presents[0].borrow().len();
    f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
    assert_eq!(f.controller.active_mode(), ModeKind::Spotlight);
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 1,
        "toggle-on: one settled repaint, no flash frames"
    );
    assert_ne!(
        f.last_present(0).pixels,
        original0().pixels,
        "the veil is back"
    );
    assert_flash_free(&f, "spotlight toggles");
}

#[test]
fn full_mode_switches_repaint_exactly_once_without_flashing() {
    let mut f = freeze(Point::new(16, 16));
    let before = f.presents[0].borrow().len();
    f.controller.set_mode(ModeKind::Snip, &f.services);
    assert_eq!(f.presents[0].borrow().len(), before + 1, "capture entry");
    f.controller.set_mode(ModeKind::Zoom, &f.services);
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 2,
        "switch out of capture"
    );
    f.controller.set_mode(ModeKind::Spotlight, &f.services);
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 3,
        "switch to spotlight"
    );
    assert_flash_free(&f, "mode switches");
}

#[test]
fn capture_entry_and_esc_exit_repaint_once_without_flashing() {
    let mut f = freeze(Point::new(16, 16));
    let before = f.presents[0].borrow().len();
    f.controller.set_mode(ModeKind::Snip, &f.services);
    assert_eq!(f.presents[0].borrow().len(), before + 1);
    f.controller.unfreeze(); // Esc in capture: exit capture, stay frozen
    assert!(f.controller.is_frozen());
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 2,
        "instant capture exit"
    );
    f.controller.unfreeze(); // second Esc: real unfreeze (instant close)
    assert!(!f.controller.is_frozen());
    assert_eq!(
        f.presents[0].borrow().len(),
        before + 2,
        "unfreeze adds no frames — the windows just close"
    );
    assert_flash_free(&f, "capture entry/exit");
}

#[test]
fn the_whole_key_driven_journey_is_flash_free() {
    let mut f = freeze(Point::new(16, 16));
    f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
    f.controller.toggle_mode(ModeKind::Spotlight, &f.services);
    f.controller.add_mode(ModeKind::Zoom, &f.services);
    f.controller.toggle_mode(ModeKind::Zoom, &f.services);
    f.controller.set_mode(ModeKind::Snip, &f.services);
    f.controller.unfreeze(); // exit capture
    f.controller.set_mode(ModeKind::Spotlight, &f.services);
    f.controller.unfreeze(); // instant close
    assert!(!f.controller.is_frozen());
    assert_flash_free(&f, "whole journey");
}
