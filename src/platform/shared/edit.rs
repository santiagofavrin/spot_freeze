//! Open the settings file in the user's default editor (tray "Edit settings").
//!
//! Detached spawn: the app never blocks on the editor. Settings are re-read on
//! the next freeze, so no file watching is needed.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Open `path` in the platform's default text editor: `xdg-open` on Linux
/// (falling back to `$EDITOR`), `open` on macOS.
pub fn open_in_editor(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if spawn(Command::new("xdg-open").arg(path)).is_ok() {
            return Ok(());
        }
        // Headless fallback: $EDITOR on the current terminal, if any.
        if let Ok(editor) = std::env::var("EDITOR") {
            spawn(Command::new(editor).arg(path))
                .context("spawning $EDITOR")?;
            return Ok(());
        }
        anyhow::bail!("neither xdg-open nor $EDITOR is available");
    }
    #[cfg(target_os = "macos")]
    {
        spawn(Command::new("open").arg(path)).context("spawning open")
    }
}

fn spawn(command: &mut Command) -> Result<()> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .with_context(|| format!("spawning {command:?}"))
}
