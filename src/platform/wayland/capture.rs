//! Screen capture via `zwlr_screencopy_manager_v1`: one synchronous
//! `capture_output` per output into a memfd-backed wl_shm buffer, rows copied
//! out into a [`DibBuffer`].
//!
//! # Contracts
//!
//! - **One-shot, synchronous**: [`WaylandCapturer::capture_all`] requests a
//!   frame for every output up front, then pumps a DEDICATED
//!   [`wayland_client::EventQueue`] (created for this capturer, never the
//!   shell's main queue) until every frame resolves to `ready` or `failed`.
//!   Events for main-queue objects arriving during the pump stay buffered in
//!   the main queue (wayland-client routes per queue) — no reentrancy into
//!   the overlay controller or the input sinks.
//! - **Format**: the first advertised `xrgb8888` or `argb8888` shm format
//!   wins. Both are 4-byte little-endian BGRA in memory, matching the
//!   [`DibBuffer`] contract byte-for-byte; alpha is forced to 255 regardless
//!   (captures are opaque, and the X byte of xrgb8888 is undefined).
//! - **Rows**: the frame's stride may exceed `width * 4`; rows are copied
//!   one at a time. A `y_invert` flag flips the row order back to top-down.
//! - **Cursor**: `overlay_cursor = 0`, always — the freeze must show the
//!   desktop exactly as it was, without the pointer.
//! - **MonitorInfo**: rect position/scale come from the shell's output
//!   snapshot; the rect SIZE is overridden with the frame's real buffer size
//!   (the physical-pixel truth — equal to `logical × scale` for the integer
//!   scales this backend supports), preserving the [`Capturer`] contract
//!   "buffer size equals rect size".
//!
//! The translatable pieces — format selection and the strided row copy — are
//! pure and unit-tested headless.

use crate::capture::{Capturer, DibBuffer, MonitorInfo};
use crate::platform::wayland::shell::ShmMapping;
use anyhow::{Context, Result, anyhow, bail};
use std::cell::RefCell;
use std::os::unix::io::AsFd;
use wayland_client::protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

/// One shm buffer offer from the frame's `buffer` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShmOffer {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

/// Index of the first offer in a [`DibBuffer`]-compatible format (xrgb8888 or
/// argb8888), in advertisement order. Pure; unit-tested.
fn choose_shm_format(offers: &[ShmOffer]) -> Option<usize> {
    offers.iter().position(|o| {
        matches!(
            o.format,
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888
        )
    })
}

/// Copy rows out of the mapped frame buffer into a tightly packed, top-down
/// [`DibBuffer`], forcing alpha to 255 (see module docs). Pure; unit-tested.
///
/// Panics (debug builds) if `src` is shorter than `src_stride * height` —
/// the caller sizes the mapping from the advertised frame parameters, so a
/// short buffer would be a bookkeeping bug, not runtime input.
fn rows_to_dib(src: &[u8], src_stride: u32, width: u32, height: u32, y_invert: bool) -> DibBuffer {
    let stride = width * 4;
    debug_assert!(
        src.len() >= src_stride as usize * height as usize,
        "frame mapping too small: {} bytes for stride {src_stride} x {height}",
        src.len()
    );
    let mut pixels = vec![0u8; stride as usize * height as usize];
    for y in 0..height {
        let src_row = if y_invert { height - 1 - y } else { y } as usize;
        let src_off = src_row * src_stride as usize;
        let dst_off = y as usize * stride as usize;
        pixels[dst_off..dst_off + stride as usize]
            .copy_from_slice(&src[src_off..src_off + stride as usize]);
    }
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 255;
    }
    DibBuffer {
        width,
        height,
        stride,
        pixels,
    }
}

/// The shm buffer + mapping a frame is being copied into (between the `copy`
/// request and `ready`).
struct PendingCopy {
    buffer: wl_buffer::WlBuffer,
    mapping: ShmMapping,
    width: u32,
    height: u32,
    stride: u32,
}

/// Per-frame progress, indexed by output index (the frame's udata).
#[derive(Default)]
struct FrameProgress {
    offers: Vec<ShmOffer>,
    y_invert: bool,
    pending: Option<PendingCopy>,
    outcome: Option<Result<DibBuffer, String>>,
}

/// Dispatch state of the capturer's private event queue.
struct CaptureState {
    shm: wl_shm::WlShm,
    frames: Vec<FrameProgress>,
    /// Frames not yet resolved to `ready` or `failed`.
    remaining: usize,
}

