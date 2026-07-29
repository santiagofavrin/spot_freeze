//! Wayland/Hyprland shell: layer-shell overlay surfaces, wlr-screencopy
//! capture, XDG GlobalShortcuts portal hotkey, StatusNotifierItem tray.
//!
//! Module map (fixed — each backend agent owns exactly these files):
//! - [`shell`]: connection, registry, output tracking, event-loop glue (the
//!   calloop loop itself is owned by [`app`]).
//! - [`capture`]: [`crate::capture::Capturer`] via zwlr-screencopy.
//! - [`surface`]: [`crate::platform::OverlaySurface`] via zwlr-layer-shell.
//! - [`input`]: wl_pointer/wl_keyboard → [`crate::overlay::events::OverlayEvent`].
//! - [`clipboard`]: PNG clipboard via wl_data_device.
//! - [`hotkeys_portal`]: global freeze hotkey via the GlobalShortcuts portal.
//! - [`tray`]: StatusNotifierItem tray icon.
//! - [`app`]: wiring, mirroring the Windows `app` module.

pub mod app;
pub mod capture;
pub mod clipboard;
pub mod hotkeys_portal;
pub mod input;
pub mod shell;
pub mod surface;
pub mod tray;

/// Entry point dispatched from `main` on Linux.
pub fn run() -> anyhow::Result<()> {
    app::run()
}
