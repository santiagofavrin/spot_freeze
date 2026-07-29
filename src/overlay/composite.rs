//! PURE pixel operations on [`DibBuffer`] — the performance-critical core.
//!
//! No `windows` types anywhere: every function works on plain buffers and
//! geometry and is exhaustively unit-tested headless. Coordinates are
//! BUFFER-LOCAL physical pixels unless a function explicitly says
//! virtual-screen.
//!
//! The render pipeline for one monitor's frame is [`compose_frame`], fed with
//! a [`RenderState`] built by the mode stack (composable layers):
//! **zoom base → colored darken → spotlight hole (reveals the ZOOMED base) →
//! snip selection (interior + border ring)**.

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect};
use crate::settings::model::Rgb;

/// Darken `buf` IN PLACE by blending toward the veil `color`:
/// `channel' = (channel * (255 - dim_alpha) + color_channel * dim_alpha) / 255`
/// for B, G, R (ONE division, single truncation); alpha untouched.
///
/// `color = Rgb::BLACK` reduces EXACTLY to the legacy black-veil formula
/// (`channel * (255 - dim_alpha) / 255`). `dim_alpha` 0 = no change,
/// 255 = exactly the veil color. Buffer channels are BGRA: `color.b` blends
/// into channel 0, `color.g` into 1, `color.r` into 2.
pub fn darken(buf: &mut DibBuffer, dim_alpha: u8, color: Rgb) {
    if dim_alpha == 0 {
        return; // identity — avoid touching memory at all
    }
    let stride = buf.stride as usize;
    if stride == 0 {
        return; // empty buffer: chunks_exact_mut(0) would panic
    }
    let keep = (255 - dim_alpha) as u32;
    let a = dim_alpha as u32;
    let veil = [color.b as u32, color.g as u32, color.r as u32];
    // Row-slice iteration: one bounds check per row instead of per pixel.
    for row in buf.pixels.chunks_exact_mut(stride) {
        for px in row.chunks_exact_mut(4) {
            // Exact contract math: floor((ch * (255 - a) + veil * a) / 255).
            // Max value is 255*255 + 255*255 = 130050 — u32 cannot overflow.
            px[0] = ((px[0] as u32 * keep + veil[0] * a) / 255) as u8;
            px[1] = ((px[1] as u32 * keep + veil[1] * a) / 255) as u8;
            px[2] = ((px[2] as u32 * keep + veil[2] * a) / 255) as u8;
            // px[3] (alpha) untouched.
        }
    }
}

/// Restore the ORIGINAL image inside the spotlight circle: copy pixels from
/// `src_original` into `dst_darkened` for every pixel whose position lies within
/// `radius` px of `center` (`dx*dx + dy*dy <= radius*radius`).
///
/// Both buffers MUST have identical dimensions (same width/height/stride);
/// `center` may be outside the buffer (the copied region is simply clipped).
/// This is the per-mouse-move fast path: cost is O(circle area).
pub fn spotlight_hole(
    dst_darkened: &mut DibBuffer,
    src_original: &DibBuffer,
    center: Point,
    radius: u32,
) {
    debug_assert_eq!(dst_darkened.width, src_original.width);
    debug_assert_eq!(dst_darkened.height, src_original.height);
    debug_assert_eq!(dst_darkened.stride, src_original.stride);
    // Release-build safety: operate on the common rectangle only.
    let w = dst_darkened.width.min(src_original.width) as i64;
    let h = dst_darkened.height.min(src_original.height) as i64;
    if w <= 0 || h <= 0 {
        return;
    }

    let cx = center.x as i64;
    let cy = center.y as i64;
    let r = radius as u64;
    let rr = r * r;

    // Vertical span the circle can touch, clipped to the buffer.
    let y0 = (cy - r as i64).max(0);
    let y1 = (cy + r as i64).min(h - 1);
    if y0 > y1 {
        return;
    }

    let dstride = dst_darkened.stride as usize;
    let sstride = src_original.stride as usize;

    for y in y0..=y1 {
        let dy = (y - cy).unsigned_abs();
        let dd = dy * dy;
        if dd > rr {
            continue; // outside the circle vertically (possible after clipping)
        }
        // Widest horizontal half-chord at this row: dx^2 + dy^2 <= r^2.
        let dx_max = isqrt_u64(rr - dd) as i64;
        let x0 = (cx - dx_max).max(0);
        let x1 = (cx + dx_max).min(w - 1);
        if x0 > x1 {
            continue;
        }
        // One contiguous memcpy per row — O(1) per pixel with no per-pixel
        // predicate evaluation.
        let len = ((x1 - x0 + 1) * 4) as usize;
        let di = y as usize * dstride + x0 as usize * 4;
        let si = y as usize * sstride + x0 as usize * 4;
        dst_darkened.pixels[di..di + len].copy_from_slice(&src_original.pixels[si..si + len]);
    }
}

/// Floor integer square root for `u64`. f64 seed plus exact correction —
/// no `unsafe`, correct for the full `u64` range we can produce (radius^2).
fn isqrt_u64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut r = (n as f64).sqrt() as u64;
    while r > 0 && r * r > n {
        r -= 1;
    }
    while r < u32::MAX as u64 && (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

/// Resampling kernel for [`zoom_resample`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ZoomFilter {
    #[default]
    Nearest,
    Bilinear,
}

