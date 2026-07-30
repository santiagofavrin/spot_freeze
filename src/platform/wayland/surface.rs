//! Layer-shell overlay surfaces: one fullscreen `zwlr_layer_surface_v1` per
//! output, presenting [`DibBuffer`] frames through double-buffered wl_shm
//! slots.
//!
//! # Contracts
//!
//! - **Surface**: layer `OVERLAY`, namespace `spotfreeze`, anchored to all
//!   four edges, `exclusive_zone = -1` (cover the output exactly, ignore
//!   struts), `keyboard_interactivity = EXCLUSIVE` — the frozen overlay must
//!   receive every key. If the compositor never grants focus, the shell
//!   demotes the surface to `ON_DEMAND` after the freeze (see
//!   [`super::shell::Shell::ensure_keyboard_focus`]).
//! - **Sizing**: the buffer is the output's PHYSICAL pixel size; the surface
//!   is logical-size driven (4 anchors, size 0) with
//!   `wl_surface.set_buffer_scale(scale)` — physical = logical × scale
//!   (integer scales only, see the shell module docs). The initial commit is
//!   buffer-less; the factory waits (bounded) for the first `configure` and
//!   acks it before the first present, per the layer-shell handshake.
//! - **Presentation**: [`OverlaySurface::present`] memcpy's the full frame or
//!   only the dirty rows into the BACK slot's shm mapping (same tightly
//!   packed BGRA layout as [`DibBuffer`], so `wl_shm::Format::Xrgb8888` on
//!   little-endian needs no conversion), then `damage_buffer` + `attach` +
//!   `commit` + connection flush.
//! - **Double buffering**: a slot is writable only after the compositor's
//!   `wl_buffer.release`. Releases arrive on a DEDICATED event queue per
//!   surface (pumped non-blocking inside `present`/`can_present`, which also
//!   read the connection themselves — required while the synchronous
//!   freeze/unfreeze fade blocks the main loop, harmless otherwise; events
//!   for the main queue stay buffered for it). `present` NEVER blocks: with
//!   both slots busy it drops the frame — the controller defers through
//!   [`OverlaySurface::can_present`] and repaints with the latest composed
//!   frame once a slot frees (high-rate pointer input would otherwise build
//!   an unbounded repaint backlog behind the compositor's release cadence).
//! - **Teardown**: `Drop` unregisters the surface from the input registry and
//!   the shell's focus list, then destroys the layer surface + wl_surface +
//!   buffers (proxy destructors).
//!
//! The translatable pieces — dirty-rect clipping, row copying, slot
//! selection — are pure and unit-tested headless; only the protocol glue
//! touches Wayland objects.

use crate::capture::DibBuffer;
use crate::geometry::Rect;
use crate::overlay::events::OverlayEventSink;
use crate::platform::OverlaySurface;
use crate::platform::wayland::input::SurfaceRegistration;
use crate::platform::wayland::shell::{FactoryParts, OutputRecord, ShellState, map_shm, pump_until};
use anyhow::{Context, Result, bail};
use std::cell::{Cell, RefCell};
use std::os::unix::io::AsFd;
use std::rc::Rc;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity,
};

/// Layer-shell namespace for every overlay surface.
const NAMESPACE: &str = "spotfreeze";

/// Number of shm buffer slots per surface (double buffering).
const SLOT_COUNT: usize = 2;

/// Bookkeeping the shell needs about a live layer surface: initial-configure
/// signaling for the factory, `closed` routing, and keyboard-focus state for
/// the on-demand fallback.
pub(crate) struct LayerHandle {
    /// wl_surface ObjectId (identity for input routing + removal).
    pub surface_id: ObjectId,
    pub layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    pub wl_surface: wl_surface::WlSurface,
    /// `Some((w, h))` once the compositor's first configure arrived.
    pub configured: Rc<Cell<Option<(u32, u32)>>>,
    /// Set by the compositor's `closed` event (presents then fail).
    pub closed: Rc<Cell<bool>>,
    /// Set while this surface holds keyboard focus (wl_keyboard.enter).
    pub keyboard_focus: Rc<Cell<bool>>,
}

