#![windows_subsystem = "windows"] // background tray app: no console window

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    return spotfreeze::app::run();
    #[cfg(target_os = "linux")]
    return spotfreeze::platform::wayland::run();
    #[cfg(target_os = "macos")]
    return spotfreeze::platform::macos::run();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    compile_error!("SpotFreeze supports only Windows, Linux (Wayland), and macOS");
}
