//! Pure hotkey gesture model: parse / display / (de)serialize / validate.
//!
//! No `windows` imports. `vk` values are Win32 virtual-key codes as **plain
//! numbers** (for `A`–`Z` and `0`–`9` they equal the uppercase ASCII code;
//! Esc = `0x1B`). [`Modifiers`] bit values match Win32
//! `MOD_ALT`/`MOD_CONTROL`/`MOD_SHIFT`/`MOD_WIN` so the Win32 layer can pass
//! [`Modifiers::bits`] straight to `RegisterHotKey`.
//!
//! Both [`Modifiers`] and [`HotkeyGesture`] serialize as their human-readable
//! display string (`"Ctrl+Alt+F"`, `"Esc"`, `"Ctrl"`) — that is the form stored
//! in `settings.json`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

// ---------------------------------------------------------------------------
// Private Win32 virtual-key knowledge (plain numbers — no `windows` import).
// ---------------------------------------------------------------------------

/// `(canonical display name, virtual-key code)` for the named non-modifier
/// keys. Lookup is case-insensitive; the display name is what
/// [`HotkeyGesture::to_display`] emits and what `settings.json` stores.
/// `pub(crate)` so [`crate::hotkeys::keymap`] tests can prove they cover the
/// parser's whole key vocabulary.
pub(crate) const NAMED_KEYS: &[(&str, u32)] = &[
    ("Esc", 0x1B),        // VK_ESCAPE
    ("Space", 0x20),      // VK_SPACE
    ("Enter", 0x0D),      // VK_RETURN
    ("Tab", 0x09),        // VK_TAB
    ("Backspace", 0x08),  // VK_BACK
    ("Delete", 0x2E),     // VK_DELETE
    ("Insert", 0x2D),     // VK_INSERT
    ("Home", 0x24),       // VK_HOME
    ("End", 0x23),        // VK_END
    ("PageUp", 0x21),     // VK_PRIOR
    ("PageDown", 0x22),   // VK_NEXT
    ("Up", 0x26),         // VK_UP
    ("Down", 0x28),       // VK_DOWN
    ("Left", 0x25),       // VK_LEFT
    ("Right", 0x27),      // VK_RIGHT
    ("PrintScreen", 0x2C), // VK_SNAPSHOT
    ("OemPlus", 0xBB),    // VK_OEM_PLUS
    ("OemMinus", 0xBD),   // VK_OEM_MINUS
    ("OemComma", 0xBC),   // VK_OEM_COMMA
    ("OemPeriod", 0xBE),  // VK_OEM_PERIOD
];

/// Win32 virtual-key codes of the modifier keys themselves (both the generic
/// and the left/right variants). A gesture whose `vk` is one of these is a
/// bare modifier chord, not a bindable hotkey.
fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk,
        0x10..=0x12        // VK_SHIFT, VK_CONTROL, VK_MENU (Alt)
        | 0x5B | 0x5C      // VK_LWIN, VK_RWIN
        | 0xA0..=0xA5      // VK_LSHIFT..VK_RMENU (left/right Shift/Ctrl/Alt)
    )
}

/// Case-insensitive modifier-name lookup; `Control` aliases `Ctrl`.
fn parse_modifier_token(tok: &str) -> Option<Modifiers> {
    if tok.eq_ignore_ascii_case("ctrl") || tok.eq_ignore_ascii_case("control") {
        Some(Modifiers::CTRL)
    } else if tok.eq_ignore_ascii_case("alt") {
        Some(Modifiers::ALT)
    } else if tok.eq_ignore_ascii_case("shift") {
        Some(Modifiers::SHIFT)
    } else if tok.eq_ignore_ascii_case("win") {
        Some(Modifiers::WIN)
    } else {
        None
    }
}

