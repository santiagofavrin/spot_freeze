//! JSONC persistence for [`AppSettings`] — `spotfreeze.jsonc` in the
//! per-platform config location.
//!
//! Pure module: no OS imports; unit tests exercise it with temp files.

use super::model::AppSettings;
use anyhow::{Context, Result, anyhow};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name used by [`default_settings_path`].
const SETTINGS_FILE_NAME: &str = "spotfreeze.jsonc";
/// File names used by earlier releases: migrated forward by
/// [`migrate_legacy_settings`] when the current file is absent.
const LEGACY_SETTINGS_FILE_NAMES: &[&str] = &["spotfreeze.json", "settings.json"];

/// `//` comments injected into the template, keyed by the JSON key they sit
/// above. ONLY non-obvious keys (units, ranges, modifier-only semantics) get
/// one; self-explanatory keys (the hotkey bindings) stay uncommented.
/// Key names must be unique across the whole serialized settings document.
const KEY_COMMENTS: &[(&str, &str)] = &[
    (
        "spotlight_radius_modifier",
        "modifier-only binding: key HELD while scrolling the wheel to resize the circle (not a full hotkey)",
    ),
    (
        "zoom_modifier",
        "modifier-only binding: key HELD while scrolling the wheel to zoom from any mode (not a full hotkey)",
    ),
    (
        "default_radius",
        "physical pixels on the monitor under the cursor",
    ),
    ("step_factor", "zoom multiplier per wheel notch (must be > 1.0)"),
    ("dim_opacity", "0 = invisible veil, 255 = fully opaque"),
    ("color", "veil color as #RRGGBB hex"),
];

/// `spotfreeze.jsonc` in the platform's conventional per-user config location:
/// `%APPDATA%\SpotFreeze\` on Windows;
/// `$XDG_CONFIG_HOME/spotfreeze/` (falling back to `~/.config/spotfreeze/`)
/// on Linux; `~/Library/Application Support/SpotFreeze/` on macOS.
/// Errors only when the location cannot be determined.
///
/// The environment is read through the small `*_config_dir` helpers below,
/// which take the variable values explicitly so the decision logic is
/// unit-testable without mutating process env.
pub fn default_settings_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let dir = windows_config_dir(std::env::var_os("APPDATA")).context("APPDATA is not set")?;
        Ok(dir.join(SETTINGS_FILE_NAME))
    }
    #[cfg(target_os = "linux")]
    {
        let dir = linux_config_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        Ok(dir.join(SETTINGS_FILE_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        let dir = macos_config_dir(std::env::var_os("HOME")).context("HOME is not set")?;
        Ok(dir.join(SETTINGS_FILE_NAME))
    }
}

/// `%APPDATA%\SpotFreeze`. `None` when `APPDATA` is unset or empty.
#[cfg(windows)]
fn windows_config_dir(appdata: Option<OsString>) -> Option<PathBuf> {
    appdata
        .filter(|v| !v.is_empty())
        .map(|appdata| PathBuf::from(appdata).join("SpotFreeze"))
}

/// `$XDG_CONFIG_HOME/spotfreeze`, falling back to `~/.config/spotfreeze` when
/// `XDG_CONFIG_HOME` is unset or empty (the freedesktop rule). `None` when
/// neither variable is usable.
#[cfg(target_os = "linux")]
fn linux_config_dir(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("spotfreeze"));
    }
    home.filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config").join("spotfreeze"))
}

/// `~/Library/Application Support/SpotFreeze`. `None` when `HOME` is unset or
/// empty.
#[cfg(target_os = "macos")]
fn macos_config_dir(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("SpotFreeze")
    })
}

/// Move a legacy settings file (`spotfreeze.json` or `settings.json`) over to
/// `path` when `path` does not exist yet: beside the executable on Windows
/// (its oldest location), next to `path` otherwise. Best effort and
/// infallible — any failure leaves the legacy file untouched and the caller's
/// [`load`] falls back to defaults.
pub fn migrate_legacy_settings(path: &Path) {
    if path.exists() {
        return;
    }
    let Some(legacy) = legacy_settings_paths(path)
        .into_iter()
        .find(|candidate| candidate.is_file())
    else {
        return;
    };
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if fs::rename(&legacy, path).is_err()
        && fs::copy(&legacy, path).is_ok()
    {
        // Cross-filesystem fallback (e.g. exe and APPDATA on different drives).
        let _ = fs::remove_file(&legacy);
    }
}

