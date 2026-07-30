//! Scenario (rework-c): settings backward compatibility.
//!
//! A `settings.json` written by the PRE-REWORK app (old schema: `overlay`
//! with `dim_opacity` only — no `color`; `hotkeys` without `zoom_modifier`;
//! old default hotkeys Ctrl+Alt+F / 1 / 2 / 3) must still load under the new
//! model: the missing `overlay.color` falls back to black and the missing
//! `hotkeys.zoom_modifier` falls back to Shift (serde `#[serde(default)]`
//! merge contract, src/settings/model.rs).
//!
//! Uses the real JSONC load path (`settings::store::load`) with unique temp
//! dirs — headless-safe.

mod common;

use common::TempDirGuard;
use spotfreeze::hotkeys::gesture::{HotkeyGesture, Modifiers};
use spotfreeze::settings::model::{AppSettings, Rgb};
use spotfreeze::settings::store;

const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

/// A full settings file as the pre-rework app would have written it (old
/// defaults, every old key present, none of the new keys).
const OLD_SCHEMA_FULL: &str = r#"{
  "hotkeys": {
    "freeze_toggle": "Ctrl+Alt+F",
    "mode_spotlight": "1",
    "mode_zoom": "2",
    "mode_snip": "3",
    // modifier-only binding: key HELD while scrolling the wheel to resize the circle (not a full hotkey)
    "spotlight_radius_modifier": "Ctrl",
    "snip_copy": "Ctrl+C",
    "cancel": "Esc",
    "reset_zoom": "0"
  },
  "spotlight": {
    // physical pixels on the monitor under the cursor
    "default_radius": 150
  },
  "zoom": {
    // zoom multiplier per wheel notch (must be > 1.0)
    "step_factor": 1.25,
    "min": 1.0,
    "max": 16.0
  },
  "overlay": {
    // 0 = invisible veil, 255 = fully black
    "dim_opacity": 160
  }
}
"#;

#[test]
fn old_schema_full_file_loads_with_new_key_defaults() {
    let (dir, _guard) = TempDirGuard::create("compat_full");
    let path = dir.join("settings.json");
    std::fs::write(&path, OLD_SCHEMA_FULL).expect("write old-schema file");

    let loaded = store::load(&path).expect("old schema must still load");

    // NEW keys merge in with their documented defaults.
    assert_eq!(loaded.overlay.color, BLACK, "missing color => black default");
    assert_eq!(
        loaded.hotkeys.zoom_modifier,
        Modifiers::SHIFT,
        "missing zoom_modifier => Shift default"
    );
    assert_eq!(
        loaded.hotkeys.zoom_hold,
        HotkeyGesture::new(Modifiers::NONE, 'F' as u32),
        "missing zoom_hold => F default"
    );
    assert!(!loaded.auto_start, "missing auto_start => false default");

    // OLD values are preserved exactly as written (user's bindings survive).
    assert_eq!(loaded.overlay.dim_opacity, 160);
    assert_eq!(
        loaded.hotkeys.freeze_toggle,
        HotkeyGesture::new(Modifiers::CTRL | Modifiers::ALT, 'F' as u32),
        "old Ctrl+Alt+F binding still parses and is kept"
    );
    assert_eq!(
        loaded.hotkeys.mode_spotlight,
        HotkeyGesture::new(Modifiers::NONE, '1' as u32)
    );
    assert_eq!(
        loaded.hotkeys.mode_snip,
        HotkeyGesture::new(Modifiers::NONE, '3' as u32)
    );
    assert_eq!(loaded.hotkeys.spotlight_radius_modifier, Modifiers::CTRL);
    assert_eq!(loaded.spotlight.default_radius, 150);
    assert_eq!(loaded.zoom, AppSettings::default().zoom);
}

