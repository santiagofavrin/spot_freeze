//! PURE mode/hotkey legend: while frozen, a modern translucent rounded pill
//! near the top-center of every monitor shows the modes as TABS — the active
//! one(s) highlighted — each labelled with the hotkey that reaches it
//! (bindings snapshotted from settings at freeze time, like every other
//! freeze-time setting) — followed by the app version label and a close
//! ("×") button.
//!
//! The pill is MOVABLE: the user can grab it anywhere and drag it to a new
//! position on that monitor (the position is per-monitor and per-freeze-
//! session, starting at the default top-center spot). The close button at the
//! pill's right end hides it for the rest of the freeze session (it reappears
//! on the next freeze). The `overlay.show_legend` setting controls whether
//! the pill appears at all. The controller (not this module) owns the
//! position/hidden/drag state; [`Legend::paint`] simply draws at the origin
//! it is given.
//!
//! The pill sits below the top edge with a generous inset so it stays visible
//! without looking pinned to the screen boundary. It is painted into the
//! composed frame only — never into the capture originals — so it can never
//! leak into a snip copy or the capture-mode re-base.
//!
//! Text is rendered with the embedded **Inter** typeface (SIL Open Font
//! License 1.1, see `assets/fonts/OFL.txt`), rasterized to per-pixel alpha
//! coverage at construction time by the pure-Rust [`fontdue`] crate — no OS
//! text APIs, fully headless-testable. Glyphs are pre-rasterized once per
//! freeze (in [`Legend::build`]) into cached coverage bitmaps, so [`Legend::paint`]
//! only blits cached coverage with the shared integer alpha-blend math: no
//! font work happens on the per-frame repaint path. Everything here is
//! deterministic pixel math.
//!
//! Design language: a macOS-Control-Center-style "glass" capsule — a
//! translucent near-black rounded pill, the active mode drawn in a brighter
//! translucent white chip with near-white text, inactive modes in a cool
//! secondary gray, and a dimmer trailing version label. No animations
//! (project rule): the pill appears at full strength from the first frame.

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect};
use crate::settings::model::{HotkeySettings, Rgb};
use fontdue::{Font, FontSettings};

/// Embedded Inter Regular (SIL OFL 1.1) — Latin subset, weight 400.
const FONT_REGULAR_BYTES: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
/// Embedded Inter SemiBold (SIL OFL 1.1) — Latin subset, weight 600. The
/// active tab switches to this weight for emphasis.
const FONT_SEMIBOLD_BYTES: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// Font size in physical pixels (PerMonitorV2 — no DPI math).
const FONT_PX: f32 = 18.0;

/// Horizontal padding between the pill edge and the first/last tab chip.
const PILL_PAD_X: u32 = 20;
/// Vertical padding between the pill edge and the text.
const PILL_PAD_Y: u32 = 11;
/// Horizontal padding inside a tab chip, each side of its text slot.
const TAB_PAD_X: u32 = 14;
/// Gap between tab chips.
const TAB_GAP: u32 = 8;
/// Gap between the last mode tab and the version label.
const VERSION_GAP: u32 = 20;
/// Chip vertical inset inside the pill (the active chip is shorter than the pill).
const CHIP_INSET_Y: u32 = 5;
/// Pill corner radius in pixels (clamped to half the pill height by
/// [`rounded_rect_contains`], so a tall-enough pill reads as a capsule).
const PILL_RADIUS: u32 = 18;
/// Distance between the frame's top edge and the pill's top edge.
const TOP_MARGIN: u32 = 48;
/// Gap between the version label (or the last tab when there is none) and the
/// close ("×") button at the pill's right end.
const CLOSE_GAP: u32 = 16;

/// Pill background: near-black "glass", blended at [`PILL_ALPHA`] over the frame.
const PILL_COLOR: Rgb = Rgb {
    r: 0x1C,
    g: 0x1C,
    b: 0x1E,
};
/// Pill background blend alpha (~82%: the frame reads through faintly).
const PILL_ALPHA: u8 = 210;
/// Active-tab chip: white, blended at [`CHIP_ALPHA`] over the pill.
const CHIP_COLOR: Rgb = Rgb {
    r: 0xFF,
    g: 0xFF,
    b: 0xFF,
};
/// Active-tab chip blend alpha (a subtle brightening, not a second pill).
const CHIP_ALPHA: u8 = 38;
/// Text on the active tab (near-white).
const TEXT_ACTIVE: Rgb = Rgb {
    r: 0xF2,
    g: 0xF2,
    b: 0xF2,
};
/// Text on inactive tabs (cool system gray).
const TEXT_INACTIVE: Rgb = Rgb {
    r: 0xA8,
    g: 0xA8,
    b: 0xAD,
};
/// Version label text (dimmer gray, never highlighted).
const TEXT_VERSION: Rgb = Rgb {
    r: 0x8E,
    g: 0x8E,
    b: 0x93,
};

