//! Wayland connection, global registry, output tracking, single-instance
//! lock, and the event-loop glue every other Wayland module hangs off.
//!
//! # Contracts
//!
//! - **One connection, one main queue**: [`Shell`] owns the
//!   [`wayland_client::Connection`] and the main
//!   [`wayland_client::EventQueue`] driving [`ShellState`]. All overlay
//!   surfaces, the seat devices, the clipboard source, and the registry live
//!   on this queue. The screencopy capturer and each surface's buffer-release
//!   tracking use their OWN queues on the same connection (see
//!   [`crate::platform::wayland::capture`]) so synchronous pumps never
//!   re-enter the main queue's dispatch.
//! - **Event-loop integration**: the app (not this module) owns the `calloop`
//!   loop. It polls the connection's `poll_fd` (dup'd) as a level-triggered
//!   source and calls [`Shell::read_and_dispatch`] when it fires;
//!   [`Shell::dispatch_pending`] runs before every poll so events buffered by
//!   OTHER queues' socket reads (the capture pump) are never stranded.
//! - **Coordinate spaces**: output records carry the LOGICAL position/size
//!   (xdg-output when available, wl_output geometry + mode ÷ scale otherwise)
//!   and the integer output scale. [`OutputRecord::monitor_info`] converts to
//!   the crate-wide contract: rect in top-left-origin VIRTUAL PHYSICAL pixels
//!   (logical × scale), `dpi = 96 × scale`, `device_name` = xdg-output name.
//!   `is_primary` is always `false` — Wayland has no primary-monitor concept,
//!   and nothing in the portable core reads it.
//! - **Output snapshot**: outputs are enumerated ONCE at connect; the capturer
//!   and the surface factory both iterate the same snapshot in the same order
//!   (the factory maps monitor index → snapshot entry). Monitor hotplug or
//!   scale changes require an app restart — a documented v1 limitation.
//! - **Integer scales only**: fractional-scale setups (`wp_fractional_scale`)
//!   are not protocolled; the wl_output integer scale is used as-is.
//!
//! # Single instance
//!
//! [`acquire_instance_lock`] takes a non-blocking exclusive `flock` on
//! `$XDG_RUNTIME_DIR/spotfreeze.lock`. A second instance gets
//! `WOULDBLOCK` and exits `Ok(())` silently, before touching the desktop —
//! the same contract as the Windows single-instance mutex. A missing
//! `$XDG_RUNTIME_DIR` is a hard error with a clear message.

use crate::capture::MonitorInfo;
use crate::geometry::Rect;
use crate::platform::wayland::clipboard::ClipboardShared;
use crate::platform::wayland::input::InputState;
use crate::platform::wayland::surface::LayerHandle;
use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{FlockOperation, flock};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_output, wl_registry, wl_seat, wl_shm,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

/// Lock file name inside `$XDG_RUNTIME_DIR`.
const LOCK_FILE_NAME: &str = "spotfreeze.lock";

/// Highest wl_compositor version we bind (v3 added `set_buffer_scale`).
const COMPOSITOR_VERSION: u32 = 4;
/// Highest wl_seat version we bind (v8 added `axis_value120` on wl_pointer).
const SEAT_VERSION: u32 = 8;
/// Highest wl_output version we bind (v4 added `name`).
const OUTPUT_VERSION: u32 = 4;
/// Highest zwlr_layer_shell_v1 version we bind (v4 added the `on_demand`
/// keyboard-interactivity fallback).
const LAYER_SHELL_VERSION: u32 = 4;
/// Highest zwlr_screencopy_manager_v1 version we bind.
const SCREENCOPY_VERSION: u32 = 3;
/// Highest wl_data_device_manager version we bind.
const DATA_DEVICE_MANAGER_VERSION: u32 = 3;
/// Highest zxdg_output_manager_v1 version we bind.
const XDG_OUTPUT_VERSION: u32 = 3;

