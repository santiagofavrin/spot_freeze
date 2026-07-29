//! Scenario (b): hotkey defaults.
//!
//! Every default hotkey in `AppSettings` parses and re-serializes to an
//! identical display string, and all defaults are pairwise non-conflicting.
//! Pure model/serde checks — headless-safe, no `RegisterHotKey` calls.

use spotfreeze::hotkeys::gesture::{HotkeyGesture, Modifiers};
use spotfreeze::settings::model::{AppSettings, HotkeySettings};
use std::collections::HashSet;

/// All full-gesture fields of `HotkeySettings` (the modifier-only
/// `spotlight_radius_modifier` is covered separately — it is a `Modifiers`,
/// not a gesture, so it cannot "conflict" with key gestures).
fn gesture_fields(h: &HotkeySettings) -> [(&'static str, HotkeyGesture); 7] {
    [
        ("freeze_toggle", h.freeze_toggle),
        ("mode_spotlight", h.mode_spotlight),
        ("mode_zoom", h.mode_zoom),
        ("mode_snip", h.mode_snip),
        ("snip_copy", h.snip_copy),
        ("cancel", h.cancel),
        ("reset_zoom", h.reset_zoom),
    ]
}

#[test]
fn documented_default_gestures_are_exact() {
    // Defaults documented per-field in src/settings/model.rs.
    // VK codes: `A`–`Z`/`0`–`9` equal uppercase ASCII; Esc = 0x1B (gesture.rs docs).
    let h = HotkeySettings::default();
    assert_eq!(
        h.freeze_toggle,
        HotkeyGesture::new(Modifiers::CTRL | Modifiers::ALT, 'F' as u32)
    );
    assert_eq!(h.mode_spotlight, HotkeyGesture::new(Modifiers::NONE, '1' as u32));
    assert_eq!(h.mode_zoom, HotkeyGesture::new(Modifiers::NONE, '2' as u32));
    assert_eq!(h.mode_snip, HotkeyGesture::new(Modifiers::NONE, '3' as u32));
    assert_eq!(
        h.snip_copy,
        HotkeyGesture::new(Modifiers::CTRL, 'C' as u32)
    );
    assert_eq!(h.cancel, HotkeyGesture::new(Modifiers::NONE, 0x1B));
    assert_eq!(h.reset_zoom, HotkeyGesture::new(Modifiers::NONE, '0' as u32));
    assert_eq!(h.spotlight_radius_modifier, Modifiers::CTRL);
}

#[test]
fn default_display_strings_match_docs() {
    let h = HotkeySettings::default();
    let expected = [
        "Ctrl+Alt+F", // freeze_toggle
        "1",          // mode_spotlight
        "2",          // mode_zoom
        "3",          // mode_snip
        "Ctrl+C",     // snip_copy
        "Esc",        // cancel
        "0",          // reset_zoom
    ];
    for ((name, g), want) in gesture_fields(&h).into_iter().zip(expected) {
        assert_eq!(g.to_display(), want, "{name} display string");
    }
    assert_eq!(h.spotlight_radius_modifier.to_display(), "Ctrl");
}

#[test]
fn every_default_parses_and_reserializes_to_identical_display_string() {
    let h = HotkeySettings::default();
    for (name, g) in gesture_fields(&h) {
        let display = g.to_display();

        // parse(to_display) == identity, and display form is canonical
        let parsed = HotkeyGesture::parse(&display)
            .unwrap_or_else(|e| panic!("{name}: parse({display:?}) failed: {e}"));
        assert_eq!(parsed, g, "{name}: parse(to_display) must be identity");
        assert_eq!(parsed.to_display(), display, "{name}: canonical display");

        // serde form IS the display string, and it deserializes back
        let json = serde_json::to_string(&g).expect("serialize gesture");
        assert_eq!(
            json,
            format!("\"{display}\""),
            "{name}: serializes to its display string"
        );
        let back: HotkeyGesture = serde_json::from_str(&json).expect("deserialize gesture");
        assert_eq!(back, g, "{name}: serde round-trip");

        assert!(
            g.is_registerable(),
            "{name}: every default must be a registerable gesture"
        );
    }

    // The modifier-only default gets the same parse/serde round-trip treatment.
    let m = h.spotlight_radius_modifier;
    let display = m.to_display();
    assert_eq!(
        Modifiers::parse(&display).expect("parse modifier display"),
        m,
        "spotlight_radius_modifier: parse(to_display) must be identity"
    );
    let json = serde_json::to_string(&m).expect("serialize modifiers");
    assert_eq!(json, format!("\"{display}\""));
    assert_eq!(
        serde_json::from_str::<Modifiers>(&json).expect("deserialize modifiers"),
        m
    );
}

#[test]
fn defaults_are_pairwise_non_conflicting() {
    let h = HotkeySettings::default();

    // HotkeyGesture equality is exact (modifiers + vk), so equality doubles as
    // conflict detection (gesture.rs contract).
    let mut seen = HashSet::new();
    for (name, g) in gesture_fields(&h) {
        assert!(seen.insert(g), "duplicate default hotkey at {name}: {g:?}");
    }
    for (i, (name_a, a)) in gesture_fields(&h).into_iter().enumerate() {
        for (name_b, b) in gesture_fields(&h).into_iter().skip(i + 1) {
            assert_ne!(a, b, "conflicting defaults: {name_a} vs {name_b}");
        }
    }
}

#[test]
fn whole_app_settings_serde_round_trip() {
    let defaults = AppSettings::default();
    let json = serde_json::to_string(&defaults).expect("serialize AppSettings");
    let back: AppSettings = serde_json::from_str(&json).expect("deserialize AppSettings");
    assert_eq!(back, defaults, "full settings model serde round-trip");
}
