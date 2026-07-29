//! SpotFreeze — freeze the screen, then spotlight / zoom / snip to clipboard.
//!
//! All logic lives in this library; `main.rs` is a thin shell that dispatches
//! to the platform entry point (`app::run` on Windows, `platform::wayland` /
//! `platform::macos` elsewhere). This makes every non-OS module unit-testable
//! via `cargo test`.
//!
//! # Global contracts (binding for ALL implementers)
//!
//! - **Pixel format**: [`capture::DibBuffer`] is 32-bit **BGRA**, 8 bits/channel,
//!   **non-premultiplied** alpha, **top-down** row order, tightly packed
//!   (`stride == width * 4`). Screen captures are always opaque (`A == 255`).
//! - **Coordinate spaces**: *virtual-screen* coordinates span the whole
//!   multi-monitor desktop; the primary monitor's top-left is `(0, 0)` and other
//!   monitors may have **negative** coordinates. *Monitor-local* coordinates have
//!   `(0, 0)` at that monitor's top-left. Every function documents which space it
//!   uses. All pixel units are **physical pixels** (the process runs PerMonitorV2
//!   DPI-aware; no DPI scaling math inside the overlay pipeline).
//! - **Platform seam**: [`platform::OverlaySurface`], [`platform::SurfaceFactory`],
//!   and [`platform::PlatformServices`] are the boundary between the portable
//!   overlay pipeline and the per-OS shells. Overlay surfaces report input as
//!   [`overlay::events::OverlayEvent`]s (monitor-local coordinates; key events
//!   carry Win32 VKs translated via [`hotkeys::keymap`]).
//! - **Pure modules** (`geometry`, `settings`, `hotkeys::gesture`,
//!   `hotkeys::frozen`, `hotkeys::keymap`, `overlay::composite`,
//!   `overlay::events`, `overlay::modes`, `overlay::controller`, `capture::png`,
//!   and the `capture::{DibBuffer, MonitorInfo, Capturer}` types) never expose
//!   OS API types in their public API, so they are fully testable headless.
//!   OS types (`HWND`, Wayland objects, AppKit objects) appear only in the
//!   platform shells: `hotkeys::manager`, `capture::gdi`, `overlay::window`,
//!   `ui`, `tray`, `app` (all Windows-only), and the `platform` backends.
//! - **Error type**: fallible cross-module APIs return [`anyhow::Result`]. Pure
//!   parsers use their own typed errors (e.g. [`hotkeys::gesture::ParseGestureError`]).

#[cfg(windows)]
pub mod app;
pub mod capture;
pub mod geometry;
pub mod hotkeys;
pub mod overlay;
pub mod platform;
pub mod settings;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod ui;