/// Resolve a non-modifier key token (already trimmed, non-empty) to its Win32
/// virtual-key code. Case-insensitive. Accepts single ASCII letters/digits,
/// `F1`–`F24`, the [`NAMED_KEYS`] table, and a `0x..` hex fallback so any `vk`
/// constructed via [`HotkeyGesture::new`] display-round-trips.
fn key_vk_from_token(tok: &str) -> Option<u32> {
    // Single ASCII letter or digit: vk == uppercase ASCII code.
    if tok.len() == 1 {
        let b = tok.as_bytes()[0];
        return b.is_ascii_alphanumeric().then(|| b.to_ascii_uppercase() as u32);
    }
    // F1..F24 (VK_F1 == 0x70, contiguous).
    let (head, digits) = tok.split_at(1);
    if head.eq_ignore_ascii_case("f")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return match digits.parse::<u32>() {
            Ok(n) if (1..=24).contains(&n) => Some(0x70 + (n - 1)),
            _ => None,
        };
    }
    // Named keys.
    for (name, vk) in NAMED_KEYS {
        if tok.eq_ignore_ascii_case(name) {
            return Some(*vk);
        }
    }
    // Hex fallback ("0x2A"): keeps display/parse round-trip for vk values
    // outside the table (only reachable via `HotkeyGesture::new`).
    if let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))
        && !hex.is_empty()
    {
        return u32::from_str_radix(hex, 16).ok();
    }
    None
}

/// Canonical display name for a non-modifier virtual-key code.
fn vk_display_name(vk: u32) -> String {
    match vk {
        // ASCII letters/digits.
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).unwrap().to_string(),
        // F1..F24.
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => {
            for (name, code) in NAMED_KEYS {
                if *code == vk {
                    return (*name).to_string();
                }
            }
            // Unknown vk (only constructible via `HotkeyGesture::new`):
            // hex fallback, parsed back by `key_vk_from_token`.
            format!("0x{vk:02X}")
        }
    }
}

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

/// Modifier key set (bitflags). Bit values match the Win32 `MOD_*` constants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Modifiers(u32);

impl Modifiers {
    pub const NONE: Self = Self(0);
    /// Matches Win32 `MOD_ALT`.
    pub const ALT: Self = Self(0x0001);
    /// Matches Win32 `MOD_CONTROL`.
    pub const CTRL: Self = Self(0x0002);
    /// Matches Win32 `MOD_SHIFT`.
    pub const SHIFT: Self = Self(0x0004);
    /// Matches Win32 `MOD_WIN`.
    pub const WIN: Self = Self(0x0008);

    /// Mask of the valid modifier bits (used by [`Modifiers::from_bits`]).
    const VALID_MASK: u32 = Self::ALT.0 | Self::CTRL.0 | Self::SHIFT.0 | Self::WIN.0;

    /// Raw Win32-compatible bit pattern.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build from a raw bit pattern; unknown bits are dropped.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & Self::VALID_MASK)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Canonical display form: modifiers in `Ctrl+Alt+Shift+Win` order, joined
    /// with `+`. Empty string for [`Modifiers::NONE`]. Examples: `"Ctrl"`,
    /// `"Ctrl+Shift"`.
    pub fn to_display(self) -> String {
        let mut out = String::new();
        for (flag, name) in [
            (Self::CTRL, "Ctrl"),
            (Self::ALT, "Alt"),
            (Self::SHIFT, "Shift"),
            (Self::WIN, "Win"),
        ] {
            if self.contains(flag) {
                if !out.is_empty() {
                    out.push('+');
                }
                out.push_str(name);
            }
        }
        out
    }

    /// Parse a modifier-only string such as `"Ctrl"` or `"ctrl+alt"`.
    /// Case-insensitive, whitespace-tolerant; accepts `Control` as an alias of
    /// `Ctrl`. Rejects unknown names and non-modifier keys.
    ///
    /// An empty (or whitespace-only) string yields [`Modifiers::NONE`], so the
    /// serde display-string form round-trips for every value.
    pub fn parse(s: &str) -> Result<Self, ParseGestureError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(Self::NONE);
        }
        let mut result = Self::NONE;
        for raw in trimmed.split('+') {
            let tok = raw.trim();
            if tok.is_empty() {
                return Err(ParseGestureError(format!(
                    "empty modifier token in \"{trimmed}\""
                )));
            }
            let flag = parse_modifier_token(tok).ok_or_else(|| {
                ParseGestureError(format!(
                    "\"{tok}\" is not a modifier (expected Ctrl, Alt, Shift or Win)"
                ))
            })?;
            if result.contains(flag) {
                return Err(ParseGestureError(format!("duplicate modifier \"{tok}\"")));
            }
            result = result | flag;
        }
        Ok(result)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl Serialize for Modifiers {
    /// Serializes as [`Modifiers::to_display`] (e.g. `"Ctrl"`).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_display())
    }
}

