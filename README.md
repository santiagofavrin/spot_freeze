# SpotFreeze

A tiny, fast utility for Windows 11, Linux (Wayland — targeting Hyprland and
other wlroots compositors), and macOS 14+ (Apple Silicon) that lives in the
system tray and **freezes your screen** on a global hotkey — then lets you
spotlight a region, zoom in, or snip part of the frozen frame to the clipboard.
Modes are **composable**: stack a spotlight on top of a zoom, or draw a snip
over both.

SpotFreeze is built for speed: a single native Rust binary per platform (raw OS
APIs, no GUI framework, no runtime), a few MB on disk, and near-zero idle
RAM/CPU. The screen is captured **once** at freeze time; overlay frames are
re-composited from reusable buffers — never a full repaint from scratch.

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
- **Customizable overlay** — set the dim-veil color and opacity.
- **Every hotkey is rebindable**, with conflict validation. On Windows the
  built-in settings window captures whatever you press — including `Win`+key
  combinations; on Linux/macOS you edit the binding strings in `spotfreeze.json`
  (same gesture syntax, e.g. `Win+F` or `Ctrl+Alt+F`).
- **Tray-based** — no window until you ask for one. On Windows, left-click the
  tray icon for settings and right-click for the Settings/Exit menu; on Linux
  the tray menu offers *Edit settings* and *Exit*; on macOS *Edit Settings…*
  and *Exit SpotFreeze*.
- **Human-friendly settings** — a commented JSONC file (see *Settings* for the
  per-OS location); malformed files never crash the app (it falls back to
  defaults).

### Platform notes

- **Linux and macOS** — settings are edited as text: the tray menu's *Edit
  settings* (*Edit Settings…* on macOS) opens `spotfreeze.json` in your default
  editor, and changes apply on the next freeze. There is no graphical settings
  window on these platforms.
- **Linux (Wayland)** — the global hotkey is bound through the XDG
  GlobalShortcuts portal; on Hyprland this requires `xdg-desktop-portal-hyprland`
  to be installed. The tray icon needs a StatusNotifierWatcher host (waybar,
  KDE Plasma, GNOME with an AppIndicator extension) to display — without one
  the tray icon is simply absent and the hotkey still works. Exiting from the
  tray is immediate (no confirmation dialog).
- **macOS** — requires macOS 14+ on Apple Silicon. Capturing the screen needs
  the Screen Recording permission: the first freeze prompts you to grant it in
  System Settings → Privacy & Security → Screen Recording. No Accessibility
  permission is needed — the global hotkey uses Carbon's
  `RegisterEventHotKey`. Exiting keeps the Yes/No confirmation dialog.

## Default hotkeys

All bindings can be changed — in the settings window on Windows, in
`spotfreeze.json` on Linux/macOS. Mode-specific keys are only active while the
screen is frozen.

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

### Windows

1. Download `spotfreeze-windows-x64.zip` from the latest
   [GitHub Release](../../releases).
