//! Per-monitor layered, topmost overlay window presenting a [`DibBuffer`] via
//! `UpdateLayeredWindowIndirect` (the `prcDirty`-capable variant of
//! `UpdateLayeredWindow`). Win32-only module.
//!
//! # Implementation notes
//!
//! - **Window**: one `WS_POPUP` window per monitor, exactly covering the
//!   monitor rect in physical pixels (process is PerMonitorV2 — no DPI math).
//!   Extended styles: `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED` —
//!   topmost, no taskbar / alt-tab presence, presented via ULW.
//!   The window class is registered once per process; the atom is shared by
//!   all overlay instances.
//! - **Input delivery (keyboard/wheel)**: option **(a) — activatable modal
//!   takeover**. The windows do NOT get `WS_EX_NOACTIVATE`; `create` shows and
//!   foregrounds the window (`SetForegroundWindow`, best-effort — allowed
//!   because the freeze is triggered by our own global hotkey, i.e. we
//!   received the last input event). Whichever overlay is active then receives
//!   `WM_KEYDOWN`/`WM_MOUSEWHEEL` natively; clicking any monitor's overlay
//!   activates that one. Critical keys (Esc / Ctrl+C / mode switches) are
//!   additionally routed by the app as global hotkeys per the controller
//!   contract, so nothing is lost if foreground is ever denied. No low-level
//!   hooks are installed — fewer moving parts, nothing to leak on crash.
//! - **Presentation**: a process-persistent 32-bit **top-down** BGRA DIB
//!   section (`biHeight < 0`, so row order matches [`DibBuffer`] exactly) is
//!   created once per window and selected into a memory DC. `present` memcpy's
//!   the full frame or only the dirty rows into the section, then calls
//!   `UpdateLayeredWindowIndirect` with `prcDirty` — the OS recomposites only
//!   the dirty region (spotlight per-mouse-move fast path: O(hole area),
//!   never a whole-frame copy). Blend: `AC_SRC_OVER` / `AC_SRC_ALPHA`,
//!   constant alpha 255. The [`DibBuffer`] contract guarantees alpha == 255
//!   wherever the window is opaque, so its non-premultiplied pixels blend
//!   identically to premultiplied ones; genuinely translucent pixels (none are
//!   produced today) would need premultiplication first.
//! - **WndProc**: state is a boxed [`WndContext`] passed through
//!   `WM_NCCREATE` into `GWLP_USERDATA` and reclaimed on `WM_NCDESTROY`
//!   (`DestroyWindow` frees it — no leak, no use-after-free). Sink callbacks
//!   are wrapped in `catch_unwind`: unwinding across an `extern "system"`
//!   boundary is UB, and aborting mid-freeze would strand the user's session.
//!
//! No window can be created in tests (headless-safety: nothing visible on the
//! user's desktop), so all translatable logic lives in small pure helpers
//! (`lparam_point`, `wheel_delta_raw`, `wheel_target`, `clip_to_frame`,
//! `copy_region`) that are unit-tested against plain memory buffers. The
//! remaining Win32 glue is intentionally thin and is exercised only by the
//! integration stage.

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect};
use crate::hotkeys::gesture::Modifiers;
use crate::overlay::composite::{monitor_index_at, virtual_to_local};
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::platform::OverlaySurface;
use anyhow::{Context as _, Result, bail};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::rc::Rc;
use std::sync::OnceLock;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, HBITMAP,
    HDC, HGDIOBJ, PAINTSTRUCT, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
    IDC_ARROW, LoadCursorW, RegisterClassExW, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
    ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Error, PCWSTR};

/// One layered topmost window covering exactly one monitor.
///
/// The frame format presented MUST be the [`DibBuffer`] contract (BGRA,
/// non-premultiplied — with alpha 255 everywhere the window renders opaque).
pub struct OverlayWindow {
    hwnd: HWND,
    /// Bookkeeping only — the live copy used at runtime is owned by the
    /// WndProc's [`WndContext`] (freed on `WM_NCDESTROY`).
    #[allow(dead_code)]
    monitor_index: usize,
    monitor_rect: Rect,
    /// See `monitor_index` — the WndProc context owns the callable clone.
    #[allow(dead_code)]
    sink: OverlayEventSink,
    /// Present surface: top-down BGRA DIB section selected into a memory DC.
    dib: DibSection,
}