/// Globals bound once at connect and shared by every module.
pub(crate) struct Globals {
    pub compositor: wl_compositor::WlCompositor,
    pub shm: wl_shm::WlShm,
    /// Ownership only: dropping a wl_seat (v5+) sends `release` and kills
    /// input. Devices are created from it by the capabilities dispatch.
    #[allow(dead_code)]
    pub seat: wl_seat::WlSeat,
    pub layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    pub screencopy: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    pub data_device_manager: wl_data_device_manager::WlDataDeviceManager,
    pub data_device: wl_data_device::WlDataDevice,
}

/// Per-output event accumulator: wl_output and zxdg_output_v1 events land
/// here (keyed by the output's wl_registry global name) until
/// [`resolve_geometry`] folds them into a record.
#[derive(Clone, Debug, Default)]
pub(crate) struct OutputDraft {
    /// Position from wl_output.geometry (compositor space, logical).
    pub geometry_pos: (i32, i32),
    /// Current mode's pixel size; the physical-pixel truth.
    pub mode_size: Option<(u32, u32)>,
    /// wl_output.scale factor; 0 = not yet received (treated as 1).
    pub scale: u32,
    /// wl_output.name (v4).
    pub wl_name: Option<String>,
    /// zxdg_output_v1.logical_position.
    pub xdg_pos: Option<(i32, i32)>,
    /// zxdg_output_v1.logical_size.
    pub xdg_size: Option<(u32, u32)>,
    /// zxdg_output_v1.name (e.g. "DP-1").
    pub xdg_name: Option<String>,
}

/// Fold an [`OutputDraft`] into `(logical_pos, logical_size, scale)`.
/// xdg-output values win; without them the logical size falls back to
/// `mode ÷ scale` (exact for integer scaling — the only supported case).
/// Pure; unit-tested headless.
pub(crate) fn resolve_geometry(draft: &OutputDraft) -> ((i32, i32), (u32, u32), u32) {
    let scale = draft.scale.max(1);
    let logical_size = draft.xdg_size.unwrap_or_else(|| match draft.mode_size {
        Some((w, h)) => (w / scale, h / scale),
        None => (0, 0),
    });
    (draft.xdg_pos.unwrap_or(draft.geometry_pos), logical_size, scale)
}

/// One Wayland output, snapshotted at connect time.
#[derive(Clone)]
pub struct OutputRecord {
    /// The bound wl_output global (screencopy source, layer-surface target).
    pub(crate) output: wl_output::WlOutput,
    /// wl_registry global name (stable identity).
    #[allow(dead_code)]
    pub(crate) global_name: u32,
    /// xdg-output name (fallback: wl_output.name, then `output-<name>`).
    pub(crate) name: String,
    /// Logical position in compositor space.
    pub(crate) logical_pos: (i32, i32),
    /// Logical size.
    pub(crate) logical_size: (u32, u32),
    /// Integer output scale (≥ 1).
    pub(crate) scale: u32,
    /// Current mode size in physical pixels.
    #[allow(dead_code)]
    pub(crate) mode_size: (u32, u32),
}

impl OutputRecord {
    /// Monitor description in the crate-wide contract: rect in top-left-origin
    /// VIRTUAL PHYSICAL pixels (`logical × scale`), `dpi = 96 × scale`,
    /// `is_primary = false` (no such concept on Wayland).
    pub fn monitor_info(&self) -> MonitorInfo {
        let scale = self.scale.max(1);
        MonitorInfo {
            rect: physical_rect(self.logical_pos, self.logical_size, scale),
            dpi_x: 96 * scale,
            dpi_y: 96 * scale,
            is_primary: false,
            device_name: self.name.clone(),
        }
    }
}

/// The monitor rect in VIRTUAL PHYSICAL pixels: logical geometry × integer
/// scale (positions included — a scaled secondary monitor left of the primary
/// keeps its negative origin). Pure; unit-tested headless.
pub(crate) fn physical_rect(logical_pos: (i32, i32), logical_size: (u32, u32), scale: u32) -> Rect {
    let scale = scale.max(1);
    Rect::new(
        logical_pos.0 * scale as i32,
        logical_pos.1 * scale as i32,
        logical_size.0 * scale,
        logical_size.1 * scale,
    )
}