/// Legacy settings locations for this platform, most recent first (see
/// [`migrate_legacy_settings`]).
fn legacy_settings_paths(new_path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = LEGACY_SETTINGS_FILE_NAMES
        .iter()
        .map(|name| new_path.with_file_name(name))
        .collect();
    candidates.extend(oldest_legacy_settings_path());
    candidates
}

/// The oldest settings location (pre-config-folder releases): `settings.json`
/// beside the executable. Only Windows releases ever used it.
fn oldest_legacy_settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("settings.json"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Load settings from `path`.
///
/// * File missing → create it from [`to_jsonc_template`] with defaults (best
///   effort: an unwritable directory is NOT an error) and return defaults.
/// * Individual missing keys → their defaults (serde `#[serde(default)]`).
/// * Comments and trailing commas are tolerated (JSONC via `jsonc-parser`).
/// * Malformed JSONC → `Err` carrying the parser's line/column info; the caller
///   (app) is expected to fall back to defaults and keep running.
pub fn load(path: &Path) -> Result<AppSettings> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let defaults = AppSettings::default();
            // Best effort: materialize a commented template for the user to edit.
            let _ = save(path, &defaults);
            return Ok(defaults);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    parse_jsonc(&text).with_context(|| format!("malformed JSONC in {}", path.display()))
}

/// Parse JSONC text into [`AppSettings`], tolerating a UTF-8 BOM, comments,
/// and trailing commas. Empty/whitespace-only text yields defaults.
fn parse_jsonc(text: &str) -> Result<AppSettings> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    if text.trim().is_empty() {
        return Ok(AppSettings::default());
    }
    // ParseOptions::default() allows comments and trailing commas.
    // ParseError's Display carries "on line X column Y" info.
    // The crate's `serde_json` feature converts the AST into a
    // `serde_json::Value`; serde `#[serde(default)]` fills missing keys.
    let ast = jsonc_parser::parse_to_ast(
        text,
        &jsonc_parser::CollectOptions::default(),
        &jsonc_parser::ParseOptions::default(),
    )
    .map_err(|e| anyhow!("{e}"))?;
    match ast.value {
        None => Ok(AppSettings::default()), // no root value (e.g. only comments)
        Some(value) => {
            let json: serde_json::Value = value.into();
            serde_json::from_value(json).context("settings data does not match the schema")
        }
    }
}

/// Atomically persist `settings` to `path`: serialize via [`to_jsonc_template`],
/// write `<path>.tmp`, then rename over `path` (same directory, so the rename is
/// atomic and replaces an existing target on Windows). Missing parent
/// directories are created first.
pub fn save(path: &Path, settings: &AppSettings) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp_path = tmp_path_for(path);
    let text = to_jsonc_template(settings);

    let write_result = (|| -> Result<()> {
        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e).with_context(|| {
            format!(
                "failed to rename {} over {}",
                tmp_path.display(),
                path.display()
            )
        });
    }
    Ok(())
}

