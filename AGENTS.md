# AGENTS.md

Guidance for AI agents (and human contributors) working in this repo.

## Project

SpotFreeze is a tiny native Rust utility for Windows 11, Linux (Wayland), and
macOS 14+ (Apple Silicon) that freezes the screen on a global hotkey, then offers
spotlight / zoom / snip-to-clipboard modes. Single crate, one binary per
platform, no GUI framework, no runtime. See `README.md` for the product spec
and `plan.md` for the architecture rationale.

## Build & test

```bash
cargo build --release     # release binary → target/release/spotfreeze
cargo test                # full headless suite (safe on a live desktop)
cargo test --lib          # unit tests only
cargo test --test legend  # one integration scenario
cargo fmt                 # format
cargo clippy              # lint (treat warnings as errorsome — fix them)
```

Linux Docker equivalents (no local toolchain needed):

```bash
docker compose run test    # headless test suite
docker compose run build   # release binary into ./target/docker/
docker compose run dev     # interactive shell with cached cargo volumes
```

Platform-specific code is gated by `cfg(windows)` / `cfg(target_os = "linux")`
/ `cfg(target_os = "macos")`. On a non-Windows host, `cargo check` still type-
checks the portable core; cross-target checks use the per-OS CI runners.

## Architecture: pure core + platform seam

The single hard rule: **keep the pure core free of OS API types.**

- **Pure, headless-testable modules** (`geometry`, `settings`, `autostart`,
  `hotkeys::gesture`, `hotkeys::frozen`, `hotkeys::keymap`, `overlay::composite`,
  `overlay::events`, `overlay::legend`, `overlay::modes`, `overlay::controller`,
  `capture::png`, and the `capture::{DibBuffer, MonitorInfo, Capturer}` types)
  never expose `HWND`, Wayland, or AppKit objects in their public API. All their
  logic is integer pixel math / parsing / state transitions — deterministic and
  unit-tested.
- **Platform shells** (`hotkeys::manager`, `capture::gdi`, `overlay::window`,
  `ui`, `tray`, `app` on Windows; `platform/{windows,wayland,macos}/` backends)
  hold the OS bindings and talk to the core only through the seam.
- **The seam** lives in `src/platform/mod.rs`: the `OverlaySurface`,
  `SurfaceFactory`, and `PlatformServices` traits. The overlay controller is
  written against these traits, so a new platform is a new shell, not a fork of
  the core.

When adding a feature, ask: does this belong in the pure core (testable) or in
a platform shell (OS-specific)? Pure logic in the core, OS glue in the shell.

## Global contracts (binding for all implementers)

- **Pixel format**: `capture::DibBuffer` is 32-bit **BGRA**, 8 bits/channel,
  **non-premultiplied** alpha, **top-down** row order, tightly packed
  (`stride == width * 4`). Screen captures are always opaque (`A == 255`). All
  platforms share this format so captured pixels flow into overlays with no
  conversion.
- **Coordinates**: *virtual-screen* coords span the whole multi-monitor desktop
  (primary top-left is `(0, 0)`; other monitors may be **negative**).
  *Monitor-local* coords have `(0, 0)` at that monitor's top-left. Every
  function documents which space it uses. All units are **physical pixels**
  (the process is PerMonitorV2 DPI-aware; no DPI math inside the overlay path).
- **Error type**: fallible cross-module APIs return `anyhow::Result`. Pure
  parsers use their own typed errors (e.g. `hotkeys::gesture::ParseGestureError`).
- **No animations**: freeze/unfreeze and every in-session mode change are
  instant — no fades, no transitions, one repaint per change.

## Mode model (don't break these)

- **Spotlight** (`S`) is a **toggle**: layer on/off; with every layer off the
  screen stays frozen but completely clear (no dim).
- **Zoom hold** (`F`) is a toggle **layer** at the last-used level
  (1.0×–16.0×); it stacks over spotlight on/off.
- **Capture** (`C`) **re-freezes**: the current view (spotlight/zoom baked in)
  becomes the new frozen frame and a drag-selection snips the *effected*
  pixels. `Esc` in capture exits back to the pre-capture view; `Esc` outside
  capture unfreezes.
- The **legend pill** is painted into the composed frame only — never into the
  capture originals — so it can never leak into a snip copy or the capture
  re-base (`rebase_freeze` composes without it).

## Settings

`spotfreeze.jsonc` (JSONC — comments and trailing commas allowed), written
atomically, in the per-OS config dir. Missing keys fall back to defaults;
malformed files never crash the app. Changes apply on the next freeze (or via
the tray *Reload settings*). The settings model (`settings/model.rs`) is the
single source of defaults; the store (`settings/store.rs`) only loads/saves.

## Conventions

- **Commits**: [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `chore:`, …). Releases are fully automated by
  release-please (`release-please-config.json`, `.github/workflows/release.yml`,
  release type `rust`): a `feat:`/`fix:` on `main` opens the release PR, bumps
  `Cargo.toml` + `CHANGELOG.md`, tags `v*`, and builds/attaches all three OS
  artifacts. `docs:`/`chore:` commits do not trigger a release.
- **Style**: match surrounding code. Module docs (`//!`) explain the contract
  at the top of each file; keep them accurate when you change behavior. Tests
  live inline (`#[cfg(test)] mod tests`) for unit tests and in `tests/` for
  end-to-end scenarios; both are headless and deterministic.
- **Tests first**: the pure core is fully unit-tested. When changing core
  logic, update/extend the inline tests and the `tests/` scenarios; run
  `cargo test` before finishing. Never commit a failing test.
- **No secrets / user content** in code, logs, or commits. The settings file
  is git-ignored; never check it in.
