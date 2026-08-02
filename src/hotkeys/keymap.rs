//! Cross-platform key map: Win32 virtual-key codes (the gesture model's
//! internal lingua franca — see [`crate::hotkeys::gesture`]) translated to
//! Linux xkb keysyms and macOS `CGKeyCode`s.
//!
//! Pure data tables + lookups. The table covers the ENTIRE key vocabulary the
//! gesture parser accepts: letters, digits, F1–F24, and every named key.
//! VK values outside that vocabulary (reachable only through the parser's
//! `0x..` hex fallback) map to `None`.
//!
//! macOS gaps (no `CGKeyCode` exists): F21–F24 (Apple keyboards top out at
//! F20) and PrintScreen (a PC PrintScreen key arrives as F13 there). The OEM
//! punctuation rows are US-layout positional on all three platforms.

/// One key across the three platforms.
struct KeyMapEntry {
    /// Win32 virtual-key code.
    vk: u32,
    /// xkb keysym (values match X11 `keysymdef.h`).
    xkb: u32,
    /// macOS `CGKeyCode` hardware keycode; `None` when macOS has no equivalent.
    cg: Option<u16>,
}

/// The full parseable vocabulary. xkb letters are the LOWERCASE keysyms
/// (`XK_a`..`XK_z`); [`xkb_to_vk`] normalizes the uppercase variants.
const KEY_MAP: &[KeyMapEntry] = &[
    // Letters: VK == uppercase ASCII, xkb == lowercase ASCII.
    KeyMapEntry {
        vk: 0x41,
        xkb: 0x61,
        cg: Some(0x00),
    }, // A
    KeyMapEntry {
        vk: 0x42,
        xkb: 0x62,
        cg: Some(0x0B),
    }, // B
    KeyMapEntry {
        vk: 0x43,
        xkb: 0x63,
        cg: Some(0x08),
    }, // C
    KeyMapEntry {
        vk: 0x44,
        xkb: 0x64,
        cg: Some(0x02),
    }, // D
    KeyMapEntry {
        vk: 0x45,
        xkb: 0x65,
        cg: Some(0x0E),
    }, // E
    KeyMapEntry {
        vk: 0x46,
        xkb: 0x66,
        cg: Some(0x03),
    }, // F
    KeyMapEntry {
        vk: 0x47,
        xkb: 0x67,
        cg: Some(0x05),
    }, // G
    KeyMapEntry {
        vk: 0x48,
        xkb: 0x68,
        cg: Some(0x04),
    }, // H
    KeyMapEntry {
        vk: 0x49,
        xkb: 0x69,
        cg: Some(0x22),
    }, // I
    KeyMapEntry {
        vk: 0x4A,
        xkb: 0x6A,
        cg: Some(0x26),
    }, // J
    KeyMapEntry {
        vk: 0x4B,
        xkb: 0x6B,
        cg: Some(0x28),
    }, // K
    KeyMapEntry {
        vk: 0x4C,
        xkb: 0x6C,
        cg: Some(0x25),
    }, // L
    KeyMapEntry {
        vk: 0x4D,
        xkb: 0x6D,
        cg: Some(0x2E),
    }, // M
    KeyMapEntry {
        vk: 0x4E,
        xkb: 0x6E,
        cg: Some(0x2D),
    }, // N
    KeyMapEntry {
        vk: 0x4F,
        xkb: 0x6F,
        cg: Some(0x1F),
    }, // O
    KeyMapEntry {
        vk: 0x50,
        xkb: 0x70,
        cg: Some(0x23),
    }, // P
    KeyMapEntry {
        vk: 0x51,
        xkb: 0x71,
        cg: Some(0x0C),
    }, // Q
    KeyMapEntry {
        vk: 0x52,
        xkb: 0x72,
        cg: Some(0x0F),
    }, // R
    KeyMapEntry {
        vk: 0x53,
        xkb: 0x73,
        cg: Some(0x01),
    }, // S
    KeyMapEntry {
        vk: 0x54,
        xkb: 0x74,
        cg: Some(0x11),
    }, // T
    KeyMapEntry {
        vk: 0x55,
        xkb: 0x75,
        cg: Some(0x20),
    }, // U
    KeyMapEntry {
        vk: 0x56,
        xkb: 0x76,
        cg: Some(0x09),
    }, // V
    KeyMapEntry {
        vk: 0x57,
        xkb: 0x77,
        cg: Some(0x0D),
    }, // W
    KeyMapEntry {
        vk: 0x58,
        xkb: 0x78,
        cg: Some(0x07),
    }, // X
    KeyMapEntry {
        vk: 0x59,
        xkb: 0x79,
        cg: Some(0x10),
    }, // Y
    KeyMapEntry {
        vk: 0x5A,
        xkb: 0x7A,
        cg: Some(0x06),
    }, // Z
    // Digits: VK == xkb == ASCII.
    KeyMapEntry {
        vk: 0x30,
        xkb: 0x30,
        cg: Some(0x1D),
    }, // 0
    KeyMapEntry {
        vk: 0x31,
        xkb: 0x31,
        cg: Some(0x12),
    }, // 1
    KeyMapEntry {
        vk: 0x32,
        xkb: 0x32,
        cg: Some(0x13),
    }, // 2
    KeyMapEntry {
        vk: 0x33,
        xkb: 0x33,
        cg: Some(0x14),
    }, // 3
    KeyMapEntry {
        vk: 0x34,
        xkb: 0x34,
        cg: Some(0x15),
    }, // 4
    KeyMapEntry {
        vk: 0x35,
        xkb: 0x35,
        cg: Some(0x17),
    }, // 5
    KeyMapEntry {
        vk: 0x36,
        xkb: 0x36,
        cg: Some(0x16),
    }, // 6
    KeyMapEntry {
        vk: 0x37,
        xkb: 0x37,
        cg: Some(0x1A),
    }, // 7
    KeyMapEntry {
        vk: 0x38,
        xkb: 0x38,
        cg: Some(0x1C),
    }, // 8
    KeyMapEntry {
        vk: 0x39,
        xkb: 0x39,
        cg: Some(0x19),
    }, // 9
    // F1-F24: VK_F1 == 0x70 and XK_F1 == 0xFFBE, both contiguous. CGKeyCode
    // stops at F20; F21-F24 have no macOS keycode.
    KeyMapEntry {
        vk: 0x70,
        xkb: 0xFFBE,
        cg: Some(0x7A),
    }, // F1
    KeyMapEntry {
        vk: 0x71,
        xkb: 0xFFBF,
        cg: Some(0x78),
    }, // F2
    KeyMapEntry {
        vk: 0x72,
        xkb: 0xFFC0,
        cg: Some(0x63),
    }, // F3
    KeyMapEntry {
        vk: 0x73,
        xkb: 0xFFC1,
        cg: Some(0x76),
    }, // F4
    KeyMapEntry {
        vk: 0x74,
        xkb: 0xFFC2,
        cg: Some(0x60),
    }, // F5
    KeyMapEntry {
        vk: 0x75,
        xkb: 0xFFC3,
        cg: Some(0x61),
    }, // F6
    KeyMapEntry {
        vk: 0x76,
        xkb: 0xFFC4,
        cg: Some(0x62),
    }, // F7
    KeyMapEntry {
        vk: 0x77,
        xkb: 0xFFC5,
        cg: Some(0x64),
    }, // F8
    KeyMapEntry {
        vk: 0x78,
        xkb: 0xFFC6,
        cg: Some(0x65),
    }, // F9
    KeyMapEntry {
        vk: 0x79,
        xkb: 0xFFC7,
        cg: Some(0x6D),
    }, // F10
    KeyMapEntry {
        vk: 0x7A,
        xkb: 0xFFC8,
        cg: Some(0x67),
    }, // F11
    KeyMapEntry {
        vk: 0x7B,
        xkb: 0xFFC9,
        cg: Some(0x6F),
    }, // F12
    KeyMapEntry {
        vk: 0x7C,
        xkb: 0xFFCA,
        cg: Some(0x69),
    }, // F13
    KeyMapEntry {
        vk: 0x7D,
        xkb: 0xFFCB,
        cg: Some(0x6B),
    }, // F14
    KeyMapEntry {
        vk: 0x7E,
        xkb: 0xFFCC,
        cg: Some(0x71),
    }, // F15
    KeyMapEntry {
        vk: 0x7F,
        xkb: 0xFFCD,
        cg: Some(0x6A),
    }, // F16
    KeyMapEntry {
        vk: 0x80,
        xkb: 0xFFCE,
        cg: Some(0x40),
    }, // F17
    KeyMapEntry {
        vk: 0x81,
        xkb: 0xFFCF,
        cg: Some(0x4F),
    }, // F18
    KeyMapEntry {
        vk: 0x82,
        xkb: 0xFFD0,
        cg: Some(0x50),
    }, // F19
    KeyMapEntry {
        vk: 0x83,
        xkb: 0xFFD1,
        cg: Some(0x5A),
    }, // F20
    KeyMapEntry {
        vk: 0x84,
        xkb: 0xFFD2,
        cg: None,
    }, // F21
    KeyMapEntry {
        vk: 0x85,
        xkb: 0xFFD3,
        cg: None,
    }, // F22
    KeyMapEntry {
        vk: 0x86,
        xkb: 0xFFD4,
        cg: None,
    }, // F23
    KeyMapEntry {
        vk: 0x87,
        xkb: 0xFFD5,
        cg: None,
    }, // F24
    // Named keys (mirrors the gesture parser's NAMED_KEYS).
    KeyMapEntry {
        vk: 0x1B,
        xkb: 0xFF1B,
        cg: Some(0x35),
    }, // Esc (XK_Escape)
    KeyMapEntry {
        vk: 0x20,
        xkb: 0x0020,
        cg: Some(0x31),
    }, // Space (XK_space)
    KeyMapEntry {
        vk: 0x0D,
        xkb: 0xFF0D,
        cg: Some(0x24),
    }, // Enter (XK_Return)
    KeyMapEntry {
        vk: 0x09,
        xkb: 0xFF09,
        cg: Some(0x30),
    }, // Tab (XK_Tab)
    KeyMapEntry {
        vk: 0x08,
        xkb: 0xFF08,
        cg: Some(0x33),
    }, // Backspace (XK_BackSpace; CG "Delete")
    KeyMapEntry {
        vk: 0x2E,
        xkb: 0xFFFF,
        cg: Some(0x75),
    }, // Delete (XK_Delete; CG ForwardDelete)
    KeyMapEntry {
        vk: 0x2D,
        xkb: 0xFF63,
        cg: Some(0x72),
    }, // Insert (XK_Insert; CG Help — where PC Insert maps)
    KeyMapEntry {
        vk: 0x24,
        xkb: 0xFF50,
        cg: Some(0x73),
    }, // Home (XK_Home)
    KeyMapEntry {
        vk: 0x23,
        xkb: 0xFF57,
        cg: Some(0x77),
    }, // End (XK_End)
    KeyMapEntry {
        vk: 0x21,
        xkb: 0xFF55,
        cg: Some(0x74),
    }, // PageUp (XK_Prior)
    KeyMapEntry {
        vk: 0x22,
        xkb: 0xFF56,
        cg: Some(0x79),
    }, // PageDown (XK_Next)
    KeyMapEntry {
        vk: 0x26,
        xkb: 0xFF52,
        cg: Some(0x7E),
    }, // Up (XK_Up)
    KeyMapEntry {
        vk: 0x28,
        xkb: 0xFF54,
        cg: Some(0x7D),
    }, // Down (XK_Down)
    KeyMapEntry {
        vk: 0x25,
        xkb: 0xFF51,
        cg: Some(0x7B),
    }, // Left (XK_Left)
    KeyMapEntry {
        vk: 0x27,
        xkb: 0xFF53,
        cg: Some(0x7C),
    }, // Right (XK_Right)
    KeyMapEntry {
        vk: 0x2C,
        xkb: 0xFF61,
        cg: None,
    }, // PrintScreen (XK_Print; arrives as F13 on macOS)
    KeyMapEntry {
        vk: 0xBB,
        xkb: 0x003D,
        cg: Some(0x18),
    }, // OemPlus (XK_equal; CG ANSI Equal)
    KeyMapEntry {
        vk: 0xBD,
        xkb: 0x002D,
        cg: Some(0x1B),
    }, // OemMinus (XK_minus; CG ANSI Minus)
    KeyMapEntry {
        vk: 0xBC,
        xkb: 0x002C,
        cg: Some(0x2B),
    }, // OemComma (XK_comma; CG ANSI Comma)
    KeyMapEntry {
        vk: 0xBE,
        xkb: 0x002E,
        cg: Some(0x2F),
    }, // OemPeriod (XK_period; CG ANSI Period)
];

