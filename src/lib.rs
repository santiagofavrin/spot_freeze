//! SpotFreeze — freeze the screen, then spotlight / zoom / snip to clipboard.
//!
//! All logic lives in this library; `main.rs` is a thin shell that calls
//! [`app::run`]. This makes every non-Win32 module unit-testable via `cargo test`.
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
//! - **Pure modules** (`geometry`, `settings`, `hotkeys::gesture`,
//!   `overlay::composite`, and every `OverlayMode` implementation) never expose
//!   `windows` crate types in their public API, so they are fully testable
//!   headless. Win32 types (`HWND`, …) appear only in `hotkeys::manager`,
//!   `capture`'s GDI impl, `overlay::window`, `overlay::controller`, `ui`, `tray`,
//!   and `app`.
//! - **Error type**: fallible cross-module APIs return [`anyhow::Result`]. Pure
//!   parsers use their own typed errors (e.g. [`hotkeys::gesture::ParseGestureError`]).

pub mod app;
pub mod capture;
pub mod geometry;
pub mod hotkeys;
pub mod overlay;
pub mod settings;
pub mod tray;
pub mod ui;