/// Dispatch state of the main event queue.
pub struct ShellState {
    /// wl_output/xdg-output event accumulators keyed by registry global name.
    pub(crate) output_drafts: HashMap<u32, OutputDraft>,
    /// Pointer/keyboard routing, xkb state, serial and cursor tracking.
    pub(crate) input: InputState,
    /// Clipboard source bookkeeping shared with [`super::clipboard::WaylandServices`].
    pub(crate) clipboard: Rc<RefCell<ClipboardShared>>,
    /// Live layer surfaces (configure/closed routing + focus fallback).
    pub(crate) layer_handles: Rc<RefCell<Vec<LayerHandle>>>,
}

/// Main-queue state + the queue itself (one borrow unit for the factory's
/// configure pump and the focus-fallback roundtrip).
pub(crate) struct ShellCore {
    pub queue: EventQueue<ShellState>,
    pub state: ShellState,
}

/// The Wayland connection handle: bound globals, the output snapshot, and the
/// main event queue. `core` is `Rc`-shared so the surface factory — which
/// must be a `'static` closure like its Windows counterpart — can pump the
/// queue for the initial-configure handshake without borrowing the `Shell`.
pub struct Shell {
    conn: Connection,
    core: Rc<RefCell<ShellCore>>,
    globals: Rc<Globals>,
    qh: QueueHandle<ShellState>,
    outputs: Rc<Vec<OutputRecord>>,
}

impl Shell {
    /// Connect to the compositor, bind the required globals, enumerate outputs
    /// (with a roundtrip for wl_output + zxdg_output_v1 details), and bind the
    /// seat devices. Errors clearly when a required protocol is missing (this
    /// is a wlroots-only backend: layer shell + screencopy).
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("cannot connect to the Wayland display (WAYLAND_DISPLAY / XDG_RUNTIME_DIR)")?;
        let (glist, queue) = registry_queue_init::<ShellState>(&conn)
            .context("initializing the Wayland global registry")?;
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = glist
            .bind(&qh, 1..=COMPOSITOR_VERSION, ())
            .context("wl_compositor is not available")?;
        let shm: wl_shm::WlShm = glist
            .bind(&qh, 1..=1, ())
            .context("wl_shm is not available")?;
        let seat: wl_seat::WlSeat = glist
            .bind(&qh, 1..=SEAT_VERSION, ())
            .context("wl_seat is not available")?;
        let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = glist
            .bind(&qh, 1..=LAYER_SHELL_VERSION, ())
            .context(
                "zwlr_layer_shell_v1 is not available — SpotFreeze needs a wlroots-based \
                 compositor (Hyprland, Sway, …)",
            )?;
        let screencopy: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 = glist
            .bind(&qh, 1..=SCREENCOPY_VERSION, ())
            .context(
                "zwlr_screencopy_manager_v1 is not available — SpotFreeze needs a wlroots-based \
                 compositor (Hyprland, Sway, …)",
            )?;
        let data_device_manager: wl_data_device_manager::WlDataDeviceManager = glist
            .bind(&qh, 1..=DATA_DEVICE_MANAGER_VERSION, ())
            .context("wl_data_device_manager is not available")?;
        // Optional: output names/positions fall back to wl_output data.
        let xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> =
            glist.bind(&qh, 1..=XDG_OUTPUT_VERSION, ()).ok();

        let mut state = ShellState {
            output_drafts: HashMap::new(),
            input: InputState::new(),
            clipboard: Rc::new(RefCell::new(ClipboardShared::new())),
            layer_handles: Rc::new(RefCell::new(Vec::new())),
        };

        // Bind every advertised wl_output (udata = its registry global name).
        let mut bound: Vec<(u32, wl_output::WlOutput)> = Vec::new();
        for global in glist.contents().clone_list() {
            if global.interface != "wl_output" {
                continue;
            }
            state.output_drafts.entry(global.name).or_default();
            let output = glist.registry().bind::<wl_output::WlOutput, u32, ShellState>(
                global.name,
                global.version.min(OUTPUT_VERSION),
                &qh,
                global.name,
            );
            bound.push((global.name, output));
        }

