//! PURE mode/hotkey legend: while frozen, a large translucent rounded pill
//! near the top-center of every monitor shows the modes as TABS — the active
//! one(s) highlighted — each labelled with the hotkey that reaches it
//! (bindings snapshotted from settings at freeze time, like every other
//! freeze-time setting).
//!
//! The pill sits below the top edge with a generous inset so it stays visible
//! without looking pinned to the screen boundary. It is painted into the
//! composed frame only — never into the capture originals — so it can never
//! leak into a snip copy or the capture-mode re-base.
//!
//! Text is rendered with the embedded 8x8 public-domain bitmap font
//! ([`FONT8X8`]) — no font crates, no OS text APIs, fully headless-testable.
//! Everything here is integer pixel math, deterministic to the byte.

use crate::capture::DibBuffer;
use crate::settings::model::{HotkeySettings, Rgb};

/// 8x8 monochrome bitmap font covering printable ASCII (U+0020..=U+007E),
/// row-major: one byte per glyph row (top first), bit 0 = leftmost pixel.
///
/// font8x8_basic by Daniel Hepper <daniel@hepper.net>
/// (https://github.com/dhepper/font8x8), Public Domain — itself based on
/// public-domain IBM VGA fonts by Marcel Sondaar.
static FONT8X8: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x20 ' '
    [0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00], // 0x21 '!'
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x22 '"'
    [0x36, 0x36, 0x7F, 0x36, 0x7F, 0x36, 0x36, 0x00], // 0x23 '#'
    [0x0C, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x0C, 0x00], // 0x24 '$'
    [0x00, 0x63, 0x33, 0x18, 0x0C, 0x66, 0x63, 0x00], // 0x25 '%'
    [0x1C, 0x36, 0x1C, 0x6E, 0x3B, 0x33, 0x6E, 0x00], // 0x26 '&'
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x27 "'"
    [0x18, 0x0C, 0x06, 0x06, 0x06, 0x0C, 0x18, 0x00], // 0x28 '('
    [0x06, 0x0C, 0x18, 0x18, 0x18, 0x0C, 0x06, 0x00], // 0x29 ')'
    [0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00], // 0x2A '*'
    [0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00], // 0x2B '+'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x06], // 0x2C ','
    [0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0x00], // 0x2D '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x00], // 0x2E '.'
    [0x60, 0x30, 0x18, 0x0C, 0x06, 0x03, 0x01, 0x00], // 0x2F '/'
    [0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00], // 0x30 '0'
    [0x0C, 0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x3F, 0x00], // 0x31 '1'
    [0x1E, 0x33, 0x30, 0x1C, 0x06, 0x33, 0x3F, 0x00], // 0x32 '2'
    [0x1E, 0x33, 0x30, 0x1C, 0x30, 0x33, 0x1E, 0x00], // 0x33 '3'
    [0x38, 0x3C, 0x36, 0x33, 0x7F, 0x30, 0x78, 0x00], // 0x34 '4'
    [0x3F, 0x03, 0x1F, 0x30, 0x30, 0x33, 0x1E, 0x00], // 0x35 '5'
    [0x1C, 0x06, 0x03, 0x1F, 0x33, 0x33, 0x1E, 0x00], // 0x36 '6'
    [0x3F, 0x33, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x00], // 0x37 '7'
    [0x1E, 0x33, 0x33, 0x1E, 0x33, 0x33, 0x1E, 0x00], // 0x38 '8'
    [0x1E, 0x33, 0x33, 0x3E, 0x30, 0x18, 0x0E, 0x00], // 0x39 '9'
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x00], // 0x3A ':'
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x06], // 0x3B ';'
    [0x18, 0x0C, 0x06, 0x03, 0x06, 0x0C, 0x18, 0x00], // 0x3C '<'
    [0x00, 0x00, 0x3F, 0x00, 0x00, 0x3F, 0x00, 0x00], // 0x3D '='
    [0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00], // 0x3E '>'
    [0x1E, 0x33, 0x30, 0x18, 0x0C, 0x00, 0x0C, 0x00], // 0x3F '?'
    [0x3E, 0x63, 0x7B, 0x7B, 0x7B, 0x03, 0x1E, 0x00], // 0x40 '@'
    [0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00], // 0x41 'A'
    [0x3F, 0x66, 0x66, 0x3E, 0x66, 0x66, 0x3F, 0x00], // 0x42 'B'
    [0x3C, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3C, 0x00], // 0x43 'C'
    [0x1F, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1F, 0x00], // 0x44 'D'
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x46, 0x7F, 0x00], // 0x45 'E'
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x06, 0x0F, 0x00], // 0x46 'F'
    [0x3C, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7C, 0x00], // 0x47 'G'
    [0x33, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x33, 0x00], // 0x48 'H'
    [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // 0x49 'I'
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E, 0x00], // 0x4A 'J'
    [0x67, 0x66, 0x36, 0x1E, 0x36, 0x66, 0x67, 0x00], // 0x4B 'K'
    [0x0F, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7F, 0x00], // 0x4C 'L'
    [0x63, 0x77, 0x7F, 0x7F, 0x6B, 0x63, 0x63, 0x00], // 0x4D 'M'
    [0x63, 0x67, 0x6F, 0x7B, 0x73, 0x63, 0x63, 0x00], // 0x4E 'N'
    [0x1C, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1C, 0x00], // 0x4F 'O'
    [0x3F, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x0F, 0x00], // 0x50 'P'
    [0x1E, 0x33, 0x33, 0x33, 0x3B, 0x1E, 0x38, 0x00], // 0x51 'Q'
    [0x3F, 0x66, 0x66, 0x3E, 0x36, 0x66, 0x67, 0x00], // 0x52 'R'
    [0x1E, 0x33, 0x07, 0x0E, 0x38, 0x33, 0x1E, 0x00], // 0x53 'S'
    [0x3F, 0x2D, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // 0x54 'T'
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3F, 0x00], // 0x55 'U'
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // 0x56 'V'
    [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00], // 0x57 'W'
    [0x63, 0x63, 0x36, 0x1C, 0x1C, 0x36, 0x63, 0x00], // 0x58 'X'
    [0x33, 0x33, 0x33, 0x1E, 0x0C, 0x0C, 0x1E, 0x00], // 0x59 'Y'
    [0x7F, 0x63, 0x31, 0x18, 0x4C, 0x66, 0x7F, 0x00], // 0x5A 'Z'
    [0x1E, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1E, 0x00], // 0x5B '['
    [0x03, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00], // 0x5C '\\'
    [0x1E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1E, 0x00], // 0x5D ']'
    [0x08, 0x1C, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00], // 0x5E '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF], // 0x5F '_'
    [0x0C, 0x0C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x60 '`'
    [0x00, 0x00, 0x1E, 0x30, 0x3E, 0x33, 0x6E, 0x00], // 0x61 'a'
    [0x07, 0x06, 0x06, 0x3E, 0x66, 0x66, 0x3B, 0x00], // 0x62 'b'
    [0x00, 0x00, 0x1E, 0x33, 0x03, 0x33, 0x1E, 0x00], // 0x63 'c'
    [0x38, 0x30, 0x30, 0x3e, 0x33, 0x33, 0x6E, 0x00], // 0x64 'd'
    [0x00, 0x00, 0x1E, 0x33, 0x3f, 0x03, 0x1E, 0x00], // 0x65 'e'
    [0x1C, 0x36, 0x06, 0x0f, 0x06, 0x06, 0x0F, 0x00], // 0x66 'f'
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x1F], // 0x67 'g'
    [0x07, 0x06, 0x36, 0x6E, 0x66, 0x66, 0x67, 0x00], // 0x68 'h'
    [0x0C, 0x00, 0x0E, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // 0x69 'i'
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E], // 0x6A 'j'
    [0x07, 0x06, 0x66, 0x36, 0x1E, 0x36, 0x67, 0x00], // 0x6B 'k'
    [0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // 0x6C 'l'
    [0x00, 0x00, 0x33, 0x7F, 0x7F, 0x6B, 0x63, 0x00], // 0x6D 'm'
    [0x00, 0x00, 0x1F, 0x33, 0x33, 0x33, 0x33, 0x00], // 0x6E 'n'
    [0x00, 0x00, 0x1E, 0x33, 0x33, 0x33, 0x1E, 0x00], // 0x6F 'o'
    [0x00, 0x00, 0x3B, 0x66, 0x66, 0x3E, 0x06, 0x0F], // 0x70 'p'
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x78], // 0x71 'q'
    [0x00, 0x00, 0x3B, 0x6E, 0x66, 0x06, 0x0F, 0x00], // 0x72 'r'
    [0x00, 0x00, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x00], // 0x73 's'
    [0x08, 0x0C, 0x3E, 0x0C, 0x0C, 0x2C, 0x18, 0x00], // 0x74 't'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6E, 0x00], // 0x75 'u'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // 0x76 'v'
    [0x00, 0x00, 0x63, 0x6B, 0x7F, 0x7F, 0x36, 0x00], // 0x77 'w'
    [0x00, 0x00, 0x63, 0x36, 0x1C, 0x36, 0x63, 0x00], // 0x78 'x'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3E, 0x30, 0x1F], // 0x79 'y'
    [0x00, 0x00, 0x3F, 0x19, 0x0C, 0x26, 0x3F, 0x00], // 0x7A 'z'
    [0x38, 0x0C, 0x0C, 0x07, 0x0C, 0x0C, 0x38, 0x00], // 0x7B '{'
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00], // 0x7C '|'
    [0x07, 0x0C, 0x0C, 0x38, 0x0C, 0x0C, 0x07, 0x00], // 0x7D '}'
    [0x6E, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x7E '~'
];

