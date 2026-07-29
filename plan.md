# SpotFreeze — Execution Plan

## Product
Windows 11 background utility: lives in the system tray, freezes the screen on a
customizable global hotkey, and offers Spotlight / Zoom / Snip-to-clipboard modes.
All hotkeys rebindable from a settings UI. Settings in a git-ignored local JSONC file.

## Priorities (user-defined, latest wins)
1. **Snappiness & speed above all** — most lightweight, fastest tool possible
2. Latest modern stack, latest stable versions (no bleeding edge)
3. Full test suite covering every scenario

## Tech stack (chosen — revised for speed)
- **Rust (stable, MSVC toolchain, edition 2024)** — native code, no runtime/JIT,
  single ~3–5 MB exe, near-zero idle RAM/CPU, instant freeze response
- **Raw Win32 via Microsoft's official `windows-rs` crate** — no GUI framework overhead:
  - Tray: `Shell_NotifyIconW` + hidden message window
  - Global hotkeys: `RegisterHotKey` / `WM_HOTKEY`
  - Overlay: per-monitor layered topmost windows composited via `UpdateLayeredWindow`
    (screen captured ONCE into a 32-bit DIB; spotlight = copy original pixels only in the
    cursor circle per mouse-move — O(hole area), not per-frame repaint)
  - Capture: GDI `BitBlt` into DIB sections (fast, no DXGI complexity)
  - Clipboard: `CF_DIB` via Win32 clipboard API
- Settings UI: Win32 common controls (no framework). JSONC settings via `jsonc-parser`/`serde`
- **Tests:** built-in `cargo test` — all logic decoupled from Win32 into pure functions
  (pixel compositing on memory buffers, geometry math, gesture parse/format, settings
  round-trip). No visible windows, no real hotkey registration, no clipboard clobbering
  during tests — the user keeps working undisturbed.
- **CI/CD:** GitHub Actions — `workflow_dispatch` build + `cargo test`; release-please
  (googleapis/release-please-action, simple release type) → on tag: `cargo build --release`
  + attach zip to the GitHub Release. All latest stable action versions.

## External prerequisites (USER must install — blocks Stage 1)
1. **Visual Studio Build Tools 2022** with "Desktop development with C++" workload
   (MSVC linker + Windows SDK that Rust needs):
   https://visualstudio.microsoft.com/visual-cpp-build-tools/
   or `winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
2. **Rust via rustup** (stable x86_64-pc-windows-msvc):
   https://rustup.rs/  (run `rustup-init.exe`, take defaults)
   or `winget install Rustlang.Rustup`
Verify afterwards (new terminal): `cargo --version` and `rustc --version`.

Everything else (crates, actions) is fetched automatically — no more installs needed.

## Architecture
```
Cargo.toml / Cargo.lock          single crate: lib.rs (testable logic) + main.rs (thin entry)
src/
  main.rs                        entry, single-instance mutex, Win32 message loop
  app.rs                         wiring, app state machine
  settings/model.rs              AppSettings + defaults (pure)
  settings/store.rs              JSONC load/save, atomic write, brief-comment template
  hotkeys/gesture.rs             pure: gesture parse/format/serialize (unit-tested)
  hotkeys/manager.rs             RegisterHotKey wrapper, WM_HOTKEY dispatch
  capture/mod.rs                 Capturer trait + GDI BitBlt impl → DIB buffers
  overlay/controller.rs          freeze/unfreeze, mode switching, Esc/Ctrl+C routing
  overlay/window.rs              per-monitor layered window, UpdateLayeredWindow present
  overlay/composite.rs           PURE pixel ops: darken, spotlight hole, zoom resample,
                                 rect crop/normalize, multi-monitor mapping (unit-tested)
  overlay/modes/{spotlight,zoom,snip}.rs
  ui/settings_window.rs          Win32 settings window: rebind every hotkey, conflict
                                 validation, Exit with confirm
  tray/mod.rs                    tray icon; left-click → settings, right-click → Settings/Exit
