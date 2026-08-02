//! Command-line interface: `--help`, `--version`, and `--daemon` (detach from
//! the terminal, nohup-style). Hand-rolled and dependency-free; the parser is
//! pure and unit-tested. Any real work is dispatched back to `main`.

use anyhow::Result;

/// What the invocation asks the process to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CliAction {
    /// Run the app normally (tray icon + freeze hotkey, foreground process).
    Run,
    /// Re-spawn detached from the terminal and exit immediately.
    Daemon,
    /// Ask the running instance to toggle the freeze (compositor keybinds).
    Toggle,
    /// Print [`HELP`] to stdout and exit 0.
    Help,
    /// Print [`version_string`] to stdout and exit 0.
    Version,
}

/// `--help` output.
pub const HELP: &str = "\
SpotFreeze — freeze the screen, then spotlight / zoom / snip to clipboard.

Usage: spotfreeze [OPTIONS] [COMMAND]

Commands:
  toggle       Ask the running SpotFreeze instance to toggle the freeze
               (Linux only; for compositor keybinds, e.g. in hyprland.conf:
                bind = SUPER, F, exec, spotfreeze toggle)

Options:
      --daemon     Start detached from the terminal (nohup-style): the
                   process survives the terminal being closed afterwards
                   (Linux/macOS only)
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

With no options SpotFreeze runs in the foreground: tray icon plus the global
freeze hotkey. Settings live in the per-OS config folder (see README.md).
";

/// `spotfreeze <version>` from the crate metadata.
pub fn version_string() -> String {
    concat!("spotfreeze ", env!("CARGO_PKG_VERSION")).to_string()
}

/// Parse the process arguments (excluding argv[0]) into a [`CliAction`].
/// Unknown arguments and conflicting action flags are errors with a
/// user-facing message; the caller prints it and exits with code 2.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut action = None;
    for arg in args {
        let flag = match arg.as_str() {
            "--daemon" => CliAction::Daemon,
            "-h" | "--help" => CliAction::Help,
            "-V" | "--version" => CliAction::Version,
            "toggle" => CliAction::Toggle,
            other => return Err(format!("unknown argument '{other}'; try --help")),
        };
        if let Some(previous) = action {
            return Err(format!(
                "conflicting options: {} and {arg}",
                flag_name(previous)
            ));
        }
        action = Some(flag);
    }
    Ok(action.unwrap_or(CliAction::Run))
}

/// The canonical flag spelling for error messages.
fn flag_name(action: CliAction) -> &'static str {
    match action {
        CliAction::Run => "(run)",
        CliAction::Daemon => "--daemon",
        CliAction::Toggle => "toggle",
        CliAction::Help => "--help",
        CliAction::Version => "--version",
    }
}

/// Re-spawn the running executable detached from the terminal — new session,
/// no controlling tty, stdio to /dev/null (setsid makes SIGHUP on terminal
/// close unreachable, which is what nohup emulates) — then let the caller
/// exit. The detached instance applies the usual single-instance guard.
#[cfg(unix)]
pub fn spawn_detached() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("cannot determine the executable path")?;
    let mut command = Command::new(exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec runs in the child between fork and exec, where only
    // async-signal-safe calls are allowed; setsid is one of them.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("spawning the detached SpotFreeze")?;
    println!("spotfreeze: started detached (pid {})", child.id());
    Ok(())
}

/// The Windows subsystem build has no console to detach from.
#[cfg(windows)]
pub fn spawn_detached() -> Result<()> {
    anyhow::bail!("--daemon is only supported on Linux and macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<CliAction, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_runs_the_app() {
        assert_eq!(parse_args(&[]), Ok(CliAction::Run));
    }

    #[test]
    fn each_flag_maps_to_its_action() {
        assert_eq!(parse_args(&["--daemon"]), Ok(CliAction::Daemon));
        assert_eq!(parse_args(&["--help"]), Ok(CliAction::Help));
        assert_eq!(parse_args(&["-h"]), Ok(CliAction::Help));
        assert_eq!(parse_args(&["--version"]), Ok(CliAction::Version));
        assert_eq!(parse_args(&["-V"]), Ok(CliAction::Version));
        assert_eq!(parse_args(&["toggle"]), Ok(CliAction::Toggle));
    }

    #[test]
    fn conflicting_action_flags_are_an_error() {
        let err = parse_args(&["--daemon", "--help"]).expect_err("conflict");
        assert!(err.contains("--daemon") && err.contains("--help"), "{err}");
        assert!(parse_args(&["--version", "-h"]).is_err());
    }

    #[test]
    fn unknown_arguments_are_an_error_pointing_at_help() {
        let err = parse_args(&["--nohup"]).expect_err("unknown long flag");
        assert!(err.contains("--nohup") && err.contains("--help"), "{err}");
        assert!(
            parse_args(&["file.txt"]).is_err(),
            "positionals are rejected"
        );
    }

    #[test]
    fn help_documents_every_flag() {
        for flag in ["--daemon", "--help", "--version", "toggle"] {
            assert!(HELP.contains(flag), "help mentions {flag}");
        }
        assert!(HELP.contains("nohup"), "the daemon flag explains itself");
        assert!(
            HELP.contains("hyprland.conf"),
            "toggle shows the compositor bind"
        );
    }

    #[test]
    fn version_string_carries_the_crate_version() {
        assert_eq!(
            version_string(),
            format!("spotfreeze {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