#[test]
fn old_schema_minimal_overlay_section_merges_color_default() {
    let (dir, _guard) = TempDirGuard::create("compat_minimal");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{ "overlay": { "dim_opacity": 200 } }"#)
        .expect("write minimal old-schema file");

    let loaded = store::load(&path).expect("minimal old schema loads");
    assert_eq!(loaded.overlay.dim_opacity, 200, "edited value wins");
    assert_eq!(loaded.overlay.color, BLACK, "color default merges in");
    assert_eq!(loaded.hotkeys.zoom_modifier, Modifiers::SHIFT);
    // Everything else is the NEW defaults.
    let defaults = AppSettings::default();
    assert_eq!(loaded.hotkeys.freeze_toggle, defaults.hotkeys.freeze_toggle);
    assert_eq!(loaded.hotkeys.mode_spotlight, defaults.hotkeys.mode_spotlight);
    assert_eq!(loaded.spotlight, defaults.spotlight);
    assert_eq!(loaded.zoom, defaults.zoom);
}

#[test]
fn old_schema_hotkeys_section_without_zoom_modifier_merges_shift() {
    let (dir, _guard) = TempDirGuard::create("compat_hotkeys");
    let path = dir.join("settings.json");
    // Old hotkeys section with a user rebind but no zoom_modifier key.
    std::fs::write(
        &path,
        r#"{ "hotkeys": { "freeze_toggle": "Ctrl+Shift+Q", "mode_snip": "F6" } }"#,
    )
    .expect("write old hotkeys file");

    let loaded = store::load(&path).expect("old hotkeys section loads");
    assert_eq!(
        loaded.hotkeys.freeze_toggle,
        HotkeyGesture::new(Modifiers::CTRL | Modifiers::SHIFT, 'Q' as u32)
    );
    assert_eq!(
        loaded.hotkeys.mode_snip,
        HotkeyGesture::new(Modifiers::NONE, 0x75), // VK_F6
        "old F-key rebind still parses"
    );
    assert_eq!(
        loaded.hotkeys.zoom_modifier,
        Modifiers::SHIFT,
        "zoom_modifier default merges in"
    );
    assert_eq!(loaded.hotkeys.spotlight_radius_modifier, Modifiers::CTRL);
}

#[test]
fn new_keys_are_honored_when_present() {
    let (dir, _guard) = TempDirGuard::create("compat_new_keys");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r##"{
  "overlay": { "dim_opacity": 90, "color": "#802020" },
  "hotkeys": { "zoom_modifier": "Alt" },
}"##,
    )
    .expect("write new-schema file");

    let loaded = store::load(&path).expect("new keys load");
    assert_eq!(loaded.overlay.dim_opacity, 90);
    assert_eq!(
        loaded.overlay.color,
        Rgb {
            r: 0x80,
            g: 0x20,
            b: 0x20
        },
        "color parses from #RRGGBB"
    );
    assert_eq!(loaded.hotkeys.zoom_modifier, Modifiers::ALT);
}

#[test]
fn new_schema_round_trip_preserves_color_and_zoom_modifier() {
    // The new template (with the new keys) saves and reloads identically.
    let (dir, _guard) = TempDirGuard::create("compat_rt");
    let path = dir.join("settings.json");
    let mut settings = AppSettings::default();
    settings.overlay.color = Rgb {
        r: 0x80,
        g: 0x20,
        b: 0x20,
    };
    settings.hotkeys.zoom_modifier = Modifiers::ALT | Modifiers::SHIFT;

    store::save(&path, &settings).expect("save");
    assert_eq!(
        store::load(&path).expect("reload"),
        settings,
        "new keys survive the save/load round-trip"
    );
}

#[test]
fn zoom_hold_and_auto_start_are_honored_when_present() {
    let (dir, _guard) = TempDirGuard::create("compat_newest_keys");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r#"{ "hotkeys": { "zoom_hold": "Ctrl+G" }, "auto_start": true }"#,
    )
    .expect("write file");

    let loaded = store::load(&path).expect("new keys load");
    assert_eq!(
        loaded.hotkeys.zoom_hold,
        HotkeyGesture::new(Modifiers::CTRL, 'G' as u32)
    );
    assert!(loaded.auto_start);

    store::save(&path, &loaded).expect("save");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("\"zoom_hold\": \"Ctrl+G\""), "{on_disk}");
    assert!(on_disk.contains("\"auto_start\": true"), "{on_disk}");
    assert_eq!(store::load(&path).expect("reload"), loaded);
}