        let mut queue = queue;
        // First roundtrip: wl_output geometry/mode/scale/name + seat caps.
        queue
            .roundtrip(&mut state)
            .context("querying Wayland outputs")?;

        // xdg-output details for every bound output (second roundtrip).
        if let Some(manager) = &xdg_output_manager {
            for &(name, ref output) in &bound {
                manager.get_xdg_output(output, &qh, name);
            }
            queue
                .roundtrip(&mut state)
                .context("querying xdg-output details")?;
        }

        // Resolve drafts into the snapshot (registry-name order = stable).
        let mut records: Vec<OutputRecord> = Vec::with_capacity(bound.len());
        for (name, output) in bound {
            let draft = state.output_drafts.get(&name).cloned().unwrap_or_default();
            let (logical_pos, logical_size, scale) = resolve_geometry(&draft);
            let mode_size = draft.mode_size.unwrap_or((0, 0));
            if logical_size.0 == 0 || logical_size.1 == 0 {
                bail!("Wayland output {name} reported no usable size");
            }
            records.push(OutputRecord {
                output,
                global_name: name,
                name: draft
                    .xdg_name
                    .or(draft.wl_name)
                    .unwrap_or_else(|| format!("output-{name}")),
                logical_pos,
                logical_size,
                scale,
                mode_size,
            });
        }
        if records.is_empty() {
            bail!("the compositor advertised no wl_output globals");
        }

        let data_device = data_device_manager.get_data_device(&seat, &qh, ());