/// Code point of `FONT8X8[0]` (glyphs are indexed by `code - FIRST_GLYPH`).
const FIRST_GLYPH: u32 = 0x20;

/// Glyph bitmap for `ch`: printable ASCII renders itself; anything else
/// (control codes, non-ASCII text) falls back to `?`.
fn glyph(ch: char) -> &'static [u8; 8] {
    let code = ch as u32;
    let index = if (FIRST_GLYPH..=0x7E).contains(&code) {
        code - FIRST_GLYPH
    } else {
        u32::from(b'?') - FIRST_GLYPH
    };
    &FONT8X8[index as usize]
}

/// Nearest-neighbor scale applied to every source-font pixel.
const UI_SCALE: u32 = 2;
/// Glyph cell advance in pixels (the font's glyphs are right-padded to 8).
const GLYPH_W: u32 = 8 * UI_SCALE;
/// Glyph height in pixels.
const GLYPH_H: u32 = 8 * UI_SCALE;
/// Horizontal padding between the pill edge and the first/last tab chip.
const PILL_PAD_X: u32 = 12 * UI_SCALE;
/// Vertical padding between the pill edge and the text.
const PILL_PAD_Y: u32 = 6 * UI_SCALE;
/// Horizontal padding inside a tab chip, each side of its text.
const TAB_PAD_X: u32 = 8 * UI_SCALE;
/// Gap between tab chips.
const TAB_GAP: u32 = 4 * UI_SCALE;
/// Chip vertical inset inside the pill.
const CHIP_INSET_Y: u32 = 3 * UI_SCALE;
/// Pill corner radius in pixels (half the pill height: capsule ends).
const PILL_RADIUS: u32 = 10 * UI_SCALE;
/// Distance between the frame's top edge and the pill's top edge.
const TOP_MARGIN: u32 = 48;

