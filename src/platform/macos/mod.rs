//! macOS shell: AppKit overlay surfaces, ScreenCaptureKit capture, Carbon
//! global hotkey, status-item tray, PNG pasteboard.
//!
//! Requirements: macOS 14+ (`SCScreenshotManager`) and the Screen Recording
//! permission (checked at capture time with a pointed error); NO
//! Accessibility permission (the Carbon hotkey does not need it).
//!
//! Module map (each file documents its own contracts and API choices):
//! - [`app`]: wiring, mirroring the Windows `app` module.
//! - [`autostart`]: LaunchAgent login item (filesystem shell over
//!   [`crate::autostart`]).
//! - [`capture`]: [`crate::capture::Capturer`] via ScreenCaptureKit.
//! - [`surface`]: [`crate::platform::OverlaySurface`] via NSWindow/NSView.
//! - [`hotkeys`]: global freeze hotkey via Carbon `RegisterEventHotKey`.
//! - [`tray`]: `NSStatusItem` tray icon + menu.
//! - [`settings_window`]: native AppKit settings editor (modal panel).
//! - [`clipboard`]: [`crate::platform::PlatformServices`] over `NSPasteboard`.
//! - `coords`: pure Cocoa-points ↔ virtual-physical-pixels conversions.

pub mod app;
pub mod autostart;
pub mod capture;
pub mod clipboard;
pub(crate) mod coords;
pub mod hotkeys;
pub mod settings_window;
pub mod surface;
pub mod tray;

/// Entry point dispatched from `main` on macOS.
pub fn run() -> anyhow::Result<()> {
    app::run()
}
