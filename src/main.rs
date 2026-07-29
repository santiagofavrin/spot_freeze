#![windows_subsystem = "windows"] // background tray app: no console window

fn main() -> anyhow::Result<()> {
    spotfreeze::app::run()
}