        Ok(Self {
            conn,
            core: Rc::new(RefCell::new(ShellCore { queue, state })),
            globals: Rc::new(Globals {
                compositor,
                shm,
                seat,
                layer_shell,
                screencopy,
                data_device_manager,
                data_device,
            }),
            qh,
            outputs: Rc::new(records),
        })
    }

    /// The capturer over the output snapshot (monitor order matches
    /// [`create_surface_factory`](Self::create_surface_factory) exactly).
    pub fn make_capturer(&self) -> super::capture::WaylandCapturer {
        super::capture::WaylandCapturer::new(
            &self.conn,
            self.globals.screencopy.clone(),
            self.globals.shm.clone(),
            self.outputs
                .iter()
                .map(|o| (o.output.clone(), o.monitor_info()))
                .collect(),
        )
    }

    /// The platform services: PNG clipboard over the seat's data device, and
    /// the input-tracked cursor position.
    pub fn make_services(&self) -> super::clipboard::WaylandServices {
        let core = self.core.borrow();
        super::clipboard::WaylandServices::new(
            &self.conn,
            self.globals.data_device_manager.clone(),
            self.globals.data_device.clone(),
            self.qh.clone(),
            core.state.clipboard.clone(),
            core.state.input.serial.clone(),
            core.state.input.cursor_virtual.clone(),
        )
    }

    /// Install the app's frozen-mode key hook: every non-repeat `KeyDown`
    /// arrives as `(vk, modifiers)` (see [`super::input`]).
    pub fn set_key_listener(&self, listener: Rc<dyn Fn(u32, crate::hotkeys::gesture::Modifiers)>) {
        self.core.borrow_mut().state.input.key_listener = Some(listener);
    }

    /// The connection's pollable fd, dup'd for the calloop source (the
    /// `Backend` handle is temporary, so the borrowed fd cannot escape).
    pub fn poll_fd(&self) -> std::io::Result<OwnedFd> {
        self.conn.backend().poll_fd().try_clone_to_owned()
    }

    /// Flush outgoing requests; `WouldBlock` is tolerated (retried next turn).
    pub fn flush(&self) -> Result<()> {
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e).context("flushing the Wayland connection"),
        }
    }

    /// Dispatch events already read into the main queue (e.g. buffered by the
    /// capture pump's socket reads) without touching the socket.
    pub fn dispatch_pending(&self) -> Result<()> {
        let core = &mut *self.core.borrow_mut();
        core.queue
            .dispatch_pending(&mut core.state)
            .context("dispatching Wayland events")?;
        Ok(())
    }

    /// The pollable-fd callback: read the socket data that made the fd fire,
    /// then dispatch everything pending. `prepare_read` returning `None` means
    /// events are already buffered — drain and retry (bounded; single-threaded,
    /// so draining always converges).
    pub fn read_and_dispatch(&self) -> Result<()> {
        let core = &mut *self.core.borrow_mut();
        for _ in 0..2 {
            core.queue
                .dispatch_pending(&mut core.state)
                .context("dispatching Wayland events")?;
            if let Some(guard) = core.queue.prepare_read() {
                match guard.read() {
                    Ok(_) => break,
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e).context("reading the Wayland socket"),
                }
            }
        }
        core.queue
            .dispatch_pending(&mut core.state)
            .context("dispatching Wayland events")?;
        Ok(())
    }

    /// Synchronous roundtrip on the main queue (server has processed every
    /// request sent so far, and the replies were dispatched).
    pub(crate) fn roundtrip(&self) -> Result<()> {
        let core = &mut *self.core.borrow_mut();
        core.queue
            .roundtrip(&mut core.state)
            .context("Wayland roundtrip")?;
        Ok(())
    }

    /// After a freeze: give the compositor a roundtrip to grant keyboard
    /// focus; if no overlay got it, demote every layer surface to `on_demand`
    /// interactivity (click-to-focus) when the bound layer-shell version
    /// supports it, else warn. Keys always keep working through the portal
    /// freeze hotkey regardless.
    pub fn ensure_keyboard_focus(&self) {
        if let Err(e) = self.roundtrip() {
            eprintln!("spotfreeze: focus check roundtrip failed: {e:#}");
            return;
        }
        let handles = self.core.borrow().state.layer_handles.clone();
        let handles = handles.borrow();
        if handles.is_empty() || handles.iter().any(|h| h.keyboard_focus.get()) {
            return;
        }
        // OnDemand keyboard interactivity exists since layer-shell v4.
        if self.globals.layer_shell.version() >= 4 {
            use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity;
            for h in handles.iter() {
                h.layer_surface
                    .set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
                h.wl_surface.commit();
            }
            let _ = self.conn.flush();
            eprintln!(
                "spotfreeze: exclusive keyboard focus was denied by the compositor; \
                 fell back to on-demand focus (click an overlay to focus it)"
            );
        } else {
            eprintln!(
                "spotfreeze: keyboard focus was denied by the compositor; frozen-mode keys \
                 will not reach the overlay — use the global freeze hotkey to unfreeze"
            );
        }
    }

    /// Build the [`crate::platform::SurfaceFactory`]: a `'static` closure
    /// (like the Windows shell's free function) creating one layer-shell
    /// overlay per monitor, mapping monitor index → snapshot output. Every
    /// capture is an `Rc`-shared or proxy clone — never a borrow of `Shell`.
    pub fn create_surface_factory(
        &self,
    ) -> impl Fn(usize, Rect, Rc<Vec<Rect>>, crate::overlay::events::OverlayEventSink) -> Result<Box<dyn crate::platform::OverlaySurface>>
           + 'static
    {
        let conn = self.conn.clone();
        let qh = self.qh.clone();
        let globals = self.globals.clone();
        let outputs = self.outputs.clone();
        let core = self.core.clone();
        move |monitor_index, monitor_rect, monitors, sink| {
            let output = outputs.get(monitor_index).with_context(|| {
                format!(
                    "monitor index {monitor_index} out of range ({} outputs)",
                    outputs.len()
                )
            })?;
            let parts = FactoryParts {
                conn: &conn,
                qh: &qh,
                globals: globals.as_ref(),
                core: core.as_ref(),
            };
            Ok(Box::new(super::surface::LayerOverlaySurface::create(
                &parts,
                output,
                monitor_index,
                monitor_rect,
                monitors,
                sink,
            )?) as Box<dyn crate::platform::OverlaySurface>)
        }
    }
}