/// Resample `src` around `focus` at `zoom` magnification into a NEW buffer of
/// `viewport.width × viewport.height` px.
///
/// `viewport.x`/`viewport.y` are IGNORED (callers pass the monitor-local
/// viewport; the fields exist to mirror the window contract). The sampled
/// source region is centered on `focus` and spans `(viewport.width / zoom) ×
/// `(viewport.height / zoom)` px. Samples outside `src` are CLIPPED to the
/// nearest edge pixel (edge pixels replicate outward). `zoom` must be > 0 —
/// callers clamp it to the settings min/max.
///
/// Pixel mapping: output pixel `(ox, oy)` samples source coordinate
/// `focus + (o + 0.5 - viewport/2) / zoom - 0.5` per axis, so the output is
/// centered on `focus` and `zoom == 1.0` is a pixel-exact pan.
pub fn zoom_resample(
    src: &DibBuffer,
    viewport: Rect,
    zoom: f32,
    focus: Point,
    filter: ZoomFilter,
) -> DibBuffer {
    let vw = viewport.width;
    let vh = viewport.height;
    let mut out = DibBuffer {
        width: vw,
        height: vh,
        stride: vw * 4,
        pixels: vec![0u8; vw as usize * vh as usize * 4],
    };
    if vw == 0 || vh == 0 || src.width == 0 || src.height == 0 {
        return out;
    }
    debug_assert!(zoom > 0.0, "zoom must be > 0 (callers clamp to settings)");
    let zoom = if zoom > 0.0 { zoom } else { 1.0 };

    let sw = src.width as i32;
    let sh = src.height as i32;
    let sstride = src.stride as usize;

    // Hoist the per-column source mapping out of the row loop.
    // src_x(ox) = focus.x + (ox + 0.5 - vw/2) / zoom - 0.5
    let half_vw = vw as f32 / 2.0;
    let half_vh = vh as f32 / 2.0;
    let map_x = |ox: u32| focus.x as f32 + (ox as f32 + 0.5 - half_vw) / zoom - 0.5;
    let map_y = |oy: u32| focus.y as f32 + (oy as f32 + 0.5 - half_vh) / zoom - 0.5;

    match filter {
        ZoomFilter::Nearest => {
            // Precompute clamped source x for every output column once.
            let xmap: Vec<i32> = (0..vw)
                .map(|ox| (map_x(ox).round() as i32).clamp(0, sw - 1))
                .collect();
            // Fast path: an unclamped contiguous run (covers zoom == 1.0 pans
            // and identity) — one memcpy per row instead of per-pixel gather.
            let contiguous = xmap[0] >= 0
                && xmap[0] + vw as i32 <= sw
                && xmap.iter().enumerate().all(|(i, &x)| x == xmap[0] + i as i32);

            let ostride = out.stride as usize;
            for oy in 0..vh {
                let sy = (map_y(oy).round() as i32).clamp(0, sh - 1) as usize;
                let orow = &mut out.pixels[oy as usize * ostride..][..ostride];
                if contiguous {
                    let si = sy * sstride + xmap[0] as usize * 4;
                    orow.copy_from_slice(&src.pixels[si..si + ostride]);
                } else {
                    let srow = &src.pixels[sy * sstride..][..sstride];
                    for (ox, &sx) in xmap.iter().enumerate() {
                        orow[ox * 4..ox * 4 + 4]
                            .copy_from_slice(&srow[sx as usize * 4..sx as usize * 4 + 4]);
                    }
                }
            }
        }
        ZoomFilter::Bilinear => {
            // Precompute clamped tap coordinates + fraction per column once.
            let xmap: Vec<(usize, usize, f32)> = (0..vw)
                .map(|ox| {
                    let fx_f = map_x(ox);
                    let x0 = fx_f.floor() as i32;
                    let frac = fx_f - x0 as f32;
                    (
                        x0.clamp(0, sw - 1) as usize,
                        (x0 + 1).clamp(0, sw - 1) as usize,
                        frac,
                    )
                })
                .collect();

            let ostride = out.stride as usize;
            for oy in 0..vh {
                let fy_f = map_y(oy);
                let y0i = fy_f.floor() as i32;
                let fy = fy_f - y0i as f32;
                let y0 = y0i.clamp(0, sh - 1) as usize;
                let y1 = (y0i + 1).clamp(0, sh - 1) as usize;
                let row0 = &src.pixels[y0 * sstride..][..sstride];
                let row1 = &src.pixels[y1 * sstride..][..sstride];
                let orow = &mut out.pixels[oy as usize * ostride..][..ostride];

                for (ox, &(x0, x1, fx)) in xmap.iter().enumerate() {
                    let w00 = (1.0 - fx) * (1.0 - fy);
                    let w10 = fx * (1.0 - fy);
                    let w01 = (1.0 - fx) * fy;
                    let w11 = fx * fy;
                    let opx = &mut orow[ox * 4..ox * 4 + 4];
                    for ch in 0..4 {
                        let v = row0[x0 * 4 + ch] as f32 * w00
                            + row0[x1 * 4 + ch] as f32 * w10
                            + row1[x0 * 4 + ch] as f32 * w01
                            + row1[x1 * 4 + ch] as f32 * w11;
                        opx[ch] = v.round() as u8;
                    }
                }
            }
        }
    }
    out
}

/// Per-monitor input to [`compose_frame`]: which composable layers contribute
/// to THIS monitor's frame, and where. Built by
/// [`crate::overlay::modes::ModeStack::render_state`] — each layer contributes
/// only on the monitor its state lives on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderState {
    /// `(factor, focus)` when the zoom layer is active on this monitor:
    /// `factor` is the magnification (>= 1.0), `focus` the monitor-local
    /// cursor position the zoomed view is centered on.
    pub zoom: Option<(f32, Point)>,
    /// `(center, radius)` when the spotlight layer is active on this monitor:
    /// the circle of undarkened base around the cursor (monitor-local).
    pub spotlight: Option<(Point, u32)>,
    /// `(a, b)` drag endpoints of the snip selection (monitor-local, ANY drag
    /// direction — normalized internally) when present on this monitor.
    pub snip: Option<(Point, Point)>,
}