/// Pill height: one glyph row plus vertical padding.
const PILL_H: u32 = GLYPH_H + 2 * PILL_PAD_Y;

/// Pill background: near-black, blended at [`PILL_ALPHA`] over the frame.
const PILL_COLOR: Rgb = Rgb {
    r: 0x12,
    g: 0x12,
    b: 0x16,
};
/// Pill background blend alpha (about 75%: the frame reads through faintly).
const PILL_ALPHA: u8 = 190;
/// Active-tab chip: white, blended at [`CHIP_ALPHA`] over the pill.
const CHIP_COLOR: Rgb = Rgb {
    r: 0xFF,
    g: 0xFF,
    b: 0xFF,
};
/// Active-tab chip blend alpha (a subtle brightening, not a second pill).
const CHIP_ALPHA: u8 = 46;
/// Text on the active tab.
const TEXT_ACTIVE: Rgb = Rgb {
    r: 0xF2,
    g: 0xF2,
    b: 0xF2,
};
/// Text on inactive tabs (dimmer, cool gray).
const TEXT_INACTIVE: Rgb = Rgb {
    r: 0xB4,
    g: 0xB8,
    b: 0xC0,
};

/// One legend tab: a mode's display name and the hotkey that reaches it.
pub struct LegendTab {
    pub name: String,
    pub hotkey: String,
}

