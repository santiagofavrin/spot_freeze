//! Scenario (b): hotkey defaults — REWORK pins (composable modes update).
//!
//! New out-of-box defaults (user feedback):
//! - `freeze_toggle` = **Win+F** (was Ctrl+Alt+F)
//! - `mode_spotlight` = **S**, `mode_zoom` = **Z**, `mode_snip` = **C**
//!   (were 1/2/3)
//! - NEW `zoom_modifier` = **Shift** (wheel-zoom chord, modifier-only)
//! - `spotlight_radius_modifier` stays **Ctrl**
//!
//! Modes are COMPOSABLE now: the app auto-registers a `Shift+<mode>` variant
//! of every mode hotkey (Shift+mode = ADD that mode as a layer). The
//! auto-registered variants are part of the registered-binding set, so they
//! are included in the pairwise non-conflict pins.
//!
//! Every default hotkey in `AppSettings` parses and re-serializes to an
//! identical display string, and all registered bindings are pairwise
//! non-conflicting. Pure model/serde checks — headless-safe, no
//! `RegisterHotKey` calls.

use spotfreeze::hotkeys::gesture::{HotkeyGesture, Modifiers};
use spotfreeze::settings::model::{AppSettings, HotkeySettings};
use std::collections::HashSet;

/// All full-gesture fields of `HotkeySettings` (the modifier-only
/// `spotlight_radius_modifier` / `zoom_modifier` are covered separately —
/// they are `Modifiers`, not gestures, so they cannot "conflict" with key
/// gestures).
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

/// The Shift+mode "add layer" variant the app derives from a mode hotkey:
/// same key, `Shift` OR-ed into the modifiers.
fn shift_variant(g: HotkeyGesture) -> HotkeyGesture {
    HotkeyGesture::new(g.modifiers | Modifiers::SHIFT, g.vk)
}

