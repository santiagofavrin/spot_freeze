//! Auto-start (launch at login) — the pure half: identifiers, payload text,
//! path resolution, and the enable/disable decision. No OS calls anywhere in
//! this module, so every test runs headless on any host.
//!
//! The OS side effects stay thin and per-platform: the Windows registry Run
//! key in [`crate::platform::windows`] (`apply_auto_start`) and the macOS
//! LaunchAgent plist in `platform::macos::autostart`. Linux is out of scope
//! (the `auto_start` key is documented as Windows/macOS only).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the per-OS shell must do to bring the OS registration in line with
/// the `auto_start` setting (startup reconciliation, settings-save apply).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReconcileAction {
    /// (Re)write the registration pointing at the current executable. Always
    /// rewriting covers first enable AND a stale entry after the exe moved.
    Install,
    /// Delete the registration. An absent registration is NOT an error.
    Remove,
}

/// The whole enable/disable decision: `true` installs, `false` removes.
pub fn reconcile_action(auto_start: bool) -> ReconcileAction {
    if auto_start {
        ReconcileAction::Install
    } else {
        ReconcileAction::Remove
    }
}

// ---------------------------------------------------------------------------
// Windows: current-user Run registry key
// ---------------------------------------------------------------------------

/// Subkey of `HKEY_CURRENT_USER` holding the per-user logon programs.
pub const WINDOWS_RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Value name inside the Run key — the stable app identifier.
pub const WINDOWS_VALUE_NAME: &str = "SpotFreeze";

/// The `REG_SZ` payload: the bare exe path (no arguments — a plain launch is
/// tray + hotkey), quoted so paths containing spaces survive.
pub fn windows_run_value_payload(exe_path: &Path) -> String {
    format!("\"{}\"", exe_path.display())
}

// ---------------------------------------------------------------------------
// macOS: LaunchAgent plist in ~/Library/LaunchAgents
// ---------------------------------------------------------------------------

/// LaunchAgent label (and plist file stem) — matches the packaged bundle
/// identifier (`packaging/macos/Info.plist`, `platform::macos::app`).
pub const LAUNCH_AGENT_LABEL: &str = "com.spotfreeze.app";

/// `~/Library/LaunchAgents/<label>.plist`. `None` when `home` is unset or
/// empty (same convention as the settings store's config-dir helpers).
pub fn launch_agent_plist_path(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
    })
}

/// The minimal LaunchAgent that runs the bare binary at login (SMAppService
/// would require a bundled .app, so a plist it is — even for the unpackaged
/// executable). No `KeepAlive`: a crashed/exited SpotFreeze stays down.
pub fn launch_agent_plist(program_path: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCH_AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#,
        xml_escape(&program_path.to_string_lossy())
    )
}

/// Escape the XML text-content entities (`&`, `<`, `>`). Quotes need no
/// escaping outside attribute values, and paths never become attributes here.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- reconcile_action ----------------------------------------------------

    #[test]
    fn reconcile_action_maps_the_setting() {
        assert_eq!(reconcile_action(true), ReconcileAction::Install);
        assert_eq!(reconcile_action(false), ReconcileAction::Remove);
    }

    // -- Windows Run key ------------------------------------------------------

    #[test]
    fn windows_run_key_targets_the_current_user_run_key() {
        assert_eq!(
            WINDOWS_RUN_KEY_PATH,
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
        assert_eq!(WINDOWS_VALUE_NAME, "SpotFreeze");
    }

    #[test]
    fn windows_payload_is_the_quoted_exe_path_without_arguments() {
        let exe = Path::new(r"C:\Tools\SpotFreeze\spotfreeze.exe");
        assert_eq!(
            windows_run_value_payload(exe),
            r#""C:\Tools\SpotFreeze\spotfreeze.exe""#
        );
        // Spaces in the install dir are exactly why the value is quoted.
        let spaced = Path::new(r"C:\Program Files\Spot Freeze\spotfreeze.exe");
        let payload = windows_run_value_payload(spaced);
        assert_eq!(payload, r#""C:\Program Files\Spot Freeze\spotfreeze.exe""#);
        assert!(payload.starts_with('"') && payload.ends_with('"'));
    }

    // -- LaunchAgent identity --------------------------------------------------

    #[test]
    fn launch_agent_label_is_the_stable_app_bundle_id() {
        // Same identifier as the packaged bundle (`packaging/macos/Info.plist`
        // is excluded from the Docker build context, so the consistency with
        // the in-code `BUNDLE_ID` is pinned in `platform::macos::autostart`'s
        // tests instead of reading the file here).
        assert_eq!(LAUNCH_AGENT_LABEL, "com.spotfreeze.app");
    }

    #[test]
    fn launch_agent_plist_path_lives_under_home_library_launchagents() {
        let path = launch_agent_plist_path(Some("/Users/u".into())).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/Users/u/Library/LaunchAgents/com.spotfreeze.app.plist")
        );
        assert_eq!(launch_agent_plist_path(None), None, "no HOME => no path");
        assert_eq!(
            launch_agent_plist_path(Some("".into())),
            None,
            "empty HOME => no path"
        );
    }

    // -- plist content ---------------------------------------------------------

    #[test]
    fn plist_is_a_runatload_agent_for_the_binary() {
        let plist = launch_agent_plist(Path::new(
            "/Applications/SpotFreeze.app/Contents/MacOS/spotfreeze",
        ));
        for needle in [
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<plist version=\"1.0\">",
            "<key>Label</key>",
            &format!("<string>{LAUNCH_AGENT_LABEL}</string>"),
            "<key>ProgramArguments</key>",
            "<string>/Applications/SpotFreeze.app/Contents/MacOS/spotfreeze</string>",
            "<key>RunAtLoad</key>",
            "<true/>",
        ] {
            assert!(plist.contains(needle), "plist missing {needle}:\n{plist}");
        }
        assert!(
            !plist.contains("<key>KeepAlive</key>"),
            "no respawn: exited SpotFreeze stays down"
        );
        assert!(plist.ends_with('\n'));
    }

    #[test]
    fn plist_escapes_xml_entities_in_the_program_path() {
        let plist = launch_agent_plist(Path::new("/Users/a&b<q>/spotfreeze"));
        assert!(
            plist.contains("<string>/Users/a&amp;b&lt;q&gt;/spotfreeze</string>"),
            "path entities escaped:\n{plist}"
        );
        // The label is a compile-time constant and must stay verbatim.
        assert!(plist.contains(&format!("<string>{LAUNCH_AGENT_LABEL}</string>")));
    }

    #[test]
    fn xml_escape_covers_the_text_content_entities() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("plain/path"), "plain/path");
        // Quotes are legal in element text and pass through untouched.
        assert_eq!(xml_escape("say \"hi\" 'now'"), "say \"hi\" 'now'");
        // `&` is escaped first, so already-escaped input double-escapes —
        // correct for raw text (callers pass raw paths, never entities).
        assert_eq!(xml_escape("&lt;"), "&amp;lt;");
    }
}
