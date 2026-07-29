# SpotFreeze

A tiny, fast Windows 11 utility that lives in the system tray and **freezes your
screen** on a global hotkey — then lets you spotlight a region, zoom in, or snip
part of the frozen frame to the clipboard. Modes are **composable**: stack a
spotlight on top of a zoom, or draw a snip over both.

SpotFreeze is built for speed: a single native Rust binary (raw Win32, no GUI
framework, no runtime), a few MB on disk, and near-zero idle RAM/CPU. The screen
is captured **once** at freeze time; overlay frames are re-composited from
reusable buffers — never a full repaint from scratch.

## Features

- **Freeze the screen** with a customizable global hotkey (`Win+F` out of the
  box, including full support for `Win`+key combos). All monitors are captured
  at once; each monitor gets its own darkened overlay, so multi-monitor setups
  are fully covered.
- **Spotlight mode** — a bright circle follows your cursor over the dimmed
  frozen screen. Hold `Ctrl` and scroll the mouse wheel to resize it.
- **Zoom mode** — magnify the frozen frame around the cursor with the mouse
  wheel (1.0×–16.0×, ×1.25 per notch by default). Zoom is also reachable from
  *any* mode: hold `Shift` and scroll.
- **Snip mode** — drag a rectangle on the frozen screen and copy it to the
  clipboard (see *Copying screenshots* below).
- **Composable modes** — layer spotlight, zoom, and snip on top of each other
  instead of being locked into one at a time (see *Mixing modes* below).
- **Border flash feedback** — every mode change flashes the screen border so
  you always know which mode you just entered.
- **Customizable overlay** — pick the dim-veil color and opacity.
- **Every hotkey is rebindable** from the built-in settings window, with
  conflict validation. Rebinding captures whatever you press — including
  `Win`+key combinations.
- **Tray-based** — no window until you ask for one. Left-click the tray icon
  for settings, right-click for the Settings/Exit menu.
- **Human-friendly settings** — a commented JSONC file next to the exe;
  malformed files never crash the app (it falls back to defaults).

## Default hotkeys

All bindings can be changed in the settings window. Mode-specific keys are only
active while the screen is frozen.

| Action | Default | Scope |
| --- | --- | --- |
| Toggle screen freeze | `Win+F` | Global — works from any app |
| Spotlight mode | `S` | While frozen |
| Zoom mode | `Z` | While frozen |
| Snip mode | `C` | While frozen |
| Add a mode as a layer (no reset) | `Shift` + mode key (`Shift+S`/`Shift+Z`/`Shift+C`) | While frozen |
| Zoom in / out (from any mode) | `Shift` + mouse wheel | While frozen — adds the zoom layer on the spot if it isn't active yet (no border flash) |
| Zoom in / out (zoom is primary mode) | Mouse wheel | Zoom mode |
| Resize spotlight circle | `Ctrl` + mouse wheel | While spotlight is active |
| Reset zoom to 1.0× | `0` | While zoom is active |
| Copy screenshot to clipboard | `Ctrl+C` | While frozen (see below) |
| Unfreeze / cancel | `Esc` | While frozen |

Other defaults: spotlight radius `150` px, dim-veil opacity `160` (0–255),
dim-veil color black (`#000000`), zoom step `1.25` (min `1.0`, max `16.0`).

Freezing starts in Spotlight mode. `Esc` unfreezes and `Ctrl+C` copies and
closes — from **any** mode combination.

## Mixing modes

Modes are **composable layers**, not exclusive states. How you press a mode key
decides whether the other layers survive:

- **Plain mode key (`S` / `Z` / `C`) — full switch.** Resets *all* mode state
  (zoom back to 1.0×, snip selection cleared, spotlight radius back to the
  default) and activates only that mode.
- **`Shift` + mode key — add a layer.** Activates that mode *on top of* the
  current combination without touching any existing layer. Example: while
  zoomed in, press `Shift+S` to add a spotlight circle over the magnified view;
  while spotlighted, press `Shift+Z` to start zooming without losing the
  spotlight.

The wheel follows the layers, not the "current mode": `Ctrl`+wheel resizes the
spotlight whenever a spotlight is active, `Shift`+wheel zooms from any layer
combination (implicitly adding the zoom layer — without a border flash — when
it isn't active yet), and a plain wheel zooms when Zoom is the primary mode.

**Border flash feedback:** every mode change flashes the screen border a number
of times that identifies the mode — **1 flash** for Spotlight (`S`), **2** for
Zoom (`Z`), **3** for Snip (`C`). Freezing the screen starts in Spotlight mode,
so you also see a single flash right at freeze time.

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

- Rebind every hotkey by pressing the new key combination — including
  `Win`+key combos; conflicting bindings are rejected.
- Adjust the spotlight radius, dim-veil opacity, and zoom limits.
- **Customize the overlay color** — pick a color with the color picker or type
  a `#RRGGBB` hex value. The veil outside spotlight/selection areas is drawn
  in this color at the configured opacity.
- Exit via right-click → *Exit* (a Yes/No confirmation prevents accidents).

Settings are stored in `settings.json` beside `spotfreeze.exe`. It is a JSONC
file — comments and trailing commas are allowed — and is written atomically, so
a crash mid-save can never corrupt it. Missing keys fall back to defaults, and
changes apply on the next freeze.

Example `settings.json` (excerpt):

```jsonc
{
  "hotkeys": {
    "freeze_toggle": "Win+F",
    "mode_spotlight": "S",
    "mode_zoom": "Z",
    "mode_snip": "C",
    // Modifier held + wheel to zoom from ANY mode (default: "Shift").
    "zoom_modifier": "Shift",
    // Modifier held + wheel to resize the spotlight (default: "Ctrl").
    "spotlight_radius_modifier": "Ctrl",
  },
  "overlay": {
    "dim_opacity": 160,     // 0 = invisible veil, 255 = solid
    "color": "#000000",     // veil color as #RRGGBB (default: black)
  },
}
```

## Copying screenshots

While frozen, pressing `Ctrl+C` (the copy binding) copies to the clipboard and
then unfreezes — from any mode or mode combination:

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
crate — tray via `Shell_NotifyIconW`, global hotkeys via a low-level keyboard
hook (`WH_KEYBOARD_LL`, so `Win`+key combos bind reliably), GDI `BitBlt`
capture into DIB sections, and per-monitor layered overlay windows presented
with `UpdateLayeredWindow`. No GUI framework, no Electron, no JIT.
