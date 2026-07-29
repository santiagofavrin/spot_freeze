//! Application settings model — pure data, serde-friendly, JSONC-backed.
//!
//! Every settings struct implements [`Default`] and is deserialized with
//! `#[serde(default)]`, so any missing key in `settings.json` falls back to the
//! default value: old config files stay valid when new keys are added.
//!
//! Hotkeys are stored as their display strings (`"Ctrl+Alt+F"`, `"Esc"`) via
//! the serde impls on [`HotkeyGesture`] / [`Modifiers`].

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use serde::{Deserialize, Serialize};

/// Root settings object persisted in `settings.json` next to the exe.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub hotkeys: HotkeySettings,
    pub spotlight: SpotlightSettings,
    pub zoom: ZoomSettings,
    pub overlay: OverlaySettings,
}

/// Every hotkey in the app is rebindable from the settings window.
/// Defaults (documented per field) are the out-of-box experience.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    /// GLOBAL hotkey: toggle screen freeze. Default: `Ctrl+Alt+F`.
    pub freeze_toggle: HotkeyGesture,
    /// While frozen: switch to Spotlight mode. Default: `1`.
    pub mode_spotlight: HotkeyGesture,
    /// While frozen: switch to Zoom mode. Default: `2`.
    pub mode_zoom: HotkeyGesture,
    /// While frozen: switch to Snip mode. Default: `3`.
    pub mode_snip: HotkeyGesture,
    /// Modifier HELD while scrolling the mouse wheel to resize the spotlight
    /// circle. This is a modifier-only binding (e.g. bare `Ctrl`), not a full
    /// gesture. Default: `Ctrl`.
    pub spotlight_radius_modifier: Modifiers,
    /// Snip mode: copy the selection (or the focused monitor's full frame when
    /// no selection exists) to the clipboard, then close the overlay.
    /// Default: `Ctrl+C`.
    pub snip_copy: HotkeyGesture,
    /// Unfreeze / cancel. Default: `Esc`.
    pub cancel: HotkeyGesture,
    /// Zoom mode: reset zoom to 1.0. Default: `0`.
    pub reset_zoom: HotkeyGesture,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            freeze_toggle: HotkeyGesture::parse("Ctrl+Alt+F").unwrap(),
            mode_spotlight: HotkeyGesture::parse("1").unwrap(),
            mode_zoom: HotkeyGesture::parse("2").unwrap(),
            mode_snip: HotkeyGesture::parse("3").unwrap(),
            spotlight_radius_modifier: Modifiers::CTRL,
            snip_copy: HotkeyGesture::parse("Ctrl+C").unwrap(),
            cancel: HotkeyGesture::parse("Esc").unwrap(),
            reset_zoom: HotkeyGesture::parse("0").unwrap(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotlightSettings {
    /// Spotlight circle radius at freeze time, in physical pixels of the
    /// monitor under the cursor. Default: 150.
    pub default_radius: u32,
}

impl Default for SpotlightSettings {
    fn default() -> Self {
        Self { default_radius: 150 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoomSettings {
    /// Multiplicative zoom change per mouse-wheel notch (one notch = 120 wheel
    /// delta units). Must be > 1.0. Default: 1.25.
    pub step_factor: f32,
    /// Minimum zoom (1.0 = no magnification). Default: 1.0.
    pub min: f32,
    /// Maximum zoom. Default: 16.0.
    pub max: f32,
}

impl Default for ZoomSettings {
    fn default() -> Self {
        Self {
            step_factor: 1.25,
            min: 1.0,
            max: 16.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlaySettings {
    /// Opacity of the dark veil applied outside spotlight / selection areas.
    /// 0 = invisible veil, 255 = fully black. Default: 160.
    pub dim_opacity: u8,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self { dim_opacity: 160 }
    }
}
