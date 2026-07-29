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