2. Unzip it anywhere you like (e.g. `C:\Tools\SpotFreeze\`).
3. Run `spotfreeze.exe` — it appears as an icon in the system tray.

On first run, `spotfreeze.json` is created automatically in the per-OS config
location (see *Settings*) with all default values. No installer, no registry
writes, no admin rights needed.

### Linux (Wayland)

1. Download `spotfreeze-linux-x64.tar.gz` from the latest
   [GitHub Release](../../releases) and extract it.
2. Run the `spotfreeze` binary — it appears as an icon in the system tray.

The binary needs `libwayland` and `libxkbcommon` present at runtime — both are
standard on any Wayland desktop, so there is usually nothing to install. For
the global hotkey, the compositor's portal backend must be installed: on
Hyprland that means `xdg-desktop-portal-hyprland`.

### macOS

1. Download `SpotFreeze-macos-arm64.zip` from the latest
   [GitHub Release](../../releases) and unzip it.
2. Move `SpotFreeze.app` wherever you like (e.g. `/Applications`).
3. The app is unsigned (ad-hoc signed), so the first launch needs a
   right-click → *Open* to get past Gatekeeper.
4. Freeze once and grant the **Screen Recording** permission — the app shows
   an alert pointing at System Settings → Privacy & Security → Screen
   Recording when it is missing.

## Command line

```
spotfreeze [--daemon] [--help] [--version]
```

- `--daemon` — start detached from the terminal (nohup-style): the process
  survives the terminal being closed afterwards. Linux/macOS only.
- `--help` — print usage and exit.
- `--version` — print the version and exit.

With no options SpotFreeze runs in the foreground: tray icon plus the global
freeze hotkey.

## Settings

On **Windows**, **left-click the tray icon** (or right-click → *Settings*) to
open the settings window. From there you can:

- Rebind every hotkey by pressing the new key combination — including
  `Win`+key combos; conflicting bindings are rejected.
- Adjust the spotlight radius, dim-veil opacity, and zoom limits.
- **Customize the overlay color** — pick a color with the color picker or type
  a `#RRGGBB` hex value. The veil outside spotlight/selection areas is drawn
  in this color at the configured opacity.
- Exit via right-click → *Exit* (a Yes/No confirmation prevents accidents).

On **Linux and macOS** there is no settings window: choose *Edit settings*
(*Edit Settings…* on macOS) in the tray menu to open `spotfreeze.json` in your
default editor and save — the same options (hotkeys, spotlight radius, veil
color/opacity, zoom limits) are keys in the file. Exiting from the tray needs
no confirmation on Linux; macOS keeps the Yes/No dialog.

The settings file lives in the per-OS config location:

| OS | Path |
| --- | --- |
| Windows | `%APPDATA%\SpotFreeze\spotfreeze.json` |
| Linux | `$XDG_CONFIG_HOME/spotfreeze/spotfreeze.json` (default `~/.config/spotfreeze/spotfreeze.json`) |
| macOS | `~/Library/Application Support/SpotFreeze/spotfreeze.json` |

A `settings.json` written by an older release (beside the exe on Windows, in
the same config folder on Linux/macOS) is migrated automatically on first
launch.

It is the same JSONC file on every OS — comments and trailing commas are
allowed — and it is written atomically, so a crash mid-save can never corrupt
it. Missing keys fall back to defaults, and changes apply on the next freeze.

Example `spotfreeze.json` (excerpt):

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

### Windows

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
run it — `spotfreeze.json` will be created on first launch.

### Linux

With stable Rust via [rustup](https://rustup.rs/) it is a plain cargo build —
no system dev packages are needed (libwayland and libxkbcommon are loaded at
runtime):

```bash
git clone https://github.com/<owner>/spot_freeze.git
cd spot_freeze
cargo build --release
```

The binary is at `target/release/spotfreeze`.

Alternatively, use the Docker workflow (no local Rust toolchain needed):

```bash
docker compose run test   # run the headless test suite
docker compose run build  # release binary into ./target/docker/
docker compose run dev    # interactive shell, cargo caches kept in volumes
```

### macOS

With stable Rust via [rustup](https://rustup.rs/) (`aarch64-apple-darwin`):

```bash
git clone https://github.com/<owner>/spot_freeze.git
cd spot_freeze
cargo build --release
# Assemble and ad-hoc sign SpotFreeze.app (version = the crate's version):
packaging/macos/build-app.sh target/release/spotfreeze 0.1.0
```

## Test

```powershell
cargo test
```

The whole suite is headless and safe to run on a live desktop on all three
OSes: all logic (pixel compositing, geometry, hotkey parsing, settings
round-trips) is decoupled from the platform APIs into pure functions. Tests
open no windows, register no hotkeys, and never touch the real clipboard or
screen. Platform-specific tests run on their CI runners (Windows, Linux via
Docker, macOS).

## Release process

Releases are managed by [release-please](https://github.com/googleapis/release-please)
(release type `rust`) via `.github/workflows/release.yml` — fully automatic,
no human steps:

1. Push changes to `main` using
   [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`,
   `fix:`, …). release-please opens the release PR (changelog + version bump,
   including `Cargo.toml`), the same workflow run squash-merges it, tags the
   `v*` tag, and creates the GitHub release. Pushes without releasable
   commits (`docs:`, `chore:`, `enhancement:`, …) produce no release.
2. The same workflow then builds all three artifacts (one job per OS) and
   attaches them to the release automatically:
   - `spotfreeze-windows-x64.zip` (`spotfreeze.exe`, built on `windows-latest`)
   - `spotfreeze-linux-x64.tar.gz` (`spotfreeze`, built in Docker)
   - `SpotFreeze-macos-arm64.zip` (`SpotFreeze.app`, built on a macOS runner)

`.github/workflows/build.yml` is a manual-only workflow (`workflow_dispatch`)
that runs `cargo test`, builds the release binaries for all three OSes, and
uploads the same artifacts for ad-hoc verification.

## Tech

Rust (stable, edition 2024), one crate with a pure, fully unit-tested core
(geometry, compositing, modes, settings, hotkey gestures) and a thin
per-platform shell behind two traits (`OverlaySurface`, `PlatformServices`).
No GUI framework, no Electron, no JIT. All platforms share the same BGRA frame
buffers, so captured pixels flow into the overlays without conversion;
clipboard images are `CF_DIB` on Windows and PNG on Linux/macOS.

- **Windows** (MSVC toolchain) — Microsoft's official `windows-rs` crate: tray
  via `Shell_NotifyIconW`, global hotkeys via a low-level keyboard hook
  (`WH_KEYBOARD_LL`, so `Win`+key combos bind reliably), GDI `BitBlt` capture
  into DIB sections, and per-monitor layered overlay windows presented with
  `UpdateLayeredWindow`.
- **Linux (Wayland)** — `wayland-client` with the wlr-layer-shell and
  wlr-screencopy protocols (libwayland and libxkbcommon loaded at runtime, so
  no dev packages are needed to build), global hotkey via the XDG
  GlobalShortcuts portal (`ashpd`/zbus), tray via StatusNotifierItem (`ksni`).
- **macOS** — `objc2` bindings to AppKit and CoreGraphics, ScreenCaptureKit
  capture (macOS 14+), Carbon `RegisterEventHotKey` for the global hotkey,
  `NSStatusItem` tray, `NSPasteboard` clipboard.