/// One double-buffering slot: a memfd-backed shm mapping + its wl_buffer.
struct ShmSlot {
    buffer: wl_buffer::WlBuffer,
    mapping: crate::platform::wayland::shell::ShmMapping,
}

/// Release-event state for the per-surface buffer queue: which slots the
/// compositor has handed back.
struct SlotState {
    free: [bool; SLOT_COUNT],
}

/// One fullscreen layer-shell overlay surface over one output.
pub struct LayerOverlaySurface {
    conn: Connection,
    wl_surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    /// Physical pixel size of the monitor rect (buffer size).
    width: u32,
    height: u32,
    slots: [ShmSlot; SLOT_COUNT],
    attached: Option<usize>,
    slot_state: SlotState,
    release_queue: EventQueue<SlotState>,
    closed: Rc<Cell<bool>>,
    /// Surface registry entry (input routing); removed on drop.
    registry: crate::platform::wayland::input::SurfaceRegistry,
    /// Shell-side live list (configure/closed/focus routing); removed on drop.
    handles: Rc<RefCell<Vec<LayerHandle>>>,
    surface_id: ObjectId,
}

impl LayerOverlaySurface {
    /// Create and map the layer surface for `output` (its physical size must
    /// equal `monitor_rect.size()`), run the layer-shell initial-configure
    /// handshake, and register input routing. The surface is unmapped until
    /// the first [`present`](OverlaySurface::present).
    pub(crate) fn create(
        parts: &FactoryParts<'_>,
        output: &OutputRecord,
        monitor_index: usize,
        monitor_rect: Rect,
        // The shared all-monitor rect list is unused on Wayland: pointer
        // events arrive per-surface (the compositor routes them), so no
        // focus-window rerouting is needed (unlike WM_MOUSEWHEEL).
        _monitors: Rc<Vec<Rect>>,
        sink: OverlayEventSink,
    ) -> Result<Self> {
        if monitor_rect.width == 0 || monitor_rect.height == 0 {
            bail!("overlay surface: monitor rect must be non-empty, got {monitor_rect:?}");
        }
        let FactoryParts {
            conn,
            qh,
            globals,
            core,
        } = *parts;
        let wl_surface = globals.compositor.create_surface(qh, ());
        let layer_surface = globals.layer_shell.get_layer_surface(
            &wl_surface,
            Some(&output.output),
            Layer::Overlay,
            NAMESPACE.to_string(),
            qh,
            (),
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        wl_surface.set_buffer_scale(output.scale.max(1) as i32);

        // Register the handle BEFORE the initial commit so the configure
        // event (arriving during the pump below) can find it.
        let configured = Rc::new(Cell::new(None));
        let closed = Rc::new(Cell::new(false));
        let keyboard_focus = Rc::new(Cell::new(false));
        let handle = LayerHandle {
            surface_id: wl_surface.id(),
            layer_surface: layer_surface.clone(),
            wl_surface: wl_surface.clone(),
            configured: configured.clone(),
            closed: closed.clone(),
            keyboard_focus: keyboard_focus.clone(),
        };
        let handles = core.borrow().state.layer_handles.clone();
        handles.borrow_mut().push(handle);

        // Layer-shell handshake: buffer-less initial commit, then wait for
        // the first configure (acked in the Dispatch impl) before presenting.
        wl_surface.commit();
        conn.flush().context("committing the layer surface")?;
        pump_until(
            core,
            |state| {
                state
                    .layer_handles
                    .borrow()
                    .iter()
                    .any(|h| h.surface_id == wl_surface.id() && h.configured.get().is_some())
            },
            "the layer surface's initial configure",
        )?;

        // Buffer slots: physical pixels, tightly packed xrgb8888 (== the
        // DibBuffer BGRA layout on little-endian).
        let (width, height) = (monitor_rect.width, monitor_rect.height);
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .context("overlay surface: monitor rect size overflows usize")?;
        let release_queue: EventQueue<SlotState> = conn.new_event_queue();
        let release_qh = release_queue.handle();
        let make_slot = |slot: usize| -> Result<ShmSlot> {
            let (fd, mapping) = map_shm(len)?;
            let pool = globals.shm.create_pool(fd.as_fd(), len as i32, &release_qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                (width * 4) as i32,
                wl_shm::Format::Xrgb8888,
                &release_qh,
                slot,
            );
            pool.destroy();
            Ok(ShmSlot { buffer, mapping })
        };
        let slots = [make_slot(0)?, make_slot(1)?];

        // Input routing for this surface.
        let registry = core.borrow().state.input.registry.clone();
        registry.borrow_mut().insert(
            wl_surface.id(),
            Rc::new(SurfaceRegistration {
                monitor_index,
                sink,
                scale: output.scale.max(1),
                rect: monitor_rect,
                keyboard_focus,
            }),
        );

        let surface_id = wl_surface.id();
        Ok(Self {
            conn: conn.clone(),
            wl_surface,
            layer_surface,
            width,
            height,
            slots,
            attached: None,
            slot_state: SlotState {
                free: [true; SLOT_COUNT],
            },
            release_queue,
            closed,
            registry,
            handles,
            surface_id,
        })
    }

    /// Process buffer releases without blocking, feeding the release queue
    /// from the socket FIRST: during the synchronous freeze/unfreeze fade
    /// the main loop is blocked inside the controller, so compositor
    /// releases would never be read off the connection and every fade step
    /// after the second would starve for a free slot (both attached). The
    /// read is a non-blocking attempt mirroring the shell's
    /// `read_and_dispatch` (`WouldBlock` = no new bytes; other errors defer
    /// to the main loop's authoritative handling there). Events addressed to
    /// the main queue stay buffered for it, exactly as with the capture
    /// pump's socket reads.
    fn pump_releases(&mut self) {
        if let Some(guard) = self.release_queue.prepare_read() {
            let _ = guard.read();
        }
        // Events addressed to this queue's proxies are now read off the
        // connection; dispatch_pending only runs their handlers, so it never
        // blocks.
        let _ = self.release_queue.dispatch_pending(&mut self.slot_state);
    }}

impl OverlaySurface for LayerOverlaySurface {
    /// Re-composite from `frame` (must match the monitor rect exactly).
    /// `dirty: Some(rect)` copies only that monitor-local region (the
    /// per-mouse-move fast path); `None` presents the full frame.
    ///
    /// Never blocks: with no free slot the frame is DROPPED (the controller
    /// gates calls through [`can_present`](Self::can_present) and repaints
    /// with the latest frame when a slot frees, so only the freshest content
    /// ever reaches the screen).
    fn present(&mut self, frame: &DibBuffer, dirty: Option<Rect>) -> Result<()> {
        if self.closed.get() {
            bail!("overlay present: the compositor closed the layer surface");
        }
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "overlay present: frame {}x{} does not match the surface {}x{}",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }
        let region = match dirty {
            None => None, // full frame
            Some(d) => match clip_dirty(d, self.width, self.height) {
                Some(r) => Some(r),
                None => return Ok(()), // fully out of frame: nothing to do
            },
        };