/// xkb keysym for a Win32 virtual-key code; `None` outside the vocabulary.
pub fn vk_to_xkb(vk: u32) -> Option<u32> {
    KEY_MAP.iter().find(|e| e.vk == vk).map(|e| e.xkb)
}

/// Win32 virtual-key code for an xkb keysym; `None` when unmapped. Uppercase
/// letter keysyms (`XK_A`..`XK_Z`, delivered while Shift is held) normalize to
/// the lowercase table rows.
pub fn xkb_to_vk(keysym: u32) -> Option<u32> {
    // XK_A..XK_Z equal the uppercase ASCII codes; the table holds lowercase.
    let keysym = match keysym {
        0x41..=0x5A => keysym + 0x20,
        _ => keysym,
    };
    KEY_MAP.iter().find(|e| e.xkb == keysym).map(|e| e.vk)
}

/// macOS `CGKeyCode` for a Win32 virtual-key code; `None` outside the
/// vocabulary or when macOS has no equivalent key (F21–F24, PrintScreen).
pub fn vk_to_cg_keycode(vk: u32) -> Option<u16> {
    KEY_MAP.iter().find(|e| e.vk == vk).and_then(|e| e.cg)
}

/// Win32 virtual-key code for a macOS `CGKeyCode`; `None` when unmapped.
pub fn cg_keycode_to_vk(code: u16) -> Option<u32> {
    KEY_MAP.iter().find(|e| e.cg == Some(code)).map(|e| e.vk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::gesture::HotkeyGesture;

    /// VKs with no macOS keycode (documented module-level gaps): F21–F24 and
    /// PrintScreen.
    const MACOS_UNMAPPED: &[u32] = &[0x84, 0x85, 0x86, 0x87, 0x2C];

    /// Every key name the gesture parser accepts, with its VK: single ASCII
    /// letters/digits, F1–F24, and the parser's own named-key table.
    fn parser_vocabulary() -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = Vec::new();
        for c in 'A'..='Z' {
            out.push((c.to_string(), c as u32));
        }
        for c in '0'..='9' {
            out.push((c.to_string(), c as u32));
        }
        for n in 1..=24u32 {
            out.push((format!("F{n}"), 0x70 + (n - 1)));
        }
        for &(name, vk) in crate::hotkeys::gesture::NAMED_KEYS {
            out.push((name.to_string(), vk));
        }
        out
    }

    #[test]
    fn vocabulary_matches_the_gesture_parser() {
        // Guards drift between this test's generated names and the parser:
        // every listed name must parse to exactly the listed VK.
        for (name, vk) in parser_vocabulary() {
            let g = HotkeyGesture::parse(&name).unwrap();
            assert_eq!(g.vk, vk, "parser VK of {name}");
        }
    }

    #[test]
    fn every_parseable_key_maps_to_xkb_and_back() {
        for (name, vk) in parser_vocabulary() {
            let keysym = vk_to_xkb(vk).unwrap_or_else(|| panic!("{name} must map to xkb"));
            assert_eq!(xkb_to_vk(keysym), Some(vk), "xkb round-trip of {name}");
        }
    }

    #[test]
    fn every_parseable_key_maps_to_cg_keycode_and_back() {
        for (name, vk) in parser_vocabulary() {
            if MACOS_UNMAPPED.contains(&vk) {
                assert_eq!(vk_to_cg_keycode(vk), None, "{name} documented macOS gap");
                continue;
            }
            let code =
                vk_to_cg_keycode(vk).unwrap_or_else(|| panic!("{name} must map to a CGKeyCode"));
            assert_eq!(
                cg_keycode_to_vk(code),
                Some(vk),
                "CGKeyCode round-trip of {name}"
            );
        }
    }

    #[test]
    fn table_has_no_duplicate_targets() {
        // Reverse lookups must be unambiguous.
        for (i, a) in KEY_MAP.iter().enumerate() {
            for b in &KEY_MAP[i + 1..] {
                assert_ne!(a.vk, b.vk, "duplicate VK");
                assert_ne!(a.xkb, b.xkb, "duplicate xkb keysym");
                if let (Some(x), Some(y)) = (a.cg, b.cg) {
                    assert_ne!(x, y, "duplicate CGKeyCode");
                }
            }
        }
    }

    #[test]
    fn spot_values_match_platform_constants() {
        // X11 keysymdef.h / xkbcommon-keysyms.h.
        assert_eq!(vk_to_xkb(0x1B), Some(0xFF1B)); // VK_ESCAPE -> XK_Escape
        assert_eq!(vk_to_xkb(0x70), Some(0xFFBE)); // VK_F1 -> XK_F1
        assert_eq!(vk_to_xkb(0x2E), Some(0xFFFF)); // VK_DELETE -> XK_Delete
        // HIToolbox Events.h.
        assert_eq!(vk_to_cg_keycode(0x41), Some(0x00)); // 'A' -> kVK_ANSI_A
        assert_eq!(vk_to_cg_keycode(0x0D), Some(0x24)); // VK_RETURN -> kVK_Return
        assert_eq!(vk_to_cg_keycode(0x20), Some(0x31)); // VK_SPACE -> kVK_Space
        assert_eq!(cg_keycode_to_vk(0x7A), Some(0x70)); // kVK_F1 -> VK_F1
    }

    #[test]
    fn uppercase_letter_keysyms_normalize() {
        // xkb delivers XK_A (not XK_a) when Shift is held; both resolve to VK 'A'.
        assert_eq!(xkb_to_vk(0x61), Some(0x41));
        assert_eq!(xkb_to_vk(0x41), Some(0x41));
        assert_eq!(xkb_to_vk(0x7A), Some(0x5A));
        assert_eq!(xkb_to_vk(0x5A), Some(0x5A));
    }

    #[test]
    fn unknown_values_map_to_none() {
        assert_eq!(vk_to_xkb(0x2A), None); // VK_SNAPSHOT-adjacent gap: VK_PRINT
        assert_eq!(vk_to_xkb(0x10), None); // VK_SHIFT (modifier, not a key)
        assert_eq!(xkb_to_vk(0xFF13), None); // XK_Pause (not in the vocabulary)
        assert_eq!(vk_to_cg_keycode(0x10), None);
        assert_eq!(cg_keycode_to_vk(0x3F), None); // kVK_Function
    }
}
