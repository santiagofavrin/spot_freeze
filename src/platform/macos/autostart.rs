//! Auto-start (macOS): a LaunchAgent plist in `~/Library/LaunchAgents/` —
//! works for the bare binary (SMAppService would require a bundled .app).
//! Identifiers, plist content, and the install/remove decision all live in
//! [`crate::autostart`]; this module is only the filesystem side effect,
//! parameterized by the home directory so tests never touch the real one.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::fs;

/// Bring the LaunchAgent plist in line with `auto_start` (startup
/// reconciliation: hand-edited JSONC takes effect on the next launch).
pub fn apply_auto_start(auto_start: bool) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    apply_auto_start_in(auto_start, home)
}

fn apply_auto_start_in(auto_start: bool, home: OsString) -> Result<()> {
    let path = crate::autostart::launch_agent_plist_path(Some(home))
        .context("locating the LaunchAgent plist")?;
    match crate::autostart::reconcile_action(auto_start) {
        crate::autostart::ReconcileAction::Install => {
            let exe = std::env::current_exe().context("cannot determine the executable path")?;
            let plist = crate::autostart::launch_agent_plist(&exe);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::write(&path, plist).with_context(|| format!("failed to write {}", path.display()))
        }
        crate::autostart::ReconcileAction::Remove => match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Idempotent remove: an absent plist is already the desired state.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
        },
    }
}

#[cfg(test)]
mod tests {
    //! Headless: only temp dirs, never the real `~/Library/LaunchAgents`.
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique temp dir (per process + per call); removed on drop, even on panic.
    struct TempHome(PathBuf);

    impl TempHome {
        fn create(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "spotfreeze_autostart_test_{}_{}_{}",
                std::process::id(),
                tag,
                n
            ));
            fs::create_dir_all(&dir).expect("create temp home");
            Self(dir)
        }

        fn os(&self) -> OsString {
            self.0.clone().into_os_string()
        }

        fn plist_path(&self) -> PathBuf {
            crate::autostart::launch_agent_plist_path(Some(self.os())).unwrap()
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn launch_agent_label_matches_the_app_bundle_id() {
        assert_eq!(
            crate::autostart::LAUNCH_AGENT_LABEL,
            crate::platform::macos::app::BUNDLE_ID,
            "LaunchAgent label and the app's bundle identifier must not diverge"
        );
    }

    #[test]
    fn install_writes_the_plist_under_the_given_home() {
        let home = TempHome::create("install");
        apply_auto_start_in(true, home.os()).expect("install");
        let path = home.plist_path();
        assert!(path.is_file(), "plist landed in the temp LaunchAgents dir");
        let expected =
            crate::autostart::launch_agent_plist(&std::env::current_exe().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn install_is_idempotent_and_replaces_stale_content() {
        let home = TempHome::create("reinstall");
        let path = home.plist_path();
        apply_auto_start_in(true, home.os()).expect("first install");
        // A stale hand-edited plist is overwritten by the next reconciliation.
        fs::write(&path, "stale").unwrap();
        apply_auto_start_in(true, home.os()).expect("re-install");
        assert_ne!(fs::read_to_string(&path).unwrap(), "stale");
    }

    #[test]
    fn remove_deletes_the_plist_and_is_ok_when_absent() {
        let home = TempHome::create("remove");
        let path = home.plist_path();
        apply_auto_start_in(false, home.os()).expect("absent plist is not an error");
        assert!(!path.exists());

        apply_auto_start_in(true, home.os()).expect("install");
        assert!(path.is_file());
        apply_auto_start_in(false, home.os()).expect("remove");
        assert!(!path.exists(), "plist deleted");
        // The intermediate `Library/LaunchAgents` dirs stay behind (they may
        // hold other agents); only our plist is removed.
    }

    #[test]
    fn remove_propagates_real_filesystem_errors() {
        // A directory at the plist path cannot be removed as a file.
        let home = TempHome::create("remove_dir");
        let path = home.plist_path();
        fs::create_dir_all(&path).unwrap();
        assert!(apply_auto_start_in(false, home.os()).is_err());
    }
}
