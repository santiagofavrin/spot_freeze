//! Application settings model — pure data, serde-friendly, JSONC-backed.
//!
//! Every settings struct implements [`Default`] and is deserialized with
//! `#[serde(default)]`, so any missing key in the settings file falls back to
//! the default value: old config files stay valid when new keys are added.
//!
//! Hotkeys are stored as their display strings (`"Win+F"`, `"Esc"`) via
//! the serde impls on [`HotkeyGesture`] / [`Modifiers`]. The overlay veil
//! color is stored as a `"#RRGGBB"` hex string via [`Rgb`]'s serde impls.

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Root settings object persisted as `spotfreeze.jsonc` in the per-platform
/// config location (see [`crate::settings::store`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub hotkeys: HotkeySettings,
    pub spotlight: SpotlightSettings,
    pub zoom: ZoomSettings,
    pub overlay: OverlaySettings,
}

/// 8-bit RGB color, serialized as an uppercase `"#RRGGBB"` hex string.
/// Parsing accepts `#RRGGBB` case-insensitively; anything else (missing `#`,
/// wrong length, non-hex digits) is rejected with a clear error.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Solid black — the default veil color.
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    /// Canonical `"#RRGGBB"` uppercase hex string (the form the settings file
    /// stores).
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Parse a `"#RRGGBB"` hex string (case-insensitive). The leading `#` is
    /// required and the string must be exactly 7 characters long.
    pub fn parse_hex(s: &str) -> Result<Self, ParseRgbError> {
        let hex = s.strip_prefix('#').ok_or_else(|| {
            ParseRgbError(format!("{s:?} is missing the leading '#' (expected \"#RRGGBB\")"))
        })?;
        // ASCII gate BEFORE any slicing: `str` indexing panics on non-char
        // boundaries, so a multi-byte UTF-8 input whose BYTE length happens to
        // pass the checks below (e.g. "#aébcd" — 6 bytes after '#') would
        // hard-crash the app (settings UI field, and settings.json load at
        // startup). Non-ASCII can never be a hex digit anyway.
        if !hex.is_ascii() {
            return Err(ParseRgbError(format!(
                "{s:?} must be ASCII hex digits only (expected \"#RRGGBB\")"
            )));
        }
        if hex.len() != 6 {
            return Err(ParseRgbError(format!(
                "{s:?} must have exactly 6 hex digits after '#' (expected \"#RRGGBB\")"
            )));
        }
        // Safe to slice: ASCII means byte length == char count and every byte
        // boundary is a char boundary.
        let channel = |pair: &str, name: &str| -> Result<u8, ParseRgbError> {
            u8::from_str_radix(pair, 16).map_err(|_| {
                ParseRgbError(format!(
                    "{s:?} has invalid hex digits in the {name} channel (expected \"#RRGGBB\")"
                ))
            })
        };
        Ok(Self {
            r: channel(&hex[0..2], "red")?,
            g: channel(&hex[2..4], "green")?,
            b: channel(&hex[4..6], "blue")?,
        })
    }
}