/// Compose one monitor's frame into `out`, COMPLETELY overwriting it, in the
/// spec pipeline order:
///
/// 1. **Zoom base**: when `state.zoom` is set, the base is
///    [`zoom_resample`] of `original` over `viewport` with
///    [`ZoomFilter::Nearest`] (the render path's filter — zero interpolation
///    cost, and the snip-copy path crops the identical resample, so the copy
///    matches the presented frame pixel-for-pixel); otherwise the base IS
///    `original`.
/// 2. **Colored darken**: [`darken`] applies the veil (`dim_alpha`, `color`)
///    to the base copied into `out`.
/// 3. **Spotlight hole**: [`spotlight_hole`] restores the UNDARKENED base
///    inside the circle.
/// 4. **Snip selection**: the interior reveals the undarkened base and a 2 px
///    border ring (1 px outside + 1 px inside the rect edge) is drawn by
///    INVERTING the current frame pixels — inversion contrasts with both the
///    darkened veil and the restored base on any content, needs no settings
///    color, and matches the layer's dirty-region contract
///    ([`crate::overlay::modes::snip`]).
///
/// `viewport.x`/`viewport.y` are ignored (mirroring [`zoom_resample`]);
/// `out` may differ in size from `original` — every stage operates on the
/// common rectangle only. No allocations on the no-zoom path.
pub fn compose_frame(
    original: &DibBuffer,
    out: &mut DibBuffer,
    viewport: Rect,
    state: &RenderState,
    dim_alpha: u8,
    color: Rgb,
) {
    // 1. Base: the zoomed view when the zoom layer is active on this monitor,
    //    else the original capture. `zoomed` owns the resample (if any) so
    //    `base` can borrow from either source uniformly.
    let zoomed;
    let base: &DibBuffer = match state.zoom {
        Some((factor, focus)) => {
            zoomed = zoom_resample(original, viewport, factor, focus, ZoomFilter::Nearest);
            &zoomed
        }
        None => original,
    };

    // 2. Copy the base into `out`, then darken in place (the veil).
    copy_into(out, base);
    darken(out, dim_alpha, color);

    // 3. Spotlight hole reveals the undarkened base inside the circle.
    if let Some((center, radius)) = state.spotlight {
        spotlight_hole(out, base, center, radius);
    }

    // 4. Snip selection: interior reveals the undarkened base; the border
    //    ring inverts whatever the frame currently shows (painted LAST so the
    //    ring is never overwritten by the other layers).
    if let Some((a, b)) = state.snip {
        let rect = Rect::from_points(a, b);
        if !rect.is_empty() {
            restore_rect(out, base, rect);
            invert_border_ring(out, rect);
        }
    }
}

/// Copy `src` into `dst` over their common rectangle (row-slice memcpy per
/// row). Same-size buffers (the controller's per-monitor case) are a straight
/// full-frame copy.
fn copy_into(dst: &mut DibBuffer, src: &DibBuffer) {
    let w = dst.width.min(src.width) as usize;
    let h = dst.height.min(src.height) as usize;
    if w == 0 || h == 0 {
        return;
    }
    let dstride = dst.stride as usize;
    let sstride = src.stride as usize;
    let row_bytes = w * 4;
    for y in 0..h {
        let di = y * dstride;
        let si = y * sstride;
        dst.pixels[di..di + row_bytes].copy_from_slice(&src.pixels[si..si + row_bytes]);
    }
}

/// Copy the base pixels of the NORMALIZED, pre-clipped rect `r` back over the
/// frame (the snip selection interior). Clipped to both buffers' common
/// rectangle; a fully-outside rect is a no-op.
fn restore_rect(out: &mut DibBuffer, base: &DibBuffer, r: Rect) {
    let w = out.width.min(base.width) as i32;
    let h = out.height.min(base.height) as i32;
    let x0 = r.x.max(0);
    let y0 = r.y.max(0);
    let x1 = (r.x + r.width as i32).min(w);
    let y1 = (r.y + r.height as i32).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let dstride = out.stride as usize;
    let sstride = base.stride as usize;
    let row_bytes = (x1 - x0) as usize * 4;
    for y in y0..y1 {
        let di = y as usize * dstride + x0 as usize * 4;
        let si = y as usize * sstride + x0 as usize * 4;
        out.pixels[di..di + row_bytes].copy_from_slice(&base.pixels[si..si + row_bytes]);
    }
}

/// Pixels the snip selection border extends OUTSIDE the selection rect (the
/// full ring is `SNIP_BORDER_OUT + 1` px: one more ring just inside the edge).
/// Mirrors the layer's dirty-region contract in `modes::snip`.
const SNIP_BORDER_OUT: i32 = 1;

/// Invert B/G/R of the pixel span `x0..=x1` on row `y` (alpha untouched);
/// clipped to the buffer, so out-of-bounds spans are safely ignored.
fn invert_span(buf: &mut DibBuffer, y: i32, x0: i32, x1: i32) {
    if y < 0 || y >= buf.height as i32 {
        return;
    }
    let x0 = x0.max(0);
    let x1 = x1.min(buf.width as i32 - 1);
    if x0 > x1 {
        return;
    }
    let stride = buf.stride as usize;
    let row = y as usize * stride;
    for x in x0..=x1 {
        let i = row + x as usize * 4;
        buf.pixels[i] = !buf.pixels[i];
        buf.pixels[i + 1] = !buf.pixels[i + 1];
        buf.pixels[i + 2] = !buf.pixels[i + 2];
    }
}

/// Draw the snip selection border as a 2 px ring (1 px outside + 1 px inside
/// the rect edge) by INVERTING the current frame pixels. `rc` is the
/// normalized selection rect (buffer-local, unclipped input tolerated).
fn invert_border_ring(buf: &mut DibBuffer, rc: Rect) {
    let bw = buf.width as i32;
    let bh = buf.height as i32;
    if bw == 0 || bh == 0 {
        return;
    }
    let rright = rc.x + rc.width as i32;
    let rbottom = rc.y + rc.height as i32;
    // Clipped outer-ring bounds, inclusive.
    let ox0 = (rc.x - SNIP_BORDER_OUT).max(0);
    let oy0 = (rc.y - SNIP_BORDER_OUT).max(0);
    let ox1 = (rright + SNIP_BORDER_OUT - 1).min(bw - 1);
    let oy1 = (rbottom + SNIP_BORDER_OUT - 1).min(bh - 1);
    if ox0 > ox1 || oy0 > oy1 {
        return;
    }
    for y in oy0..=oy1 {
        // The two edge rows on each side (outer + inner ring) span the full
        // width; middle rows touch only the two side columns. For 1-px-wide
        // selections the side columns overlap into a full-width span.
        if y <= rc.y || y >= rbottom - 1 || rc.width == 1 {
            invert_span(buf, y, ox0, ox1);
        } else {
            invert_span(buf, y, ox0, rc.x);
            invert_span(buf, y, rright - 1, ox1);
        }
    }
}