/// The freeze-time legend: tab texts and layout metrics, computed once.
pub struct Legend {
    /// Rendered tab texts (`NAME (HOTKEY)`), in display order.
    tabs: Vec<String>,
    /// Pixel width of each tab's chip (text + padding), parallel to `tabs`.
    chip_widths: Vec<u32>,
    /// Total pill width in pixels.
    pill_width: u32,
}

impl Legend {
    /// The legend for a freeze session: one tab per mode in the fixed
    /// Spotlight / Zoom / Snip order, labelled with the freeze-time binding.
    pub fn from_hotkeys(hotkeys: &HotkeySettings) -> Self {
        Self::new(&[
            LegendTab {
                name: "SPOTLIGHT".into(),
                hotkey: hotkeys.mode_spotlight.to_display(),
            },
            LegendTab {
                name: "ZOOM".into(),
                hotkey: hotkeys.zoom_hold.to_display(),
            },
            LegendTab {
                name: "SNIP".into(),
                hotkey: hotkeys.mode_snip.to_display(),
            },
        ])
    }

    /// Tabs in display order; each renders as `NAME (HOTKEY)`.
    pub fn new(tabs: &[LegendTab]) -> Self {
        let texts: Vec<String> = tabs
            .iter()
            .map(|t| format!("{} ({})", t.name, t.hotkey))
            .collect();
        let chip_widths = texts
            .iter()
            .map(|t| text_width(t) + 2 * TAB_PAD_X)
            .collect::<Vec<_>>();
        let pill_width = 2 * PILL_PAD_X
            + chip_widths.iter().sum::<u32>()
            + TAB_GAP * chip_widths.len().saturating_sub(1) as u32;
        Self {
            tabs: texts,
            chip_widths,
            pill_width,
        }
    }

    /// `(width, height)` of the pill in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.pill_width, PILL_H)
    }

    /// Paint the pill centered horizontally near the top of `buf`.
    /// `active[i]` highlights tab `i` (missing flags read as inactive).
    /// `alpha` scales the whole pill (the freeze fade-in blends it in with
    /// the veil; 255 = full strength, 0 = nothing painted). Skips monitors
    /// smaller than the pill instead of clipping it.
    pub fn paint(&self, buf: &mut DibBuffer, active: &[bool], alpha: u8) {
        let (pw, ph) = self.size();
        if alpha == 0 || self.tabs.is_empty() || pw > buf.width || ph > buf.height {
            return;
        }
        let x0 = ((buf.width - pw) / 2) as i32;
        let slack = buf.height - ph; // >= 0 (checked above)
        let y0 = TOP_MARGIN.min(slack) as i32;
        // Pill body (translucent dark, rounded corners).
        let pill_alpha = scale_alpha(PILL_ALPHA, alpha);
        for y in y0..y0 + ph as i32 {
            for x in x0..x0 + pw as i32 {
                if rounded_rect_contains(x, y, x0, y0, pw, ph, PILL_RADIUS) {
                    blend_px(buf, x, y, PILL_COLOR, pill_alpha);
                }
            }
        }
        // Tab chips (active highlight) and text.
        let mut chip_x = x0 + PILL_PAD_X as i32;
        let text_y = y0 + PILL_PAD_Y as i32;
        for (i, text) in self.tabs.iter().enumerate() {
            let cw = self.chip_widths[i];
            let on = active.get(i).copied().unwrap_or(false);
            if on {
                let chip_alpha = scale_alpha(CHIP_ALPHA, alpha);
                let cy = y0 + CHIP_INSET_Y as i32;
                let ch = ph - 2 * CHIP_INSET_Y;
                for y in cy..cy + ch as i32 {
                    for x in chip_x..chip_x + cw as i32 {
                        if rounded_rect_contains(x, y, chip_x, cy, cw, ch, ch / 2) {
                            blend_px(buf, x, y, CHIP_COLOR, chip_alpha);
                        }
                    }
                }
            }
            draw_text(
                buf,
                chip_x + TAB_PAD_X as i32,
                text_y,
                text,
                if on { TEXT_ACTIVE } else { TEXT_INACTIVE },
                alpha,
            );
            chip_x += (cw + TAB_GAP) as i32;
        }
    }
}

