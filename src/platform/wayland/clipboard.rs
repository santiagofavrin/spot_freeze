//! PNG clipboard via `wl_data_device`: the copy action encodes the frame as
//! PNG, creates a data source offering `image/png` only, and claims the seat
//! selection with the latest input serial while the overlay holds keyboard
//! focus.
//!
//! # Contracts
//!
//! - **Serial**: `wl_data_device.set_selection` requires the serial of an
//!   input event delivered to THIS client; the copy is always triggered by a
//!   key press, so the input module's latest-serial cell is the right one. A
//!   serial of 0 (no input ever received) fails the copy instead of risking
//!   a protocol error.
//! - **Lifetime**: the source must outlive the overlay — Wayland selections
//!   are served by their owner, not stored by the compositor. The source +
//!   PNG bytes live in the app-held [`WaylandServices`] (which lives as long
//!   as the app, tray included), are REPLACED on the next copy (the old
//!   source is destroyed), and dropped on exit. A `cancelled` event clears
//!   the source.
//! - **Serving**: `send` requests write the stored PNG bytes to the
//!   compositor-provided fd, synchronously — the standard wl_data_device
//!   flow (clipboard readers on wlroots drain promptly).
//! - **Cursor**: Wayland has no global cursor query, so
//!   [`PlatformServices::cursor_position_virtual`] returns the position
//!   tracked by the input module while the pointer is over one of our
//!   overlay surfaces (always the case while frozen, where copies happen),
//!   and `None` otherwise — which the controller tolerates.
//!
//! Headless tests cover the serial guard; the PNG encoding itself is tested
//! in [`crate::capture::png`].

use crate::capture::png::encode_png;
use crate::capture::DibBuffer;
use crate::geometry::Point;
use crate::platform::PlatformServices;
use crate::platform::wayland::shell::ShellState;
use anyhow::{Context, Result, bail};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use wayland_client::backend::ObjectData;
use wayland_client::protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

/// The only MIME type the source offers (PNG only, per the platform contract).
pub(crate) const MIME_IMAGE_PNG: &str = "image/png";

/// Clipboard state shared between [`WaylandServices`] (which sets the
/// selection) and the main queue's `WlDataSource` dispatch (which serves it).
pub(crate) struct ClipboardShared {
    /// PNG bytes of the last copy; served on `send`.
    png: Option<Rc<[u8]>>,
    /// The live selection source; kept alive so the selection survives
    /// unfreeze. Replaced on the next copy, dropped on exit.
    source: Option<wl_data_source::WlDataSource>,
}

impl ClipboardShared {
    pub(crate) fn new() -> Self {
        Self {
            png: None,
            source: None,
        }
    }
}

/// The copy must carry the serial of a real input event (the triggering key
/// press); 0 means no input ever arrived and the compositor would reject the
/// claim. Pure; unit-tested.
fn require_serial(serial: u32) -> Result<u32> {
    if serial == 0 {
        bail!("no input event has been received yet; cannot claim the clipboard selection");
    }
    Ok(serial)
}

/// [`PlatformServices`] for the Wayland shell (see module docs).
pub struct WaylandServices {
    conn: Connection,
    shared: Rc<RefCell<ClipboardShared>>,
    manager: wl_data_device_manager::WlDataDeviceManager,
    device: wl_data_device::WlDataDevice,
    qh: QueueHandle<ShellState>,
    serial: Rc<Cell<u32>>,
    cursor: Rc<Cell<Option<Point>>>,
}

impl WaylandServices {
    pub(crate) fn new(
        conn: &Connection,
        manager: wl_data_device_manager::WlDataDeviceManager,
        device: wl_data_device::WlDataDevice,
        qh: QueueHandle<ShellState>,
        shared: Rc<RefCell<ClipboardShared>>,
        serial: Rc<Cell<u32>>,
        cursor: Rc<Cell<Option<Point>>>,
    ) -> Self {
        Self {
            conn: conn.clone(),
            shared,
            manager,
            device,
            qh,
            serial,
            cursor,
        }
    }
}

impl PlatformServices for WaylandServices {
    /// The tracked pointer position in virtual-screen physical pixels while
    /// the pointer is over an overlay surface; `None` otherwise (Wayland has
    /// no global cursor query — the controller tolerates `None`).
    fn cursor_position_virtual(&self) -> Option<Point> {
        self.cursor.get()
    }

    /// Encode `frame` as PNG and claim the seat selection with a fresh
    /// `image/png`-only source (replacing any previous one).
    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
        let png = encode_png(frame).context("encoding the snip as PNG")?;
        let serial = require_serial(self.serial.get())?;

        let source = self.manager.create_data_source(&self.qh, ());
        source.offer(MIME_IMAGE_PNG.to_string());
        {
            let mut shared = self.shared.borrow_mut();
            shared.png = Some(Rc::from(png.into_boxed_slice()));
            // Replacing drops (and thereby destroys) the previous source.
            shared.source = Some(source.clone());
        }
        self.device.set_selection(Some(&source), serial);
        // Flush now: the selection must be claimed before the overlay closes
        // and other clients are asked to paste.
        let _ = self.conn.flush();
        Ok(())
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for ShellState {
    fn event(
        state: &mut Self,
        proxy: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if mime_type != MIME_IMAGE_PNG {
                    return;
                }
                // Clone the Rc out so the shared cell is not borrowed across
                // the (potentially blocking) write.
                let png = state.clipboard.borrow().png.clone();
                let Some(png) = png else { return };
                let mut file: std::fs::File = fd.into();
                if let Err(e) = file.write_all(&png) {
                    eprintln!("spotfreeze: failed to serve the clipboard PNG: {e}");
                }
            }
            wl_data_source::Event::Cancelled => {
                let mut shared = state.clipboard.borrow_mut();
                if let Some(source) = &shared.source
                    && source.id() == proxy.id()
                {
                    // Dropping the proxy destroys the cancelled source.
                    shared.source = None;
                }
            }
            // Target / DndDropPerformed / DndFinished / Action: drag-and-drop
            // is never started (we only set the selection).
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_data_device::WlDataDevice,
        _event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Selection offers and drag-and-drop events are irrelevant: we only
        // ever OWN the selection, never read it.
    }

    fn event_created_child(
        _opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> Arc<dyn ObjectData> {
        // data_offer is the only child-creating event.
        qhandle.make_data::<wl_data_offer::WlDataOffer, ()>(())
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_data_offer::WlDataOffer,
        _event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: the serial guard is the only pure logic (serving bytes
// needs a live compositor fd).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_zero_is_rejected() {
        let err = require_serial(0).unwrap_err();
        assert!(err.to_string().contains("no input event"), "{err}");
    }

    #[test]
    fn nonzero_serial_passes_through() {
        assert_eq!(require_serial(1).unwrap(), 1);
        assert_eq!(require_serial(u32::MAX).unwrap(), u32::MAX);
    }
}