/// Draw a solid border ring `thickness` px wide around the frame edge in
/// `color` (B/G/R channels; alpha untouched) — the mode-change flash frame
/// painted by the controller on top of a freshly composed frame.
///
/// `thickness` is clamped to the buffer dimensions: an oversized thickness
/// simply fills the whole frame. `thickness == 0` is a no-op.
pub fn draw_border(buf: &mut DibBuffer, color: Rgb, thickness: u32) {
    let w = buf.width as usize;
    let h = buf.height as usize;
    if thickness == 0 || w == 0 || h == 0 {
        return;
    }
    let t = (thickness as usize).min(w).min(h);
    let stride = buf.stride as usize;
    for y in 0..h {
        let row = &mut buf.pixels[y * stride..y * stride + w * 4];
        if y < t || y >= h - t {
            // Top/bottom bands: full-width rows.
            for px in row.chunks_exact_mut(4) {
                px[0] = color.b;
                px[1] = color.g;
                px[2] = color.r;
            }
        } else {
            // Middle rows: left and right bands only (overlap when the ring
            // fills the width — painting twice is harmless).
            for x in 0..t {
                px_assign(&mut row[x * 4..], color);
            }
            for x in (w - t)..w {
                px_assign(&mut row[x * 4..], color);
            }
        }
    }
}

/// Set the B/G/R channels of the pixel at the start of `px` (a >= 4-byte
/// slice into the frame); alpha untouched.
#[inline]
fn px_assign(px: &mut [u8], color: Rgb) {
    px[0] = color.b;
    px[1] = color.g;
    px[2] = color.r;
}

