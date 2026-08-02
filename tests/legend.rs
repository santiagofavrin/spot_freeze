//! Scenario (new): the mode/hotkey legend pill.
//!
//! While frozen, every monitor shows a large translucent rounded pill near
//! its top-center: the modes as TABS (active highlighted), each labelled with
//! the hotkey that reaches it, read from the freeze-time bindings. Rendering
//! is pure pixel math in `src/overlay/legend.rs` over the embedded
//! public-domain 8x8 bitmap font — driven here through the public API:
//! settings -> [`Legend::from_hotkeys`] -> painted frames.
//!
//! Covered: tab text and width from default + custom bindings, per-monitor
//! centering, translucency, the active-tab highlight, and the controller
//! painting the pill into presented frames while keeping it out of the
//! clipboard (the last point is pinned end-to-end in the controller's own
//! tests).

mod common;

use common::{FakeFreeze, buffer_with, monitor_info};
use spotfreeze::capture::DibBuffer;
use spotfreeze::geometry::{Point, Rect};
use spotfreeze::overlay::composite::{RenderState, compose_frame};
use spotfreeze::overlay::legend::{Legend, LegendTab};
use spotfreeze::settings::model::{AppSettings, HotkeySettings};

fn dark_frame(w: u32, h: u32) -> DibBuffer {
    buffer_with(w, h, |x, y| [(x & 0xFF) as u8, (y & 0xFF) as u8, 60, 255])
}

/// First differing pixel between two frames, if any.
fn first_diff(a: &DibBuffer, b: &DibBuffer) -> Option<(u32, u32)> {
    (0..a.height)
        .flat_map(|y| (0..a.width).map(move |x| (x, y)))
        .find(|&(x, y)| a.pixel(x, y) != b.pixel(x, y))
}

#[test]
fn default_bindings_render_mode_tabs_with_their_hotkeys() {
    let legend = Legend::from_hotkeys(&HotkeySettings::default());
    let (w, h) = legend.size();
    // "SPOTLIGHT (S)" (13 chars), "ZOOM (F)" / "SNIP (C)" (8 chars each)
    // at 16 px per char + per-tab padding, plus pill padding and gaps.
    assert_eq!(h, 40, "16 px text + 2 * 12 px vertical padding");
    assert_eq!(w, 48 + (240 + 160 + 160) + 16);
}

#[test]
fn custom_bindings_change_tab_text_and_pill_width() {
    let mut hotkeys = HotkeySettings::default();
    hotkeys.zoom_hold = spotfreeze::hotkeys::gesture::HotkeyGesture::parse("Ctrl+Shift+Z").unwrap();
    let default_w = Legend::from_hotkeys(&HotkeySettings::default()).size().0;
    let wider = Legend::from_hotkeys(&hotkeys).size().0;
    assert!(
        wider > default_w,
        "a longer binding widens the pill: {wider} vs {default_w}"
    );
    assert_eq!(
        wider - default_w,
        11 * 16,
        "eleven extra characters at 16 px"
    );
}

#[test]
fn pill_is_top_centered_inset_translucent_and_highlights_the_active_tab() {
    let legend = Legend::new(&[LegendTab {
        name: "SPOTLIGHT".into(),
        hotkey: "S".into(),
    }]);
    let (pw, ph) = legend.size();
    let frame_w = 800u32;
    let frame_h = 160u32;

    let mut active = dark_frame(frame_w, frame_h);
    legend.paint(&mut active, &[true], 255);
    let mut inactive = dark_frame(frame_w, frame_h);
    legend.paint(&mut inactive, &[false], 255);
    let plain = dark_frame(frame_w, frame_h);

    let x0 = (frame_w - pw) / 2;
    let y0 = 48;
    // The pill is painted in the top-center band with a generous inset: the first
    // painted pixel is the top edge at the corner radius (the rounded corner
    // leaves the bbox corner itself untouched).
    let diff = first_diff(&inactive, &plain).expect("pill changes pixels");
    assert_eq!(
        diff,
        (x0 + 20, y0),
        "first pill pixel (top edge at the radius)"
    );
    // ...it is translucent: in the padding between the pill edge and the
    // chip (off-text), the frame reads through the dark pill (blended toward
    // near-black, not replaced).
    let pad_probe = inactive.pixel(x0 + 8, y0 + ph / 2).unwrap();
    let plain_probe = plain.pixel(x0 + 8, y0 + ph / 2).unwrap();
    assert!(
        pad_probe[..3].iter().map(|&c| u16::from(c)).sum::<u16>()
            < plain_probe[..3].iter().map(|&c| u16::from(c)).sum::<u16>(),
        "the pill darkens: {pad_probe:?} vs {plain_probe:?}"
    );
    assert_ne!(pad_probe, [0x12, 0x12, 0x16, 255], "translucent, not solid");
    // ...and the active tab's chip brightens its area versus the inactive one.
    let chip = (x0 + 24 + 8, y0 + 20);
    let [b_on, g_on, r_on, _] = active.pixel(chip.0, chip.1).unwrap();
    let [b_off, g_off, r_off, _] = inactive.pixel(chip.0, chip.1).unwrap();
    assert!(
        b_on > b_off && g_on > g_off && r_on > r_off,
        "active tab highlighted: on={b_on},{g_on},{r_on} off={b_off},{g_off},{r_off}"
    );
    // Nothing outside the pill area changes.
    assert_eq!(inactive.pixel(0, 0).unwrap(), plain.pixel(0, 0).unwrap());
    assert_eq!(
        inactive.pixel(frame_w - 1, frame_h - 1).unwrap(),
        plain.pixel(frame_w - 1, frame_h - 1).unwrap()
    );
}

#[test]
fn controller_paints_the_pill_centered_on_every_monitor() {
    // Two 1024x160 monitors (big enough for the pill), spotlight active.
    let captured = vec![
        (
            monitor_info(Rect::new(0, 0, 1024, 160)),
            buffer_with(1024, 160, |x, y| {
                [(x & 0xFF) as u8, (y & 0xFF) as u8, 40, 255]
            }),
        ),
        (
            monitor_info(Rect::new(-1024, 0, 1024, 160)),
            buffer_with(1024, 160, |x, y| {
                [200, (x & 0xFF) as u8, (y & 0xFF) as u8, 255]
            }),
        ),
    ];
    let f = FakeFreeze::new(captured, &AppSettings::default(), Point::new(512, 100));
    let legend = Legend::from_hotkeys(&AppSettings::default().hotkeys);
    let (pw, ph) = legend.size();
    for m in 0..2 {
        let frame = f.last_present(m);
        // The pill sits near THIS monitor's top-center (each frame is
        // monitor-local): probe the pill band only.
        let x0 = (1024 - pw) / 2;
        let y0 = 48;
        let pill_pixel = frame.pixel(x0 + pw / 2, y0 + ph / 2).unwrap();
        // Reference: the same monitor's composed frame without the pill.
        let mut bare = DibBuffer::new(1024, 160);
        let state = RenderState {
            spotlight: Some((Point::new(512, 100), 150)),
            ..RenderState::default()
        };
        compose_frame(
            &f.captured[m].1,
            &mut bare,
            Rect::new(0, 0, 1024, 160),
            &state,
            160,
            spotfreeze::settings::model::Rgb::BLACK,
        );
        assert_ne!(
            pill_pixel,
            bare.pixel(x0 + pw / 2, y0 + ph / 2).unwrap(),
            "monitor {m}: the pill is painted near its top-center"
        );
    }
}
