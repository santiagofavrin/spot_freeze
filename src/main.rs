#![windows_subsystem = "windows"] // background tray app: no console window

use spotfreeze::cli::{self, CliAction};
use std::process::ExitCode;

fn main() -> ExitCode {
    let action = match cli::parse(std::env::args().skip(1)) {
        Ok(action) => action,
        Err(message) => {
            eprintln!("spotfreeze: {message}");
            return ExitCode::from(2);
        }
    };
    let result = match action {
        CliAction::Help => {
            print!("{}", cli::HELP);
            Ok(())
        }
        CliAction::Version => {
            println!("{}", cli::version_string());
            Ok(())
        }
        CliAction::Daemon => cli::spawn_detached(),
        CliAction::Toggle => toggle_running_instance(),
        CliAction::Run => run_app(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn toggle_running_instance() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return spotfreeze::platform::wayland::ipc::toggle_running_instance();
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("toggle is only supported on Linux (Wayland)")
}

fn run_app() -> anyhow::Result<()> {
    #[cfg(windows)]
    return spotfreeze::app::run();
    #[cfg(target_os = "linux")]
    return spotfreeze::platform::wayland::run();
    #[cfg(target_os = "macos")]
    return spotfreeze::platform::macos::run();
}
