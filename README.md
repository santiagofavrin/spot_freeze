# SpotFreeze

A tiny, fast Windows 11 utility that lives in the system tray and **freezes your
screen** on a global hotkey — then lets you spotlight a region, zoom in, or snip
part of the frozen frame to the clipboard.

SpotFreeze is built for speed: a single native Rust binary (raw Win32, no GUI
framework, no runtime), a few MB on disk, and near-zero idle RAM/CPU. The screen
is captured **once** at freeze time; the spotlight "hole" is re-composited per
mouse move over the cursor circle only — never a full repaint.

## Features

- **Freeze the screen** with a customizable global hotkey (`Ctrl+Alt+F` out of
  the box). All monitors are captured at once; each monitor gets its own
  darkened overlay, so multi-monitor setups are fully covered.
- **Spotlight mode** — a bright circle follows your cursor over the dimmed
  frozen screen. Hold `Ctrl` and scroll the mouse wheel to resize it.
- **Zoom mode** — magnify the frozen frame around the cursor with the mouse
  wheel (1.0×–16.0×, ×1.25 per notch by default).
- **Snip mode** — drag a rectangle on the frozen screen and copy it to the
  clipboard (see *Copying screenshots* below).
- **Every hotkey is rebindable** from the built-in settings window, with
  conflict validation.
- **Tray-based** — no window until you ask for one. Left-click the tray icon
  for settings, right-click for the Settings/Exit menu.
- **Human-friendly settings** — a commented JSONC file next to the exe;
  malformed files never crash the app (it falls back to defaults).

## Default hotkeys

All bindings can be changed in the settings window. Mode-specific keys are only
active while the screen is frozen.

| Action | Default | Scope |
| --- | --- | --- |
| Toggle screen freeze | `Ctrl+Alt+F` | Global — works from any app |
| Switch to Spotlight mode | `1` | While frozen |
| Switch to Zoom mode | `2` | While frozen |
| Switch to Snip mode | `3` | While frozen |
| Resize spotlight circle | `Ctrl` + mouse wheel | Spotlight mode |
| Reset zoom to 1.0× | `0` | Zoom mode |
| Copy screenshot to clipboard | `Ctrl+C` | While frozen (see below) |
| Unfreeze / cancel | `Esc` | While frozen |

Other defaults: spotlight radius `150` px, dim-veil opacity `160` (0–255),
zoom step `1.25` (min `1.0`, max `16.0`).

## Install

1. Download `spotfreeze-windows-x64.zip` from the latest
   [GitHub Release](../../releases).
2. Unzip it anywhere you like (e.g. `C:\Tools\SpotFreeze\`).
3. Run `spotfreeze.exe` — it appears as an icon in the system tray.

On first run, `settings.json` is created automatically **next to the exe** with
all default values. No installer, no registry writes, no admin rights needed.

## Settings

**Left-click the tray icon** (or right-click → *Settings*) to open the settings
window. From there you can:

- Rebind every hotkey by pressing the new key combination; conflicting
  bindings are rejected.
- Adjust the spotlight radius, dim-veil opacity, and zoom limits.
- Exit via right-click → *Exit* (a Yes/No confirmation prevents accidents).

Settings are stored in `settings.json` beside `spotfreeze.exe`. It is a JSONC
file — comments and trailing commas are allowed — and is written atomically, so
a crash mid-save can never corrupt it. Missing keys fall back to defaults, and
changes apply on the next freeze.

## Copying screenshots

While frozen, pressing `Ctrl+C` (the copy binding) copies to the clipboard and
then unfreezes:

- **If you drew a selection** in Snip mode → the selected rectangle is copied,
  cropped from the *original, undarkened* capture.
- **If no selection exists** → the **entire screen currently under the cursor**
  (the "focused" monitor) is copied.

Copying is multi-monitor aware: the focused screen is whichever physical
monitor the cursor is on, regardless of monitor arrangement.

Press `Esc` at any time while frozen to unfreeze without copying.

## Build from source

Prerequisites:

1. **Visual Studio Build Tools 2022** with the "Desktop development with C++"
   workload (MSVC linker + Windows SDK):
   `winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
2. **Rust via rustup** (stable `x86_64-pc-windows-msvc`): https://rustup.rs/
   or `winget install Rustlang.Rustup`

Then:

```powershell
git clone https://github.com/<owner>/spot_freeze.git
cd spot_freeze
cargo build --release
```

The binary is at `target\release\spotfreeze.exe`. Copy it wherever you want and
run it — `settings.json` will be created beside it on first launch.

## Test

```powershell
cargo test
```

The whole suite is headless and safe to run on a live desktop: all logic
(pixel compositing, geometry, hotkey parsing, settings round-trips) is decoupled
from Win32 into pure functions. Tests open no windows, register no hotkeys, and
never touch the real clipboard or screen.

## Release process

Releases are managed by [release-please](https://github.com/googleapis/release-please)
(release type `simple`) via `.github/workflows/release.yml`:

1. Merge changes to `main` using
   [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`,
   `fix:`, …). release-please opens (or updates) a release PR with the
   changelog and next version.
2. Merge the release PR → release-please creates the `v*` tag and GitHub
   release, and the same workflow builds `spotfreeze.exe` on
   `windows-latest`, zips it as `spotfreeze-windows-x64.zip`, and attaches it
   to the release automatically.

`.github/workflows/build.yml` is a manual-only workflow (`workflow_dispatch`)
that runs `cargo test`, builds the release binary, and uploads the same zip as
a workflow artifact for ad-hoc verification.

## Tech

Rust (stable, MSVC, edition 2024) on top of Microsoft's official `windows-rs`
crate — tray via `Shell_NotifyIconW`, global hotkeys via `RegisterHotKey`, GDI
`BitBlt` capture into DIB sections, and per-monitor layered overlay windows
presented with `UpdateLayeredWindow`. No GUI framework, no Electron, no JIT.