impl Serialize for Rgb {
    /// Serializes as [`Rgb::to_hex`] (e.g. `"#1A2B3C"`).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    /// Parses from the `"#RRGGBB"` hex string via [`Rgb::parse_hex`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// [`Rgb::parse_hex`] failure with a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseRgbError(pub String);

impl std::fmt::Display for ParseRgbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseRgbError {}

/// Every hotkey in the app is rebindable from the settings window.
/// Defaults (documented per field) are the out-of-box experience.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    /// GLOBAL hotkey: toggle screen freeze. Default: `Win+F`.
    pub freeze_toggle: HotkeyGesture,
    /// While frozen: switch to Spotlight mode (`Shift`+this key ADDS Spotlight
    /// as a layer instead of switching). Default: `S`.
    pub mode_spotlight: HotkeyGesture,
    /// While frozen: switch to Snip mode (`Shift`+this key ADDS Snip as a
    /// layer instead of switching). Default: `C`.
    pub mode_snip: HotkeyGesture,
    /// Modifier HELD while scrolling the mouse wheel to resize the spotlight
    /// circle. This is a modifier-only binding (e.g. bare `Ctrl`), not a full
    /// gesture. Default: `Ctrl`.
    pub spotlight_radius_modifier: Modifiers,
    /// Modifier HELD while scrolling the mouse wheel to zoom from ANY active
    /// mode combination. This is a modifier-only binding (e.g. bare `Shift`),
    /// not a full gesture. Default: `Shift`.
    pub zoom_modifier: Modifiers,
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
            freeze_toggle: HotkeyGesture::parse("Win+F").unwrap(),
            mode_spotlight: HotkeyGesture::parse("S").unwrap(),
            mode_snip: HotkeyGesture::parse("C").unwrap(),
            spotlight_radius_modifier: Modifiers::CTRL,
            zoom_modifier: Modifiers::SHIFT,
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
    /// 0 = invisible veil, 255 = fully opaque. Default: 160.
    pub dim_opacity: u8,
    /// Color of the veil. Default: black (`#000000`).
    pub color: Rgb,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            dim_opacity: 160,
            color: Rgb::BLACK,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (headless-safe, std + serde_json only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn gesture(s: &str) -> HotkeyGesture {
        HotkeyGesture::parse(s).unwrap()
    }

    // -- New defaults are pinned ------------------------------------------------

    #[test]
    fn hotkey_defaults_are_the_new_out_of_box_experience() {
        let d = HotkeySettings::default();
        assert_eq!(d.freeze_toggle, gesture("Win+F"));
        assert_eq!(d.mode_spotlight, gesture("S"));
        assert_eq!(d.mode_snip, gesture("C"));
        assert_eq!(d.spotlight_radius_modifier, Modifiers::CTRL);
        assert_eq!(d.zoom_modifier, Modifiers::SHIFT);
        assert_eq!(d.snip_copy, gesture("Ctrl+C"));
        assert_eq!(d.cancel, gesture("Esc"));
        assert_eq!(d.reset_zoom, gesture("0"));
    }

    #[test]
    fn freeze_toggle_default_is_win_plus_f() {
        let d = HotkeySettings::default().freeze_toggle;
        // Pinned explicitly: WIN modifier bit + vk of 'F'.
        assert_eq!(d.modifiers, Modifiers::WIN);
        assert_eq!(d.vk, 'F' as u32);
        assert_eq!(d.to_display(), "Win+F");
    }

    #[test]
    fn overlay_defaults_to_black_dimmed_veil() {
        let d = OverlaySettings::default();
        assert_eq!(d.dim_opacity, 160);
        assert_eq!(d.color, Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(d.color, Rgb::BLACK);
    }

    #[test]
    fn app_settings_default_propagates_section_defaults() {
        let d = AppSettings::default();
        assert_eq!(d.hotkeys, HotkeySettings::default());
        assert_eq!(d.overlay, OverlaySettings::default());
        assert_eq!(d.spotlight.default_radius, 150);
        assert_eq!(d.zoom.step_factor, 1.25);
        assert_eq!(d.zoom.min, 1.0);
        assert_eq!(d.zoom.max, 16.0);
    }

    // -- Rgb: traits, hex format, serde round-trip -------------------------------

    #[test]
    fn rgb_is_copy_and_compares_by_value() {
        let a = Rgb { r: 1, g: 2, b: 3 };
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, Rgb::BLACK);
        // Default is black.
        assert_eq!(Rgb::default(), Rgb::BLACK);
    }

    #[test]
    fn rgb_to_hex_is_uppercase_rrggbb() {
        assert_eq!(Rgb::BLACK.to_hex(), "#000000");
        assert_eq!(
            Rgb { r: 0x1A, g: 0x2B, b: 0x3C }.to_hex(),
            "#1A2B3C"
        );
        assert_eq!(
            Rgb { r: 255, g: 255, b: 255 }.to_hex(),
            "#FFFFFF"
        );
        // Single-nibble channels are zero-padded.
        assert_eq!(Rgb { r: 0x0A, g: 0x00, b: 0x0F }.to_hex(), "#0A000F");
    }

    #[test]
    fn rgb_parse_hex_accepts_mixed_case() {
        let want = Rgb { r: 0x1A, g: 0x2B, b: 0x3C };
        assert_eq!(Rgb::parse_hex("#1A2B3C").unwrap(), want);
        assert_eq!(Rgb::parse_hex("#1a2b3c").unwrap(), want);
        assert_eq!(Rgb::parse_hex("#1a2B3c").unwrap(), want);
        assert_eq!(Rgb::parse_hex("#000000").unwrap(), Rgb::BLACK);
        assert_eq!(
            Rgb::parse_hex("#FfFfFf").unwrap(),
            Rgb { r: 255, g: 255, b: 255 }
        );
    }

    #[test]
    fn rgb_parse_hex_rejects_malformed_with_clear_error() {
        for bad in [
            "",        // empty
            "1A2B3C",  // missing '#'
            "#1A2B3",  // too short
            "#1A2B3CD", // too long
            "#GG0000", // non-hex digits
            "#12 456", // whitespace inside
            "##1A2B3C", // double '#'
            " #1A2B3C", // leading whitespace
            "#1A2B3C ", // trailing whitespace
        ] {
            let err = Rgb::parse_hex(bad).expect_err(bad);
            let msg = err.to_string();
            assert!(!msg.is_empty());
            assert!(msg.contains("#RRGGBB"), "clear error for {bad:?}: {msg}");
        }
    }

    #[test]
    fn rgb_parse_hex_rejects_non_ascii_without_panicking() {
        // Regression (D1): a multi-byte UTF-8 char straddling an even byte
        // boundary makes `hex.len() == 6` pass, then `&hex[0..2]` PANICS.
        // Every one of these must return a clear Err — never panic.
        for bad in [
            "#aébcd",   // é straddles byte boundary 1..3, total 6 bytes
            "#abécd",   // é straddles byte boundary 2..4
            "#abcdé",   // é straddles byte boundary 4..6
            "#ébcdef",  // non-ASCII AND 7 bytes (length check would catch it)
            "#１２３４５６", // six fullwidth digits (18 bytes)
            "#😀abcde", // emoji (4 bytes)
        ] {
            let err = Rgb::parse_hex(bad).expect_err(bad);
            let msg = err.to_string();
            assert!(!msg.is_empty());
            assert!(msg.contains("#RRGGBB"), "clear error for {bad:?}: {msg}");
        }
        // The serde (settings file load) path must reject without panicking too.
        assert!(serde_json::from_str::<Rgb>("\"#aébcd\"").is_err());
    }

    #[test]
    fn rgb_parse_display_round_trip_is_stable() {
        for c in [
            Rgb::BLACK,
            Rgb { r: 255, g: 255, b: 255 },
            Rgb { r: 0xDE, g: 0xAD, b: 0xBE },
            Rgb { r: 1, g: 2, b: 3 },
        ] {
            let hex = c.to_hex();
            assert_eq!(Rgb::parse_hex(&hex).unwrap(), c, "round-trip of {hex}");
        }
    }

    #[test]
    fn rgb_serde_is_hex_string() {
        let c = Rgb { r: 0x1A, g: 0x2B, b: 0x3C };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"#1A2B3C\"");
        assert_eq!(serde_json::from_str::<Rgb>(&json).unwrap(), c);
        // Deserialize accepts lowercase too.
        assert_eq!(serde_json::from_str::<Rgb>("\"#1a2b3c\"").unwrap(), c);
        assert_eq!(serde_json::from_str::<Rgb>("\"#000000\"").unwrap(), Rgb::BLACK);
        // Malformed values and wrong types error.
        assert!(serde_json::from_str::<Rgb>("\"black\"").is_err());
        assert!(serde_json::from_str::<Rgb>("\"#12345\"").is_err());
        assert!(serde_json::from_str::<Rgb>("123").is_err());
        assert!(serde_json::from_str::<Rgb>("{ \"r\": 1 }").is_err());
    }

