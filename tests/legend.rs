//! Scenario: the mode/hotkey legend pill.
//!
//! While frozen, every monitor shows a modern translucent rounded pill near
//! its top-center: the modes as TABS (active highlighted), each labelled with
//! the hotkey that reaches it, read from the freeze-time bindings. Text is
//! anti-aliased vector typography from the embedded Inter typeface
//! (rasterized by `fontdue`) — driven here through the public API:
//! settings -> [`Legend::from_hotkeys`] -> painted frames.
//!
//! Covered: tab labels from default + custom bindings, the ZOOM tab reflecting
//! the zoom-modifier wheel chord, per-monitor centering, translucency, the
//! active-tab highlight, and the controller painting the pill into presented
//! frames while keeping it out of the clipboard (the last point is pinned
//! end-to-end in the controller's own tests).

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

/// Sum of the BGRA color channels over a rectangle (a robust luminance proxy
/// that doesn't depend on exact anti-aliased pixel values).
fn region_sum(buf: &DibBuffer, x0: u32, y0: u32, w: u32, h: u32) -> u64 {
    let mut s = 0u64;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            if let Some(p) = buf.pixel(x, y) {
                s += p[0] as u64 + p[1] as u64 + p[2] as u64;
            }
        }
    }
    s
}

#[test]
fn default_bindings_render_mode_tabs_with_their_hotkeys() {
    let legend = Legend::from_hotkeys(&HotkeySettings::default());
    // The ZOOM tab is labelled with the zoom-modifier wheel chord — zoom is
    // implicit in every mode (no dedicated zoom hotkey).
    assert_eq!(
        legend.tab_labels(),
        vec![
            "SPOTLIGHT (S)".to_string(),
            "ZOOM (Shift+Wheel)".to_string(),
            "SNIP (C)".to_string(),
        ]
    );
    assert_eq!(
        legend.version_label(),
        &format!("v{}", env!("CARGO_PKG_VERSION")),
        "the legend carries the app version"
    );
    let (w, h) = legend.size();
    assert!(w > 200, "pill width is sizable: {w}");
    assert!(h > 0, "pill has a positive height");
}

#[test]
fn custom_zoom_modifier_changes_the_zoom_tab_label_and_pill_width() {
    let mut hotkeys = HotkeySettings::default();
    hotkeys.zoom_modifier = spotfreeze::hotkeys::gesture::Modifiers::CTRL
        | spotfreeze::hotkeys::gesture::Modifiers::ALT
        | spotfreeze::hotkeys::gesture::Modifiers::SHIFT
        | spotfreeze::hotkeys::gesture::Modifiers::WIN;
    let legend = Legend::from_hotkeys(&hotkeys);
    assert_eq!(
        legend.tab_labels()[1],
        "ZOOM (Ctrl+Alt+Shift+Win+Wheel)",
        "the zoom tab follows the configured zoom modifier"
    );
    let default_w = Legend::from_hotkeys(&HotkeySettings::default()).size().0;
    let wider = legend.size().0;
    assert!(
        wider > default_w,
        "a longer zoom-modifier chord widens the pill: {wider} vs {default_w}"
    );
}

#[test]
fn pill_is_top_centered_translucent_and_highlights_the_active_tab() {
    let legend = Legend::new(&[LegendTab {
        name: "SPOTLIGHT".into(),
        hotkey: "S".into(),
    }]);
    let (pw, ph) = legend.size();
    let frame_w = 800u32;
    let frame_h = 160u32;

    let mut active = dark_frame(frame_w, frame_h);
    let origin = legend.default_origin(frame_w, frame_h);
    legend.paint(&mut active, &[true], origin);
    let mut inactive = dark_frame(frame_w, frame_h);
    legend.paint(&mut inactive, &[false], origin);
    let plain = dark_frame(frame_w, frame_h);

    let x0 = origin.x as u32;
    let y0 = origin.y as u32;
    // The pill is painted in the top-center band and is translucent: the pill
    // center is dimmer than the plain frame (blended toward near-black) but
    // not solid black.
    let center = inactive.pixel(x0 + pw / 2, y0 + ph / 2).unwrap();
    let plain_center = plain.pixel(x0 + pw / 2, y0 + ph / 2).unwrap();
    assert!(
        center[..3].iter().map(|&c| u16::from(c)).sum::<u16>()
            < plain_center[..3].iter().map(|&c| u16::from(c)).sum::<u16>(),
        "the pill darkens: {center:?} vs {plain_center:?}"
    );
    // The rounded corner pixel (bbox corner) is untouched by the pill body.
    assert_eq!(
        inactive.pixel(x0, y0).unwrap(),
        plain.pixel(x0, y0).unwrap(),
        "the rounded corner leaves the bbox corner untouched"
    );
    // Nothing outside the pill area changes.
    assert_eq!(inactive.pixel(0, 0).unwrap(), plain.pixel(0, 0).unwrap());
    assert_eq!(
        inactive.pixel(frame_w - 1, frame_h - 1).unwrap(),
        plain.pixel(frame_w - 1, frame_h - 1).unwrap()
    );
    // The active tab's chip area is brighter overall than the inactive one
    // (chip fill + brighter text).
    let chip_w = pw - 2 * 20; // pill padding on each side; one tab fills the inner width
    let on_sum = region_sum(&active, x0 + 20, y0, chip_w, ph);
    let off_sum = region_sum(&inactive, x0 + 20, y0, chip_w, ph);
    assert!(
        on_sum > off_sum,
        "active tab highlighted: on={on_sum} off={off_sum}"
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