        self.pump_releases();
        let Some(slot) = pick_slot(&self.slot_state.free, self.attached) else {
            return Ok(()); // both slots busy: drop this frame, never block
        };

        copy_frame(self.slots[slot].mapping.as_mut_slice(), frame, region);
        self.wl_surface.attach(Some(&self.slots[slot].buffer), 0, 0);
        match region {
            Some(r) => self
                .wl_surface
                .damage_buffer(r.x, r.y, r.width as i32, r.height as i32),
            None => self
                .wl_surface
                .damage_buffer(0, 0, self.width as i32, self.height as i32),
        }
        self.wl_surface.commit();
        self.conn.flush().context("flushing the present")?;

        self.slot_state.free[slot] = false;
        self.attached = Some(slot);
        Ok(())
    }

    /// `true` when a buffer slot is free for the next [`present`]. Releases
    /// that already arrived are accounted (non-blocking pump).
    fn can_present(&mut self) -> bool {
        self.pump_releases();
        pick_slot(&self.slot_state.free, self.attached).is_some()
    }
}

impl Drop for LayerOverlaySurface {
    fn drop(&mut self) {
        self.registry.borrow_mut().remove(&self.surface_id);
        self.handles
            .borrow_mut()
            .retain(|h| h.surface_id != self.surface_id);
        self.layer_surface.destroy();
        self.wl_surface.destroy();
        // wl_buffer proxies, shm mappings, and the release queue tear down
        // with their own drops.
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested headless)
// ---------------------------------------------------------------------------

/// Pick a writable slot: any free slot, preferring the one NOT currently
/// attached (the just-presented slot stays busy until the compositor's
/// release). `None` when every slot is busy.
fn pick_slot(free: &[bool; SLOT_COUNT], attached: Option<usize>) -> Option<usize> {
    (0..SLOT_COUNT)
        .find(|&i| free[i] && Some(i) != attached)
        .or_else(|| (0..SLOT_COUNT).find(|&i| free[i]))
}

/// Clip `dirty` (monitor-local, may be negative/oversized) to a
/// `width`×`height` frame; `None` when nothing overlaps. Pure; mirrors the
/// Windows overlay's `clip_to_frame`.
fn clip_dirty(dirty: Rect, width: u32, height: u32) -> Option<Rect> {
    // i64 math: dirty.x + dirty.width cannot overflow regardless of inputs.
    let x0 = (dirty.x as i64).max(0);
    let y0 = (dirty.y as i64).max(0);
    let x1 = (dirty.x as i64 + dirty.width as i64).min(width as i64);
    let y1 = (dirty.y as i64 + dirty.height as i64).min(height as i64);
    if x1 > x0 && y1 > y0 {
        Some(Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32))
    } else {
        None
    }
}