/// The Wayland handles the surface factory needs, bundled to keep
/// [`super::surface::LayerOverlaySurface::create`] argument lists short.
pub(crate) struct FactoryParts<'a> {
    pub conn: &'a Connection,
    pub qh: &'a QueueHandle<ShellState>,
    pub globals: &'a Globals,
    pub core: &'a RefCell<ShellCore>,
}

/// Pump the main queue until `done(&state)` holds (bounded by a handful of
/// roundtrips; used by the surface factory to await the first configure).
pub(crate) fn pump_until(
    core: &RefCell<ShellCore>,
    mut done: impl FnMut(&ShellState) -> bool,
    what: &str,
) -> Result<()> {
    const MAX_ROUNDTRIPS: u32 = 10;
    for _ in 0..MAX_ROUNDTRIPS {
        if done(&core.borrow().state) {
            return Ok(());
        }
        let guard = &mut *core.borrow_mut();
        guard
            .queue
            .roundtrip(&mut guard.state)
            .context("Wayland roundtrip")?;
    }
    if done(&core.borrow().state) {
        Ok(())
    } else {
        Err(anyhow!("the compositor did not answer while waiting for {what}"))
    }
}

/// Create an anonymous shared-memory file of `len` bytes and mmap it RW —
/// the backing store for wl_shm pools (capture frames, surface buffers).
pub(crate) fn map_shm(len: usize) -> Result<(OwnedFd, ShmMapping)> {
    let fd = rustix::fs::memfd_create("spotfreeze-shm", rustix::fs::MemfdFlags::CLOEXEC)
        .context("memfd_create")?;
    rustix::fs::ftruncate(&fd, len as u64).context("ftruncate of the shm file")?;
    // SAFETY: null hint address, valid len/fd; the mapping is unmapped in Drop.
    let ptr = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            len,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &fd,
            0,
        )
    }
    .context("mmap of the shm file")?;
    Ok((fd, ShmMapping { ptr, len }))
}

/// An mmap'd region; unmapped on drop. Always tightly packed BGRA rows.
pub(crate) struct ShmMapping {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

impl ShmMapping {
    /// The mapping as a byte slice.
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr..ptr+len` is a live mapping owned by `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }

    /// The mapping as a mutable byte slice.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr..ptr+len` is a live writable mapping owned by `self`,
        // and `&mut self` is the only alias handed out.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from the `map_shm` mmap and are unmapped once.
        unsafe {
            let _ = rustix::mm::munmap(self.ptr, self.len);
        }
    }
}

/// `$XDG_RUNTIME_DIR/spotfreeze.lock`. Pure; unit-tested.
fn lock_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(LOCK_FILE_NAME)
}

/// Create/open `path` and try a non-blocking exclusive flock. `Ok(None)` =
/// another instance holds it (caller exits silently). The returned fd carries
/// the lock; the lock is released when it closes.
fn try_lock(path: &Path) -> Result<Option<OwnedFd>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file.into())),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// Single-instance lock (see module docs). `Ok(None)` when another instance
/// is running.
pub fn acquire_instance_lock() -> Result<Option<OwnedFd>> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .context("XDG_RUNTIME_DIR is not set; cannot create the single-instance lock")?;
    try_lock(&lock_path(Path::new(&dir)))
}