impl OverlayWindow {
    /// Create a topmost layered window covering `monitor_rect` (virtual-screen
    /// coordinates, physical pixels) and show it immediately. Events are
    /// reported to `sink`, tagged with `monitor_index`.
    ///
    /// `monitors` lists EVERY overlay's monitor rect in index order (shared
    /// across all overlay windows): the wheel handler reroutes `WM_MOUSEWHEEL`
    /// to the monitor actually under the cursor, which is not necessarily the
    /// window the OS delivered the message to (see `overlay_wndproc`).
    pub fn create(
        monitor_index: usize,
        monitor_rect: Rect,
        monitors: Rc<Vec<Rect>>,
        sink: OverlayEventSink,
    ) -> Result<Self> {
        if monitor_rect.width == 0 || monitor_rect.height == 0 {
            bail!("overlay window: monitor rect must be non-empty, got {monitor_rect:?}");
        }
        if monitor_index >= monitors.len() || monitors[monitor_index] != monitor_rect {
            bail!(
                "overlay window: monitor index {monitor_index} inconsistent with shared rect list ({} entries)",
                monitors.len()
            );
        }

        let atom = overlay_class_atom()?;
        let hinstance = module_instance()?;

        // Present surface up front: the first present() is then a pure blit.
        let dib = DibSection::new(monitor_rect.width, monitor_rect.height)
            .context("overlay window: failed to create DIB section")?;

        // Hand the context to the WndProc via WM_NCCREATE; ownership moves to
        // the window and is reclaimed on WM_NCDESTROY.
        let ctx = Box::new(WndContext {
            sink: sink.clone(),
            monitor_index,
            monitors,
        });
        let ctx_raw = Box::into_raw(ctx);

        // SAFETY: all pointers (`ctx_raw`, class name atom) outlive the call;
        // `ctx_raw` is reclaimed below on failure and by WM_NCDESTROY on success.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                PCWSTR(atom as usize as *const u16),
                PCWSTR::null(),
                WS_POPUP,
                monitor_rect.x,
                monitor_rect.y,
                monitor_rect.width as i32,
                monitor_rect.height as i32,
                None,
                None,
                Some(hinstance),
                Some(ctx_raw as *const c_void),
            )
        };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                // SAFETY: the window was never created, so WM_NCCREATE never
                // ran and we still own the box.
                drop(unsafe { Box::from_raw(ctx_raw) });
                return Err(e).context("overlay window: CreateWindowExW failed");
            }
        };

        // Input delivery choice (a): activatable modal takeover — show and
        // foreground the window so it receives keyboard/wheel input natively.
        // Best-effort: global hotkeys are the app's fallback for critical keys.
        // SAFETY: `hwnd` is a live window we just created on this thread.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }

        Ok(Self {
            hwnd,
            monitor_index,
            monitor_rect,
            sink,
            dib,
        })
    }

    /// Re-composite from `frame`. `frame` MUST exactly match the monitor rect
    /// size in physical pixels.
    ///
    /// `dirty: Some(rect)` re-composites ONLY that monitor-local region — the
    /// spotlight per-mouse-move fast path (O(hole area)); `None` re-composites
    /// the full frame.
    pub fn present(&mut self, frame: &DibBuffer, dirty: Option<Rect>) -> Result<()> {
        let w = self.monitor_rect.width;
        let h = self.monitor_rect.height;
        if frame.width != w || frame.height != h || frame.pixels.len() != self.dib.len {
            bail!(
                "overlay present: frame {}x{} ({} bytes) does not match monitor rect {}x{} ({} bytes)",
                frame.width,
                frame.height,
                frame.pixels.len(),
                w,
                h,
                self.dib.len
            );
        }
        if self.hwnd.0.is_null() {
            bail!("overlay present: window is closed");
        }

        // Clip the dirty rect to the frame; an out-of-frame dirty rect is a no-op.
        let region = match dirty {
            None => None, // full-frame path
            Some(d) => match clip_to_frame(d, w, h) {
                Some(r) => Some(r),
                None => return Ok(()),
            },
        };

        // SAFETY: `frame.pixels` covers `self.dib.len` bytes (checked above);
        // `self.dib.bits` is a live DIB section of exactly `self.dib.len`
        // bytes; `region` is clipped to the frame so every offset is in-bounds.
        unsafe {
            copy_region(self.dib.bits, &frame.pixels, w as usize * 4, region);
        }

        // Composite. prcDirty limits the OS-side recomposite to the changed
        // region — the spotlight fast path never recomposites the full frame.
        let dst = POINT {
            x: self.monitor_rect.x,
            y: self.monitor_rect.y,
        };
        let size = SIZE {
            cx: w as i32,
            cy: h as i32,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dirty_rect = region.map(|r| RECT {
            left: r.x,
            top: r.y,
            right: r.x + r.width as i32,
            bottom: r.y + r.height as i32,
        });
        let prc_dirty = dirty_rect.as_ref().map_or(ptr::null(), |r| r as *const RECT);
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(), // null → the screen DC
            pptDst: &dst,
            psize: &size,
            hdcSrc: self.dib.mem_dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: prc_dirty,
        };
        // SAFETY: all pointees (`dst`, `size`, `src`, `blend`, `dirty_rect`)
        // outlive the call; `mem_dc` has the correctly-sized DIB selected.
        let ok = unsafe { UpdateLayeredWindowIndirect(self.hwnd, &info) };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(Error::from_thread()).context("overlay present: UpdateLayeredWindowIndirect failed")
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Bounds in virtual-screen coordinates.
    pub fn monitor_rect(&self) -> Rect {
        self.monitor_rect
    }

    /// Hide and destroy the window; idempotent. Also runs on `Drop`.
    pub fn close(&mut self) {
        if !self.hwnd.0.is_null() {
            let hwnd = std::mem::take(&mut self.hwnd);
            // DestroyWindow fires WM_NCDESTROY, which reclaims the WndContext
            // box — after this call the WndProc context is gone.
            // SAFETY: `hwnd` is a live window owned by this thread; called at
            // most once (hwnd is nulled above).
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        self.close();
    }
}

impl OverlaySurface for OverlayWindow {
    fn present(&mut self, frame: &DibBuffer, dirty: Option<Rect>) -> Result<()> {
        OverlayWindow::present(self, frame, dirty)
    }
}

// ---------------------------------------------------------------------------
// Window class (registered once per process, atom shared by all instances)
// ---------------------------------------------------------------------------

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// Register the overlay window class once and return its atom.
///
/// Single-UI-threaded app: no registration race. Registration copies the class
/// name, so the temporary `Vec<u16>` does not need to outlive the call.
fn overlay_class_atom() -> Result<u16> {
    if let Some(&atom) = CLASS_ATOM.get() {
        return Ok(atom);
    }
    let name: Vec<u16> = "SpotFreezeOverlay\0".encode_utf16().collect();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: module_instance()?,
        hCursor: cursor,
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialized; `name` outlives the call.
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err(Error::from_thread()).context("RegisterClassExW failed for SpotFreezeOverlay");
    }
    // Best-effort cache. If another overlay registered first (impossible on a
    // single UI thread), the atoms are identical anyway.
    let _ = CLASS_ATOM.set(atom);
    Ok(atom)
}