impl CaptureState {
    fn frame(&mut self, index: usize) -> &mut FrameProgress {
        &mut self.frames[index]
    }

    /// Record a terminal outcome and destroy the frame (protocol: the client
    /// destroys the frame after `ready`/`failed`).
    fn finish(
        &mut self,
        index: usize,
        proxy: &ZwlrScreencopyFrameV1,
        outcome: Result<DibBuffer, String>,
    ) {
        if self.frame(index).outcome.is_some() {
            return;
        }
        self.frame(index).outcome = Some(outcome);
        self.remaining -= 1;
        proxy.destroy();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, usize> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        index: &usize,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.frame(*index).offers.push(ShmOffer {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            Event::BufferDone => {
                let offer = {
                    let frame = state.frame(*index);
                    choose_shm_format(&frame.offers).map(|i| frame.offers[i])
                };
                let Some(offer) = offer else {
                    state.finish(
                        *index,
                        proxy,
                        Err("the compositor offered no xrgb8888/argb8888 shm format".into()),
                    );
                    return;
                };
                match create_copy_buffer(state, qhandle, &offer) {
                    Ok(pending) => {
                        proxy.copy(&pending.buffer);
                        state.frame(*index).pending = Some(pending);
                    }
                    Err(e) => {
                        state.finish(*index, proxy, Err(format!("allocating the frame buffer: {e:#}")));
                    }
                }
            }
            Event::Flags { flags } => {
                state.frame(*index).y_invert = matches!(
                    flags,
                    WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
                );
            }
            Event::Ready { .. } => {
                let frame = state.frame(*index);
                let outcome = match frame.pending.take() {
                    Some(pending) => {
                        let dib = rows_to_dib(
                            pending.mapping.as_slice(),
                            pending.stride,
                            pending.width,
                            pending.height,
                            frame.y_invert,
                        );
                        pending.buffer.destroy();
                        Ok(dib)
                    }
                    None => Err("the frame reported ready before a buffer was accepted".into()),
                };
                state.finish(*index, proxy, outcome);
            }
            Event::Failed => {
                state.finish(
                    *index,
                    proxy,
                    Err("the compositor reported a frame copy failure".into()),
                );
            }
            // LinuxDmabuf (dmabuf path unused — shm only), Damage (only sent
            // for copy_with_damage, which we never request).
            _ => {}
        }
    }
}

/// Allocate the memfd + wl_shm pool + buffer for an accepted frame offer.
fn create_copy_buffer(
    state: &CaptureState,
    qhandle: &QueueHandle<CaptureState>,
    offer: &ShmOffer,
) -> Result<PendingCopy> {
    let len = offer.stride as usize * offer.height as usize;
    let (fd, mapping) = crate::platform::wayland::shell::map_shm(len)?;
    let pool = state
        .shm
        .create_pool(fd.as_fd(), len as i32, qhandle, ());
    let buffer = pool.create_buffer(
        0,
        offer.width as i32,
        offer.height as i32,
        offer.stride as i32,
        offer.format,
        qhandle,
        (),
    );
    pool.destroy();
    Ok(PendingCopy {
        buffer,
        mapping,
        width: offer.width,
        height: offer.height,
        stride: offer.stride,
    })
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for CaptureState {
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

impl Dispatch<wl_buffer::WlBuffer, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Release is expected after ready; the buffer is destroyed there.
    }
}

/// [`Capturer`] over wlr-screencopy. Cheap to construct; owns a private
/// event queue so captures are fully synchronous and never touch the shell's
/// main queue.
pub struct WaylandCapturer {
    manager: ZwlrScreencopyManagerV1,
    shm: wl_shm::WlShm,
    /// Outputs to capture, in monitor order, with their snapshot info.
    outputs: Vec<(wl_output::WlOutput, MonitorInfo)>,
    queue: RefCell<EventQueue<CaptureState>>,
    qh: QueueHandle<CaptureState>,
}

impl WaylandCapturer {
    pub(crate) fn new(
        conn: &Connection,
        manager: ZwlrScreencopyManagerV1,
        shm: wl_shm::WlShm,
        outputs: Vec<(wl_output::WlOutput, MonitorInfo)>,
    ) -> Self {
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        Self {
            manager,
            shm,
            outputs,
            queue: RefCell::new(queue),
            qh,
        }
    }
}

impl Capturer for WaylandCapturer {
    /// Capture every output once, in the shell's snapshot order (see module
    /// docs for the semantics).
    fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
        if self.outputs.is_empty() {
            bail!("no Wayland outputs to capture");
        }
        let mut state = CaptureState {
            shm: self.shm.clone(),
            frames: (0..self.outputs.len()).map(|_| FrameProgress::default()).collect(),
            remaining: self.outputs.len(),
        };
        for (index, (output, _)) in self.outputs.iter().enumerate() {
            self.manager.capture_output(0, output, &self.qh, index);
        }
        let mut queue = self.queue.borrow_mut();
        while state.remaining > 0 {
            queue
                .blocking_dispatch(&mut state)
                .context("dispatching screencopy events")?;
        }
        drop(queue);

