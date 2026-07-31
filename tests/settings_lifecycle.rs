//! Scenario (a): settings lifecycle.
//!
//! defaults => save => hand-edit JSONC text => load => edited values + default merge.
//!
//! Headless-safe: uses unique temp dirs under `std::env::temp_dir()`, cleaned up
//! by `TempDirGuard`. Never touches the real `settings.json` next to the exe
//! (`default_settings_path` is intentionally NOT exercised here — it resolves
//! next to the test-host exe and could litter; its logic is one
//! `current_exe` call covered by manual QA).

mod common;

use common::TempDirGuard;
use spotfreeze::hotkeys::gesture::{HotkeyGesture, Modifiers};
use spotfreeze::settings::model::{AppSettings, Rgb};
use spotfreeze::settings::store;

#[test]
fn documented_default_values() {
    // Defaults documented per-field in src/settings/model.rs (out-of-box contract).
    let s = AppSettings::default();
    assert_eq!(s.spotlight.default_radius, 150, "spotlight.default_radius");
    assert_eq!(s.zoom.step_factor, 1.25, "zoom.step_factor");
    assert_eq!(s.zoom.min, 1.0, "zoom.min");
    assert_eq!(s.zoom.max, 16.0, "zoom.max");
    assert_eq!(s.overlay.dim_opacity, 160, "overlay.dim_opacity");
    assert_eq!(
        s.overlay.snip_dim_opacity, 90,
        "overlay.snip_dim_opacity (capture veil, much lighter)"
    );
    assert_eq!(
        s.overlay.snip_color,
        Rgb {
            r: 0x16,
            g: 0x28,
            b: 0x3A
        },
        "overlay.snip_color (cool dark slate)"
    );
    // Rework additions (composable modes update, SHARED API SPEC):
    assert_eq!(
        s.overlay.color,
        Rgb { r: 0, g: 0, b: 0 },
        "overlay.color default = black"
    );
    assert_eq!(
        s.hotkeys.zoom_modifier,
        Modifiers::SHIFT,
        "hotkeys.zoom_modifier default = Shift"
    );
    assert_eq!(
        s.hotkeys.zoom_hold,
        HotkeyGesture::new(Modifiers::NONE, 'F' as u32),
        "hotkeys.zoom_hold default = F"
    );
    assert!(!s.auto_start, "auto_start default = false");
}

#[test]
fn missing_file_loads_defaults_and_materializes_template() {
    let (dir, _guard) = TempDirGuard::create("load_missing");
    let path = dir.join("settings.json");
    assert!(!path.exists());

    // Contract: file missing => create from template (best effort) + return defaults.
    let loaded = store::load(&path).expect("load of missing file returns defaults");
    assert_eq!(loaded, AppSettings::default());
    assert!(
        path.exists(),
        "load must materialize settings.json from the template"
    );

    // The materialized file parses back to defaults.
    let reloaded = store::load(&path).expect("reload materialized file");
    assert_eq!(reloaded, AppSettings::default());
}

#[test]
fn defaults_save_hand_edited_jsonc_loads_with_default_merge() {
    let (dir, _guard) = TempDirGuard::create("lifecycle");
    let path = dir.join("settings.json");

    // 1) defaults => save (atomic: no leftover .tmp afterwards)
    let defaults = AppSettings::default();
    store::save(&path, &defaults).expect("save defaults");
    assert!(path.exists(), "save writes settings.json");
    assert!(
        !dir.join("settings.json.tmp").exists(),
        "atomic save must not leave the .tmp file behind"
    );
    assert_eq!(
        store::load(&path).expect("reload saved defaults"),
        defaults,
        "saved defaults round-trip identically"
    );

    // 2) user hand-edits the JSONC text: comments, trailing commas, one rebound
    //    hotkey, changed numeric values, and an intentionally OMITTED section.
    let hand_edited = r#"{
  // user: bigger spotlight circle
  "spotlight": { "default_radius": 222, },
  "hotkeys": {
    "freeze_toggle": "Ctrl+Shift+Q", // user rebind
  },
  "overlay": { "dim_opacity": 200, },
  // "zoom" section intentionally omitted -> defaults must merge in
}
"#;
    std::fs::write(&path, hand_edited).expect("hand-edit the file");

    // 3) load => edited values win, everything else falls back to defaults
    let loaded = store::load(&path).expect("load hand-edited JSONC");
    assert_eq!(loaded.spotlight.default_radius, 222, "edited value wins");
    assert_eq!(loaded.overlay.dim_opacity, 200, "edited value wins");
    assert_eq!(
        loaded.hotkeys.freeze_toggle,
        HotkeyGesture::new(Modifiers::CTRL | Modifiers::SHIFT, 'Q' as u32),
        "rebound hotkey parses from its display string"
    );

    // default merge: untouched hotkeys keep their defaults
    let dh = AppSettings::default().hotkeys;
    assert_eq!(loaded.hotkeys.mode_spotlight, dh.mode_spotlight);
    assert_eq!(loaded.hotkeys.mode_snip, dh.mode_snip);
    assert_eq!(loaded.hotkeys.snip_copy, dh.snip_copy);
    assert_eq!(loaded.hotkeys.cancel, dh.cancel);
    assert_eq!(loaded.hotkeys.reset_zoom, dh.reset_zoom);
    assert_eq!(
        loaded.hotkeys.spotlight_radius_modifier, dh.spotlight_radius_modifier,
        "modifier-only binding merges from defaults"
    );

    // omitted section: full defaults
    assert_eq!(loaded.zoom, AppSettings::default().zoom);
}

#[test]
fn template_of_defaults_round_trips() {
    let (dir, _guard) = TempDirGuard::create("template_rt");
    let path = dir.join("settings.json");
    let text = store::to_jsonc_template(&AppSettings::default());
    std::fs::write(&path, text).expect("write template text");
    assert_eq!(
        store::load(&path).expect("template must be valid JSONC"),
        AppSettings::default(),
        "to_jsonc_template(defaults) parses back to defaults"
    );
}

#[test]
fn malformed_jsonc_errors_instead_of_panicking() {
    let (dir, _guard) = TempDirGuard::create("malformed");
    let path = dir.join("settings.json");
    std::fs::write(&path, "{ this is not jsonc ").expect("write garbage");
    assert!(
        store::load(&path).is_err(),
        "malformed JSONC => Err (caller falls back to defaults)"
    );
}
