//! Scenario (rework-d): `Rgb` hex serde.
//!
//! `spotfreeze::settings::model::Rgb { r, g, b }` serializes as a `"#RRGGBB"`
//! hex string (SHARED API SPEC) — the form stored in `settings.json` — and
//! deserializes back; malformed strings are rejected.
//!
//! SPEC ASSUMPTIONS (INTEGRATION FLAGS — adjust only if the landed serde
//! impl deliberately differs, per the spec's "#RRGGBB" wording):
//! - Serialization emits UPPERCASE hex digits (`format!("#{:02X}..")` style).
//! - Deserialization accepts hex digits in either case
//!   (`u8::from_str_radix(.., 16)` is case-insensitive).
//!
//! Round-trip equality itself is pinned unconditionally.

use spotfreeze::settings::model::Rgb;

const SAMPLES: &[(&str, Rgb)] = &[
    ("#000000", Rgb { r: 0, g: 0, b: 0 }),
    (
        "#FFFFFF",
        Rgb {
            r: 255,
            g: 255,
            b: 255,
        },
    ),
    (
        "#802020",
        Rgb {
            r: 0x80,
            g: 0x20,
            b: 0x20,
        },
    ),
    (
        "#A1B2C3",
        Rgb {
            r: 0xA1,
            g: 0xB2,
            b: 0xC3,
        },
    ),
    (
        "#0F1E2D",
        Rgb {
            r: 0x0F,
            g: 0x1E,
            b: 0x2D,
        },
    ),
];

#[test]
fn serializes_to_hash_rrggbb_uppercase() {
    for (want_json_value, rgb) in SAMPLES {
        let json = serde_json::to_string(rgb).expect("serialize Rgb");
        assert_eq!(json, format!("\"{want_json_value}\""), "serialize {rgb:?}");
    }
}

#[test]
fn serde_round_trip_is_identity() {
    for (_, rgb) in SAMPLES {
        let json = serde_json::to_string(rgb).expect("serialize Rgb");
        let back: Rgb = serde_json::from_str(&json).expect("deserialize Rgb");
        assert_eq!(&back, rgb, "round-trip {rgb:?}");
    }
}

#[test]
fn deserialization_accepts_lowercase_hex() {
    let rgb: Rgb = serde_json::from_str("\"#a1b2c3\"").expect("lowercase hex parses");
    assert_eq!(
        rgb,
        Rgb {
            r: 0xA1,
            g: 0xB2,
            b: 0xC3
        }
    );
    let mixed: Rgb = serde_json::from_str("\"#8f2F0a\"").expect("mixed case parses");
    assert_eq!(
        mixed,
        Rgb {
            r: 0x8F,
            g: 0x2F,
            b: 0x0A
        }
    );
}

#[test]
fn channel_positions_are_rgb_order() {
    // #802020 => r=0x80, g=0x20, b=0x20 (NOT BGR, NOT #AARRGGBB slicing).
    let rgb: Rgb = serde_json::from_str("\"#802020\"").expect("parse");
    assert_eq!(rgb.r, 0x80);
    assert_eq!(rgb.g, 0x20);
    assert_eq!(rgb.b, 0x20);
}

#[test]
fn malformed_hex_strings_are_rejected() {
    for bad in [
        "\"802020\"",    // missing '#'
        "\"#80202\"",    // too short (5 hex digits)
        "\"#8020202\"",  // too long (7 hex digits)
        "\"#\"",         // empty after '#'
        "\"\"",          // empty string
        "\"#GGGGGG\"",   // non-hex digits
        "\"#80 20 20\"", // spaces inside
        "\" #802020\"",  // leading space (serialized form is exact)
        "\"#-2020\"",    // sign is not hex
        "\"0x802020\"",  // C-style prefix is not the serde form
        "\"red\"",       // color names are not the serde form
        "\"#aébcd\"",    // non-ASCII straddling an even byte boundary (D1: must
        // error, never panic — this is the settings.json startup-load path)
        "\"#１２３４５６\"", // fullwidth digits are not ASCII hex
        "123",               // non-string JSON
        "null",              // null
        "[128, 32, 32]",     // array form is not the serde form
    ] {
        assert!(
            serde_json::from_str::<Rgb>(bad).is_err(),
            "{bad} must be rejected"
        );
    }
}

#[test]
fn rgb_in_overlay_settings_json_round_trips() {
    // The persisted shape inside AppSettings: { "overlay": { "dim_opacity": ..,
    // "color": "#RRGGBB" } }.
    use spotfreeze::settings::model::OverlaySettings;
    let overlay: OverlaySettings =
        serde_json::from_str(r##"{ "dim_opacity": 90, "color": "#802020" }"##)
            .expect("overlay with color parses");
    assert_eq!(overlay.dim_opacity, 90);
    assert_eq!(
        overlay.color,
        Rgb {
            r: 0x80,
            g: 0x20,
            b: 0x20
        }
    );
    let json = serde_json::to_string(&overlay).expect("serialize overlay");
    let back: OverlaySettings = serde_json::from_str(&json).expect("round-trip");
    assert_eq!(back, overlay);
    assert!(
        json.contains("\"#802020\""),
        "color persists as #RRGGBB in the overlay JSON: {json}"
    );
}