/// The auto-registered Shift+mode variants (composable layer adds).
fn shift_mode_variants(h: &HotkeySettings) -> [(&'static str, HotkeyGesture); 3] {
    [
        ("mode_spotlight+Shift", shift_variant(h.mode_spotlight)),
        ("mode_zoom+Shift", shift_variant(h.mode_zoom)),
        ("mode_snip+Shift", shift_variant(h.mode_snip)),
    ]
}

/// Every gesture the app registers out of the box: the 7 bindings plus the
/// 3 auto-registered Shift+mode variants.
fn all_registered(h: &HotkeySettings) -> Vec<(&'static str, HotkeyGesture)> {
    gesture_fields(h)
        .into_iter()
        .chain(shift_mode_variants(h))
        .collect()
}

#[test]
fn documented_default_gestures_are_exact() {
    // Defaults documented per-field in src/settings/model.rs.
    // VK codes: `A`–`Z`/`0`–`9` equal uppercase ASCII; Esc = 0x1B (gesture.rs docs).
    let h = HotkeySettings::default();
    assert_eq!(
        h.freeze_toggle,
        HotkeyGesture::new(Modifiers::WIN, 'F' as u32),
        "freeze_toggle = Win+F"
    );
    assert_eq!(
        h.mode_spotlight,
        HotkeyGesture::new(Modifiers::NONE, 'S' as u32),
        "mode_spotlight = S"
    );
    assert_eq!(
        h.mode_zoom,
        HotkeyGesture::new(Modifiers::NONE, 'Z' as u32),
        "mode_zoom = Z"
    );
    assert_eq!(
        h.mode_snip,
        HotkeyGesture::new(Modifiers::NONE, 'C' as u32),
        "mode_snip = C"
    );
    assert_eq!(
        h.snip_copy,
        HotkeyGesture::new(Modifiers::CTRL, 'C' as u32)
    );
    assert_eq!(h.cancel, HotkeyGesture::new(Modifiers::NONE, 0x1B));
    assert_eq!(h.reset_zoom, HotkeyGesture::new(Modifiers::NONE, '0' as u32));
    assert_eq!(h.spotlight_radius_modifier, Modifiers::CTRL);
    assert_eq!(
        h.zoom_modifier,
        Modifiers::SHIFT,
        "zoom_modifier = Shift (new field, SHARED API SPEC)"
    );
}

#[test]
fn default_display_strings_match_docs() {
    let h = HotkeySettings::default();
    let expected = [
        "Win+F", // freeze_toggle
        "S",     // mode_spotlight
        "Z",     // mode_zoom
        "C",     // mode_snip
        "Ctrl+C", // snip_copy
        "Esc",   // cancel
        "0",     // reset_zoom
    ];
    for ((name, g), want) in gesture_fields(&h).into_iter().zip(expected) {
        assert_eq!(g.to_display(), want, "{name} display string");
    }
    assert_eq!(h.spotlight_radius_modifier.to_display(), "Ctrl");
    assert_eq!(h.zoom_modifier.to_display(), "Shift");
}

#[test]
fn every_default_parses_and_reserializes_to_identical_display_string() {
    let h = HotkeySettings::default();
    // Includes the auto-registered Shift+mode variants: "Shift+S" etc. must
    // be well-formed registerable gestures too.
    for (name, g) in all_registered(&h) {
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

    // Both modifier-only defaults get the same parse/serde round-trip treatment.
    for (name, m) in [
        ("spotlight_radius_modifier", h.spotlight_radius_modifier),
        ("zoom_modifier", h.zoom_modifier),
    ] {
        let display = m.to_display();
        assert_eq!(
            Modifiers::parse(&display).expect("parse modifier display"),
            m,
            "{name}: parse(to_display) must be identity"
        );
        let json = serde_json::to_string(&m).expect("serialize modifiers");
        assert_eq!(json, format!("\"{display}\""));
        assert_eq!(
            serde_json::from_str::<Modifiers>(&json).expect("deserialize modifiers"),
            m,
            "{name}: serde round-trip"
        );
    }
}

#[test]
fn defaults_are_pairwise_non_conflicting() {
    let h = HotkeySettings::default();

    // HotkeyGesture equality is exact (modifiers + vk), so equality doubles as
    // conflict detection (gesture.rs contract). The check spans ALL registered
    // bindings: the 7 settings gestures AND the 3 auto-registered Shift+mode
    // variants (S vs Shift+S differ, but Shift+S must not collide with
    // anything else, e.g. a Ctrl+Shift/Ctrl+Alt binding).
    let registered = all_registered(&h);
    let mut seen = HashSet::new();
    for (name, g) in &registered {
        assert!(seen.insert(g), "duplicate registered hotkey at {name}: {g:?}");
    }
    for (i, (name_a, a)) in registered.iter().enumerate() {
        for (name_b, b) in registered.iter().skip(i + 1) {
            assert_ne!(a, b, "conflicting defaults: {name_a} vs {name_b}");
        }
    }
}

#[test]
fn shift_mode_variants_are_exactly_shift_ored_mode_keys() {
    // The auto-registered add-layer variants share the mode key with Shift
    // added; with bare-key defaults that is exactly "Shift+<mode key>".
    let h = HotkeySettings::default();
    let expected = [
        ("mode_spotlight+Shift", "Shift+S"),
        ("mode_zoom+Shift", "Shift+Z"),
        ("mode_snip+Shift", "Shift+C"),
    ];
    for ((name, g), (want_name, want_display)) in
        shift_mode_variants(&h).into_iter().zip(expected)
    {
        assert_eq!(name, want_name);
        assert_eq!(g.to_display(), want_display, "{name}");
        assert!(g.modifiers.contains(Modifiers::SHIFT), "{name} holds Shift");
        assert!(g.is_registerable(), "{name}");
    }
    // Each Shift variant differs from its plain mode key (no self-collision).
    for (name, g) in gesture_fields(&h) {
        assert_ne!(g, shift_variant(g), "{name} vs its Shift variant");
    }
}

#[test]
fn whole_app_settings_serde_round_trip() {
    let defaults = AppSettings::default();
    let json = serde_json::to_string(&defaults).expect("serialize AppSettings");
    let back: AppSettings = serde_json::from_str(&json).expect("deserialize AppSettings");
    assert_eq!(back, defaults, "full settings model serde round-trip");
}