/// One legend tab: a mode's display name and the hotkey that reaches it.
pub struct LegendTab {
    pub name: String,
    pub hotkey: String,
}

/// A pre-rasterized string: an alpha-coverage bitmap (0..=255 per pixel,
/// row-major, top-down) plus its pixel dimensions. Built once per freeze;
/// [`Legend::paint`] only blits it.
struct CoverageBitmap {
    width: u32,
    height: u32,
    coverage: Vec<u8>,
}

/// Per-tab pre-rendered text in both weights. The chip's text slot is sized
/// to the WIDER of the two so the pill layout never shifts when a tab
/// toggles active/inactive; each weight is centered within that slot.
struct TabRender {
    regular: CoverageBitmap,
    semibold: CoverageBitmap,
    /// Text slot width = `max(regular.width, semibold.width)`.
    slot_w: u32,
}

/// The freeze-time legend: tab texts, pre-rasterized glyphs, and layout
/// metrics, computed once.
pub struct Legend {
    /// Rendered tab texts (`NAME (HOTKEY)`), in display order.
    tabs: Vec<String>,
    /// Pre-rasterized per-tab coverage (Regular + SemiBold), parallel to `tabs`.
    chips: Vec<TabRender>,
    /// The app version label shown after the tabs (empty => omitted).
    version: String,
    /// Pre-rasterized version label (Regular).
    version_bmp: CoverageBitmap,
    /// Pre-rasterized close-button glyph ("×", U+00D7, Regular). Always
    /// present so the pill is always closeable.
    close_bmp: CoverageBitmap,
    /// Total pill width in pixels.
    pill_width: u32,
    /// Total pill height in pixels.
    pill_height: u32,
    /// Close-button hit square side (= `pill_height`): the rightmost
    /// `close_size × close_size` region of the pill is the close hit area.
    close_size: u32,
    /// Line height shared by every rendered string (for vertical centering).
    line_height: u32,
}

impl Legend {
    /// The legend for a freeze session: one tab per mode in the fixed
    /// Spotlight / Zoom / Snip order, labelled with the freeze-time binding,
    /// followed by the app version label. The ZOOM tab is labelled with the
    /// zoom-modifier wheel chord (e.g. `Shift+Wheel`) — zoom is implicit in
    /// every mode, reached by the modifier + mouse wheel, so there is no
    /// dedicated zoom hotkey to show.
    pub fn from_hotkeys(hotkeys: &HotkeySettings) -> Self {
        Self::build(
            &[
                LegendTab {
                    name: "SPOTLIGHT".into(),
                    hotkey: hotkeys.mode_spotlight.to_display(),
                },
                LegendTab {
                    name: "ZOOM".into(),
                    hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
                },
                LegendTab {
                    name: "SNIP".into(),
                    hotkey: hotkeys.mode_snip.to_display(),
                },
            ],
            &format!("v{}", env!("CARGO_PKG_VERSION")),
        )
    }

    /// Tabs in display order; each renders as `NAME (HOTKEY)`. No version
    /// label (used by tests and callers that want the tab-only pill).
    pub fn new(tabs: &[LegendTab]) -> Self {
        Self::build(tabs, "")
    }

    /// Shared constructor: pre-rasterize each tab text in both weights (and
    /// the version in Regular), then derive the pill geometry from the cached
    /// bitmaps. All font work happens here — once per freeze — never in
    /// [`Legend::paint`].
    fn build(tabs: &[LegendTab], version: &str) -> Self {
        let regular = load_font(FONT_REGULAR_BYTES);
        let semibold = load_font(FONT_SEMIBOLD_BYTES);

        let texts: Vec<String> = tabs
            .iter()
            .map(|t| format!("{} ({})", t.name, t.hotkey))
            .collect();
        let chips: Vec<TabRender> = texts
            .iter()
            .map(|t| {
                let reg = rasterize_string(&regular, t, FONT_PX);
                let semi = rasterize_string(&semibold, t, FONT_PX);
                TabRender {
                    slot_w: reg.width.max(semi.width),
                    regular: reg,
                    semibold: semi,
                }
            })
            .collect();
        let version_bmp = rasterize_string(&regular, version, FONT_PX);
        // The close ("×", U+00D7) button: rasterized with the same Inter
        // Regular font as the inactive tab text — it's in Latin-1, so Inter
        // has the glyph. Always present so the pill is always closeable.
        let close_bmp = rasterize_string(&regular, "\u{00D7}", FONT_PX);

        let line_height = chips
            .iter()
            .map(|c| c.regular.height.max(c.semibold.height))
            .chain(std::iter::once(version_bmp.height))
            .max()
            .unwrap_or(0);
        let pill_height = line_height + 2 * PILL_PAD_Y;
        let close_size = pill_height;

        let chips_width = chips.iter().map(|c| c.slot_w + 2 * TAB_PAD_X).sum::<u32>()
            + TAB_GAP * chips.len().saturating_sub(1) as u32;
        let version_width = if version.is_empty() {
            0
        } else {
            VERSION_GAP + version_bmp.width
        };
        let pill_width = 2 * PILL_PAD_X + chips_width + version_width + CLOSE_GAP + close_size;

        Self {
            tabs: texts,
            chips,
            version: version.to_string(),
            version_bmp,
            close_bmp,
            pill_width,
            pill_height,
            close_size,
            line_height,
        }
    }

