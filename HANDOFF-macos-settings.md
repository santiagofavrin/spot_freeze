# Handoff — macOS settings window + tray version/open-folder + permission fix

Branch: `feat/macos-settings-window-and-tray-menu`

This branch is **work in progress and does NOT compile yet**. See the BLOCKER
section — there is exactly one file left to create.

## Original request (4 asks)

1. Show the app **version** as a **disabled** (plain text) entry in the tray
   context menu (left/right click on the SpotFreeze icon) — all platforms.
2. Add a tray option to **open the folder containing the settings file** — all
   platforms.
3. Give **macOS a native settings edit window** like Windows has, but "very
   stylish, very Apple-like".
4. **Fix the macOS Screen Recording permission bug**: even with permission
   granted, freezing failed with "SpotFreeze does not have Screen Recording
   permission".

## Status at a glance

| Area | Status |
|------|--------|
| Shared `open_settings_folder` helper | ✅ done |
| Windows tray: version + open folder | ✅ done (not compiled locally; macOS host) |
| Wayland tray: version + open folder | ✅ done (not compiled locally; macOS host) |
| macOS permission fix | ✅ done + validated (build/test/clippy green) |
| macOS tray: version + open folder + "Settings…" | ✅ done (wiring) |
| macOS `app.rs` wiring for the settings window | ✅ done |
| **macOS `settings_window.rs` implementation** | ❌ **MISSING — build broken** |
| Final integration (`cargo fmt/build/test/clippy`) | ❌ not run (blocked by above) |

## 🚧 BLOCKER / the one remaining task

`src/platform/macos/mod.rs` declares `pub mod settings_window;` and
`src/platform/macos/app.rs` calls it, but **`src/platform/macos/settings_window.rs`
does not exist**, so the crate does not build.

Create `src/platform/macos/settings_window.rs` implementing EXACTLY this public
API (this is the contract `app.rs::open_settings` already depends on):

```rust
use crate::settings::model::AppSettings;
use objc2::MainThreadMarker;

/// Show the settings editor modally and return the edited settings on Save, or
/// None on Cancel/close. Runs its own modal loop, so the caller holds NO
/// AppState borrow across the call (see app.rs module docs).
pub fn run_modal(mtm: MainThreadMarker, current: &AppSettings) -> Option<AppSettings>;
```

### Window requirements (the "stylish, Apple-like" settings editor)

- Native `NSWindow` (titled, closable, centered, ~520×640), backed by an
  `NSVisualEffectView` (vibrant/translucent System-Settings look). App is an
  `.accessory` (LSUIElement) — before running the modal, call
  `NSApplication::sharedApplication(mtm).activate()` +
  `window.makeKeyAndOrderFront(None)` + `window.center()` so it appears and
  takes focus (mirror the `NSAlert` code in `app.rs`).
- Sections with bold header labels + generous spacing: **Hotkeys**,
  **Spotlight**, **Zoom**, **Overlay**, **General**. Fixed-frame column layout
  is acceptable (label column left, control column right) — avoid Auto Layout
  constraint pitfalls unless comfortable.
- **Edit the full `AppSettings` model** (see `src/settings/model.rs`):
  - Hotkeys (full `HotkeyGesture`, edited as `to_display()` strings in
    `NSTextField`s): `freeze_toggle`, `mode_spotlight`, `mode_snip`,
    `zoom_hold`, `snip_copy`, `cancel`, `reset_zoom`. Plus `zoom_modifier`
    (modifier-only `Modifiers`). Add a hint about the format (e.g. `Cmd+Shift+F`,
    `Esc`, `S`). Note: on macOS the `Win` modifier displays as Command — show
    whatever the pure `to_display()` produces; do not invent formatting.
    A text field showing/accepting the display form is the accepted approach
    (no live key-capture control required).
  - `spotlight.default_radius` (u32), `zoom.step_factor` (f32 > 1.0),
    `zoom.min` (f32 ≥ 1.0), `zoom.max` (f32), `overlay.dim_opacity` (u8 0..=255),
    `overlay.snip_dim_opacity` (u8 0..=255) — `NSTextField` (± `NSStepper`).
  - `overlay.color`, `overlay.snip_color` — prefer `NSColorWell` seeded from
    `Rgb` (sRGB components /255, read back to nearest u8 → `Rgb`). A `#RRGGBB`
    text field is an acceptable fallback.
  - `auto_start` — `NSButton` checkbox (switch style if easy).
  - Buttons: **Save** (default, Return key-equiv) and **Cancel** (Esc).
- Modal loop via `NSApplication::runModalForWindow:` (or `stopModal` from button
  actions). On **Save**: read all controls → VALIDATE. If invalid, show an
  `NSAlert` (raise its window level above the panel) describing the first error
  and keep the window open. If valid, stop the modal returning the built
  `AppSettings`. On Cancel/close, return None. Keep control state in the action
  target's ivars or an `Rc<RefCell<..>>` (mirror the `define_class!` pattern in
  `tray.rs`).

### Validation rules (match the Windows `src/ui/settings_window.rs`, READ-ONLY reference)