/// Pixel width of a text at one glyph cell per character.
fn text_width(text: &str) -> u32 {
    text.chars().count() as u32 * GLYPH_W
}

/// A blend alpha scaled by the pill's global alpha, rounded to nearest.
fn scale_alpha(alpha: u8, global: u8) -> u8 {
    ((alpha as u32 * global as u32 + 127) / 255) as u8
}

/// Blend pixel `(x, y)` of `buf` toward `color` at `alpha` (the one-division
/// integer family shared with `composite::darken`); the alpha byte is
/// untouched and out-of-bounds coordinates are ignored.
fn blend_px(buf: &mut DibBuffer, x: i32, y: i32, color: Rgb, alpha: u8) {
    if alpha == 0 || x < 0 || y < 0 || x >= buf.width as i32 || y >= buf.height as i32 {
        return;
    }
    let i = y as usize * buf.stride as usize + x as usize * 4;
    let keep = 255 - alpha as u32;
    let a = alpha as u32;
    // BGRA: color.b blends into channel 0, g into 1, r into 2.
    for (ch, fg) in [(0usize, color.b), (1, color.g), (2, color.r)] {
        buf.pixels[i + ch] = ((buf.pixels[i + ch] as u32 * keep + fg as u32 * a) / 255) as u8;
    }
}

/// `true` when pixel `(px, py)` lies inside the `w`x`h` rectangle at
/// `(x0, y0)` with corner radius `r` (clamped to half the shorter side).
fn rounded_rect_contains(px: i32, py: i32, x0: i32, y0: i32, w: u32, h: u32, r: u32) -> bool {
    let x1 = x0 + w as i32;
    let y1 = y0 + h as i32;
    if px < x0 || px >= x1 || py < y0 || py >= y1 {
        return false;
    }
    let r = r.min(w / 2).min(h / 2) as i32;
    // Corner circle centers sit `r` inside the rect; pixels in the central
    // bands are unconditionally inside.
    let (il, it) = (x0 + r, y0 + r);
    let (ir, ib) = (x1 - r, y1 - r);
    let dx = if px < il {
        il - px
    } else if px >= ir {
        px - ir + 1
    } else {
        0
    };
    let dy = if py < it {
        it - py
    } else if py >= ib {
        py - ib + 1
    } else {
        0
    };
    dx * dx + dy * dy <= r * r
}