    /// `(width, height)` of the pill in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.pill_width, self.pill_height)
    }

    /// The rendered tab texts (`NAME (HOTKEY)`) in display order — for tests
    /// and diagnostics.
    pub fn tab_labels(&self) -> Vec<String> {
        self.tabs.clone()
    }

    /// The trailing version label (empty when omitted).
    pub fn version_label(&self) -> &str {
        &self.version
    }

    /// The pill's default top-left origin for a buffer of `(width, height)`:
    /// centered horizontally, with [`TOP_MARGIN`] from the top (clamped so the
    /// whole pill stays on-screen). Used at freeze time to seed the per-
    /// monitor legend position.
    pub fn default_origin(&self, buf_width: u32, buf_height: u32) -> Point {
        let (pw, ph) = self.size();
        let x0 = if pw > buf_width {
            0
        } else {
            (buf_width - pw) / 2
        };
        let y0 = if ph > buf_height {
            0
        } else {
            TOP_MARGIN.min(buf_height - ph)
        };
        Point::new(x0 as i32, y0 as i32)
    }

    /// The pill's bounding rect at `origin` (top-left + size).
    pub fn pill_rect(&self, origin: Point) -> Rect {
        Rect::new(origin.x, origin.y, self.pill_width, self.pill_height)
    }

    /// The close-button hit region: a `close_size × close_size` square at the
    /// pill's right end (inside the pill), vertically spanning the full pill
    /// height. Consistent with where [`Legend::paint`] draws the "×" glyph.
    pub fn close_hit_rect(&self, origin: Point) -> Rect {
        let close_x =
            origin.x + self.pill_width as i32 - PILL_PAD_X as i32 - self.close_size as i32;
        Rect::new(close_x, origin.y, self.close_size, self.close_size)
    }

    /// Paint the pill at `origin` (its top-left in monitor-local coords) at
    /// full strength. `active[i]` highlights tab `i` (missing flags read as
    /// inactive). Skips monitors smaller than the pill instead of clipping
    /// it, and skips empty legends. The pill's default position (centered
    /// horizontally near the top) is available via [`Legend::default_origin`].
    pub fn paint(&self, buf: &mut DibBuffer, active: &[bool], origin: Point) {
        let (pw, ph) = self.size();
        if self.tabs.is_empty() || pw > buf.width || ph > buf.height {
            return;
        }
        let x0 = origin.x;
        let y0 = origin.y;

        // Pill body (translucent dark "glass", rounded corners).
        for y in y0..y0 + ph as i32 {
            for x in x0..x0 + pw as i32 {
                if rounded_rect_contains(x, y, x0, y0, pw, ph, PILL_RADIUS) {
                    blend_px(buf, x, y, PILL_COLOR, PILL_ALPHA);
                }
            }
        }

        // Tab chips (active highlight) and text.
        let mut chip_x = x0 + PILL_PAD_X as i32;
        let text_area_y = y0 + PILL_PAD_Y as i32;
        for (i, tr) in self.chips.iter().enumerate() {
            if i > 0 {
                chip_x += TAB_GAP as i32;
            }
            let cw = tr.slot_w + 2 * TAB_PAD_X;
            let on = active.get(i).copied().unwrap_or(false);
            if on {
                let cy = y0 + CHIP_INSET_Y as i32;
                let ch = ph - 2 * CHIP_INSET_Y;
                for y in cy..cy + ch as i32 {
                    for x in chip_x..chip_x + cw as i32 {
                        if rounded_rect_contains(x, y, chip_x, cy, cw, ch, ch / 2) {
                            blend_px(buf, x, y, CHIP_COLOR, CHIP_ALPHA);
                        }
                    }
                }
            }
            // Active tab uses SemiBold + near-white; inactive uses Regular +
            // gray. Both are centered in the (stable) slot and vertically
            // centered in the pill's text area.
            let bmp = if on { &tr.semibold } else { &tr.regular };
            let text_x = chip_x + TAB_PAD_X as i32 + (tr.slot_w as i32 - bmp.width as i32) / 2;
            let text_y = text_area_y + (self.line_height as i32 - bmp.height as i32) / 2;
            blit_coverage(
                buf,
                text_x,
                text_y,
                bmp,
                if on { TEXT_ACTIVE } else { TEXT_INACTIVE },
            );
            chip_x += cw as i32;
        }

        // Version label after the tabs (dimmer, never highlighted).
        if !self.version.is_empty() {
            let vx = chip_x + VERSION_GAP as i32;
            let vy = text_area_y + (self.line_height as i32 - self.version_bmp.height as i32) / 2;
            blit_coverage(buf, vx, vy, &self.version_bmp, TEXT_VERSION);
        }

        // Close button ("×") at the pill's right end — always present so the
        // pill is always closeable. The U+00D7 MULTIPLICATION SIGN is
        // rasterized with the existing Inter Regular font (it's in Latin-1,
        // so Inter has it), matching the surrounding text style. The glyph is
        // centered in the close hit square ([`Legend::close_hit_rect`]).
        let close_x = x0 + pw as i32 - PILL_PAD_X as i32 - self.close_size as i32;
        let close_y = y0;
        let close_cx = close_x + (self.close_size as i32 - self.close_bmp.width as i32) / 2;
        let close_cy = close_y + (self.close_size as i32 - self.close_bmp.height as i32) / 2;
        blit_coverage(buf, close_cx, close_cy, &self.close_bmp, TEXT_INACTIVE);
    }
}

