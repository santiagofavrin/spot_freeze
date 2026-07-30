# SpotFreeze

A tiny, fast utility for Windows 11, Linux (Wayland — targeting Hyprland and
other wlroots compositors), and macOS 14+ (Apple Silicon) that lives in the
system tray and **freezes your screen** on a global hotkey — then lets you
spotlight a region, zoom in as a persistent layer, or enter **capture mode**
to snip part of the frozen frame to the clipboard — with the spotlight and
zoom effects baked in.

SpotFreeze is built for speed: a single native Rust binary per platform (raw OS
APIs, no GUI framework, no runtime), a few MB on disk, and near-zero idle
RAM/CPU. The screen is captured **once** at freeze time; overlay frames are
re-composited from reusable buffers — never a full repaint from scratch.

## Features

- **Freeze the screen** with a customizable global hotkey (`Win+F` out of the
  box, including full support for `Win`+key combos). All monitors are captured
  at once; each monitor gets its own darkened overlay, so multi-monitor setups
  are fully covered.
- **Spotlight toggle** — a bright circle follows your cursor over the dimmed
  frozen screen; `S` turns it on and off (off = frozen but clear, no dim).
  Hold `Ctrl` and scroll the mouse wheel to resize it.
- **Zoom hold** — `F` toggles a persistent zoom layer at the last-used level
  (1.0×–16.0×, ×1.25 per notch by default): magnify around the cursor with
  the mouse wheel, on top of spotlight on or off. Zoom is also reachable from
  anywhere: hold `Shift` and scroll.
- **Capture mode** — `C` re-freezes the screen with the effects active at
  that moment (spotlight and/or zoom) baked in, then drag a rectangle and
  copy the *effected* pixels to the clipboard (see *Copying screenshots*
  below). A persistent accent frame border marks capture mode.
- **Border flash feedback** — every mode change flashes the screen border so
  you always know which mode you just entered.
- **Customizable overlay** — set the dim-veil color and opacity.
- **Every hotkey is rebindable**, with conflict validation. On Windows the
  built-in settings window captures whatever you press — including `Win`+key
  combinations; on Linux/macOS you edit the binding strings in `spotfreeze.jsonc`
  (same gesture syntax, e.g. `Win+F` or `Ctrl+Alt+F`).
- **Tray-based** — no window until you ask for one. On Windows, left-click the
  tray icon for settings and right-click for the Settings/Exit menu; on Linux
  the tray menu offers *Edit settings*, *Reload settings*, and *Exit*; on macOS
  *Edit Settings…*, *Reload Settings*, and *Exit SpotFreeze*.
- **Human-friendly settings** — a commented JSONC file (see *Settings* for the
  per-OS location); malformed files never crash the app (it falls back to
  defaults).

### Platform notes

- **Linux and macOS** — settings are edited as text: the tray menu's *Edit
  settings* (*Edit Settings…* on macOS) opens `spotfreeze.jsonc` in your default
  editor. Changes apply on the next freeze, or immediately via the tray's
  *Reload settings* (*Reload Settings* on macOS) — that also re-registers a
  changed freeze binding on the spot. There is no graphical settings window on
  these platforms.
- **Linux (Wayland)** — on Hyprland, bind the freeze hotkey in `hyprland.conf`
  (see *Install*): `bind = SUPER, F, exec, spotfreeze toggle`. On KDE/GNOME the
  XDG GlobalShortcuts portal is used instead (Hyprland also supports the portal,
  but only with a manual `global` bind in `hyprland.conf`). The tray icon needs
  a StatusNotifierWatcher host (waybar, KDE Plasma, GNOME with an AppIndicator
  extension) to display — without one the tray icon is simply absent and the
  hotkey still works. Exiting from the tray is immediate (no confirmation
  dialog).
- **macOS** — requires macOS 14+ on Apple Silicon. Capturing the screen needs
  the Screen Recording permission: the first freeze prompts you to grant it in
  System Settings → Privacy & Security → Screen Recording. No Accessibility
  permission is needed — the global hotkey uses Carbon's
  `RegisterEventHotKey`. Exiting keeps the Yes/No confirmation dialog.

## Default hotkeys

All bindings can be changed — in the settings window on Windows, in
`spotfreeze.jsonc` on Linux/macOS. Mode-specific keys are only active while the
screen is frozen.

| Action | Default | Scope |
| --- | --- | --- |
| Toggle screen freeze | `Win+F` | Global — works from any app (in capture mode it backs out of capture first, like `Esc`) |
| Spotlight toggle | `S` | While frozen — off = screen stays frozen but CLEAR (no dim), on = spotlight back (1 border flash) |
| Capture mode | `C` | While frozen — re-freezes with the current effects baked in (3 border flashes + a persistent accent frame) |
| Zoom hold toggle | `F` | While frozen — applies the last-used zoom level as a layer over spotlight on/off (2 border flashes); toggle off to drop it |
| Zoom in / out (from anywhere) | `Shift` + mouse wheel | While frozen — adds the zoom layer on the spot if it isn't active yet (no border flash) |
| Zoom in / out (zoom hold active) | Mouse wheel | While frozen, whenever the zoom layer is on |
| Resize spotlight circle | `Ctrl` + mouse wheel | While spotlight is active |
| Reset zoom to 1.0× | `0` | While zoom is active |
| Copy screenshot to clipboard | `Ctrl+C` | While frozen (see below) |
| Unfreeze / exit capture | `Esc` | While frozen — see below |

Other defaults: spotlight radius `150` px, dim-veil opacity `160` (0–255),
dim-veil color black (`#000000`), zoom step `1.25` (min `1.0`, max `16.0`).