impl<'de> Deserialize<'de> for Modifiers {
    /// Parses from the display string via [`Modifiers::parse`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// HotkeyGesture
// ---------------------------------------------------------------------------

/// A complete hotkey: modifier set + one non-modifier key.
/// Equality is exact (modifiers + vk), so equality doubles as conflict detection
/// for rebind validation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HotkeyGesture {
    pub modifiers: Modifiers,
    /// Win32 virtual-key code of the non-modifier key. Never a modifier VK
    /// (bare `Ctrl` is a [`Modifiers`], not a gesture).
    pub vk: u32,
}

impl HotkeyGesture {
    /// Direct constructor. Callers must uphold the "non-modifier vk"
    /// invariant themselves; [`HotkeyGesture::is_registerable`] checks it
    /// (parsed gestures always satisfy it).
    pub const fn new(modifiers: Modifiers, vk: u32) -> Self {
        Self { modifiers, vk }
    }

    /// Parse `"Ctrl+Shift+F"`, `"Ctrl+Alt+F"`, `"Esc"`, `"1"`, …
    /// Case-insensitive, whitespace-tolerant; `Control` aliases `Ctrl`.
    ///
    /// Supported key names (minimum contract): `A`–`Z`, `0`–`9`, `F1`–`F24`,
    /// `Esc`, `Space`, `Tab`, `Enter`, `Backspace`, `Delete`, `Insert`, `Home`,
    /// `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `PrintScreen`.
    /// A trailing modifier with no key (e.g. `"Ctrl+"`, `"Ctrl"`) is an error —
    /// use [`Modifiers::parse`] for modifier-only bindings.
    pub fn parse(s: &str) -> Result<Self, ParseGestureError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ParseGestureError("empty hotkey string".into()));
        }
        let tokens: Vec<&str> = trimmed.split('+').collect();

        // All tokens but the last must be distinct modifiers.
        let mut modifiers = Modifiers::NONE;
        for raw in &tokens[..tokens.len() - 1] {
            let tok = raw.trim();
            if tok.is_empty() {
                return Err(ParseGestureError(format!(
                    "empty token in \"{trimmed}\""
                )));
            }
            match parse_modifier_token(tok) {
                Some(flag) => {
                    if modifiers.contains(flag) {
                        return Err(ParseGestureError(format!(
                            "duplicate modifier \"{tok}\""
                        )));
                    }
                    modifiers = modifiers | flag;
                }
                None => {
                    return Err(ParseGestureError(format!(
                        "\"{tok}\" is not a modifier; only the last token may be a key"
                    )));
                }
            }
        }

        // Last token must be a non-modifier key.
        let key_tok = tokens[tokens.len() - 1].trim();
        if key_tok.is_empty() {
            return Err(ParseGestureError(format!(
                "trailing \"+\" with no key in \"{trimmed}\""
            )));
        }
        if parse_modifier_token(key_tok).is_some() {
            return Err(ParseGestureError(format!(
                "\"{key_tok}\" is a modifier; a hotkey needs a non-modifier key \
                 (use Modifiers::parse for modifier-only bindings)"
            )));
        }
        let vk = key_vk_from_token(key_tok)
            .ok_or_else(|| ParseGestureError(format!("unknown key \"{key_tok}\"")))?;
        if vk == 0 || is_modifier_vk(vk) {
            // Only reachable through the hex fallback (e.g. "0x11"), but keeps
            // the "never a modifier vk" invariant absolute.
            return Err(ParseGestureError(format!(
                "\"{key_tok}\" is not a bindable key"
            )));
        }
        Ok(Self::new(modifiers, vk))
    }

    /// Canonical display string: modifiers in canonical order, then the key name.
    /// Examples: `"Ctrl+Alt+F"`, `"Esc"`, `"1"`, `"Ctrl+C"`. Round-trips through
    /// [`HotkeyGesture::parse`].
    pub fn to_display(self) -> String {
        let mods = self.modifiers.to_display();
        let key = vk_display_name(self.vk);
        if mods.is_empty() { key } else { format!("{mods}+{key}") }
    }

    /// `true` when this gesture is acceptable for binding: the key is a real
    /// non-modifier key (modifier-only chords are not gestures) — i.e. the form
    /// Win32 `RegisterHotKey` accepts. System-reserved combinations are NOT
    /// rejected here; registration failure is reported by the manager at runtime.
    pub fn is_registerable(self) -> bool {
        self.vk != 0 && !is_modifier_vk(self.vk)
    }
}