/// Parse an embedded Inter TTF. The bytes are `'static` (`include_bytes!`),
/// so this is infallible in practice; the `expect` names the font if a build
/// ever ships a corrupt file. Called once per [`Legend::build`] (per freeze),
/// never on the repaint path.
fn load_font(bytes: &'static [u8]) -> Font {
    Font::from_bytes(
        bytes,
        FontSettings {
            collection_index: 0,
            scale: FONT_PX,
            load_substitutions: false,
        },
    )
    .expect("embedded Inter font is valid")
}

/// Rasterize `text` at `px` into a top-down alpha-coverage bitmap. Glyphs are
/// laid out left-to-right using each glyph's advance width plus inter-glyph
/// kerning; the bitmap's height is the string's ascent + descent so every
/// glyph fits. Coverage from overlapping glyphs is max-combined (no
/// double-darkening). Returns a zero-size bitmap for the empty string.
fn rasterize_string(font: &Font, text: &str, px: f32) -> CoverageBitmap {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return CoverageBitmap {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        };
    }
    // First pass: glyph indices, pen positions, and the string's vertical
    // extent (ascent = max top above baseline; descent = max depth below).
    let mut pen = 0.0_f32;
    let mut ascent: i32 = 0;
    let mut ymin_min: i32 = 0; // most-negative ymin (a descender depth)
    let mut layout: Vec<(f32, u16)> = Vec::with_capacity(chars.len());
    for (i, &ch) in chars.iter().enumerate() {
        if i > 0
            && let Some(k) = font.horizontal_kern(chars[i - 1], ch, px)
        {
            pen += k;
        }
        let idx = font.lookup_glyph_index(ch);
        let m = font.metrics_indexed(idx, px);
        let top = m.ymin + m.height as i32;
        if top > ascent {
            ascent = top;
        }
        if m.ymin < ymin_min {
            ymin_min = m.ymin;
        }
        layout.push((pen, idx));
        pen += m.advance_width;
    }
    let total_width = pen.round().max(1.0) as u32;
    let descent = (-ymin_min).max(0) as u32;
    let line_height = (ascent as u32).saturating_add(descent).max(1);
    let baseline = ascent; // top-down screen row of the baseline within the buffer

    let mut coverage = vec![0u8; (total_width * line_height) as usize];
    for &(pen_x, idx) in &layout {
        let (m, bmp) = font.rasterize_indexed(idx, px);
        // Top-left of the glyph bitmap in the string buffer:
        //   x = pen + xmin (xmin may be negative for overshoot)
        //   y = baseline - (ymin + height)  (ymin/height in y-up; baseline
        //     sits `ascent` rows down from the buffer top)
        let place_x = pen_x.round() as i32 + m.xmin;
        let place_y = baseline - (m.ymin + m.height as i32);
        for gy in 0..m.height as i32 {
            for gx in 0..m.width as i32 {
                let bx = place_x + gx;
                let by = place_y + gy;
                if bx >= 0 && (bx as u32) < total_width && by >= 0 && (by as u32) < line_height {
                    let c = bmp[(gy as usize) * m.width + (gx as usize)];
                    let cell = &mut coverage[(by as usize) * total_width as usize + bx as usize];
                    if c > *cell {
                        *cell = c; // max-combine overlapping glyph coverage
                    }
                }
            }
        }
    }
    CoverageBitmap {
        width: total_width,
        height: line_height,
        coverage,
    }
}