    // -- Backward compatibility: old settings.json files keep loading -------------

    #[test]
    fn old_json_without_color_or_zoom_modifier_gets_defaults() {
        // Shape of settings.json written before color / zoom_modifier existed.
        let old = r#"{
            "hotkeys": {
                "freeze_toggle": "Ctrl+Alt+F",
                "mode_spotlight": "1",
                "mode_zoom": "2",
                "mode_snip": "3",
                "spotlight_radius_modifier": "Ctrl",
                "snip_copy": "Ctrl+C",
                "cancel": "Esc",
                "reset_zoom": "0"
            },
            "overlay": { "dim_opacity": 200 }
        }"#;
        let loaded: AppSettings = serde_json::from_str(old).unwrap();
        // Old explicit values survive …
        assert_eq!(loaded.hotkeys.freeze_toggle, gesture("Ctrl+Alt+F"));
        assert_eq!(loaded.overlay.dim_opacity, 200);
        // … and the new keys fall back to their defaults.
        assert_eq!(loaded.hotkeys.zoom_modifier, Modifiers::SHIFT);
        assert_eq!(loaded.overlay.color, Rgb::BLACK);
    }

    #[test]
    fn empty_json_object_yields_full_defaults() {
        let loaded: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded, AppSettings::default());
    }

    #[test]
    fn full_settings_serde_round_trip() {
        let mut s = AppSettings::default();
        s.overlay.color = Rgb { r: 0x12, g: 0x34, b: 0x56 };
        s.hotkeys.zoom_modifier = Modifiers::CTRL | Modifiers::SHIFT;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"#123456\""), "color serialized as hex: {json}");
        assert!(
            json.contains("\"Ctrl+Shift\""),
            "zoom_modifier serialized as display string: {json}"
        );
        assert_eq!(serde_json::from_str::<AppSettings>(&json).unwrap(), s);
    }

    #[test]
    fn parse_rgb_error_is_a_std_error() {
        let e = Rgb::parse_hex("nope").unwrap_err();
        let _: &dyn std::error::Error = &e;
        assert_eq!(e, ParseRgbError(e.to_string()));
    }
}