impl Serialize for HotkeyGesture {
    /// Serializes as [`HotkeyGesture::to_display`] (e.g. `"Ctrl+Alt+F"`).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_display())
    }
}

impl<'de> Deserialize<'de> for HotkeyGesture {
    /// Parses from the display string via [`HotkeyGesture::parse`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Gesture/modifier parse failure with a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseGestureError(pub String);

impl fmt::Display for ParseGestureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseGestureError {}

// ---------------------------------------------------------------------------
// Tests (headless-safe, std + serde_json only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Modifier bit values -------------------------------------------------

    #[test]
    fn modifier_bits_match_win32_mod_constants() {
        assert_eq!(Modifiers::ALT.bits(), 0x0001); // MOD_ALT
        assert_eq!(Modifiers::CTRL.bits(), 0x0002); // MOD_CONTROL
        assert_eq!(Modifiers::SHIFT.bits(), 0x0004); // MOD_SHIFT
        assert_eq!(Modifiers::WIN.bits(), 0x0008); // MOD_WIN
        assert_eq!(Modifiers::NONE.bits(), 0);
    }

    #[test]
    fn from_bits_drops_unknown_bits() {
        assert_eq!(Modifiers::from_bits(0), Modifiers::NONE);
        assert_eq!(
            Modifiers::from_bits(0xFFFF),
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::WIN
        );
        assert_eq!(Modifiers::from_bits(0x0003), Modifiers::ALT | Modifiers::CTRL);
        assert_eq!(Modifiers::from_bits(0x0010), Modifiers::NONE);
    }

    #[test]
    fn contains_and_is_empty() {
        let m = Modifiers::CTRL | Modifiers::ALT;
        assert!(m.contains(Modifiers::CTRL));
        assert!(m.contains(Modifiers::ALT));
        assert!(!m.contains(Modifiers::SHIFT));
        assert!(m.contains(Modifiers::NONE));
        assert!(!Modifiers::NONE.contains(Modifiers::CTRL));
        assert!(Modifiers::NONE.is_empty());
        assert!(!m.is_empty());
    }

    #[test]
    fn bitor_combines_and_is_idempotent() {
        assert_eq!(Modifiers::CTRL | Modifiers::CTRL, Modifiers::CTRL);
        assert_eq!(
            (Modifiers::CTRL | Modifiers::ALT) | Modifiers::SHIFT,
            Modifiers::CTRL | (Modifiers::ALT | Modifiers::SHIFT)
        );
    }

    // -- Modifier parse / display --------------------------------------------