tests/                           integration tests (headless-safe)
.github/workflows/build.yml      workflow_dispatch: test + release build + zip artifact
.github/workflows/release.yml    release-please → tag → build + attach zip to release
release-please-config.json / .release-please-manifest.json
.gitignore (incl. settings.json)  README.md  CHANGELOG.md (release-please managed)
```

## Stages
- **Stage 0 (BLOCKED):** user installs VS Build Tools + rustup → verify `cargo --version`
- **Stage 1:** scaffold + contracts: Cargo.toml, module skeleton, traits/pure-fn signatures,
  settings model with all hotkey fields + defaults, .gitignore — single agent
- **Stage 2 (parallel swarm, one module per agent, disjoint files):**
  settings store · hotkey gesture+manager · capture · composite pixel ops ·
  overlay controller+window · spotlight mode · zoom mode · snip mode ·
  settings UI · tray · app wiring+main · CI/release workflows ·
  test suites per pure module (interfaces already fixed in Stage 1)
- **Stage 3:** integration agent — `cargo test` + `cargo build --release`, fix everything
- **Stage 4:** QA agent checks requirements checklist; final report

## Constraints from user
- Do NOT interrupt the PC session: no app launches, no window flashing, no focus stealing,
  no clipboard/hotkey interference. Only `cargo test` / `cargo build` run locally.
- Crate versions: latest stable at scaffold time (resolved via `cargo add`).

## Cross-platform port (Linux/macOS)

The crate now targets Windows 11, Linux (Wayland, targeting Hyprland/wlroots),
and macOS 14+ (Apple Silicon) from one codebase, keeping the project's
identity: single native binary, no GUI framework, pure-logic core fully
unit-tested headless.

Key decisions (user-approved): settings on Linux/macOS are JSONC-file only
(tray "Edit settings" opens the file in the default editor; changes apply on
next freeze); the Linux global hotkey uses the XDG GlobalShortcuts portal
(rebindable from `settings.json`, requires `xdg-desktop-portal-hyprland` on
Hyprland); Docker covers Linux builds + headless tests, while macOS/Windows
build on CI runners.

### Seam architecture

- **Pure portable core** (no OS imports, headless-tested): geometry, settings
  model/store, hotkey gesture parsing, pixel compositing, overlay modes, the
  overlay controller's mode/copy logic, frozen-hotkey matching, VK↔platform
  keymaps, and PNG encoding for the clipboard.
- **Platform seam** (`src/platform/mod.rs`): two traits — `OverlaySurface`
  (one per monitor: `present` a composed `DibBuffer` frame, full or dirty
  rect; `Drop` closes) and `PlatformServices` (virtual-screen cursor position,
  image-to-clipboard) — plus a `SurfaceFactory` creating one surface per
  monitor. The controller is generic over these. All platforms share the same
  BGRA `DibBuffer` frame format, so captured pixels flow into overlays with no
  conversion anywhere.
- **Per-OS shells** (`src/platform/{windows,wayland,macos}/`): capture,
  overlay surfaces, global-hotkey binding, tray, clipboard, and the app/event
  loop; `main.rs` dispatches on `target_os`. Wayland: wlr-layer-shell +
  wlr-screencopy + XDG GlobalShortcuts portal (ashpd) + ksni StatusNotifierItem
  tray. macOS: AppKit borderless windows + ScreenCaptureKit capture + Carbon
  `RegisterEventHotKey` + `NSStatusItem` tray.

### Stages

- **Stage 1 — seam refactor (no Windows behavior change):** introduce the
  traits, genericize the overlay controller, split GDI capture and Win32-only
  modules behind `cfg(windows)`, extract pure frozen-hotkey matching, keymaps,
  PNG encoding, and per-OS settings paths.
- **Stage 2 — parallel work (disjoint file sets):** Wayland core, Wayland
  services (portal hotkeys, SNI tray, shared editor launcher), macOS backend,
  Docker + CI + packaging (`docker compose` services: `build`, `test`, `dev`;
  macOS `.app` packaging script), docs.
- **Stage 3 — integration:** wire everything; `cargo test` green on Linux,
  `cargo check` green for the Windows and macOS targets, Docker build/test
  green, all CI jobs green.
- **Stage 4 — manual runtime QA:** checklist-driven verification on a real
  Hyprland session and a real Mac (permission prompt, freeze/spotlight/zoom/
  snip/copy/paste/tray/exit, rebinding) — headless tests cannot cover live
  sessions.

### Non-goals (v1)

Native settings windows on Linux/macOS · X11 session support · macOS Intel
builds · IPC CLI toggle · musl static binaries · Apple notarization · Windows
behavior changes beyond the Stage-1 seam refactor.
