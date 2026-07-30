//! Windows adapters for the platform seam: [`OverlayWindow`] as an
//! [`OverlaySurface`] factory, cursor/clipboard services over Win32, and the
//! auto-start registration in the current-user Run registry key.

use crate::capture::{DibBuffer, copy_dib_to_clipboard};
use crate::geometry::{Point, Rect};
use crate::overlay::events::OverlayEventSink;
use crate::overlay::window::OverlayWindow;
use crate::platform::{OverlaySurface, PlatformServices};
use anyhow::{Context, Result};
use std::rc::Rc;
use ::windows::Win32::Foundation::POINT;
use ::windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use ::windows::core::PCWSTR;

/// [`SurfaceFactory`](crate::platform::SurfaceFactory) implementation: one
/// layered [`OverlayWindow`] per monitor.
pub fn create_overlay_surface(
    monitor_index: usize,
    monitor_rect: Rect,
    monitors: Rc<Vec<Rect>>,
    sink: OverlayEventSink,
) -> Result<Box<dyn OverlaySurface>> {
    Ok(Box::new(OverlayWindow::create(
        monitor_index,
        monitor_rect,
        monitors,
        sink,
    )?))
}

/// [`PlatformServices`] over `GetCursorPos` and the `CF_DIB` clipboard.
pub struct WindowsServices;

impl PlatformServices for WindowsServices {
    /// Current cursor position in virtual-screen coordinates; `None` on failure.
    fn cursor_position_virtual(&self) -> Option<Point> {
        let mut pt = POINT::default();
        // SAFETY: read-only query writing to a caller-provided POINT; touches no
        // window, hook, clipboard, or input state. Never called from tests.
        unsafe { GetCursorPos(&mut pt) }.ok()?;
        Some(Point::new(pt.x, pt.y))
    }

    /// `CF_DIB` clipboard copy (the maximally paste-compatible format).
    fn copy_image_to_clipboard(&self, frame: &DibBuffer) -> Result<()> {
        copy_dib_to_clipboard(frame)
    }
}

// ---------------------------------------------------------------------------
// Auto-start: the current-user Run registry key. All identifiers and payload
// text are built in `crate::autostart`; below is only the registry side effect.
// ---------------------------------------------------------------------------

// The registry APIs live behind the `Win32_System_Registry` feature, which is
// NOT enabled in the frozen Cargo.toml — declare them directly against
// advapi32 (no new crate dependency, the same pattern as `CreateMutexW` in
// `app.rs`). `HKEY` is declared as `isize` (its real layout: a pointer-sized
// handle) so no `windows`-type layout is relied on; LSTATUS is a 32-bit long.
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        hkey: isize,
        lpsubkey: PCWSTR,
        uloptions: u32,
        samdesired: u32,
        phkresult: *mut isize,
    ) -> i32;
    fn RegSetValueExW(
        hkey: isize,
        lpvaluename: PCWSTR,
        reserved: u32,
        dwtype: u32,
        lpdata: *const u8,
        cbdata: u32,
    ) -> i32;
    fn RegDeleteValueW(hkey: isize, lpvaluename: PCWSTR) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
}

/// `HKEY_CURRENT_USER` — `(ULONG_PTR)((LONG)0x80000001)`: the 32-bit value
/// sign-extended to pointer width on 64-bit Windows.
const HKEY_CURRENT_USER: isize = -2147483647;
/// The only access right `RegSetValueExW`/`RegDeleteValueW` need.
const KEY_SET_VALUE: u32 = 0x0002;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;
/// `RegDeleteValueW` on an absent value: an idempotent remove treats it as
/// already done (see [`crate::autostart::ReconcileAction::Remove`]).
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// Bring the HKCU Run-key registration in line with `auto_start` (startup
/// reconciliation and settings-save apply).
pub fn apply_auto_start(auto_start: bool) -> Result<()> {
    match crate::autostart::reconcile_action(auto_start) {
        crate::autostart::ReconcileAction::Install => {
            let exe =
                std::env::current_exe().context("cannot determine the executable path")?;
            set_run_value(&crate::autostart::windows_run_value_payload(&exe))
        }
        crate::autostart::ReconcileAction::Remove => delete_run_value(),
    }
}

/// Open the Run key, run `f` on the handle, and always close it afterwards.
fn with_run_key<R>(f: impl FnOnce(isize) -> Result<R>) -> Result<R> {
    let subkey = wide(crate::autostart::WINDOWS_RUN_KEY_PATH);
    let mut key: isize = 0;
    // SAFETY: `subkey` is NUL-terminated and outlives the call; `key` is a
    // valid out-pointer.
    let status = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()), 0, KEY_SET_VALUE, &mut key)
    };
    if status != ERROR_SUCCESS {
        return Err(status_error("RegOpenKeyExW", status));
    }
    let result = f(key);
    // SAFETY: `key` came from the successful open above; closed exactly once.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn set_run_value(payload: &str) -> Result<()> {
    with_run_key(|key| {
        let name = wide(crate::autostart::WINDOWS_VALUE_NAME);
        let value = wide(payload);
        // SAFETY: `name`/`value` are NUL-terminated and outlive the call. For
        // REG_SZ the data is the UTF-16 encoding INCLUDING the terminator and
        // `cbData` is the BYTE count, per the Win32 contract.
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(name.as_ptr()),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status_error("RegSetValueExW", status));
        }
        Ok(())
    })
}

fn delete_run_value() -> Result<()> {
    with_run_key(|key| {
        let name = wide(crate::autostart::WINDOWS_VALUE_NAME);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        match status {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            _ => Err(status_error("RegDeleteValueW", status)),
        }
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Registry LSTATUS values are Win32 error codes, so the OS error text applies.
fn status_error(api: &str, status: i32) -> anyhow::Error {
    anyhow::Error::from(std::io::Error::from_raw_os_error(status)).context(format!("{api} failed"))
}