/// Copy frame rows from `src` (a [`DibBuffer`]) into the slot mapping. Both
/// buffers share the same tightly-packed BGRA layout (`width * 4` bytes per
/// row, top-down). `region: None` copies the whole frame with one memcpy;
/// `Some(r)` copies only that pre-clipped region row by row — the
/// O(dirty area) fast path. Pure; unit-tested headless.
fn copy_frame(dst: &mut [u8], src: &DibBuffer, region: Option<Rect>) {
    let stride = src.stride as usize;
    match region {
        None => dst[..src.pixels.len()].copy_from_slice(&src.pixels),
        Some(r) => {
            let row_bytes = r.width as usize * 4;
            let col_off = r.x as usize * 4;
            for y in r.y..r.y + r.height as i32 {
                let off = y as usize * stride + col_off;
                dst[off..off + row_bytes]
                    .copy_from_slice(&src.pixels[off..off + row_bytes]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch: layer-surface configure/closed on the main queue; buffer releases
// on each surface's own queue.
// ---------------------------------------------------------------------------

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for ShellState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                // The ack must precede the next commit; the first configure
                // unblocks the factory's pump (the buffer size comes from the
                // monitor rect, never from the configure's suggested size).
                proxy.ack_configure(serial);
                let handles = state.layer_handles.borrow();
                if let Some(h) = handles
                    .iter()
                    .find(|h| h.layer_surface.id() == proxy.id())
                    && h.configured.get().is_none()
                {
                    h.configured.set(Some((width, height)));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let handles = state.layer_handles.borrow();
                if let Some(h) = handles.iter().find(|h| h.layer_surface.id() == proxy.id()) {
                    h.closed.set(true);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // enter/leave (per-output) and preferred_buffer_scale are irrelevant:
        // each surface is bound to one output and drives its scale explicitly.
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for SlotState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for SlotState {
    fn event(
        state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        slot: &usize,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && *slot < SLOT_COUNT
        {
            state.free[*slot] = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: dirty-rect clipping, row copying, slot selection against
// plain memory buffers. No Wayland objects are ever created.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::DibBuffer;

    // ---- pick_slot ----

    #[test]
    fn pick_slot_prefers_the_unattached_free_slot() {
        assert_eq!(pick_slot(&[true, true], Some(0)), Some(1));
        assert_eq!(pick_slot(&[true, true], Some(1)), Some(0));
        assert_eq!(pick_slot(&[true, true], None), Some(0));
    }

    #[test]
    fn pick_slot_reuses_attached_when_it_is_the_only_free_one() {
        assert_eq!(pick_slot(&[true, false], Some(0)), Some(0));
        assert_eq!(pick_slot(&[false, true], Some(1)), Some(1));
    }

    #[test]
    fn pick_slot_is_none_when_all_busy() {
        assert_eq!(pick_slot(&[false, false], None), None);
        assert_eq!(pick_slot(&[false, false], Some(0)), None);
    }

    // ---- clip_dirty ----

    #[test]
    fn clip_dirty_keeps_fully_inside_rect() {
        let r = Rect::new(2, 3, 10, 20);
        assert_eq!(clip_dirty(r, 100, 100), Some(r));
    }

    #[test]
    fn clip_dirty_clips_edges() {
        assert_eq!(
            clip_dirty(Rect::new(90, 90, 50, 50), 100, 100),
            Some(Rect::new(90, 90, 10, 10))
        );
        assert_eq!(
            clip_dirty(Rect::new(-5, -8, 10, 10), 100, 100),
            Some(Rect::new(0, 0, 5, 2))
        );
    }

    #[test]
    fn clip_dirty_rejects_non_overlapping() {
        assert_eq!(clip_dirty(Rect::new(100, 0, 10, 10), 100, 100), None);
        assert_eq!(clip_dirty(Rect::new(-20, 0, 10, 10), 100, 100), None);
        assert_eq!(clip_dirty(Rect::new(50, 0, 0, 10), 100, 100), None);
    }

    // ---- copy_frame ----

    /// 4×3 BGRA frame: byte value = linear offset, so every byte is unique.
    fn test_frame() -> DibBuffer {
        let (w, h) = (4u32, 3u32);
        let stride = w * 4;
        let pixels: Vec<u8> = (0..stride * h).map(|i| i as u8).collect();
        DibBuffer {
            width: w,
            height: h,
            stride,
            pixels,
        }
    }

    #[test]
    fn copy_frame_full_frame_copies_everything() {
        let src = test_frame();
        let mut dst = vec![0u8; src.pixels.len()];
        copy_frame(&mut dst, &src, None);
        assert_eq!(dst, src.pixels);
    }

    #[test]
    fn copy_frame_dirty_copies_only_the_rect() {
        let src = test_frame();
        let mut dst = vec![0u8; src.pixels.len()];
        copy_frame(&mut dst, &src, Some(Rect::new(1, 1, 2, 2)));
        let stride = src.stride as usize;
        for y in 0..3usize {
            for x in 0..4usize {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                for k in 0..4 {
                    let off = y * stride + x * 4 + k;
                    assert_eq!(
                        dst[off],
                        if inside { src.pixels[off] } else { 0 },
                        "byte at pixel ({x}, {y}) channel {k}"
                    );
                }
            }
        }
    }

    #[test]
    fn copy_frame_single_pixel() {
        let src = test_frame();
        let mut dst = vec![0u8; src.pixels.len()];
        copy_frame(&mut dst, &src, Some(Rect::new(3, 2, 1, 1)));
        let off = 2 * src.stride as usize + 3 * 4;
        assert_eq!(&dst[off..off + 4], &src.pixels[off..off + 4]);
        assert_eq!(dst.iter().filter(|&&b| b != 0).count(), 4);
    }
}