/// This module's `HINSTANCE` (the exe itself).
fn module_instance() -> Result<HINSTANCE> {
    // SAFETY: `None` → the current process's executable module; always valid.
    let hmodule = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
    Ok(HINSTANCE(hmodule.0))
}

// ---------------------------------------------------------------------------
// WndProc glue
// ---------------------------------------------------------------------------

/// Per-window state handed to the WndProc through `GWLP_USERDATA`.
/// Owned by the window: boxed in `create`, freed on `WM_NCDESTROY`.
struct WndContext {
    sink: OverlayEventSink,
    monitor_index: usize,
    /// Every overlay's monitor rect in index order (shared, immutable): the
    /// wheel handler maps the cursor to the monitor it is actually over.
    monitors: Rc<Vec<Rect>>,
}

impl WndContext {
    /// Dispatch an event to the sink without ever unwinding across the FFI
    /// boundary (UB for `extern "system"`; an abort mid-freeze would strand
    /// the user's session behind topmost black windows).
    fn emit(&self, event: OverlayEvent) {
        self.emit_for(self.monitor_index, event);
    }

    /// [`emit`](Self::emit) for an event belonging to a DIFFERENT monitor than
    /// this window's — the cursor-routed wheel path (see `overlay_wndproc`).
    fn emit_for(&self, monitor: usize, event: OverlayEvent) {
        let _ = catch_unwind(AssertUnwindSafe(|| (self.sink)(monitor, event)));
    }
}