/// Crop `src` to the rectangle between drag endpoints `a` and `b` given in ANY
/// drag direction (negative drags are normalized), clipped to buffer bounds.
/// Returns `None` when the normalized/clipped rectangle is empty.
pub fn crop_normalized(src: &DibBuffer, a: Point, b: Point) -> Option<DibBuffer> {
    // Normalize any drag direction (implemented inline so this module does
    // not depend on Rect helpers).
    let x0 = a.x.min(b.x).max(0);
    let y0 = a.y.min(b.y).max(0);
    let x1 = a.x.max(b.x).min(src.width as i32);
    let y1 = a.y.max(b.y).min(src.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let w = (x1 - x0) as u32;
    let h = (y1 - y0) as u32;
    let stride = src.stride as usize;
    let row_bytes = w as usize * 4;
    let mut pixels = Vec::with_capacity(row_bytes * h as usize);
    for y in y0..y1 {
        let i = y as usize * stride + x0 as usize * 4;
        pixels.extend_from_slice(&src.pixels[i..i + row_bytes]);
    }
    Some(DibBuffer {
        width: w,
        height: h,
        stride: w * 4,
        pixels,
    })
}

/// Index of the monitor (rects in VIRTUAL-SCREEN coordinates) containing
/// `point` (also virtual-screen); `None` when outside all monitors.
pub fn monitor_index_at(point: Point, monitors: &[Rect]) -> Option<usize> {
    let px = point.x as i64;
    let py = point.y as i64;
    // First match wins on overlapping rects. Left/top inclusive,
    // right/bottom exclusive. i64 edges avoid i32 overflow on extreme rects.
    monitors.iter().position(|m| {
        let x0 = m.x as i64;
        let y0 = m.y as i64;
        px >= x0 && px < x0 + m.width as i64 && py >= y0 && py < y0 + m.height as i64
    })
}

/// Virtual-screen → monitor-local: subtracts the monitor's top-left corner.
pub fn virtual_to_local(point: Point, monitor: Rect) -> Point {
    Point::new(point.x - monitor.x, point.y - monitor.y)
}

/// Monitor-local → virtual-screen: adds the monitor's top-left corner.
pub fn local_to_virtual(point: Point, monitor: Rect) -> Point {
    Point::new(point.x + monitor.x, point.y + monitor.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -------------------------------------------------------

    /// Build a buffer from a pixel generator (fields are pub — no reliance
    /// on `DibBuffer::new`, owned by another module).
    fn make_buf(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> DibBuffer {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&f(x, y));
            }
        }
        DibBuffer {
            width: w,
            height: h,
            stride: w * 4,
            pixels,
        }
    }

    /// Distinct, deterministic pattern: every pixel unique-ish per channel.
    fn pattern(x: u32, y: u32) -> [u8; 4] {
        [
            (x * 7 + y) as u8,
            (y * 5 + x) as u8,
            (x.wrapping_add(y) * 3) as u8,
            255,
        ]
    }

    fn px(buf: &DibBuffer, x: u32, y: u32) -> [u8; 4] {
        let i = (y * buf.stride + x * 4) as usize;
        buf.pixels[i..i + 4].try_into().unwrap()
    }

    fn solid(w: u32, h: u32, c: [u8; 4]) -> DibBuffer {
        make_buf(w, h, |_, _| c)
    }

    // ---- darken --------------------------------------------------------

    /// The default veil (black) — the legacy behavior.
    const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
    /// A non-black veil for the colored-darken pins: #802020 (dark red).
    const VEIL: Rgb = Rgb {
        r: 0x80,
        g: 0x20,
        b: 0x20,
    };

    #[test]
    fn darken_alpha_zero_is_noop() {
        for color in [BLACK, VEIL] {
            let mut buf = make_buf(16, 16, pattern);
            let before = buf.pixels.clone();
            darken(&mut buf, 0, color);
            assert_eq!(buf.pixels, before);
        }
    }

    #[test]
    fn darken_full_alpha_is_black_but_keeps_alpha() {
        let mut buf = make_buf(8, 8, |x, y| [x as u8, y as u8, 200, 123]);
        darken(&mut buf, 255, BLACK);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&buf, x, y), [0, 0, 0, 123]);
            }
        }
    }

    #[test]
    fn darken_exact_channel_math() {
        // Exhaustive over representative channel values × dim alphas (black
        // veil == the legacy formula exactly).
        for &ch in &[0u8, 1, 2, 127, 128, 200, 254, 255] {
            for &a in &[0u8, 1, 63, 128, 191, 254, 255] {
                let mut buf = solid(1, 1, [ch, ch, ch, 77]);
                darken(&mut buf, a, BLACK);
                let expect = (ch as u32 * (255 - a as u32) / 255) as u8;
                assert_eq!(px(&buf, 0, 0), [expect, expect, expect, 77], "ch={ch} a={a}");
            }
        }
    }

    #[test]
    fn darken_colored_veil_exact_channel_math() {
        // channel' = (ch * (255 - a) + veil_ch * a) / 255, ONE division; BGRA:
        // color.b -> ch 0, color.g -> ch 1, color.r -> ch 2; alpha untouched.
        let blend = |ch: u8, veil: u8, a: u8| {
            ((ch as u32 * (255 - a as u32) + veil as u32 * a as u32) / 255) as u8
        };
        for &ch in &[0u8, 1, 127, 200, 255] {
            for &a in &[1u8, 63, 160, 191, 254] {
                let mut buf = solid(1, 1, [ch, ch, ch, 77]);
                darken(&mut buf, a, VEIL);
                let want = [
                    blend(ch, VEIL.b, a),
                    blend(ch, VEIL.g, a),
                    blend(ch, VEIL.r, a),
                    77,
                ];
                assert_eq!(px(&buf, 0, 0), want, "ch={ch} a={a}");
            }
        }
    }

    #[test]
    fn darken_full_alpha_is_exactly_the_veil_color() {
        let mut buf = make_buf(4, 4, |x, y| [x as u8, y as u8, 90, 55]);
        darken(&mut buf, 255, VEIL);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(px(&buf, x, y), [VEIL.b, VEIL.g, VEIL.r, 55]);
            }
        }
    }

    #[test]
    fn darken_empty_buffer_no_panic() {
        let mut buf = DibBuffer::default();
        darken(&mut buf, 128, VEIL);
        assert!(buf.pixels.is_empty());
    }

    // ---- spotlight_hole ------------------------------------------------

    #[test]
    fn spotlight_exact_circle_boundary() {
        // 9x9, center (4,4), r=2: verify EVERY pixel against dx^2+dy^2<=4.
        let src = make_buf(9, 9, pattern);
        let mut dst = solid(9, 9, [10, 10, 10, 255]);
        spotlight_hole(&mut dst, &src, Point::new(4, 4), 2);
        for y in 0..9i32 {
            for x in 0..9i32 {
                let dx = x - 4;
                let dy = y - 4;
                let inside = dx * dx + dy * dy <= 4;
                let got = px(&dst, x as u32, y as u32);
                if inside {
                    assert_eq!(got, pattern(x as u32, y as u32), "({x},{y}) should be restored");
                } else {
                    assert_eq!(got, [10, 10, 10, 255], "({x},{y}) should stay dark");
                }
            }
        }
    }

    #[test]
    fn spotlight_radius_zero_copies_single_pixel() {
        let src = make_buf(5, 5, pattern);
        let mut dst = solid(5, 5, [0, 0, 0, 255]);
        spotlight_hole(&mut dst, &src, Point::new(2, 3), 0);
        assert_eq!(px(&dst, 2, 3), pattern(2, 3));
        // Everything else untouched.
        for y in 0..5 {
            for x in 0..5 {
                if (x, y) != (2, 3) {
                    assert_eq!(px(&dst, x, y), [0, 0, 0, 255]);
                }
            }
        }
    }

    #[test]
    fn spotlight_radius_zero_offscreen_copies_nothing() {
        let src = make_buf(4, 4, pattern);
        let mut dst = solid(4, 4, [9, 9, 9, 255]);
        spotlight_hole(&mut dst, &src, Point::new(-3, 10), 0);
        assert_eq!(dst, solid(4, 4, [9, 9, 9, 255]));
    }

    #[test]
    fn spotlight_circle_partially_offscreen_top_left() {
        // Center (-1,-1) r=2 on 4x4: only pixels with (x+1)^2+(y+1)^2<=4.
        let src = make_buf(4, 4, pattern);
        let mut dst = solid(4, 4, [1, 2, 3, 255]);
        spotlight_hole(&mut dst, &src, Point::new(-1, -1), 2);
        for y in 0..4i32 {
            for x in 0..4i32 {
                let inside = (x + 1) * (x + 1) + (y + 1) * (y + 1) <= 4;
                let expect = if inside {
                    pattern(x as u32, y as u32)
                } else {
                    [1, 2, 3, 255]
                };
                assert_eq!(px(&dst, x as u32, y as u32), expect, "({x},{y})");
            }
        }
        // Sanity: (0,0) inside (1+1=2<=4), (0,1) inside, (1,1) outside (2+... = 1+1+... )
        assert_eq!(px(&dst, 0, 0), pattern(0, 0));
    }

    #[test]
    fn spotlight_circle_partially_offscreen_bottom_right() {
        let src = make_buf(6, 6, pattern);
        let mut dst = solid(6, 6, [0, 0, 0, 255]);
        spotlight_hole(&mut dst, &src, Point::new(7, 7), 3);
        for y in 0..6i32 {
            for x in 0..6i32 {
                let inside = (x - 7) * (x - 7) + (y - 7) * (y - 7) <= 9;
                let expect = if inside {
                    pattern(x as u32, y as u32)
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(px(&dst, x as u32, y as u32), expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn spotlight_huge_radius_restores_everything() {
        let src = make_buf(32, 24, pattern);
        let mut dst = solid(32, 24, [5, 5, 5, 255]);
        spotlight_hole(&mut dst, &src, Point::new(16, 12), 10_000);
        assert_eq!(dst.pixels, src.pixels);
    }

    // ---- zoom_resample -------------------------------------------------

    #[test]
    fn zoom_identity_is_exact_both_filters() {
        // Even dimensions + centered focus => zoom 1.0 reproduces src exactly.
        let src = make_buf(8, 6, pattern);
        let viewport = Rect::new(0, 0, 8, 6);
        let focus = Point::new(4, 3);
        for filter in [ZoomFilter::Nearest, ZoomFilter::Bilinear] {
            let out = zoom_resample(&src, viewport, 1.0, focus, filter);
            assert_eq!(out.width, 8);
            assert_eq!(out.height, 6);
            assert_eq!(out.stride, 32);
            assert_eq!(out.pixels, src.pixels, "filter {filter:?}");
        }
    }

    #[test]
    fn zoom_2x_nearest_exact_mapping() {
        // viewport 8x8, zoom 2, focus (2,2) on 4x4 src.
        // src_x = 2 + (ox+0.5-4)/2 - 0.5 => nearest column map [0,0,1,1,2,2,3,3].
        let src = make_buf(4, 4, pattern);
        let out = zoom_resample(&src, Rect::new(0, 0, 8, 8), 2.0, Point::new(2, 2), ZoomFilter::Nearest);
        for oy in 0..8u32 {
            for ox in 0..8u32 {
                let expect = pattern(ox / 2, oy / 2);
                assert_eq!(px(&out, ox, oy), expect, "({ox},{oy})");
            }
        }
    }

    #[test]
    fn zoom_out_edge_clipping_replicates_edge_pixels() {
        // 4x4 src, viewport 8x8, zoom 0.5, focus at the CORNER (0,0).
        // src_x = 0 + (ox+0.5-4)/0.5 - 0.5 = 2*ox - 7.5 (nearest, then clamp).
        // ox:      0    1    2    3   4   5   6   7
        // src_x: -7.5 -5.5 -3.5 -1.5 0.5 2.5 4.5 6.5
        // round:  -8   -6   -4   -2   1   3   5   7  (f32::round, half away from 0)
        // clamp:   0    0    0    0   1   3   3   3
        let src = make_buf(4, 4, pattern);
        let out = zoom_resample(&src, Rect::new(0, 0, 8, 8), 0.5, Point::new(0, 0), ZoomFilter::Nearest);
        let colmap = [0u32, 0, 0, 0, 1, 3, 3, 3];
        for oy in 0..8u32 {
            for ox in 0..8u32 {
                let expect = pattern(colmap[ox as usize], colmap[oy as usize]);
                assert_eq!(px(&out, ox, oy), expect, "({ox},{oy})");
            }
        }
        // Corner is the replicated edge pixel, not black.
        assert_eq!(px(&out, 0, 0), pattern(0, 0));
    }

    #[test]
    fn zoom_bilinear_exact_half_blend() {
        // 2x1 src [0,0,0,255] / [100,200,50,255]; viewport 3x1, focus (1,0), zoom 1.
        // src_x = 1 + (ox+0.5-1.5) - 0.5 = ox - 1.5 => samples -1.5, -0.5, 0.5...
        // wait: ox-0.5? recompute: src_x = 1 + (ox + 0.5 - 1.5)/1 - 0.5 = ox - 0.5.
        // ox=0 -> -0.5 (clamped: both taps pixel 0) ; ox=1 -> 0.5 (50/50 blend);
        // ox=2 -> 1.5 (clamped: both taps pixel 1).
        let src = make_buf(2, 1, |x, _| if x == 0 { [0, 0, 0, 255] } else { [100, 200, 50, 255] });
        let out = zoom_resample(&src, Rect::new(0, 0, 3, 1), 1.0, Point::new(1, 0), ZoomFilter::Bilinear);
        assert_eq!(px(&out, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&out, 1, 0), [50, 100, 25, 255]);
        assert_eq!(px(&out, 2, 0), [100, 200, 50, 255]);
    }

    #[test]
    fn zoom_bilinear_vertical_quarter_blend() {
        // 1x2 src: top [0,0,0,255], bottom [200,100,0,255].
        // viewport 1x5, focus (0,1), zoom 1: src_y = 1 + (oy+0.5-2.5) - 0.5 = oy - 1.5.
        // oy=0 -> -1.5 clamp top; oy=3 -> 1.5 clamp bottom; oy=2 -> 0.5 => 50/50.
        let src = make_buf(1, 2, |_, y| if y == 0 { [0, 0, 0, 255] } else { [200, 100, 0, 255] });
        let out = zoom_resample(&src, Rect::new(0, 0, 1, 5), 1.0, Point::new(0, 1), ZoomFilter::Bilinear);
        assert_eq!(px(&out, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&out, 0, 2), [100, 50, 0, 255]);
        assert_eq!(px(&out, 0, 3), [200, 100, 0, 255]);
        assert_eq!(px(&out, 0, 4), [200, 100, 0, 255]);
    }

    #[test]
    fn zoom_zero_viewport_and_empty_src_are_safe() {
        let src = make_buf(4, 4, pattern);
        let out = zoom_resample(&src, Rect::new(0, 0, 0, 0), 2.0, Point::new(0, 0), ZoomFilter::Nearest);
        assert_eq!(out.pixels.len(), 0);
        let empty = DibBuffer::default();
        let out2 = zoom_resample(&empty, Rect::new(0, 0, 4, 4), 2.0, Point::new(0, 0), ZoomFilter::Bilinear);
        assert_eq!(out2.pixels.len(), 4 * 4 * 4); // zeroed, no panic
    }

    #[test]
    fn zoom_viewport_xy_ignored() {
        let src = make_buf(8, 6, pattern);
        let a = zoom_resample(&src, Rect::new(0, 0, 8, 6), 1.0, Point::new(4, 3), ZoomFilter::Nearest);
        let b = zoom_resample(&src, Rect::new(-1920, 500, 8, 6), 1.0, Point::new(4, 3), ZoomFilter::Nearest);
        assert_eq!(a.pixels, b.pixels);
    }

    // ---- crop_normalized -----------------------------------------------

    #[test]
    fn crop_positive_drag_exact_contents() {
        let src = make_buf(8, 8, pattern);
        let out = crop_normalized(&src, Point::new(2, 3), Point::new(6, 5)).unwrap();
        assert_eq!((out.width, out.height, out.stride), (4, 2, 16));
        for y in 0..2u32 {
            for x in 0..4u32 {
                assert_eq!(px(&out, x, y), pattern(x + 2, y + 3));
            }
        }
    }

    #[test]
    fn crop_negative_drag_normalized() {
        let src = make_buf(8, 8, pattern);
        let fwd = crop_normalized(&src, Point::new(2, 3), Point::new(6, 5)).unwrap();
        // Both reversed axes and swapped endpoint order must give the same crop.
        let rev = crop_normalized(&src, Point::new(6, 5), Point::new(2, 3)).unwrap();
        let mixed = crop_normalized(&src, Point::new(6, 3), Point::new(2, 5)).unwrap();
        assert_eq!(fwd.pixels, rev.pixels);
        assert_eq!(fwd.pixels, mixed.pixels);
        assert_eq!((rev.width, rev.height), (4, 2));
    }

    #[test]
    fn crop_partially_outside_is_clipped() {
        let src = make_buf(4, 4, pattern);
        let out = crop_normalized(&src, Point::new(-10, 2), Point::new(3, 90)).unwrap();
        assert_eq!((out.width, out.height), (3, 2)); // x: 0..3, y: 2..4
        for y in 0..2u32 {
            for x in 0..3u32 {
                assert_eq!(px(&out, x, y), pattern(x, y + 2));
            }
        }
    }

    #[test]
    fn crop_fully_outside_returns_none() {
        let src = make_buf(4, 4, pattern);
        assert!(crop_normalized(&src, Point::new(10, 10), Point::new(20, 20)).is_none());
        assert!(crop_normalized(&src, Point::new(-20, -20), Point::new(-10, -10)).is_none());
        assert!(crop_normalized(&src, Point::new(0, -50), Point::new(4, -1)).is_none());
        assert!(crop_normalized(&src, Point::new(5, 0), Point::new(50, 4)).is_none());
    }

    #[test]
    fn crop_zero_area_returns_none() {
        let src = make_buf(4, 4, pattern);
        assert!(crop_normalized(&src, Point::new(2, 2), Point::new(2, 2)).is_none());
        assert!(crop_normalized(&src, Point::new(1, 1), Point::new(3, 1)).is_none()); // zero height
        assert!(crop_normalized(&src, Point::new(1, 1), Point::new(1, 3)).is_none()); // zero width
    }

    #[test]
    fn crop_full_buffer() {
        let src = make_buf(4, 4, pattern);
        let out = crop_normalized(&src, Point::new(0, 0), Point::new(4, 4)).unwrap();
        assert_eq!(out.pixels, src.pixels);
    }

    // ---- multi-monitor mapping ------------------------------------------

    fn three_monitors() -> Vec<Rect> {
        vec![
            Rect::new(0, 0, 1920, 1080),       // primary
            Rect::new(-1920, 0, 1920, 1080),   // left of primary (negative x)
            Rect::new(1920, -200, 2560, 1440), // right, slightly higher
        ]
    }

    #[test]
    fn monitor_index_at_hits_and_misses() {
        let mons = three_monitors();
        assert_eq!(monitor_index_at(Point::new(0, 0), &mons), Some(0));
        assert_eq!(monitor_index_at(Point::new(1919, 1079), &mons), Some(0));
        assert_eq!(monitor_index_at(Point::new(-1, 500), &mons), Some(1));
        assert_eq!(monitor_index_at(Point::new(-1920, 0), &mons), Some(1));
        assert_eq!(monitor_index_at(Point::new(1920, 0), &mons), Some(2)); // right edge exclusive
        assert_eq!(monitor_index_at(Point::new(3000, 1000), &mons), Some(2));
        assert_eq!(monitor_index_at(Point::new(1920, -201), &mons), None); // above monitor 2
        assert_eq!(monitor_index_at(Point::new(-1921, 0), &mons), None);
        assert_eq!(monitor_index_at(Point::new(0, 1080), &mons), None); // below primary
        assert_eq!(monitor_index_at(Point::new(0, 0), &[]), None);
    }

    #[test]
    fn coordinate_mapping_roundtrip_negative_virtual() {
        let mon = Rect::new(-1920, -100, 1920, 1080);
        let virt = Point::new(-1000, 500);
        let local = virtual_to_local(virt, mon);
        assert_eq!(local, Point::new(920, 600));
        assert_eq!(local_to_virtual(local, mon), virt);
        // Corners.
        assert_eq!(virtual_to_local(Point::new(-1920, -100), mon), Point::new(0, 0));
        assert_eq!(local_to_virtual(Point::new(0, 0), mon), Point::new(-1920, -100));
    }

    #[test]
    fn coordinate_mapping_primary_is_identity() {
        let primary = Rect::new(0, 0, 1920, 1080);
        let p = Point::new(37, 42);
        assert_eq!(virtual_to_local(p, primary), p);
        assert_eq!(local_to_virtual(p, primary), p);
    }

    // ---- compose_frame ---------------------------------------------------

    /// Reference colored-dim math, mirroring the documented darken formula.
    fn dimmed(p: [u8; 4], a: u8, color: Rgb) -> [u8; 4] {
        let ch = |c: u8, v: u8| ((c as u32 * (255 - a as u32) + v as u32 * a as u32) / 255) as u8;
        [ch(p[0], color.b), ch(p[1], color.g), ch(p[2], color.r), p[3]]
    }

    #[test]
    fn compose_no_layers_is_plain_colored_dim_of_original() {
        let original = make_buf(16, 12, pattern);
        let mut out = DibBuffer::new(16, 12);
        compose_frame(
            &original,
            &mut out,
            Rect::new(0, 0, 16, 12),
            &RenderState::default(),
            160,
            VEIL,
        );
        for y in 0..12 {
            for x in 0..16 {
                assert_eq!(px(&out, x, y), dimmed(pattern(x, y), 160, VEIL), "({x},{y})");
            }
        }
    }

    #[test]
    fn compose_dim_alpha_zero_is_the_original_frame() {
        let original = make_buf(8, 8, pattern);
        let mut out = DibBuffer::new(8, 8);
        compose_frame(
            &original,
            &mut out,
            Rect::new(0, 0, 8, 8),
            &RenderState::default(),
            0,
            VEIL,
        );
        assert_eq!(out, original);
    }

    #[test]
    fn compose_spotlight_hole_reveals_original_colored_dim_outside() {
        let original = make_buf(16, 16, pattern);
        let state = RenderState {
            zoom: None,
            spotlight: Some((Point::new(8, 8), 2)),
            snip: None,
        };
        let mut out = DibBuffer::new(16, 16);
        compose_frame(&original, &mut out, Rect::new(0, 0, 16, 16), &state, 160, VEIL);
        for y in 0..16i32 {
            for x in 0..16i32 {
                let inside = (x - 8) * (x - 8) + (y - 8) * (y - 8) <= 4;
                let p = pattern(x as u32, y as u32);
                let want = if inside { p } else { dimmed(p, 160, VEIL) };
                assert_eq!(px(&out, x as u32, y as u32), want, "({x},{y})");
            }
        }
    }

    #[test]
    fn compose_zoom_spotlight_reveals_the_zoomed_base() {
        // 2x zoom around (8,8); the hole must show the ZOOMED base, not the
        // original: output (10,8) samples src 8 + (10.5-8)/2 - 0.5 = 8.75 ->
        // 9 (nearest) — discriminated against original(10,8) below.
        let original = make_buf(16, 16, pattern);
        let focus = Point::new(8, 8);
        let state = RenderState {
            zoom: Some((2.0, focus)),
            spotlight: Some((Point::new(8, 8), 3)),
            snip: None,
        };
        let zoomed = zoom_resample(
            &original,
            Rect::new(0, 0, 16, 16),
            2.0,
            focus,
            ZoomFilter::Nearest,
        );
        let mut out = DibBuffer::new(16, 16);
        compose_frame(&original, &mut out, Rect::new(0, 0, 16, 16), &state, 160, BLACK);
        // Inside the hole: exact zoomed base. Outside: dimmed zoomed base.
        assert_eq!(px(&out, 8, 8), px(&zoomed, 8, 8));
        assert_eq!(px(&out, 0, 0), dimmed(px(&zoomed, 0, 0), 160, BLACK));
        // The hole is the zoomed base, not the original.
        assert_eq!(px(&out, 10, 8), px(&zoomed, 10, 8));
        assert_ne!(px(&out, 10, 8), pattern(10, 8));
    }

    #[test]
    fn compose_snip_shows_base_inside_dimmed_outside_with_inverted_ring() {
        let original = make_buf(20, 20, pattern);
        let (a, b) = (Point::new(5, 5), Point::new(12, 10)); // rect x 5..12, y 5..10
        let state = RenderState {
            zoom: None,
            spotlight: None,
            snip: Some((a, b)),
        };
        let mut out = DibBuffer::new(20, 20);
        compose_frame(&original, &mut out, Rect::new(0, 0, 20, 20), &state, 160, BLACK);

        // Deep interior (>= 2 px off every edge): exact original.
        for y in 7..=7u32 {
            for x in 7..=10u32 {
                assert_eq!(px(&out, x, y), pattern(x, y), "interior ({x},{y})");
            }
        }
        // Far exterior: dimmed original.
        assert_eq!(px(&out, 0, 0), dimmed(pattern(0, 0), 160, BLACK));
        assert_eq!(px(&out, 19, 19), dimmed(pattern(19, 19), 160, BLACK));
        // The ring is 1 px OUTSIDE + 1 px INSIDE the rect edge, inverted:
        // outer-ring pixel (4,5) shows the inverted DIMMED original; inner-ring
        // pixel (5,6) shows the inverted RESTORED original; one px further
        // out/in the frame is untouched by the ring.
        let outer = px(&out, 4, 5);
        let expect_outer = {
            let d = dimmed(pattern(4, 5), 160, BLACK);
            [!d[0], !d[1], !d[2], d[3]]
        };
        assert_eq!(outer, expect_outer, "outer ring inverts the dimmed frame");
        let inner = px(&out, 5, 6);
        let p56 = pattern(5, 6);
        assert_eq!(inner, [!p56[0], !p56[1], !p56[2], p56[3]], "inner ring inverts the base");
        assert_eq!(px(&out, 6, 6), pattern(6, 6), "just inside the ring: plain base");
        assert_eq!(px(&out, 3, 5), dimmed(pattern(3, 5), 160, BLACK), "beyond the ring");
        // Negative drags normalize identically.
        let state2 = RenderState {
            zoom: None,
            spotlight: None,
            snip: Some((b, a)),
        };
        let mut out2 = DibBuffer::new(20, 20);
        compose_frame(&original, &mut out2, Rect::new(0, 0, 20, 20), &state2, 160, BLACK);
        assert_eq!(out.pixels, out2.pixels);
    }

    #[test]
    fn compose_degenerate_snip_renders_nothing() {
        let original = make_buf(8, 8, pattern);
        let state = RenderState {
            zoom: None,
            spotlight: None,
            snip: Some((Point::new(4, 4), Point::new(4, 4))),
        };
        let mut out = DibBuffer::new(8, 8);
        compose_frame(&original, &mut out, Rect::new(0, 0, 8, 8), &state, 160, BLACK);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&out, x, y), dimmed(pattern(x, y), 160, BLACK), "({x},{y})");
            }
        }
    }

    // ---- draw_border (mode-change flash frame) ----------------------------

    #[test]
    fn draw_border_paints_a_solid_ring_and_keeps_alpha() {
        let mut buf = solid(10, 8, [1, 2, 3, 200]);
        let white = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        draw_border(&mut buf, white, 2);
        for y in 0..8u32 {
            for x in 0..10u32 {
                let in_ring = !(2..8).contains(&x) || !(2..6).contains(&y);
                let want = if in_ring {
                    [255, 255, 255, 200]
                } else {
                    [1, 2, 3, 200]
                };
                assert_eq!(px(&buf, x, y), want, "({x},{y})");
            }
        }
    }

    #[test]
    fn draw_border_zero_thickness_and_empty_buffer_are_noops() {
        let mut buf = make_buf(4, 4, pattern);
        let before = buf.pixels.clone();
        draw_border(&mut buf, Rgb::BLACK, 0);
        assert_eq!(buf.pixels, before);
        let mut empty = DibBuffer::default();
        draw_border(&mut empty, Rgb::BLACK, 6); // must not panic
        assert!(empty.pixels.is_empty());
    }

    #[test]
    fn draw_border_oversized_thickness_fills_the_frame() {
        let mut buf = solid(4, 4, [9, 9, 9, 255]);
        draw_border(&mut buf, Rgb { r: 1, g: 2, b: 3 }, 100);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(px(&buf, x, y), [3, 2, 1, 255]);
            }
        }
    }
}