/// Draw `text` at `(x, y)` (top-left of the first glyph cell) in `color`
/// blended at `alpha`. One glyph cell per character; no clipping beyond the
/// buffer itself (callers position the text inside the pill).
fn draw_text(buf: &mut DibBuffer, x: i32, y: i32, text: &str, color: Rgb, alpha: u8) {
    for (i, ch) in text.chars().enumerate() {
        let gx = x + i as i32 * GLYPH_W as i32;
        for (row, &bits) in glyph(ch).iter().enumerate() {
            for col in 0..8 {
                if bits >> col & 1 == 1 {
                    let px = gx + col * UI_SCALE as i32;
                    let py = y + row as i32 * UI_SCALE as i32;
                    for sy in 0..UI_SCALE as i32 {
                        for sx in 0..UI_SCALE as i32 {
                            blend_px(buf, px + sx, py + sy, color, alpha);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid test frame (`[B, G, R, A]` per pixel).
    fn frame(w: u32, h: u32, c: [u8; 4]) -> DibBuffer {
        DibBuffer {
            width: w,
            height: h,
            stride: w * 4,
            pixels: c.repeat((w * h) as usize),
        }
    }

    fn px(buf: &DibBuffer, x: u32, y: u32) -> [u8; 4] {
        let i = (y * buf.stride + x * 4) as usize;
        buf.pixels[i..i + 4].try_into().unwrap()
    }

    fn tabs(spec: &[(&str, &str)]) -> Vec<LegendTab> {
        spec.iter()
            .map(|&(name, hotkey)| LegendTab {
                name: name.into(),
                hotkey: hotkey.into(),
            })
            .collect()
    }

    // ---- font table ---------------------------------------------------------

    #[test]
    fn font_covers_printable_ascii() {
        assert_eq!(FONT8X8.len(), 95);
        // Space is empty; every letter/digit glyph has some pixels set.
        assert_eq!(glyph(' '), &[0; 8]);
        for ch in ('0'..='9').chain('A'..='Z').chain('a'..='z') {
            assert!(
                glyph(ch).iter().any(|&b| b != 0),
                "glyph for {ch:?} must not be empty"
            );
        }
    }

    #[test]
    fn glyph_matches_the_font8x8_basic_reference() {
        // Pinned rows from font8x8_basic (public domain): 'A', '0', '+'.
        assert_eq!(
            glyph('A'),
            &[0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00]
        );
        assert_eq!(
            glyph('0'),
            &[0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00]
        );
        assert_eq!(
            glyph('+'),
            &[0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00]
        );
    }

    #[test]
    fn non_ascii_falls_back_to_question_mark() {
        assert_eq!(glyph('\u{1F600}'), glyph('?'));
        assert_eq!(glyph('\u{7}'), glyph('?'));
        assert_ne!(glyph('?'), &[0; 8]);
    }

    // ---- layout -------------------------------------------------------------

    #[test]
    fn size_is_exact_from_tab_texts() {
        // Two-times scaling: "A (B)" = 5 chars -> text 80 px, chip 112;
        // "CC (DD)" = 7 chars -> text 112 px, chip 144.
        let legend = Legend::new(&tabs(&[("A", "B"), ("CC", "DD")]));
        let (w, h) = legend.size();
        assert_eq!(h, 40, "glyph 16 + 2 * pad 12");
        assert_eq!(w, 2 * 24 + (112 + 144) + 8, "pads + chips + one gap");
    }

    #[test]
    fn from_hotkeys_uses_the_freeze_time_bindings() {
        let hotkeys = HotkeySettings::default();
        let legend = Legend::from_hotkeys(&hotkeys);
        assert_eq!(
            legend.tabs,
            vec![
                "SPOTLIGHT (S)".to_string(),
                "ZOOM (F)".to_string(),
                "SNIP (C)".to_string(),
            ]
        );
        // Default bindings: "SPOTLIGHT (S)" = 13 chars, the others 8.
        let (w, _) = legend.size();
        assert_eq!(w, 48 + (240 + 160 + 160) + 16);
    }

    #[test]
    fn from_hotkeys_renders_custom_bindings() {
        let mut hotkeys = HotkeySettings::default();
        hotkeys.zoom_hold = crate::hotkeys::gesture::HotkeyGesture::parse("Ctrl+G").unwrap();
        let legend = Legend::from_hotkeys(&hotkeys);
        assert_eq!(legend.tabs[1], "ZOOM (Ctrl+G)");
    }

    // ---- rounded_rect_contains ----------------------------------------------

    #[test]
    fn rounded_rect_includes_bands_excludes_corner_outside_the_radius() {
        // 20x20 rect at (0,0), radius 5: corner (0,0) is out (dx=dy=5 -> 50 > 25),
        // edge midpoints and band pixels are in.
        let contains = |x, y| rounded_rect_contains(x, y, 0, 0, 20, 20, 5);
        assert!(!contains(0, 0), "diagonal corner pixel outside");
        assert!(contains(5, 0), "top edge at the radius");
        assert!(contains(10, 0), "top band");
        assert!(contains(0, 10), "left band");
        assert!(!contains(19, 19), "far corner symmetric to (0,0)");
        assert!(!contains(-1, 10), "outside the rect");
        assert!(!contains(20, 10), "right edge exclusive");
        // The corner circle boundary itself is inclusive.
        assert!(contains(5, 5), "corner circle center pixel");
    }

    #[test]
    fn rounded_rect_radius_clamps_to_half_the_short_side() {
        // 20x6 rect, radius 10 -> clamped to 3: (0,0) dx=dy=3 -> 18 > 9 out.
        assert!(!rounded_rect_contains(0, 0, 0, 0, 20, 6, 10));
        assert!(rounded_rect_contains(3, 0, 0, 0, 20, 6, 10));
        // Zero radius: the plain rectangle.
        assert!(rounded_rect_contains(0, 0, 0, 0, 20, 6, 0));
    }

    // ---- blend_px / scale_alpha ----------------------------------------------

    #[test]
    fn blend_px_exact_math_and_bounds() {
        let mut buf = frame(4, 4, [100, 100, 100, 200]);
        let red = Rgb { r: 200, g: 0, b: 0 };
        blend_px(&mut buf, 1, 1, red, 128);
        // channel = (100 * 127 + fg * 128) / 255; B and G take fg 0, R takes 200.
        let [b, g, r, a] = px(&buf, 1, 1);
        assert_eq!(b, ((100u32 * 127) / 255) as u8);
        assert_eq!(g, ((100u32 * 127) / 255) as u8);
        assert_eq!(r, ((100u32 * 127 + 200 * 128) / 255) as u8);
        assert_eq!(a, 200, "alpha byte untouched");
        // Out of bounds and zero alpha: no-ops, no panic.
        let before = buf.pixels.clone();
        blend_px(&mut buf, -1, 0, red, 255);
        blend_px(&mut buf, 4, 0, red, 255);
        blend_px(&mut buf, 0, 0, red, 0);
        assert_eq!(buf.pixels, before);
    }

    #[test]
    fn scale_alpha_rounds_and_keeps_endpoints() {
        assert_eq!(scale_alpha(190, 255), 190);
        assert_eq!(scale_alpha(190, 0), 0);
        assert_eq!(scale_alpha(190, 128), ((190u32 * 128 + 127) / 255) as u8);
    }

    // ---- paint ---------------------------------------------------------------

    #[test]
    fn paint_centers_the_pill_near_the_top() {
        let legend = Legend::new(&tabs(&[("A", "B")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(400, 160, [100, 100, 100, 255]);
        legend.paint(&mut buf, &[false], 255);
        let x0 = (400 - pw) / 2;
        let y0 = TOP_MARGIN;
        // Pill center: blended toward PILL_COLOR at PILL_ALPHA.
        let want = |fg: u8| ((100u32 * (255 - 190) + fg as u32 * 190) / 255) as u8;
        assert_eq!(
            px(&buf, x0 + pw / 2, y0 + ph / 2),
            [
                want(PILL_COLOR.b),
                want(PILL_COLOR.g),
                want(PILL_COLOR.r),
                255
            ]
        );
        // The pill bbox corner pixel is outside the rounded shape: untouched.
        assert_eq!(px(&buf, x0, y0), [100, 100, 100, 255]);
        // Far outside the pill: untouched.
        assert_eq!(px(&buf, 0, 0), [100, 100, 100, 255]);
        assert_eq!(px(&buf, 399, 159), [100, 100, 100, 255]);
    }

    #[test]
    fn paint_renders_the_tab_text_glyphs() {
        let legend = Legend::new(&tabs(&[("A", "B")]));
        let (pw, _) = legend.size();
        let mut buf = frame(400, 160, [0, 0, 0, 255]);
        legend.paint(&mut buf, &[false], 255);
        let x0 = ((400 - pw) / 2) as u32;
        let y0 = TOP_MARGIN;
        let text_x = x0 + PILL_PAD_X + TAB_PAD_X;
        let text_y = y0 + PILL_PAD_Y;
        // 'A' row 0 is 0x0C (bits 2-3): at 2x scale the pixel 4 columns in
        // carries the inactive text color; pixel 8 carries only the pill.
        let text_pixel = px(&buf, text_x + 4, text_y);
        assert_eq!(text_pixel[3], 255);
        assert!(
            text_pixel[0] >= TEXT_INACTIVE.b - 1,
            "text pixel blended toward the text color: {text_pixel:?}"
        );
        assert_ne!(
            text_pixel,
            px(&buf, text_x + 8, text_y),
            "set vs unset glyph pixel"
        );
    }

    #[test]
    fn paint_highlights_the_active_tab() {
        let legend = Legend::new(&tabs(&[("AA", "B"), ("CC", "D")]));
        let (pw, _) = legend.size();
        let mut on = frame(400, 160, [60, 60, 60, 255]);
        legend.paint(&mut on, &[true, false], 255);
        let mut off = frame(400, 160, [60, 60, 60, 255]);
        legend.paint(&mut off, &[false, false], 255);
        let x0 = ((400 - pw) / 2) as u32;
        let y0 = TOP_MARGIN;
        // A pixel inside the first chip but off its text: brighter when active.
        let chip_px = (x0 + PILL_PAD_X + 4, y0 + CHIP_INSET_Y + 14);
        let [b_on, g_on, r_on, _] = px(&on, chip_px.0, chip_px.1);
        let [b_off, g_off, r_off, _] = px(&off, chip_px.0, chip_px.1);
        assert!(
            b_on > b_off && g_on > g_off && r_on > r_off,
            "chip brightens"
        );
        // The same probe under an inactive tab never brightens.
        let second_chip = x0 + PILL_PAD_X + legend.chip_widths[0] + TAB_GAP + 4;
        assert_eq!(
            px(&on, second_chip, chip_px.1),
            px(&off, second_chip, chip_px.1),
            "inactive tab identical in both paints"
        );
    }

    #[test]
    fn paint_scales_with_alpha_and_alpha_zero_is_a_noop() {
        let legend = Legend::new(&tabs(&[("A", "B")]));
        let mut buf = frame(400, 160, [100, 100, 100, 255]);
        let before = buf.pixels.clone();
        legend.paint(&mut buf, &[false], 0);
        assert_eq!(buf.pixels, before, "alpha 0 paints nothing");
        let mut half = frame(400, 160, [100, 100, 100, 255]);
        legend.paint(&mut half, &[false], 128);
        let (pw, ph) = legend.size();
        let cx = ((400 - pw) / 2 + pw / 2) as u32;
        let cy = TOP_MARGIN + ph / 2;
        let want = ((100u32 * (255 - scale_alpha(190, 128) as u32)
            + PILL_COLOR.b as u32 * scale_alpha(190, 128) as u32)
            / 255) as u8;
        assert_eq!(px(&half, cx, cy)[0], want);
    }

    #[test]
    fn paint_skips_monitors_smaller_than_the_pill_and_empty_legends() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S"), ("ZOOM", "F"), ("SNIP", "C")]));
        let mut tiny = frame(32, 32, [100, 100, 100, 255]);
        let before = tiny.pixels.clone();
        legend.paint(&mut tiny, &[true, false, false], 255);
        assert_eq!(tiny.pixels, before, "tiny monitor: skipped, not clipped");
        let empty = Legend::new(&[]);
        let mut buf = frame(400, 64, [100, 100, 100, 255]);
        let before = buf.pixels.clone();
        empty.paint(&mut buf, &[], 255);
        assert_eq!(buf.pixels, before);
        // Fewer active flags than tabs: the rest read as inactive (no panic).
        let mut buf2 = frame(400, 64, [100, 100, 100, 255]);
        legend.paint(&mut buf2, &[true], 255);
    }
}