Freezing starts with the spotlight on. `Esc` unfreezes from spotlight mode
(on or off); in capture mode it exits capture instead — back to the
pre-capture frozen view with its spotlight/zoom state restored (the capture
re-freeze is dropped). `Ctrl+C` copies and closes from anywhere.

Freezing and unfreezing play a quick fade (160 ms, never more than 180 ms)
instead of an abrupt cut: the frozen view fades in over the live screen on
freeze and back out when the freeze fully ends. On Wayland the unfreeze fade
blends back to the freeze-time capture, so anything that changed on screen
while frozen reappears with a small pop as the overlay closes. Exiting
capture mode and copying with `Ctrl+C` stay instant — no transition there.

## Layers and capture

Spotlight and zoom are **layers**; capture is the only real mode switch:

- **`S` (Spotlight) — toggle.** Turns the spotlight layer on and off without
  touching the zoom layer. With every layer off, the screen stays frozen (all
  input still captured) but the overlay is completely clear — no dim at all.
- **`F` (Zoom hold) — toggle.** Adds the zoom layer at the last-used zoom
  level, on top of spotlight on or off; press again to drop it. Zooming
  changes the level for next time (`0` resets it to 1.0×).
- **`C` (Capture) — re-freeze.** The view exactly as it is now — spotlight
  and/or zoom baked in — becomes the new frozen frame, and a drag-selection
  snips the *effected* pixels from it. `Esc` discards the re-frozen frame and
  returns to the pre-capture view (spotlight on/off and zoom restored);
  pressing `C` again while in capture just clears the selection.

The wheel follows the layers: `Ctrl`+wheel resizes the spotlight whenever it
is active, `Shift`+wheel zooms from anywhere (implicitly adding the zoom
layer — without a border flash — when it isn't active yet), and a plain
wheel zooms whenever the zoom layer is on.

**Border flash feedback:** every activation flashes the screen border a
number of times that identifies it — **1 flash** for Spotlight (`S`), **2**
for Zoom hold (`F`), **3** for Capture (`C`); deactivating a layer does not
flash. Freezing the screen starts with the spotlight on, so you also see a
single flash right at freeze time. While capture mode is active, a thin
accent-colored frame border stays on screen (separate from these one-off
flashes).

## Install

### Windows

1. Download `spotfreeze-windows-x64.zip` from the latest
   [GitHub Release](../../releases).
2. Unzip it anywhere you like (e.g. `C:\Tools\SpotFreeze\`).
3. Run `spotfreeze.exe` — it appears as an icon in the system tray.

On first run, `spotfreeze.jsonc` is created automatically in the per-OS config
location (see *Settings*) with all default values. No installer, no registry
writes, no admin rights needed.

### Linux (Wayland)

1. Download `spotfreeze-linux-x64.tar.gz` from the latest
   [GitHub Release](../../releases) and extract it.
2. Run the `spotfreeze` binary — it appears as an icon in the system tray.

The binary needs `libwayland` and `libxkbcommon` present at runtime — both are
standard on any Wayland desktop, so there is usually nothing to install.

**The freeze hotkey on Hyprland** is a compositor bind running the CLI toggle —
add to `~/.config/hypr/hyprland.conf` and reload Hyprland:

```
bind = SUPER, F, exec, spotfreeze toggle
```

(Pick any combo you like — it does not have to match `freeze_toggle` in
`spotfreeze.jsonc`; the CLI toggles the running instance directly. The XDG
GlobalShortcuts portal is also supported for desktops that auto-bind it, like
KDE and GNOME.)

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
spotfreeze [toggle] [--daemon] [--help] [--version]
```

- `toggle` — ask the running instance to toggle the freeze and exit. Linux
  only; this is what compositor keybinds call (see *Install*).
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
- **Toggle auto-start** — launch SpotFreeze at login (registered in the
  current-user Run registry key; no admin rights needed).
- Exit via right-click → *Exit* (a Yes/No confirmation prevents accidents).

On **Linux and macOS** there is no settings window: choose *Edit settings*
(*Edit Settings…* on macOS) in the tray menu to open `spotfreeze.jsonc` in your
default editor and save — the same options (hotkeys, spotlight radius, veil
color/opacity, zoom limits) are keys in the file. Exiting from the tray needs
no confirmation on Linux; macOS keeps the Yes/No dialog.

`auto_start` (Windows/macOS only, default `false`) launches SpotFreeze at
login: on Windows via the current-user Run registry key, on macOS via a
`~/Library/LaunchAgents/com.spotfreeze.app.plist` LaunchAgent (works for the
bare binary and the packaged `.app` alike). Hand-edited JSONC is reconciled
with the registry/plist on the next launch.

The settings file lives in the per-OS config location:

| OS | Path |
| --- | --- |
| Windows | `%APPDATA%\SpotFreeze\spotfreeze.jsonc` |
| Linux | `$XDG_CONFIG_HOME/spotfreeze/spotfreeze.jsonc` (default `~/.config/spotfreeze/spotfreeze.jsonc`) |
| macOS | `~/Library/Application Support/SpotFreeze/spotfreeze.jsonc` |

A `spotfreeze.json` or `settings.json` written by an older release (in the
same config folder; beside the exe on early Windows releases) is migrated
automatically on first launch.

It is the same JSONC file on every OS — comments and trailing commas are
allowed — and it is written atomically, so a crash mid-save can never corrupt
it. Missing keys fall back to defaults, and changes apply on the next freeze.

Example `spotfreeze.jsonc` (excerpt):

```jsonc
{
  "hotkeys": {
    "freeze_toggle": "Win+F",
    "mode_spotlight": "S",
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
  // Launch at login — Windows/macOS only (default: false).
  "auto_start": false,
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
run it — `spotfreeze.jsonc` will be created on first launch.

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