/// WndProc for every overlay window. Kept deliberately thin: translate the
/// message to an [`OverlayEvent`] (monitor-local coordinates) and emit.
///
/// SAFETY contract: installed only on our own window class; the `GWLP_USERDATA`
/// pointer is a valid `Box<WndContext>` from `WM_NCCREATE` until `WM_NCDESTROY`.
unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // SAFETY: for WM_NCCREATE, `lparam` is a `CREATESTRUCTW` whose
            // `lpCreateParams` is the pointer passed to CreateWindowExW.
            let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_NCDESTROY => {
            let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WndContext;
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            if !raw.is_null() {
                // SAFETY: `raw` came from Box::into_raw in create() and this is
                // its single reclaim point (window destruction happens once).
                drop(unsafe { Box::from_raw(raw) });
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => {
            let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndContext;
            if raw.is_null() {
                // Before WM_NCCREATE / after WM_NCDESTROY: no context yet.
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            // SAFETY: non-null `raw` is a live Box<WndContext> (see contract).
            let ctx = unsafe { &*raw };
            match msg {
                WM_MOUSEMOVE => {
                    // Client coords == monitor-local (window covers the monitor).
                    ctx.emit(OverlayEvent::MouseMove {
                        at: lparam_point(lparam.0),
                    });
                    LRESULT(0)
                }
                WM_LBUTTONDOWN => {
                    ctx.emit(OverlayEvent::LeftButtonDown {
                        at: lparam_point(lparam.0),
                    });
                    LRESULT(0)
                }
                WM_LBUTTONUP => {
                    ctx.emit(OverlayEvent::LeftButtonUp {
                        at: lparam_point(lparam.0),
                    });
                    LRESULT(0)
                }
                WM_MOUSEWHEEL => {
                    // Wheel routing (multi-monitor correctness):
                    //
                    // `WM_MOUSEWHEEL` is delivered to the FOCUS window, not
                    // the window under the cursor — and our overlays take
                    // foreground as they are created, so the focused overlay
                    // is typically the LAST-created one regardless of where
                    // the cursor is. Tagging the event with the receiving
                    // window's monitor (and converting to ITS client coords)
                    // would route scrolls aimed at another monitor to the
                    // wrong mode state with garbage coordinates.
                    //
                    // Instead, the receiving window's identity is IGNORED:
                    // `lParam` carries the cursor position in SCREEN
                    // coordinates at message time (documented Win32 behavior),
                    // so we map the cursor to the monitor actually containing
                    // it and emit monitor-local coordinates for THAT monitor.
                    // Routing depends only on the cursor, so whichever overlay
                    // happens to receive the message emits the identical event
                    // — and since only the focus window ever receives
                    // `WM_MOUSEWHEEL`, exactly one event is emitted per wheel
                    // message: no duplicates, no misrouting.
                    //
                    // The delta is passed through RAW (no notch truncation):
                    // smooth-scroll hardware sends sub-notch deltas that the
                    // modes accumulate (see `OverlayEvent::MouseWheel`).
                    let cursor_screen = lparam_point(lparam.0);
                    let (monitor, at) =
                        wheel_target(cursor_screen, &ctx.monitors, ctx.monitor_index);
                    ctx.emit_for(
                        monitor,
                        OverlayEvent::MouseWheel {
                            at,
                            delta: wheel_delta_raw(wparam.0),
                            modifiers: current_modifiers(),
                        },
                    );
                    LRESULT(0)
                }
                WM_KEYDOWN => {
                    // Auto-repeat messages are forwarded too; modes/controller
                    // decide whether repeats matter for a given key.
                    ctx.emit(OverlayEvent::KeyDown {
                        vk: wparam.0 as u32,
                        modifiers: current_modifiers(),
                    });
                    LRESULT(0)
                }
                WM_ERASEBKGND => {
                    // Content comes from UpdateLayeredWindow; never erase.
                    LRESULT(1)
                }
                WM_PAINT => {
                    // Layered + ULW window: nothing to paint, just validate.
                    let mut ps = PAINTSTRUCT::default();
                    // SAFETY: `ps` is a valid out-pointer for this call.
                    unsafe {
                        let _ = BeginPaint(hwnd, &mut ps);
                        let _ = EndPaint(hwnd, &ps);
                    }
                    LRESULT(0)
                }
                _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }
    }
}

/// Modifier state at message time (`GetKeyState` reflects the queued-message
/// snapshot, which is what we want for mouse/wheel/key events).
fn current_modifiers() -> Modifiers {
    // SAFETY: GetKeyState is a pure query, no preconditions.
    let down = |vk: u16| unsafe { GetKeyState(vk as i32) } < 0;
    let mut m = Modifiers::NONE;
    if down(VK_SHIFT.0) {
        m = m | Modifiers::SHIFT;
    }
    if down(VK_CONTROL.0) {
        m = m | Modifiers::CTRL;
    }
    if down(VK_MENU.0) {
        m = m | Modifiers::ALT;
    }
    if down(VK_LWIN.0) || down(VK_RWIN.0) {
        m = m | Modifiers::WIN;
    }
    m
}

// ---------------------------------------------------------------------------
// Present surface: 32-bit top-down BGRA DIB section in a memory DC
// ---------------------------------------------------------------------------

/// A DIB section matching the [`DibBuffer`] layout exactly (BGRA, top-down,
/// `stride == width * 4`), selected into a memory DC for ULW presentation.
struct DibSection {
    mem_dc: HDC,
    hbitmap: HBITMAP,
    /// Bitmap selected into `mem_dc` before ours; restored on drop so the
    /// bitmap can be deleted cleanly.
    prev_bitmap: HGDIOBJ,
    bits: *mut u8,
    /// `width * height * 4` — exactly `DibBuffer::pixels.len()` for our size.
    len: usize,
}

impl DibSection {
    fn new(width: u32, height: u32) -> Result<Self> {
        // SAFETY: pure creation call, no preconditions.
        let mem_dc = unsafe { CreateCompatibleDC(None) };
        if mem_dc.is_invalid() {
            return Err(Error::from_thread()).context("CreateCompatibleDC failed");
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative height → TOP-DOWN DIB: row order matches DibBuffer,
                // so present() is a straight row-wise memcpy (no flipping).
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: `bmi` describes a valid 32bpp BI_RGB bitmap; `bits` is a
        // valid out-pointer. `mem_dc`/`hbitmap` are cleaned up on every path.
        let hbitmap = unsafe { CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
        let hbitmap = match hbitmap {
            Ok(h) if !bits.is_null() => h,
            Ok(_) => {
                let _ = unsafe { DeleteDC(mem_dc) };
                bail!("CreateDIBSection returned a null bits pointer");
            }
            Err(e) => {
                let _ = unsafe { DeleteDC(mem_dc) };
                return Err(e).context("CreateDIBSection failed");
            }
        };
        // SAFETY: both handles are live; selecting a DIB into a memory DC is
        // the standard ULW source-DC setup.
        let prev_bitmap = unsafe { SelectObject(mem_dc, HGDIOBJ(hbitmap.0)) };
        Ok(Self {
            mem_dc,
            hbitmap,
            prev_bitmap,
            bits: bits.cast(),
            len: width as usize * height as usize * 4,
        })
    }
}

impl Drop for DibSection {
    fn drop(&mut self) {
        // SAFETY: handles are live (owned by self); restoring the previous
        // bitmap before DeleteObject is the required cleanup order.
        unsafe {
            let _ = SelectObject(self.mem_dc, self.prev_bitmap);
            let _ = DeleteObject(HGDIOBJ(self.hbitmap.0));
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested headless)
// ---------------------------------------------------------------------------

/// Decode signed client coordinates from a message `lParam`
/// (the `GET_X_LPARAM` / `GET_Y_LPARAM` math).
#[inline]
fn lparam_point(lparam: isize) -> Point {
    let lo = lparam as i32;
    Point::new(lo as i16 as i32, (lo >> 16) as i16 as i32)
}

/// Decode the signed wheel delta from a `WM_MOUSEWHEEL` `wParam`
/// (`GET_WHEEL_DELTA_WPARAM`).
#[inline]
fn wheel_delta_raw(wparam: usize) -> i32 {
    ((wparam >> 16) as u16 as i16) as i32
}

/// Cursor-based wheel routing (see the `WM_MOUSEWHEEL` arm of
/// `overlay_wndproc`): map a cursor position in VIRTUAL-SCREEN coordinates to
/// `(monitor_index, monitor_local_point)` for the monitor containing it.
///
/// `monitors` lists every overlay's rect in index order; `fallback` is the
/// RECEIVING window's index, used only when the cursor is outside every known
/// monitor (transient display-change state) — local coordinates are then
/// computed against that window's rect and may be out of bounds, which modes
/// tolerate (they track cursor position freely; dirty regions are clipped on
/// present). Pure and unit-tested headless.
fn wheel_target(cursor_screen: Point, monitors: &[Rect], fallback: usize) -> (usize, Point) {
    match monitor_index_at(cursor_screen, monitors) {
        Some(i) => (i, virtual_to_local(cursor_screen, monitors[i])),
        None => (fallback, virtual_to_local(cursor_screen, monitors[fallback])),
    }
}

/// Clip `dirty` (monitor-local, may be negative/oversized) to a
/// `width`×`height` frame; `None` when nothing overlaps. Self-contained (no
/// cross-module math) so it stays testable headless.
fn clip_to_frame(dirty: Rect, width: u32, height: u32) -> Option<Rect> {
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

/// Copy frame rows from `src` (a [`DibBuffer`]'s pixels) into the raw DIB
/// destination. Both buffers share the same tightly-packed BGRA layout
/// (`stride` bytes per row, top-down). `region: None` copies the whole frame
/// with one memcpy; `Some(r)` copies only that pre-clipped region row by row —
/// the O(dirty area) fast path.
///
/// SAFETY: `dst` must point to a writable buffer of at least `src.len()`
/// bytes; `region` must be fully inside the frame (see [`clip_to_frame`]);
/// the buffers must not overlap.
unsafe fn copy_region(dst: *mut u8, src: &[u8], stride: usize, region: Option<Rect>) {
    match region {
        None => unsafe { ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) },
        Some(r) => {
            let row_bytes = r.width as usize * 4;
            let col_off = r.x as usize * 4;
            for y in r.y..r.y + r.height as i32 {
                let off = y as usize * stride + col_off;
                unsafe {
                    ptr::copy_nonoverlapping(src.as_ptr().add(off), dst.add(off), row_bytes);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: no windows, no GDI objects, pure memory buffers only.
// Window creation, ULW presentation, and real message delivery are exercised
// only by the integration stage (they cannot run without a visible window).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an lParam the way Windows packs signed client coords.
    fn pack_lparam(x: i16, y: i16) -> isize {
        (((y as u16 as u32) << 16) | (x as u16 as u32)) as usize as isize
    }

    /// Build a wParam with a signed HIWORD (wheel delta style).
    fn pack_wparam_hi(hi: i16, lo: u16) -> usize {
        (((hi as u16 as u32) << 16) | (lo as u32)) as usize
    }

    #[test]
    fn lparam_point_decodes_positive_coords() {
        assert_eq!(lparam_point(pack_lparam(10, 20)), Point::new(10, 20));
        assert_eq!(lparam_point(pack_lparam(0, 0)), Point::new(0, 0));
        assert_eq!(lparam_point(pack_lparam(1920, 1080)), Point::new(1920, 1080));
    }

    #[test]
    fn lparam_point_sign_extends_negative_coords() {
        // Possible in exotic message routing; must not wrap to ~65531.
        assert_eq!(lparam_point(pack_lparam(-5, -12)), Point::new(-5, -12));
        assert_eq!(lparam_point(pack_lparam(-1, 1)), Point::new(-1, 1));
        assert_eq!(lparam_point(pack_lparam(1, -1)), Point::new(1, -1));
    }

    #[test]
    fn wheel_delta_raw_decodes_signed_hiword() {
        assert_eq!(wheel_delta_raw(pack_wparam_hi(120, 0)), 120);
        assert_eq!(wheel_delta_raw(pack_wparam_hi(-120, 0)), -120);
        assert_eq!(wheel_delta_raw(pack_wparam_hi(60, 0xFFFF)), 60);
        assert_eq!(wheel_delta_raw(0), 0);
    }

    /// Primary 1920x1080 at (0,0) + secondary 2560x1440 LEFT of it (negative x).
    fn two_monitors() -> Vec<Rect> {
        vec![Rect::new(0, 0, 1920, 1080), Rect::new(-2560, 0, 2560, 1440)]
    }

    #[test]
    fn wheel_target_routes_to_monitor_under_cursor() {
        // D3 regression: the wheel event belongs to the monitor containing
        // the CURSOR, regardless of which window received the message. The
        // fallback argument (the receiving window) must be irrelevant here.
        let mons = two_monitors();
        for fallback in [0, 1] {
            assert_eq!(
                wheel_target(Point::new(100, 200), &mons, fallback),
                (0, Point::new(100, 200)),
                "cursor on primary, receiver {fallback}"
            );
            assert_eq!(
                wheel_target(Point::new(-100, 200), &mons, fallback),
                (1, Point::new(2460, 200)), // local = screen - (-2560, 0)
                "cursor on secondary, receiver {fallback}"
            );
        }
    }

    #[test]
    fn wheel_target_converts_to_monitor_local_coords() {
        let mons = two_monitors();
        // Corners: top-left of each monitor maps to local (0, 0).
        assert_eq!(wheel_target(Point::new(0, 0), &mons, 1), (0, Point::new(0, 0)));
        assert_eq!(
            wheel_target(Point::new(-2560, 0), &mons, 0),
            (1, Point::new(0, 0))
        );
        // Bottom-right pixel of the secondary (right/bottom edges exclusive).
        assert_eq!(
            wheel_target(Point::new(-1, 1439), &mons, 0),
            (1, Point::new(2559, 1439))
        );
    }

    #[test]
    fn wheel_target_outside_all_monitors_falls_back_to_receiver() {
        // Transient display-change state: keep the event on the receiving
        // window's monitor rather than dropping it.
        let mons = two_monitors();
        assert_eq!(
            wheel_target(Point::new(5000, 5000), &mons, 1),
            (1, Point::new(7560, 5000)) // local against the receiver's rect
        );
        assert_eq!(
            wheel_target(Point::new(5000, 5000), &mons, 0),
            (0, Point::new(5000, 5000))
        );
    }

    #[test]
    fn clip_to_frame_keeps_fully_inside_rect() {
        let r = Rect::new(2, 3, 10, 20);
        assert_eq!(clip_to_frame(r, 100, 100), Some(r));
    }

    #[test]
    fn clip_to_frame_clips_edges() {
        // Overflows right/bottom.
        assert_eq!(
            clip_to_frame(Rect::new(90, 90, 50, 50), 100, 100),
            Some(Rect::new(90, 90, 10, 10))
        );
        // Negative origin: clipped to left/top.
        assert_eq!(
            clip_to_frame(Rect::new(-5, -8, 10, 10), 100, 100),
            Some(Rect::new(0, 0, 5, 2))
        );
    }

    #[test]
    fn clip_to_frame_rejects_non_overlapping() {
        assert_eq!(clip_to_frame(Rect::new(100, 0, 10, 10), 100, 100), None);
        assert_eq!(clip_to_frame(Rect::new(0, 100, 10, 10), 100, 100), None);
        assert_eq!(clip_to_frame(Rect::new(-20, 0, 10, 10), 100, 100), None);
        // Touching edges count as empty overlap.
        assert_eq!(clip_to_frame(Rect::new(50, 0, 0, 10), 100, 100), None);
        assert_eq!(clip_to_frame(Rect::new(0, 0, 10, 0), 100, 100), None);
    }

    #[test]
    fn clip_to_frame_handles_extreme_values() {
        // u32::MAX width must not overflow the edge math.
        assert_eq!(
            clip_to_frame(Rect::new(0, 0, u32::MAX, u32::MAX), 4, 4),
            Some(Rect::new(0, 0, 4, 4))
        );
        assert_eq!(clip_to_frame(Rect::new(i32::MIN, i32::MIN, 10, 10), 4, 4), None);
    }

    /// 4×3 BGRA frame: byte value = linear offset, so every byte is unique.
    fn test_frame() -> (Vec<u8>, usize) {
        let (w, h) = (4usize, 3usize);
        let stride = w * 4;
        let pixels: Vec<u8> = (0..stride * h).map(|i| i as u8).collect();
        (pixels, stride)
    }

    #[test]
    fn copy_region_full_frame_copies_everything() {
        let (src, stride) = test_frame();
        let mut dst = vec![0u8; src.len()];
        // SAFETY: dst is exactly src.len() bytes; non-overlapping.
        unsafe { copy_region(dst.as_mut_ptr(), &src, stride, None) };
        assert_eq!(dst, src);
    }

    #[test]
    fn copy_region_dirty_copies_only_the_rect() {
        let (src, stride) = test_frame();
        let mut dst = vec![0u8; src.len()];
        let dirty = Rect::new(1, 1, 2, 2); // pixels x∈[1,3), y∈[1,3)
        // SAFETY: dst is src.len() bytes; rect is inside the 4x3 frame.
        unsafe { copy_region(dst.as_mut_ptr(), &src, stride, Some(dirty)) };
        for y in 0..3usize {
            for x in 0..4usize {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                for k in 0..4 {
                    let off = y * stride + x * 4 + k;
                    assert_eq!(
                        dst[off],
                        if inside { src[off] } else { 0 },
                        "byte at pixel ({x}, {y}) channel {k}"
                    );
                }
            }
        }
    }

    #[test]
    fn copy_region_single_pixel() {
        let (src, stride) = test_frame();
        let mut dst = vec![0u8; src.len()];
        let dirty = Rect::new(3, 2, 1, 1); // bottom-right pixel
        // SAFETY: dst is src.len() bytes; rect is inside the frame.
        unsafe { copy_region(dst.as_mut_ptr(), &src, stride, Some(dirty)) };
        let off = 2 * stride + 3 * 4; // byte offset 44 → values 44..=47, all non-zero
        assert_eq!(&dst[off..off + 4], &src[off..off + 4]);
        assert_eq!(dst.iter().filter(|&&b| b != 0).count(), 4); // exactly that one pixel
    }
}
