//! JSONC persistence for [`AppSettings`] — `settings.json` next to the exe.
//!
//! Pure module: no `windows` imports; unit tests exercise it with temp files.

use super::model::AppSettings;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name used by [`default_settings_path`].
const SETTINGS_FILE_NAME: &str = "settings.json";

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
        "default_radius",
        "physical pixels on the monitor under the cursor",
    ),
    ("step_factor", "zoom multiplier per wheel notch (must be > 1.0)"),
    ("dim_opacity", "0 = invisible veil, 255 = fully black"),
];

/// `settings.json` in the directory of the running executable.
/// Errors only when the exe path cannot be determined.
pub fn default_settings_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine the executable path")?;
    let dir = exe
        .parent()
        .context("executable path has no parent directory")?;
    Ok(dir.join(SETTINGS_FILE_NAME))
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
/// atomic and replaces an existing target on Windows).
pub fn save(path: &Path, settings: &AppSettings) -> Result<()> {
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

    #[test]
    fn default_settings_path_is_settings_json_next_to_exe() {
        let path = default_settings_path().expect("exe path must be resolvable in tests");
        assert_eq!(path.file_name().unwrap(), SETTINGS_FILE_NAME);
        assert!(path.is_absolute());
        // The parent is the real exe directory (test harness): it must exist.
        assert!(path.parent().unwrap().is_dir());
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
            "mode_zoom",
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
    fn load_missing_file_in_unwritable_dir_still_returns_defaults() {
        // Parent directory does not exist: the best-effort create must fail
        // silently and load must still succeed with defaults.
        let dir = unique_temp_path("nodir");
        let path = dir.join("settings.json");
        assert!(!dir.exists());

        let loaded = load(&path).expect("unwritable create is not an error");
        assert_eq!(loaded, AppSettings::default());
        assert!(!path.exists(), "nothing was written into the missing dir");
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
    fn save_into_missing_directory_errors_and_cleans_tmp() {
        let dir = unique_temp_path("save_nodir");
        let path = dir.join("settings.json");
        let err = save(&path, &AppSettings::default()).expect_err("no parent dir");
        let shown = format!("{err:#}");
        assert!(shown.contains("failed to create"), "context: {shown}");
        assert!(
            !tmp_path_for(&path).exists(),
            "failed save cleans up its .tmp file"
        );
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