        let mut out = Vec::with_capacity(self.outputs.len());
        for ((_, info), frame) in self.outputs.iter().zip(state.frames) {
            let dib = frame
                .outcome
                .unwrap_or_else(|| Err("the frame never resolved".into()))
                .map_err(|m| anyhow!("capturing {}: {m}", info.device_name))?;
            let mut info = info.clone();
            // The frame's real buffer size is the physical-pixel truth.
            info.rect.width = dib.width;
            info.rect.height = dib.height;
            out.push((info, dib));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: format selection and strided row copies over plain
// buffers. No Wayland connection, no compositor, no shm.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(format: wl_shm::Format) -> ShmOffer {
        ShmOffer {
            format,
            width: 2,
            height: 2,
            stride: 8,
        }
    }

    // ---- choose_shm_format ----

    #[test]
    fn chooses_the_first_compatible_offer() {
        let offers = [
            offer(wl_shm::Format::Nv12),
            offer(wl_shm::Format::Argb8888),
            offer(wl_shm::Format::Xrgb8888),
        ];
        assert_eq!(choose_shm_format(&offers), Some(1));
    }

    #[test]
    fn chooses_xrgb8888_when_only_it_is_offered() {
        let offers = [offer(wl_shm::Format::Nv12), offer(wl_shm::Format::Xrgb8888)];
        assert_eq!(choose_shm_format(&offers), Some(1));
    }

    #[test]
    fn no_compatible_offer_is_none() {
        let offers = [offer(wl_shm::Format::Nv12), offer(wl_shm::Format::Rgb565)];
        assert_eq!(choose_shm_format(&offers), None);
        assert_eq!(choose_shm_format(&[]), None);
    }

    // ---- rows_to_dib ----

    /// 3×2 frame with unique bytes per pixel channel: pixel (x, y) =
    /// [10y+x, 100+10y+x, 200+10y+x, 0] (alpha left garbage on purpose).
    fn frame_bytes(stride: u32) -> Vec<u8> {
        let mut v = vec![0u8; stride as usize * 2];
        for y in 0..2u32 {
            for x in 0..3u32 {
                let off = (y * stride + x * 4) as usize;
                v[off] = (10 * y + x) as u8;
                v[off + 1] = (100 + 10 * y + x) as u8;
                v[off + 2] = (200 + 10 * y + x) as u8;
                v[off + 3] = 0;
            }
        }
        v
    }

    #[test]
    fn tight_stride_copies_verbatim_and_forces_alpha() {
        let src = frame_bytes(12);
        let dib = rows_to_dib(&src, 12, 3, 2, false);
        assert_eq!((dib.width, dib.height, dib.stride), (3, 2, 12));
        assert_eq!(dib.pixel(0, 0), Some([0, 100, 200, 255]));
        assert_eq!(dib.pixel(2, 1), Some([12, 112, 212, 255]));
    }

    #[test]
    fn padded_stride_copies_rows_without_padding() {
        let stride = 16; // 4 bytes of padding per row
        let src = frame_bytes(stride);
        let dib = rows_to_dib(&src, stride, 3, 2, false);
        assert_eq!(dib.stride, 12);
        assert_eq!(dib.pixel(0, 1), Some([10, 110, 210, 255]));
        assert_eq!(dib.pixel(2, 0), Some([2, 102, 202, 255]));
        // Padding bytes must not leak into the image.
        assert!(dib.pixels.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn y_invert_flips_row_order() {
        let src = frame_bytes(12);
        let dib = rows_to_dib(&src, 12, 3, 2, true);
        // Source row 1 becomes output row 0.
        assert_eq!(dib.pixel(0, 0), Some([10, 110, 210, 255]));
        assert_eq!(dib.pixel(0, 1), Some([0, 100, 200, 255]));
    }
}