/// `<path>.tmp` — sibling of `path`, so the rename stays on one volume.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Serialize to JSONC text with super-brief `//` comments ONLY above non-obvious
/// keys (units, ranges, modifier-only semantics). Self-explanatory keys (the
/// hotkey bindings) get no comment.
pub fn to_jsonc_template(settings: &AppSettings) -> String {
    // Serializing AppSettings is infallible (plain data + string gestures).
    let json = serde_json::to_string_pretty(settings)
        .expect("AppSettings serialization cannot fail");

    let mut out = String::with_capacity(json.len() + 256);
    for line in json.lines() {
        let trimmed = line.trim_start();
        for &(key, comment) in KEY_COMMENTS {
            if trimmed.starts_with(&format!("\"{key}\":")) {
                let indent = &line[..line.len() - trimmed.len()];
                out.push_str(indent);
                out.push_str("// ");
                out.push_str(comment);
                out.push('\n');
                break; // at most one comment per line
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique temp file path; never collides across tests or processes.
    fn unique_temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "spotfreeze_store_test_{}_{}_{}_{}.json",
            std::process::id(),
            tag,
            n,
            nanos
        ))
    }

    /// Temp file that removes itself (and any `.tmp` sibling) on drop,
    /// even when a test panics.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            Self(unique_temp_path(tag))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(tmp_path_for(&self.0));
        }
    }

    // ---------- default_settings_path ----------

    #[cfg(windows)]
    #[test]
    fn default_settings_path_is_spotfreeze_json_in_appdata() {
        let path = default_settings_path().expect("APPDATA is set on any real Windows session");
        assert_eq!(path.file_name().unwrap(), SETTINGS_FILE_NAME);
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "SpotFreeze");
        assert!(path.is_absolute());
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_dir_prefers_appdata() {
        let dir = windows_config_dir(Some(r"C:\Users\u\AppData\Roaming".into())).unwrap();
        assert_eq!(dir, PathBuf::from(r"C:\Users\u\AppData\Roaming\SpotFreeze"));
        assert_eq!(windows_config_dir(None), None);
        assert_eq!(windows_config_dir(Some("".into())), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_settings_path_is_spotfreeze_json_in_the_config_dir() {
        let path = default_settings_path().expect("HOME is set in any real session");
        assert_eq!(path.file_name().unwrap(), SETTINGS_FILE_NAME);
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "spotfreeze");
        assert!(path.is_absolute());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_config_dir_prefers_xdg_config_home() {
        let dir = linux_config_dir(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/xdg/spotfreeze"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_config_dir_falls_back_to_dot_config_when_xdg_missing_or_empty() {
        for xdg in [None, Some("".into())] {
            let dir = linux_config_dir(xdg, Some("/home/u".into())).unwrap();
            assert_eq!(dir, PathBuf::from("/home/u/.config/spotfreeze"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_config_dir_is_none_without_any_base() {
        assert_eq!(linux_config_dir(None, None), None);
        assert_eq!(linux_config_dir(Some("".into()), Some("".into())), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_config_dir_is_application_support() {
        let dir = macos_config_dir(Some("/Users/u".into())).unwrap();
        assert_eq!(
            dir,
            PathBuf::from("/Users/u/Library/Application Support/SpotFreeze")
        );
        assert_eq!(macos_config_dir(None), None);
        assert_eq!(macos_config_dir(Some("".into())), None);
    }

    // ---------- template ----------

    #[test]
    fn template_comments_only_on_intended_keys() {
        let template = to_jsonc_template(&AppSettings::default());
        // Exactly one `//` comment per registered key, nothing else.
        let comment_lines: Vec<&str> = template
            .lines()
            .filter(|l| l.trim_start().starts_with("//"))
            .collect();
        assert_eq!(
            comment_lines.len(),
            KEY_COMMENTS.len(),
            "template must contain exactly the intended comments: {template}"
        );
        for &(key, comment) in KEY_COMMENTS {
            assert!(
                comment_lines.iter().any(|l| l.contains(comment)),
                "missing comment for {key}"
            );
            // The comment sits directly above its key, with matching indent.
            let needle = format!("// {comment}\n");
            let pos = template.find(&needle).expect("comment line present");
            let next_line_start = pos + needle.len();
            let next_line = template[next_line_start..]
                .lines()
                .next()
                .expect("key line follows comment");
            assert!(
                next_line.trim_start().starts_with(&format!("\"{key}\":")),
                "comment for {key} must sit directly above the key line: {next_line}"
            );
        }
        // Hotkey bindings stay uncommented (self-explanatory).
        for hotkey_key in [
            "freeze_toggle",
            "mode_spotlight",
            "mode_snip",
            "snip_copy",
            "cancel",
            "reset_zoom",
        ] {
            let key_line_pos = template
                .find(&format!("\"{hotkey_key}\":"))
                .expect("hotkey key present");
            let before = &template[..key_line_pos];
            let prev_line = before.trim_end().lines().last().unwrap_or("");
            assert!(
                !prev_line.trim_start().starts_with("//"),
                "{hotkey_key} must not have a comment above it"
            );
        }
    }

    #[test]
    fn template_ends_with_newline_and_is_parseable() {
        let template = to_jsonc_template(&AppSettings::default());
        assert!(template.ends_with('\n'));
        assert!(template.starts_with('{'));
    }

    #[test]
    fn template_contains_new_keys_with_defaults() {
        let template = to_jsonc_template(&AppSettings::default());
        assert!(
            template.contains("\"zoom_modifier\": \"Shift\""),
            "zoom_modifier appears with its default: {template}"
        );
        assert!(
            template.contains("\"color\": \"#000000\""),
            "color appears as hex with its default: {template}"
        );
        assert!(
            template.contains("\"freeze_toggle\": \"Win+F\""),
            "new freeze hotkey default: {template}"
        );
        for (key, value) in [("mode_spotlight", "S"), ("mode_snip", "C")] {
            assert!(
                template.contains(&format!("\"{key}\": \"{value}\"")),
                "{key} default in template"
            );
        }
    }

    // ---------- round-trip ----------

    #[test]
    fn default_round_trip_through_template() {
        let defaults = AppSettings::default();
        let parsed = parse_jsonc(&to_jsonc_template(&defaults)).expect("template must parse");
        assert_eq!(parsed, defaults);
    }

    #[test]
    fn save_then_load_round_trip() {
        let tmp = TempFile::new("roundtrip");
        let mut settings = AppSettings::default();
        settings.spotlight.default_radius = 222;
        settings.zoom.max = 8.0;
        settings.overlay.dim_opacity = 90;

        save(tmp.path(), &settings).expect("save");
        let loaded = load(tmp.path()).expect("load");
        assert_eq!(loaded, settings);
    }

    // ---------- missing file ----------

    #[test]
    fn load_creates_missing_file_from_template_and_returns_defaults() {
        let tmp = TempFile::new("missing");
        assert!(!tmp.path().exists());

        let loaded = load(tmp.path()).expect("missing file is not an error");
        assert_eq!(loaded, AppSettings::default());

        // The file was materialized with the commented default template.
        let on_disk = fs::read_to_string(tmp.path()).expect("file created");
        assert_eq!(on_disk, to_jsonc_template(&AppSettings::default()));

        // A second load reads what was written — same result.
        let loaded_again = load(tmp.path()).expect("reload");
        assert_eq!(loaded_again, loaded);
    }

    #[test]
    fn load_missing_file_in_missing_dir_materializes_template_and_returns_defaults() {
        // The parent directory is created on demand, so the template lands
        // even when the whole config path did not exist yet.
        let dir = unique_temp_path("nodir");
        let path = dir.join("spotfreeze.jsonc");
        assert!(!dir.exists());

        let loaded = load(&path).expect("missing dir is not an error");
        assert_eq!(loaded, AppSettings::default());
        assert_eq!(
            fs::read_to_string(&path).expect("template written"),
            to_jsonc_template(&AppSettings::default())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------- partial JSON merges with defaults ----------

    #[test]
    fn partial_json_merges_with_defaults() {
        let tmp = TempFile::new("partial");
        fs::write(tmp.path(), r#"{ "zoom": { "max": 8.0 } }"#).unwrap();

        let loaded = load(tmp.path()).expect("partial settings parse");
        assert_eq!(loaded.zoom.max, 8.0);
        // Every untouched key keeps its default.
        let defaults = AppSettings::default();
        assert_eq!(loaded.zoom.step_factor, defaults.zoom.step_factor);
        assert_eq!(loaded.zoom.min, defaults.zoom.min);
        assert_eq!(loaded.hotkeys, defaults.hotkeys);
        assert_eq!(loaded.spotlight, defaults.spotlight);
        assert_eq!(loaded.overlay, defaults.overlay);
    }

    #[test]
    fn partial_nested_section_merges_with_defaults() {
        let tmp = TempFile::new("partial_nested");
        fs::write(tmp.path(), r#"{ "hotkeys": { "cancel": "Q" } }"#).unwrap();

        let loaded = load(tmp.path()).expect("partial hotkeys parse");
        assert_eq!(
            loaded.hotkeys.cancel,
            crate::hotkeys::gesture::HotkeyGesture::parse("Q").unwrap()
        );
        assert_eq!(
            loaded.hotkeys.freeze_toggle,
            AppSettings::default().hotkeys.freeze_toggle
        );
    }

    #[test]
    fn old_file_without_color_or_zoom_modifier_loads_with_defaults() {
        // Backward compat: a settings.json written before `color` and
        // `zoom_modifier` existed must load, keep its explicit values, and
        // get the new keys' defaults.
        let tmp = TempFile::new("old_file");
        fs::write(
            tmp.path(),
            r#"{
    "hotkeys": {
        "freeze_toggle": "Ctrl+Alt+F",
        "mode_spotlight": "1",
        "mode_zoom": "2",
        "mode_snip": "3",
        "spotlight_radius_modifier": "Ctrl",
        "snip_copy": "Ctrl+C",
        "cancel": "Esc",
        "reset_zoom": "0"
    },
    "overlay": { "dim_opacity": 200 },
}"#,
        )
        .unwrap();

        let loaded = load(tmp.path()).expect("old settings file must still load");
        // Explicit old values survive.
        assert_eq!(
            loaded.hotkeys.freeze_toggle,
            crate::hotkeys::gesture::HotkeyGesture::parse("Ctrl+Alt+F").unwrap()
        );
        assert_eq!(loaded.overlay.dim_opacity, 200);
        // New keys fall back to their defaults.
        assert_eq!(
            loaded.hotkeys.zoom_modifier,
            crate::hotkeys::gesture::Modifiers::SHIFT
        );
        assert_eq!(loaded.overlay.color, crate::settings::model::Rgb::BLACK);
    }

    #[test]
    fn custom_color_and_zoom_modifier_round_trip() {
        let tmp = TempFile::new("new_keys_roundtrip");
        let mut settings = AppSettings::default();
        settings.overlay.color = crate::settings::model::Rgb {
            r: 0x12,
            g: 0xAB,
            b: 0xFF,
        };
        settings.hotkeys.zoom_modifier = crate::hotkeys::gesture::Modifiers::ALT;

        save(tmp.path(), &settings).expect("save");
        let on_disk = fs::read_to_string(tmp.path()).unwrap();
        assert!(on_disk.contains("\"#12ABFF\""), "color as hex: {on_disk}");
        assert!(
            on_disk.contains("\"zoom_modifier\": \"Alt\""),
            "zoom_modifier as display string: {on_disk}"
        );
        assert_eq!(load(tmp.path()).expect("load"), settings);
    }

    #[test]
    fn malformed_color_errors() {
        let tmp = TempFile::new("bad_color");
        fs::write(tmp.path(), r#"{ "overlay": { "color": "black" } }"#).unwrap();

        let err = load(tmp.path()).expect_err("malformed color must error");
        let shown = format!("{err:#}");
        assert!(
            shown.contains("#RRGGBB"),
            "error explains the expected format: {shown}"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let tmp = TempFile::new("unknown_keys");
        fs::write(
            tmp.path(),
            r#"{ "future_key": 42, "zoom": { "max": 4.0, "also_future": true } }"#,
        )
        .unwrap();

        let loaded = load(tmp.path()).expect("unknown keys tolerated");
        assert_eq!(loaded.zoom.max, 4.0);
        assert_eq!(loaded.hotkeys, AppSettings::default().hotkeys);
    }

    #[test]
    fn empty_file_yields_defaults() {
        for (tag, content) in [("empty", ""), ("whitespace", "  \n\t  \r\n")] {
            let tmp = TempFile::new(tag);
            fs::write(tmp.path(), content).unwrap();
            let loaded = load(tmp.path()).expect("empty file yields defaults");
            assert_eq!(loaded, AppSettings::default());
        }
    }

    // ---------- JSONC tolerance ----------

    #[test]
    fn comments_and_trailing_commas_are_tolerated() {
        let tmp = TempFile::new("jsonc");
        fs::write(
            tmp.path(),
            r#"{
    // line comment
    /* block comment */
    "zoom": {
        "max": 6.0, // trailing comment
    },
    "overlay": { "dim_opacity": 200 },
}"#,
        )
        .unwrap();

        let loaded = load(tmp.path()).expect("JSONC parses");
        assert_eq!(loaded.zoom.max, 6.0);
        assert_eq!(loaded.overlay.dim_opacity, 200);
        assert_eq!(loaded.hotkeys, AppSettings::default().hotkeys);
    }

    #[test]
    fn utf8_bom_is_tolerated() {
        let tmp = TempFile::new("bom");
        let mut content = String::from("\u{FEFF}");
        content.push_str(r#"{ "zoom": { "max": 3.5 } }"#);
        fs::write(tmp.path(), content).unwrap();

        let loaded = load(tmp.path()).expect("BOM-prefixed JSONC parses");
        assert_eq!(loaded.zoom.max, 3.5);
    }

    // ---------- malformed input ----------

    #[test]
    fn malformed_jsonc_errors_with_line_and_column() {
        let tmp = TempFile::new("malformed");
        fs::write(tmp.path(), "{\n  \"zoom\": {\n").unwrap();

        let err = load(tmp.path()).expect_err("malformed JSONC must error");
        let shown = format!("{err:#}");
        assert!(
            shown.contains("malformed JSONC"),
            "error names the problem: {shown}"
        );
        assert!(
            shown.contains(tmp.path().to_string_lossy().as_ref()),
            "error names the file: {shown}"
        );
        assert!(
            shown.contains("line") && shown.contains("column"),
            "error carries the parser's line/column info: {shown}"
        );
    }

    #[test]
    fn wrong_value_type_errors() {
        let tmp = TempFile::new("wrong_type");
        fs::write(
            tmp.path(),
            r#"{ "overlay": { "dim_opacity": "black" } }"#,
        )
        .unwrap();

        let err = load(tmp.path()).expect_err("type mismatch must error");
        let shown = format!("{err:#}");
        assert!(shown.contains("malformed JSONC"), "error context: {shown}");
    }

    #[test]
    fn invalid_utf8_errors() {
        let tmp = TempFile::new("bad_utf8");
        fs::write(tmp.path(), [0xFF, 0xFE, 0x00, 0x7B]).unwrap();
        assert!(load(tmp.path()).is_err());
    }

    // ---------- save / atomicity ----------

    #[test]
    fn save_overwrites_existing_file_and_leaves_no_tmp() {
        let tmp = TempFile::new("overwrite");
        let tmp_sibling = tmp_path_for(tmp.path());

        save(tmp.path(), &AppSettings::default()).expect("first save");
        let first = fs::read_to_string(tmp.path()).unwrap();
        assert!(!tmp_sibling.exists(), "no .tmp left after save");

        let mut updated = AppSettings::default();
        updated.overlay.dim_opacity = 42;
        save(tmp.path(), &updated).expect("overwrite save");
        let second = fs::read_to_string(tmp.path()).unwrap();

        assert_ne!(first, second, "overwrite actually replaced content");
        assert_eq!(second, to_jsonc_template(&updated));
        assert!(!tmp_sibling.exists(), "no .tmp left after overwrite");
        assert_eq!(load(tmp.path()).unwrap(), updated);
    }

    #[test]
    fn save_content_matches_template_exactly() {
        let tmp = TempFile::new("content");
        let settings = AppSettings::default();
        save(tmp.path(), &settings).expect("save");
        let on_disk = fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(on_disk, to_jsonc_template(&settings));
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = unique_temp_path("save_nodir").join("nested").join("deeper");
        let path = dir.join("spotfreeze.jsonc");
        save(&path, &AppSettings::default()).expect("save creates parent dirs");
        assert!(path.is_file());
        assert!(!tmp_path_for(&path).exists());
        let _ = fs::remove_dir_all(unique_temp_path("save_nodir"));
    }

    // ---------- migrate_legacy_settings ----------

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn migration_moves_each_legacy_name_next_to_the_new_path() {
        for (tag, legacy_name) in [("json", "spotfreeze.json"), ("jsonc", "settings.json")] {
            let dir = unique_temp_path(&format!("migrate_{tag}"));
            fs::create_dir_all(&dir).unwrap();
            let legacy = dir.join(legacy_name);
            fs::write(&legacy, "{ \"spotlight\": { \"default_radius\": 42 } }").unwrap();

            let new_path = dir.join(SETTINGS_FILE_NAME);
            migrate_legacy_settings(&new_path);

            assert!(new_path.is_file(), "legacy {legacy_name} moved onto the new path");
            assert!(!legacy.exists(), "legacy file is gone after the move");
            let settings = load(&new_path).expect("migrated file loads");
            assert_eq!(settings.spotlight.default_radius, 42);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn migration_prefers_the_most_recent_legacy_name() {
        let dir = unique_temp_path("migrate_order");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("spotfreeze.json"), "{ \"spotlight\": { \"default_radius\": 42 } }")
            .unwrap();
        fs::write(dir.join("settings.json"), "{ \"spotlight\": { \"default_radius\": 7 } }")
            .unwrap();

        let new_path = dir.join(SETTINGS_FILE_NAME);
        migrate_legacy_settings(&new_path);

        let settings = load(&new_path).expect("migrated file loads");
        assert_eq!(settings.spotlight.default_radius, 42, "spotfreeze.json wins");
        assert!(dir.join("settings.json").exists(), "older legacy untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn migration_is_a_noop_when_the_new_file_exists() {
        let dir = unique_temp_path("migrate_keeps");
        fs::create_dir_all(&dir).unwrap();
        let new_path = dir.join(SETTINGS_FILE_NAME);
        fs::write(&new_path, "{}").unwrap();
        let legacy = dir.join("settings.json");
        fs::write(&legacy, "{ \"spotlight\": { \"default_radius\": 42 } }").unwrap();

        migrate_legacy_settings(&new_path);

        assert!(legacy.exists(), "existing new file wins; legacy untouched");
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "{}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn migration_is_a_noop_without_a_legacy_file() {
        let dir = unique_temp_path("migrate_none");
        let new_path = dir.join(SETTINGS_FILE_NAME);
        migrate_legacy_settings(&new_path);
        assert!(!new_path.exists());
        assert!(!dir.exists(), "no directories are created without a legacy file");
    }

    #[cfg(windows)]
    #[test]
    fn legacy_settings_paths_cover_the_exe_dir_and_both_names() {
        let paths = legacy_settings_paths(Path::new(r"C:\ignored\spotfreeze.jsonc"));
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(paths[0], Path::new(r"C:\ignored").join("spotfreeze.json"));
        assert_eq!(paths[1], Path::new(r"C:\ignored").join("settings.json"));
        assert!(paths.contains(&exe_dir.join("settings.json")));
    }

    // ---------- parse_jsonc internals ----------

    #[test]
    fn parse_jsonc_strips_bom_only_at_start() {
        // A BOM character inside a string value is data, not a BOM.
        let err = parse_jsonc("{\u{FEFF}").expect_err("BOM mid-document is not stripped");
        assert!(format!("{err:#}").contains("line"));
    }

    #[test]
    fn template_reflects_non_default_values() {
        let mut settings = AppSettings::default();
        settings.zoom.max = 12.5;
        settings.overlay.dim_opacity = 7;
        let template = to_jsonc_template(&settings);
        assert!(template.contains("\"max\": 12.5"));
        assert!(template.contains("\"dim_opacity\": 7"));
        assert_eq!(parse_jsonc(&template).unwrap(), settings);
    }
}