Replicate the same guarantees so a saved config is always valid:
- Every hotkey parses via `HotkeyGesture::parse` (and the modifier-only parse
  for `zoom_modifier`).
- **Hotkey conflict detection** consistent with the Windows rules.
- `zoom.min < zoom.max`; `zoom.step_factor > 1.0`; opacities 0..=255; radius
  within the Windows-enforced bounds; color hex via `Rgb::parse_hex`.

### Pure-core requirement (AGENTS.md)

Put the non-trivial decision logic into a **PURE** function taking primitives
(strings/numbers) → `Result<AppSettings, Vec<String>>` (or `Result<_, String>`),
with **`#[cfg(test)]` unit tests** (no AppKit): valid round-trip
(settings → field strings → parse back == original), each validation failure
(bad hotkey, `min >= max`, `step <= 1.0`, opacity out of range, bad hex), and
hotkey-conflict detection. The AppKit window is then thin glue over this.

### objc2 notes

Use the objc2 / objc2-app-kit / objc2-foundation versions already in the tree
(do NOT add dependencies). The existing macOS modules are the ground truth for
binding shapes: see `tray.rs` (`define_class!`, `MainThreadMarker`,
`NSString::from_str`, `msg_send!`, `Retained`, menu construction) and `app.rs`
(`NSAlert`, `activate`, window level). If an API detail is uncertain, copy the
idiom from those files rather than guessing, and iterate until `cargo build`
is clean.

## How to validate (this repo is on a macOS host)

```bash
cargo build
cargo test        # includes the new pure settings_window tests
cargo clippy      # fix NEW warnings only
rustfmt --edition 2024 src/platform/macos/settings_window.rs
```

Toolchain note: if `cargo` is not on PATH, a working stable toolchain lives at
`/Users/san/code/santiagofavrin/slack-cskk/.toolchain/` — invoke with
`CARGO_HOME=.../cargo RUSTUP_HOME=.../rustup`. `rustfmt` needs `--edition 2024`
(the crate is edition 2024; a let-chain in `app.rs::open_settings` fails plain
`rustfmt`).

Pre-existing (NOT ours): a `needless_range_loop` clippy warning at
`src/overlay/controller.rs:703`. Leave it.

## What was changed on this branch (per file)

- `src/platform/shared/edit.rs` — new `open_settings_folder(path)`:
  macOS `open -R <file>` (reveal+select in Finder), Linux `xdg-open <folder>`.
- `src/tray/mod.rs` (Windows) — disabled `SpotFreeze v{version}` line at top +
  "Open settings folder" item; new `TrayEvent::MenuOpenSettingsFolder`;
  renumbered `IDM_*` ids; doc updates.
- `src/app.rs` (Windows) — `on_tray_event` arm + `open_settings_folder()` that
  runs `explorer /select,<path>` detached.
- `src/platform/wayland/tray.rs` — disabled version line + "Open settings
  folder"; added generic closure `K`; updated all `#[cfg(test)]` expectations
  (labels, separator indices `[1, 4]`, disabled-item check, new counter).
- `src/platform/wayland/app.rs` — `Intent::OpenSettingsFolder` + handler +
  tray `spawn` closure.
- `src/platform/macos/capture.rs` — **permission fix**: `CGPreflightScreenCaptureAccess`
  is now advisory (it returns a stale false-negative for rebuilt ad-hoc-signed
  apps). On false preflight, call `CGRequestScreenCaptureAccess()` once but do
  not bail; attempt the real capture; detect genuine denial from
  ScreenCaptureKit (`SCStreamErrorUserDeclined` -3801 / TCC wording /
  empty-displays-under-false-preflight) via a PURE, unit-tested
  `is_permission_denial(...)` helper, and only then show the friendly
  "enable + restart" message. 6 new pure tests. Validated green.
- `src/platform/macos/mod.rs` — declares `pub mod settings_window;` (+ doc).
- `src/platform/macos/tray.rs` — disabled version line; "Open Settings Folder"
  item + selector + `TrayEvent::MenuOpenSettingsFolder`; renamed
  "Edit Settings…" → "Settings…" (now opens the native window).
- `src/platform/macos/app.rs` — `open_settings()` now clones settings+path,
  drops the borrow, calls `settings_window::run_modal(mtm, &current)`, and on
  `Some` saves + swaps settings + `rebind_freeze_hotkey_if_changed()` +
  reconciles auto-start; added `open_settings_folder()` + tray sink arm.

## Suggested next steps for the new conversation

1. Create `src/platform/macos/settings_window.rs` per the contract + spec above.
2. `cargo build` → fix until clean.
3. `cargo test` (add/confirm the pure tests) → `cargo clippy`.
4. Run the final integration pass: `cargo fmt --all`, then build/test/clippy
   once more across the whole tree.
5. Manual smoke test on the Mac: freeze works with permission granted (the bug),
   tray shows the version line, "Open Settings Folder" reveals the file, and
   "Settings…" opens the new window and round-trips a save.
6. Note: Windows and Wayland changes could not be compiled on this macOS host;
   verify them on their CI runners / Docker before release.