    #[test]
    fn all_modifier_combos_round_trip() {
        let parts = [Modifiers::CTRL, Modifiers::ALT, Modifiers::SHIFT, Modifiers::WIN];
        for mask in 0u32..16 {
            let mut m = Modifiers::NONE;
            for (i, part) in parts.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    m = m | *part;
                }
            }
            let display = m.to_display();
            let parsed = Modifiers::parse(&display).expect(display.as_str());
            assert_eq!(m, parsed, "round-trip of \"{display}\"");
        }
    }

    #[test]
    fn modifier_display_canonical_order() {
        assert_eq!(Modifiers::NONE.to_display(), "");
        assert_eq!(Modifiers::CTRL.to_display(), "Ctrl");
        assert_eq!(Modifiers::ALT.to_display(), "Alt");
        assert_eq!((Modifiers::ALT | Modifiers::CTRL).to_display(), "Ctrl+Alt");
        assert_eq!(
            (Modifiers::WIN | Modifiers::SHIFT | Modifiers::ALT | Modifiers::CTRL).to_display(),
            "Ctrl+Alt+Shift+Win"
        );
        assert_eq!((Modifiers::WIN | Modifiers::CTRL).to_display(), "Ctrl+Win");
    }

    #[test]
    fn modifier_parse_case_and_whitespace_tolerant() {
        assert_eq!(
            Modifiers::parse(" cTrL + aLt ").unwrap(),
            Modifiers::CTRL | Modifiers::ALT
        );
        assert_eq!(Modifiers::parse("CONTROL").unwrap(), Modifiers::CTRL);
        assert_eq!(
            Modifiers::parse("control+Shift").unwrap(),
            Modifiers::CTRL | Modifiers::SHIFT
        );
        assert_eq!(Modifiers::parse("WIN").unwrap(), Modifiers::WIN);
        assert_eq!(Modifiers::parse("").unwrap(), Modifiers::NONE);
        assert_eq!(Modifiers::parse("   ").unwrap(), Modifiers::NONE);
    }

    #[test]
    fn modifier_parse_rejects_bad_input() {
        for bad in [
            "F",        // non-modifier key
            "Esc",      // non-modifier key
            "1",        // non-modifier key
            "Ctrl+F",   // key mixed in
            "Ctrl+Ctrl",
            "Ctrl+Control", // duplicate through the alias
            "alt+ALT",
            "Ctrl+",    // trailing separator
            "+Ctrl",    // leading separator
            "Ctrl++Alt", // empty token
            "Bogus",
            "Ct rl",
        ] {
            assert!(Modifiers::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    // -- Virtual-key numbers --------------------------------------------------

    #[test]
    fn vk_numbers_match_win32() {
        let cases: &[(&str, u32)] = &[
            ("A", 0x41), ("Z", 0x5A), ("M", 0x4D),
            ("0", 0x30), ("9", 0x39), ("1", 0x31),
            ("F1", 0x70), ("F12", 0x7B), ("F24", 0x87),
            ("Esc", 0x1B), ("Space", 0x20), ("Enter", 0x0D), ("Tab", 0x09),
            ("Backspace", 0x08), ("Delete", 0x2E), ("Insert", 0x2D),
            ("Home", 0x24), ("End", 0x23),
            ("PageUp", 0x21), ("PageDown", 0x22),
            ("Up", 0x26), ("Down", 0x28), ("Left", 0x25), ("Right", 0x27),
            ("PrintScreen", 0x2C),
            ("OemPlus", 0xBB), ("OemMinus", 0xBD),
            ("OemComma", 0xBC), ("OemPeriod", 0xBE),
        ];
        for (name, vk) in cases {
            let g = HotkeyGesture::parse(name).unwrap();
            assert_eq!(g.vk, *vk, "vk of {name}");
            assert_eq!(g.modifiers, Modifiers::NONE);
        }
    }

    // -- Gesture parse / display ----------------------------------------------

    #[test]
    fn every_named_key_round_trips() {
        for (name, vk) in NAMED_KEYS {
            let g = HotkeyGesture::parse(name).unwrap();
            assert_eq!(g.vk, *vk);
            assert_eq!(g.to_display(), *name, "display of {name}");
            assert_eq!(HotkeyGesture::parse(&g.to_display()).unwrap(), g);
        }
    }

    #[test]
    fn letters_digits_and_fkeys_round_trip() {
        for c in 'A'..='Z' {
            let g = HotkeyGesture::parse(c.to_string().as_str()).unwrap();
            assert_eq!(g.vk, c as u32);
            assert_eq!(g.to_display(), c.to_string());
        }
        for c in '0'..='9' {
            let g = HotkeyGesture::parse(c.to_string().as_str()).unwrap();
            assert_eq!(g.vk, c as u32);
            assert_eq!(g.to_display(), c.to_string());
        }
        for n in 1..=24u32 {
            let name = format!("F{n}");
            let g = HotkeyGesture::parse(&name).unwrap();
            assert_eq!(g.vk, 0x70 + (n - 1));
            assert_eq!(g.to_display(), name);
        }
        // Lowercase letters normalize to the uppercase vk.
        assert_eq!(HotkeyGesture::parse("f").unwrap().vk, 'F' as u32);
        assert_eq!(HotkeyGesture::parse("f").unwrap().to_display(), "F");
        assert_eq!(HotkeyGesture::parse("f6").unwrap().vk, 0x75);
    }

    #[test]
    fn gesture_parse_case_and_whitespace_tolerant() {
        let want = HotkeyGesture::new(Modifiers::CTRL | Modifiers::ALT, 'F' as u32);
        for s in ["Ctrl+Alt+F", "ctrl+alt+f", " CTRL + ALT + F ", "Control+Alt+f"] {
            assert_eq!(HotkeyGesture::parse(s).unwrap(), want, "parse {s:?}");
        }
        assert_eq!(HotkeyGesture::parse(" esc ").unwrap().vk, 0x1B);
        assert_eq!(HotkeyGesture::parse("PAGEUP").unwrap().vk, 0x21);
        assert_eq!(HotkeyGesture::parse("oemminus").unwrap().vk, 0xBD);
    }

    #[test]
    fn gesture_display_canonical() {
        assert_eq!(HotkeyGesture::parse("alt+ctrl+f").unwrap().to_display(), "Ctrl+Alt+F");
        assert_eq!(HotkeyGesture::parse("esc").unwrap().to_display(), "Esc");
        assert_eq!(HotkeyGesture::parse("1").unwrap().to_display(), "1");
        assert_eq!(HotkeyGesture::parse("control+c").unwrap().to_display(), "Ctrl+C");
        assert_eq!(
            HotkeyGesture::parse("win+shift+ctrl+alt+a").unwrap().to_display(),
            "Ctrl+Alt+Shift+Win+A"
        );
        assert_eq!(
            HotkeyGesture::parse("shift+printscreen").unwrap().to_display(),
            "Shift+PrintScreen"
        );
    }

    #[test]
    fn gesture_parse_rejects_bad_input() {
        for bad in [
            "",
            "   ",
            "Ctrl",          // bare modifier — use Modifiers::parse
            "Alt+Shift",     // all modifiers, no key
            "Ctrl+Win",      // trailing modifier
            "Ctrl+",         // trailing separator
            "Ctrl + ",       // trailing separator with whitespace
            "+F",            // leading separator
            "Ctrl++F",       // empty token
            "Ctrl+Ctrl+F",   // duplicate modifier
            "Ctrl+Control+F",// duplicate through the alias
            "F+Ctrl",        // key before modifier
            "Ctrl+F+Alt",    // key in modifier position
            "Ctrl+Foo",      // unknown key
            "Foo",
            "F0",
            "F25",
            "F123",
            "Ctrl+Shift",    // modifier-only chord
            "0x11",          // VK_CONTROL through hex fallback
            "0xA2",          // VK_LCONTROL through hex fallback
            "0x0",           // vk 0 is invalid
        ] {
            assert!(HotkeyGesture::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn parse_display_parse_is_stable() {
        for s in [
            "Ctrl+Alt+F", "Esc", "1", "0", "Ctrl+C", "Ctrl+Shift+F24",
            "Alt+PageDown", "Shift+OemComma", "Ctrl+Alt+Shift+Win+Right",
            "Win+PrintScreen", "Space",
        ] {
            let g = HotkeyGesture::parse(s).unwrap();
            assert_eq!(HotkeyGesture::parse(&g.to_display()).unwrap(), g, "round-trip {s:?}");
            assert_eq!(g.to_display(), s, "display of {s:?} is already canonical");
        }
    }

    // -- is_registerable --------------------------------------------------------

    #[test]
    fn parsed_gestures_are_registerable() {
        for s in ["Esc", "Ctrl+Alt+F", "1", "F24", "Shift+OemPeriod"] {
            assert!(HotkeyGesture::parse(s).unwrap().is_registerable(), "{s:?}");
        }
    }

    #[test]
    fn bare_modifier_vk_is_not_registerable() {
        // VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN,
        // VK_LSHIFT..VK_RMENU, and vk 0.
        for vk in [0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0] {
            assert!(
                !HotkeyGesture::new(Modifiers::NONE, vk).is_registerable(),
                "vk {vk:#04X} must not be registerable"
            );
        }
    }

    #[test]
    fn hex_fallback_round_trips_unknown_vks() {
        // Only reachable via `new` — e.g. an OEM key not in the name table.
        let g = HotkeyGesture::new(Modifiers::NONE, 0x2A);
        assert_eq!(g.to_display(), "0x2A");
        assert_eq!(HotkeyGesture::parse("0x2A").unwrap(), g);
        let with_mods = HotkeyGesture::new(Modifiers::CTRL, 0xDF);
        assert_eq!(with_mods.to_display(), "Ctrl+0xDF");
        assert_eq!(HotkeyGesture::parse("Ctrl+0xDF").unwrap(), with_mods);
    }

    // -- Equality = conflict detection ------------------------------------------

    #[test]
    fn equality_doubles_as_conflict_detection() {
        assert_eq!(
            HotkeyGesture::parse("ctrl+alt+f").unwrap(),
            HotkeyGesture::parse("Ctrl+Alt+F").unwrap()
        );
        assert_ne!(
            HotkeyGesture::parse("Ctrl+F").unwrap(),
            HotkeyGesture::parse("Alt+F").unwrap()
        );
        assert_ne!(
            HotkeyGesture::parse("Ctrl+F").unwrap(),
            HotkeyGesture::parse("Ctrl+G").unwrap()
        );
        assert_ne!(
            HotkeyGesture::parse("Ctrl+Alt+F").unwrap(),
            HotkeyGesture::parse("Ctrl+Alt+Shift+F").unwrap()
        );
    }

    // -- serde -------------------------------------------------------------------

    #[test]
    fn serde_gesture_is_display_string() {
        let g = HotkeyGesture::parse("Ctrl+Alt+F").unwrap();
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(json, "\"Ctrl+Alt+F\"");
        let back: HotkeyGesture = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);

        let bare: HotkeyGesture = serde_json::from_str("\"Esc\"").unwrap();
        assert_eq!(bare, HotkeyGesture::parse("Esc").unwrap());

        assert!(serde_json::from_str::<HotkeyGesture>("\"Ctrl+Bogus\"").is_err());
        assert!(serde_json::from_str::<HotkeyGesture>("\"Ctrl\"").is_err());
        assert!(serde_json::from_str::<HotkeyGesture>("123").is_err());
    }

    #[test]
    fn serde_modifiers_is_display_string() {
        let m = Modifiers::CTRL | Modifiers::ALT;
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "\"Ctrl+Alt\"");
        assert_eq!(serde_json::from_str::<Modifiers>(&json).unwrap(), m);

        // NONE round-trips through the empty string.
        assert_eq!(serde_json::to_string(&Modifiers::NONE).unwrap(), "\"\"");
        assert_eq!(serde_json::from_str::<Modifiers>("\"\"").unwrap(), Modifiers::NONE);

        assert_eq!(serde_json::from_str::<Modifiers>("\"ctrl\"").unwrap(), Modifiers::CTRL);
        assert!(serde_json::from_str::<Modifiers>("\"Ctrl+F\"").is_err());
        assert!(serde_json::from_str::<Modifiers>("\"Bogus\"").is_err());
    }

    // -- Error type ----------------------------------------------------------------

    #[test]
    fn error_display_is_human_readable() {
        let e = HotkeyGesture::parse("Ctrl+").unwrap_err();
        let msg = e.to_string();
        assert!(!msg.is_empty());
        assert!(msg.contains("no key"), "unexpected message: {msg}");
        // Equality is by message (used by tests / logging).
        assert_eq!(e, ParseGestureError(msg.clone()));
        // std::error::Error is implemented.
        let _: &dyn std::error::Error = &e;
    }
}