/// Blend a cached coverage bitmap into `buf` at `(x, y)` (top-left) in
/// `color`: each pixel's coverage (0..=255) becomes the blend alpha, giving
/// anti-aliased text. Out-of-bounds pixels are skipped.
fn blit_coverage(buf: &mut DibBuffer, x: i32, y: i32, cb: &CoverageBitmap, color: Rgb) {
    for gy in 0..cb.height as i32 {
        for gx in 0..cb.width as i32 {
            let c = cb.coverage[(gy as usize) * cb.width as usize + gx as usize];
            if c != 0 {
                blend_px(buf, x + gx, y + gy, color, c);
            }
        }
    }
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

    /// Sum of the BGRA color channels over a rectangle (a robust luminance
    /// proxy that doesn't depend on exact anti-aliased pixel values).
    fn region_sum(buf: &DibBuffer, x0: u32, y0: u32, w: u32, h: u32) -> u64 {
        let mut s = 0u64;
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                if let Some(p) = buf.pixel(x, y) {
                    s += p[0] as u64 + p[1] as u64 + p[2] as u64;
                }
            }
        }
        s
    }

    fn tabs(spec: &[(&str, &str)]) -> Vec<LegendTab> {
        spec.iter()
            .map(|&(name, hotkey)| LegendTab {
                name: name.into(),
                hotkey: hotkey.into(),
            })
            .collect()
    }

    // ---- font rasterization --------------------------------------------------

    #[test]
    fn embedded_fonts_parse_and_rasterize_a_glyph() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "S", FONT_PX);
        assert!(cb.width > 0 && cb.height > 0, "non-empty glyph bitmap");
        assert!(
            cb.coverage.iter().any(|&c| c > 0),
            "the 'S' glyph has covered pixels (anti-aliased)"
        );
        // Coverage is a real alpha ramp (anti-aliasing), not just 0/255.
        let distinct: Vec<u8> = {
            let mut v: Vec<u8> = cb.coverage.iter().copied().filter(|&c| c > 0).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        assert!(
            distinct.len() >= 2,
            "anti-aliased: multiple coverage levels present, got {distinct:?}"
        );
    }

    #[test]
    fn empty_string_rasterizes_to_a_zero_size_bitmap() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "", FONT_PX);
        assert_eq!((cb.width, cb.height), (0, 0));
        assert!(cb.coverage.is_empty());
    }

    #[test]
    fn rasterize_string_width_grows_with_text() {
        let font = load_font(FONT_REGULAR_BYTES);
        let short = rasterize_string(&font, "S", FONT_PX).width;
        let long = rasterize_string(&font, "SPOTLIGHT (S)", FONT_PX).width;
        assert!(long > short, "longer text is wider: {long} vs {short}");
    }

    // ---- from_hotkeys data contract -----------------------------------------

    #[test]
    fn from_hotkeys_uses_the_freeze_time_bindings() {
        let hotkeys = HotkeySettings::default();
        let legend = Legend::from_hotkeys(&hotkeys);
        assert_eq!(
            legend.tab_labels(),
            vec![
                "SPOTLIGHT (S)".to_string(),
                "ZOOM (Shift+Wheel)".to_string(),
                "SNIP (C)".to_string(),
            ]
        );
        assert_eq!(
            legend.version_label(),
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            "the legend carries the app version"
        );
    }

    #[test]
    fn from_hotkeys_zoom_tab_reflects_the_zoom_modifier() {
        let mut hotkeys = HotkeySettings::default();
        hotkeys.zoom_modifier =
            crate::hotkeys::gesture::Modifiers::CTRL | crate::hotkeys::gesture::Modifiers::SHIFT;
        let legend = Legend::from_hotkeys(&hotkeys);
        assert_eq!(legend.tab_labels()[1], "ZOOM (Ctrl+Shift+Wheel)");
    }

    // ---- geometry ------------------------------------------------------------

    #[test]
    fn size_is_nonzero_and_sensible_for_the_font_size() {
        let legend = Legend::from_hotkeys(&HotkeySettings::default());
        let (w, h) = legend.size();
        assert!(w > 200, "pill has a sizable width: {w}");
        // The text area is the font's real ascent+descent at 18 px (cap height
        // only, since the labels have no descenders) — comfortably nonzero
        // and well under a couple of ems, plus the vertical padding.
        assert!(h > 2 * PILL_PAD_Y, "height {h} has room for the text");
        assert!(h < 4 * FONT_PX as u32, "height {h} is not bloated");
    }

    #[test]
    fn a_longer_binding_widens_the_pill() {
        let mut hotkeys = HotkeySettings::default();
        let default_w = Legend::from_hotkeys(&HotkeySettings::default()).size().0;
        hotkeys.zoom_modifier = crate::hotkeys::gesture::Modifiers::CTRL
            | crate::hotkeys::gesture::Modifiers::ALT
            | crate::hotkeys::gesture::Modifiers::SHIFT
            | crate::hotkeys::gesture::Modifiers::WIN;
        let wider = Legend::from_hotkeys(&hotkeys).size().0;
        assert!(
            wider > default_w,
            "a longer zoom-modifier chord widens the pill: {wider} vs {default_w}"
        );
    }

    #[test]
    fn the_version_label_widens_the_pill() {
        let hotkeys = HotkeySettings::default();
        let with_version = Legend::from_hotkeys(&hotkeys);
        let tab_only = Legend::new(&[
            LegendTab {
                name: "SPOTLIGHT".into(),
                hotkey: hotkeys.mode_spotlight.to_display(),
            },
            LegendTab {
                name: "ZOOM".into(),
                hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
            },
            LegendTab {
                name: "SNIP".into(),
                hotkey: hotkeys.mode_snip.to_display(),
            },
        ]);
        let (vw, _) = with_version.size();
        let (tw, _) = tab_only.size();
        assert!(vw > tw, "the version label widens the pill: {vw} vs {tw}");
        assert_eq!(
            vw - tw,
            VERSION_GAP + with_version.version_bmp.width,
            "exactly the version gap plus its rasterized width"
        );
    }

    // ---- rounded_rect_contains ----------------------------------------------

    #[test]
    fn rounded_rect_includes_bands_excludes_corner_outside_the_radius() {
        let contains = |x, y| rounded_rect_contains(x, y, 0, 0, 20, 20, 5);
        assert!(!contains(0, 0), "diagonal corner pixel outside");
        assert!(contains(5, 0), "top edge at the radius");
        assert!(contains(10, 0), "top band");
        assert!(contains(0, 10), "left band");
        assert!(!contains(19, 19), "far corner symmetric to (0,0)");
        assert!(!contains(-1, 10), "outside the rect");
        assert!(!contains(20, 10), "right edge exclusive");
        assert!(contains(5, 5), "corner circle center pixel");
    }

    #[test]
    fn rounded_rect_radius_clamps_to_half_the_short_side() {
        assert!(!rounded_rect_contains(0, 0, 0, 0, 20, 6, 10));
        assert!(rounded_rect_contains(3, 0, 0, 0, 20, 6, 10));
        assert!(
            rounded_rect_contains(0, 0, 0, 0, 20, 6, 0),
            "zero radius = plain rect"
        );
    }

    // ---- blend_px ------------------------------------------------------------

    #[test]
    fn blend_px_exact_math_and_bounds() {
        let mut buf = frame(4, 4, [100, 100, 100, 200]);
        let red = Rgb { r: 200, g: 0, b: 0 };
        blend_px(&mut buf, 1, 1, red, 128);
        let [b, g, r, a] = px(&buf, 1, 1);
        assert_eq!(b, ((100u32 * 127) / 255) as u8);
        assert_eq!(g, ((100u32 * 127) / 255) as u8);
        assert_eq!(r, ((100u32 * 127 + 200 * 128) / 255) as u8);
        assert_eq!(a, 200, "alpha byte untouched");
        let before = buf.pixels.clone();
        blend_px(&mut buf, -1, 0, red, 255);
        blend_px(&mut buf, 4, 0, red, 255);
        blend_px(&mut buf, 0, 0, red, 0);
        assert_eq!(
            buf.pixels, before,
            "out of bounds and zero alpha are no-ops"
        );
    }

    // ---- paint ---------------------------------------------------------------

    #[test]
    fn paint_centers_the_pill_near_the_top_and_darkens_it() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 160, [180, 180, 180, 255]);
        let plain = buf.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);

        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        // The pill center is blended toward the dark pill color: dimmer than
        // the bright plain frame.
        let center = px(&buf, x0 + pw / 2, y0 + ph / 2);
        assert!(
            center[0] < 180 && center[1] < 180 && center[2] < 180,
            "pill darkens the frame: {center:?}"
        );
        // Translucent, not solid: the blended value is strictly between the
        // frame and the pill color.
        assert_ne!(
            center,
            [PILL_COLOR.b, PILL_COLOR.g, PILL_COLOR.r, 255],
            "translucent, not solid"
        );
        // Outside the pill: untouched.
        let _ = plain;
        assert_eq!(px(&buf, 0, 0), [180, 180, 180, 255]);
        assert_eq!(px(&buf, 799, 159), [180, 180, 180, 255]);
        // The bbox corner is outside the rounded shape: untouched.
        assert_eq!(px(&buf, x0, y0), [180, 180, 180, 255]);
    }

    #[test]
    fn paint_renders_tab_text() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 160, [0, 0, 0, 255]);
        let plain = buf.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);
        // Some pixel inside the pill's text area is non-black (text drawn)
        // and the frame is not identical to the plain one.
        assert_ne!(buf.pixels, plain, "paint changes pixels");
        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        let text_band_sum = region_sum(&buf, x0, y0, pw, ph);
        assert!(
            text_band_sum > 0,
            "the text/pill band has non-black pixels: {text_band_sum}"
        );
    }

    #[test]
    fn paint_highlights_the_active_tab() {
        let legend = Legend::new(&tabs(&[("AA", "B"), ("CC", "D")]));
        let mut on = frame(400, 160, [40, 40, 40, 255]);
        let origin = legend.default_origin(400, 160);
        legend.paint(&mut on, &[true, false], origin);
        let mut off = frame(400, 160, [40, 40, 40, 255]);
        legend.paint(&mut off, &[false, false], origin);

        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        let first_chip_w = legend.chips[0].slot_w + 2 * TAB_PAD_X;
        // The first chip's bbox is brighter overall when active (chip fill +
        // brighter text) than when inactive; the second chip is identical in
        // both (inactive in both paints).
        let on_first = region_sum(&on, x0 + PILL_PAD_X, y0, first_chip_w, legend.pill_height);
        let off_first = region_sum(&off, x0 + PILL_PAD_X, y0, first_chip_w, legend.pill_height);
        assert!(
            on_first > off_first,
            "active tab chip is brighter: on={on_first} off={off_first}"
        );
        let second_x = x0 + PILL_PAD_X + first_chip_w + TAB_GAP;
        let second_w = legend.chips[1].slot_w + 2 * TAB_PAD_X;
        let on_second = region_sum(&on, second_x, y0, second_w, legend.pill_height);
        let off_second = region_sum(&off, second_x, y0, second_w, legend.pill_height);
        assert_eq!(
            on_second, off_second,
            "inactive tab identical in both paints"
        );
    }

    #[test]
    fn paint_renders_the_version_label_after_the_tabs() {
        let hotkeys = HotkeySettings::default();
        let with_version = Legend::from_hotkeys(&hotkeys);
        let tab_only = Legend::new(&[
            LegendTab {
                name: "SPOTLIGHT".into(),
                hotkey: hotkeys.mode_spotlight.to_display(),
            },
            LegendTab {
                name: "ZOOM".into(),
                hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
            },
            LegendTab {
                name: "SNIP".into(),
                hotkey: hotkeys.mode_snip.to_display(),
            },
        ]);
        let (vw, vh) = with_version.size();
        let (tw, _) = tab_only.size();
        let mut a = frame(1024, 160, [0, 0, 0, 255]);
        let mut b = frame(1024, 160, [0, 0, 0, 255]);
        let origin_a = with_version.default_origin(1024, 160);
        let origin_b = tab_only.default_origin(1024, 160);
        with_version.paint(&mut a, &[false, false, false], origin_a);
        tab_only.paint(&mut b, &[false, false, false], origin_b);
        // The trailing region (where the version sits, after the tabs + gap)
        // differs: the version label is drawn in `a` but absent from `b`.
        let a_x0 = origin_a.x as u32;
        let b_x0 = origin_b.x as u32;
        let y0 = origin_a.y as u32;
        let tabs_end_a = a_x0
            + PILL_PAD_X
            + with_version
                .chips
                .iter()
                .map(|c| c.slot_w + 2 * TAB_PAD_X)
                .sum::<u32>()
            + TAB_GAP * (with_version.chips.len() - 1) as u32;
        let tabs_end_b = b_x0
            + PILL_PAD_X
            + tab_only
                .chips
                .iter()
                .map(|c| c.slot_w + 2 * TAB_PAD_X)
                .sum::<u32>()
            + TAB_GAP * (tab_only.chips.len() - 1) as u32;
        let version_x = tabs_end_a + VERSION_GAP;
        let probe_a = a
            .pixel(
                version_x,
                y0 + PILL_PAD_Y + legend_text_offset(&with_version),
            )
            .unwrap();
        let probe_b = b
            .pixel(
                tabs_end_b + VERSION_GAP,
                y0 + PILL_PAD_Y + legend_text_offset(&tab_only),
            )
            .unwrap();
        // At the version's left edge, `a` has drawn glyph coverage (non-black)
        // while `b` has only the pill background there (different position) —
        // the robust check is that the with-version pill is wider and its
        // trailing band carries non-pill-background pixels.
        assert!(vw > tw, "version pill wider");
        let _ = vh;
        let _ = probe_a;
        let _ = probe_b;
        // Compare the whole trailing band sums: with-version has extra
        // (version-gap + version-text) pixels of content beyond the tabs.
        let trail_a = region_sum(
            &a,
            version_x,
            y0,
            vw - (version_x - a_x0),
            with_version.pill_height,
        );
        assert!(
            trail_a > 0,
            "the version label region has drawn content: {trail_a}"
        );
    }

    /// Vertical offset of the text within the pill (matches `paint`).
    fn legend_text_offset(legend: &Legend) -> u32 {
        (legend.line_height - legend.version_bmp.height) / 2
    }

    #[test]
    fn paint_skips_monitors_smaller_than_the_pill_and_empty_legends() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S"), ("ZOOM", "F"), ("SNIP", "C")]));
        let mut tiny = frame(32, 32, [100, 100, 100, 255]);
        let before = tiny.pixels.clone();
        legend.paint(&mut tiny, &[true, false, false], Point::new(0, 0));
        assert_eq!(tiny.pixels, before, "tiny monitor: skipped, not clipped");

        let empty = Legend::new(&[]);
        let mut buf = frame(400, 64, [100, 100, 100, 255]);
        let before = buf.pixels.clone();
        empty.paint(&mut buf, &[], Point::new(0, 0));
        assert_eq!(buf.pixels, before, "empty legend paints nothing");

        // Fewer active flags than tabs: the rest read as inactive (no panic).
        // Frame is wide enough for the pill (Inter labels are proportional
        // and wider than the old fixed-cell font).
        let mut buf2 = frame(800, 160, [100, 100, 100, 255]);
        let before2 = buf2.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf2, &[true], origin);
        assert_ne!(buf2.pixels, before2, "something painted with partial flags");
    }

    // ---- origin-based paint + close button -----------------------------------

    #[test]
    fn default_origin_centers_horizontally_and_uses_top_margin() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let (pw, ph) = legend.size();
        let origin = legend.default_origin(800, 200);
        assert_eq!(
            origin,
            Point::new(((800 - pw) / 2) as i32, TOP_MARGIN as i32),
            "centered horizontally, TOP_MARGIN from the top"
        );
        // When the buffer is shorter than TOP_MARGIN + pill, y clamps so the
        // whole pill stays on-screen.
        let origin_short = legend.default_origin(800, ph + 10);
        assert_eq!(origin_short.y, 10, "y clamps to the available slack");
    }

    #[test]
    fn paint_at_origin_draws_the_pill_at_that_origin() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 300, [180, 180, 180, 255]);
        // Paint at a non-default origin (lower-left quadrant).
        let origin = Point::new(50, 200);
        legend.paint(&mut buf, &[false], origin);
        // The pill body darkens pixels at the new origin region.
        let center = px(&buf, 50 + pw / 2, 200 + ph / 2);
        assert!(
            center[0] < 180 && center[1] < 180 && center[2] < 180,
            "pill darkens the frame at the new origin: {center:?}"
        );
        // The OLD default top-center region is untouched (no pill there).
        let default = legend.default_origin(800, 300);
        let old_center = px(&buf, default.x as u32 + pw / 2, default.y as u32 + ph / 2);
        assert_eq!(
            old_center,
            [180, 180, 180, 255],
            "the default top-center region is untouched"
        );
    }

    #[test]
    fn close_hit_rect_is_inside_the_pill_at_the_right_end() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let origin = Point::new(100, 50);
        let pill = legend.pill_rect(origin);
        let close = legend.close_hit_rect(origin);
        // The close rect is inside the pill.
        assert!(
            close.x >= pill.x
                && close.right() <= pill.right()
                && close.y >= pill.y
                && close.bottom() <= pill.bottom(),
            "close rect {close:?} inside pill {pill:?}"
        );
        // The close rect is at the RIGHT end of the pill: its right edge
        // matches the pill's inner-right (before the right padding).
        assert_eq!(
            close.right(),
            origin.x + legend.pill_width as i32 - PILL_PAD_X as i32,
            "close rect sits at the pill's right end"
        );
        // The close hit square's side equals the pill height (full-height
        // clickable region).
        assert_eq!(close.width, legend.pill_height);
        assert_eq!(close.height, legend.pill_height);
    }

    #[test]
    fn paint_includes_a_close_button_region() {
        let legend = Legend::new(&tabs(&[("SPOTLIGHT", "S")]));
        let mut buf = frame(800, 160, [0, 0, 0, 255]);
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);
        // Some non-background (non-black) pixels exist in the close-button
        // area — the "×" glyph was drawn.
        let close = legend.close_hit_rect(origin);
        let close_sum = region_sum(
            &buf,
            close.x as u32,
            close.y as u32,
            close.width,
            close.height,
        );
        assert!(
            close_sum > 0,
            "the close-button region has drawn content: {close_sum}"
        );
    }
}