// ---------------------------------------------------------------------------
// Dispatch implementations for the globals bound on the main queue.
// Event-less managers and the registry get empty bodies; wl_output and
// zxdg_output_v1 events feed the per-output drafts.
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Dynamic global add/remove is out of scope for v1: outputs are a
        // connect-time snapshot (documented module limitation).
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Advertised formats are irrelevant: screencopy frames advertise
        // their own, and surface buffers always use xrgb8888.
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_data_device_manager::WlDataDeviceManager,
        _event: wl_data_device_manager::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for ShellState {
    fn event(
        _state: &mut Self,
        _proxy: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _event: zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for ShellState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let draft = state.output_drafts.entry(*name).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                draft.geometry_pos = (x, y);
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current =
                    matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                if current || draft.mode_size.is_none() {
                    draft.mode_size = Some((width.max(0) as u32, height.max(0) as u32));
                }
            }
            wl_output::Event::Scale { factor } => {
                draft.scale = factor.max(1) as u32;
            }
            wl_output::Event::Name { name } => {
                draft.wl_name = Some(name);
            }
            // Done, Description: no state to track.
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for ShellState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let draft = state.output_drafts.entry(*name).or_default();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                draft.xdg_pos = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                draft.xdg_size = Some((width.max(0) as u32, height.max(0) as u32));
            }
            zxdg_output_v1::Event::Name { name } => {
                draft.xdg_name = Some(name);
            }
            // Done (deprecated in v3), Description: no state to track.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: geometry resolution, monitor-info assembly, lock file.
// No Wayland connection is ever opened.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn draft_with_mode(w: u32, h: u32, scale: u32) -> OutputDraft {
        OutputDraft {
            geometry_pos: (10, 20),
            mode_size: Some((w, h)),
            scale,
            ..Default::default()
        }
    }

    // ---- resolve_geometry ----

    #[test]
    fn xdg_values_win_over_wl_output_fallbacks() {
        let mut d = draft_with_mode(3840, 2160, 2);
        d.xdg_pos = Some((1920, 0));
        d.xdg_size = Some((1920, 1080));
        assert_eq!(resolve_geometry(&d), ((1920, 0), (1920, 1080), 2));
    }

    #[test]
    fn logical_size_falls_back_to_mode_divided_by_scale() {
        let d = draft_with_mode(3840, 2160, 2);
        assert_eq!(resolve_geometry(&d), ((10, 20), (1920, 1080), 2));
    }

    #[test]
    fn unset_scale_is_one() {
        let d = draft_with_mode(1920, 1080, 0);
        assert_eq!(resolve_geometry(&d), ((10, 20), (1920, 1080), 1));
    }

    #[test]
    fn missing_mode_yields_zero_size() {
        let d = OutputDraft::default();
        assert_eq!(resolve_geometry(&d), ((0, 0), (0, 0), 1));
    }

    // ---- monitor rect / dpi assembly (OutputRecord::monitor_info's math) ----

    #[test]
    fn monitor_rect_is_logical_times_scale() {
        assert_eq!(
            physical_rect((1920, 0), (1920, 1080), 2),
            Rect::new(3840, 0, 3840, 2160)
        );
    }

    #[test]
    fn monitor_rect_keeps_negative_logical_positions() {
        assert_eq!(
            physical_rect((-1920, 0), (1920, 1080), 1),
            Rect::new(-1920, 0, 1920, 1080)
        );
    }

    #[test]
    fn monitor_rect_clamps_zero_scale() {
        assert_eq!(
            physical_rect((0, 0), (100, 50), 0),
            Rect::new(0, 0, 100, 50)
        );
    }

    #[test]
    fn dpi_is_96_times_scale() {
        // Same scale clamp physical_rect applies; dpi follows it exactly.
        for (scale, dpi) in [(0, 96), (1, 96), (2, 192), (3, 288)] {
            let scale = scale.max(1);
            assert_eq!(96 * scale, dpi);
        }
    }

    // ---- lock file ----

    #[test]
    fn lock_path_lives_in_the_runtime_dir() {
        assert_eq!(
            lock_path(Path::new("/run/user/1000")),
            PathBuf::from("/run/user/1000/spotfreeze.lock")
        );
    }

    /// Unique temp path; never collides across tests or processes.
    fn unique_temp_lock() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "spotfreeze_lock_test_{}_{}.lock",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn try_lock_is_exclusive_and_released_on_close() {
        let path = unique_temp_lock();
        let first = try_lock(&path).unwrap().expect("first acquire succeeds");
        // Second acquire while the first is held: WOULDBLOCK → None.
        assert!(try_lock(&path).unwrap().is_none(), "second acquire blocked");
        drop(first);
        // After release the lock is acquirable again.
        let third = try_lock(&path).unwrap().expect("acquire after release");
        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
