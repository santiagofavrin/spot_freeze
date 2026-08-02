//! Single-instance IPC over a unix socket
//! (`$XDG_RUNTIME_DIR/spotfreeze.sock`): `spotfreeze --spotlight` and
//! `spotfreeze --capture` ask the running instance to activate a mode. This is
//! the compositor-keybind path, independent of the XDG GlobalShortcuts portal.
//!
//! The single-instance lock ([`crate::platform::wayland::shell`]) is always
//! taken first, so a live socket always has exactly one owner and a stale
//! file can be unlinked blindly.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Mode commands the daemon understands (newline-free, trimmed on receive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeCommand {
    Spotlight,
    Capture,
}

impl ModeCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spotlight => "spotlight",
            Self::Capture => "capture",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "spotlight" => Some(Self::Spotlight),
            "capture" => Some(Self::Capture),
            _ => None,
        }
    }
}

/// Socket file name inside `$XDG_RUNTIME_DIR`.
const SOCKET_FILE_NAME: &str = "spotfreeze.sock";

/// The daemon's socket path. Errors when the runtime dir is unknown.
pub fn socket_path() -> Result<PathBuf> {
    socket_path_from(std::env::var_os("XDG_RUNTIME_DIR")).context("XDG_RUNTIME_DIR is not set")
}

/// Pure path resolution: `$XDG_RUNTIME_DIR/spotfreeze.sock`, `None` when the
/// variable is unset or empty (unit-testable without mutating process env).
fn socket_path_from(xdg_runtime_dir: Option<OsString>) -> Option<PathBuf> {
    xdg_runtime_dir
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join(SOCKET_FILE_NAME))
}

/// CLI client: forward a mode request to the running instance. Errors when no
/// instance is listening.
pub fn send_mode_command(command: &str) -> Result<()> {
    let command = ModeCommand::parse(command)
        .with_context(|| format!("unknown IPC mode command '{command}'"))?;
    let path = socket_path()?;
    send_mode_command_at(&path, command).with_context(|| {
        format!(
            "could not reach the running SpotFreeze instance ({})",
            path.display()
        )
    })
}

/// Server side: the daemon's command listener, nonblocking. Any stale socket
/// file is unlinked first (the single-instance lock guarantees no live owner).
pub fn bind_listener() -> Result<UnixListener> {
    let path = socket_path()?;
    bind_listener_at(&path)
}

/// Drain every pending command; returns the last recognized mode request.
/// Unknown payloads are ignored (forward-compatible with future commands).
pub fn drain_mode_command(listener: &UnixListener) -> Option<ModeCommand> {
    let mut command = None;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf)
                    && let Ok(payload) = std::str::from_utf8(&buf[..n])
                {
                    command = ModeCommand::parse(payload.trim()).or(command);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    command
}

/// Path-parametrized bind (the testable core of [`bind_listener`]).
fn bind_listener_at(path: &Path) -> Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("setting the IPC listener nonblocking")?;
    Ok(listener)
}

/// Path-parametrized send (the testable core of [`send_mode_command`]).
fn send_mode_command_at(path: &Path, command: ModeCommand) -> Result<()> {
    let mut stream = UnixStream::connect(path).context("connecting to the IPC socket")?;
    stream
        .write_all(command.as_str().as_bytes())
        .context("sending the mode command")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("spotfreeze-ipc-{tag}-{}.sock", std::process::id()))
    }

    #[test]
    fn socket_path_honors_xdg_runtime_dir() {
        assert_eq!(
            socket_path_from(Some("/run/user/1000".into())),
            Some(PathBuf::from("/run/user/1000/spotfreeze.sock"))
        );
        assert_eq!(socket_path_from(None), None);
        assert_eq!(socket_path_from(Some("".into())), None);
    }

    #[test]
    fn mode_command_round_trip() {
        let path = temp_path("roundtrip");
        let listener = bind_listener_at(&path).expect("bind");
        send_mode_command_at(&path, ModeCommand::Capture).expect("send");
        assert_eq!(
            drain_mode_command(&listener),
            Some(ModeCommand::Capture),
            "the capture command is received"
        );
        assert_eq!(
            drain_mode_command(&listener),
            None,
            "no command is replayed"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_payloads_are_ignored() {
        let path = temp_path("garbage");
        let listener = bind_listener_at(&path).expect("bind");
        let mut stream = UnixStream::connect(&path).expect("connect");
        stream.write_all(b"explode\n").unwrap();
        assert_eq!(drain_mode_command(&listener), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn send_to_missing_socket_errors() {
        let path = temp_path("absent");
        assert!(send_mode_command_at(&path, ModeCommand::Spotlight).is_err());
    }

    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let path = temp_path("stale");
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind_listener_at(&path).expect("stale file is unlinked");
        let _second = bind_listener_at(&path).expect("re-bind also works");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
