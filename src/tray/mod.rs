//! System tray icon (`Shell_NotifyIconW`). Win32-only module.
//!
//! Implementation notes (Stage 2):
//! * The icon self-contains its message routing: `create` installs a
//!   `SetWindowSubclass` hook on the owner HWND, so the tray callback message
//!   (`WM_TRAY_CALLBACK`) and the popup menu are handled here without the
//!   owner window proc needing to know anything about the tray.
//! * Resources bound to the owner window's lifetime (the subclass reference
//!   and the shell icon registration) live in an `Rc<RefCell<TrayShared>>`
//!   shared between this struct and the subclass proc (`dwRefData`). Exactly
//!   one extra `Rc` reference is held by the subclass chain and is released
//!   exactly once — either by [`TrayIcon::remove`] or by the subclass proc
//!   itself on `WM_DESTROY` — guarded by `TrayShared::subclass_ref_held`, so
//!   every interleaving of `remove()` and window destruction is safe.
//! * The shell icon uses callback version 0 (no `NIM_SETVERSION`), so `lParam`
//!   of the callback message carries the raw mouse messages
//!   (`WM_LBUTTONUP` / `WM_RBUTTONUP`) per the frozen contract.
//! * The icon image is generated at runtime with `CreateIcon` (a light circle
//!   on a dark square) — no resource files, no extra crates.

use anyhow::{Result, anyhow};
use std::cell::RefCell;
use std::rc::Rc;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, MF_STRING, PostMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};
use windows::core::w;

/// User intents reported by the tray icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayEvent {
    /// Left-click on the icon → the app opens the settings window.
    LeftClick,
    /// "Settings" chosen from the right-click popup menu.
    MenuSettings,
    /// "Exit" chosen from the right-click popup menu. The tray itself NEVER
    /// asks and NEVER exits — the app runs its Yes/No confirmation flow.
    MenuExit,
}

/// `uID` of our single notification-area icon.
const TRAY_ICON_ID: u32 = 1;
/// `uIdSubclass` for our `SetWindowSubclass` hook.
const TRAY_SUBCLASS_ID: usize = 1;
/// Callback message delivered to the owner window (`lParam` = mouse message).
/// `WM_APP + 1`; the app reserves `WM_APP + 2..` for its own posted messages.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;

/// Popup-menu item ids (returned by `TrackPopupMenu` with `TPM_RETURNCMD`).
const IDM_SETTINGS: usize = 1;
const IDM_EXIT: usize = 2;

/// State shared between [`TrayIcon`] and the subclass proc. The subclass chain
/// owns one `Rc` reference (passed as `dwRefData`) while
/// `subclass_ref_held` is true.
struct TrayShared {
    /// Owned icon handle; destroyed on remove/`WM_DESTROY`.
    hicon: Option<HICON>,
    /// True while the icon is registered with the shell (`NIM_ADD` done,
    /// `NIM_DELETE` not yet done).
    icon_added: bool,
    /// True while the subclass chain holds its extra `Rc` reference, i.e. the
    /// subclass is installed and has not released its ref yet.
    subclass_ref_held: bool,
    /// Where left-clicks and menu choices are forwarded.
    sink: Rc<dyn Fn(TrayEvent)>,
}

/// Tray icon with tooltip. Right-click shows a popup menu (Settings / Exit);
/// left-click and menu choices are forwarded to the sink.
pub struct TrayIcon {
    hwnd: HWND,
    #[allow(dead_code)] // retained for API parity; the live sink is in `shared`
    sink: Rc<dyn Fn(TrayEvent)>,
    shared: Rc<RefCell<TrayShared>>,
}

impl TrayIcon {
    /// Attach the icon to the hidden message window `hwnd` (which receives the
    /// tray callback messages). `tooltip` is truncated to Win32's 127-char
    /// `szTip` limit.
    pub fn create(hwnd: HWND, tooltip: &str, sink: Rc<dyn Fn(TrayEvent)>) -> Result<Self> {
        let hicon = create_app_icon()?;

        let mut nid = notify_base(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = hicon;
        write_tooltip(&mut nid.szTip, tooltip);

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
            unsafe {
                let _ = DestroyIcon(hicon);
            }
            return Err(anyhow!("Shell_NotifyIconW(NIM_ADD) failed"));
        }

        let shared = Rc::new(RefCell::new(TrayShared {
            hicon: Some(hicon),
            icon_added: true,
            subclass_ref_held: true,
            sink: sink.clone(),
        }));
        // Hand one Rc reference to the subclass chain as dwRefData. The raw
        // pointer addresses the same allocation as `shared`, so `remove()` can
        // reconstruct it later via `Rc::as_ptr`.
        let refdata = Rc::into_raw(shared.clone()) as usize;

        if !unsafe {
            SetWindowSubclass(hwnd, Some(tray_subclass_proc), TRAY_SUBCLASS_ID, refdata)
        }
        .as_bool()
        {
            // Release the subclass ref we just created, then the shell icon.
            unsafe {
                drop(Rc::from_raw(refdata as *const RefCell<TrayShared>));
            }
            remove_tray_icon_inner(&mut shared.borrow_mut(), hwnd);
            return Err(anyhow!("SetWindowSubclass failed"));
        }

        Ok(Self { hwnd, sink, shared })
    }

    pub fn set_tooltip(&mut self, tooltip: &str) -> Result<()> {
        if !self.shared.borrow().icon_added {
            return Err(anyhow!("tray icon is not registered with the shell"));
        }
        let mut nid = notify_base(self.hwnd);
        nid.uFlags = NIF_TIP;
        write_tooltip(&mut nid.szTip, tooltip);
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool() {
            return Err(anyhow!("Shell_NotifyIconW(NIM_MODIFY) failed"));
        }
        Ok(())
    }

    /// Remove the icon from the notification area; idempotent. Also on `Drop`.
    pub fn remove(&mut self) {
        let raw = Rc::as_ptr(&self.shared);
        // Decide under the borrow, act after it ends: releasing the subclass
        // ref may drop the last `Rc`, which would invalidate any live RefMut.
        let release_subclass = {
            let mut s = self.shared.borrow_mut();
            remove_tray_icon_inner(&mut s, self.hwnd);
            std::mem::replace(&mut s.subclass_ref_held, false)
        };
        if release_subclass {
            unsafe {
                let _ = RemoveWindowSubclass(self.hwnd, Some(tray_subclass_proc), TRAY_SUBCLASS_ID);
                drop(Rc::from_raw(raw));
            }
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Minimal `NOTIFYICONDATAW` identifying our icon (for DELETE/MODIFY).
fn notify_base(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    }
}

/// `NIM_DELETE` (once) + `DestroyIcon` (once). Idempotent via the flags.
fn remove_tray_icon_inner(s: &mut TrayShared, hwnd: HWND) {
    if s.icon_added {
        s.icon_added = false;
        let nid = notify_base(hwnd);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
    if let Some(hicon) = s.hicon.take() {
        unsafe {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Subclass proc on the owner HWND: handles the tray callback message and
/// self-cleans on `WM_DESTROY` (in case `remove()` was never called).
///
/// # Safety
/// `dwRefData` is the `Rc::into_raw` pointer installed by [`TrayIcon::create`]
/// and stays valid while `subclass_ref_held` is true — which always covers any
/// message dispatched through this proc, because the ref is released only
/// after `RemoveWindowSubclass` (no further dispatches) or here on the final
/// `WM_DESTROY` path (after the borrow ends).
unsafe extern "system" fn tray_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid_subclass: usize,
    ref_data: usize,
) -> LRESULT {
    let shared = unsafe { &*(ref_data as *const RefCell<TrayShared>) };

    if msg == WM_TRAY_CALLBACK {
        // Callback version 0: lParam carries the raw mouse message.
        match lparam.0 as u32 {
            WM_LBUTTONUP => {
                let sink = shared.borrow().sink.clone();
                sink(TrayEvent::LeftClick);
            }
            WM_RBUTTONUP => {
                let sink = shared.borrow().sink.clone();
                show_context_menu(hwnd, &sink);
            }
            _ => {}
        }
        return LRESULT(0);
    }

    if msg == WM_DESTROY {
        // Forward first (the owner's own WM_DESTROY handler runs inside this
        // call and may itself call TrayIcon::remove — borrows are short on
        // both sides, and the flags make double cleanup a no-op).
        let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        let release_subclass = {
            let mut s = shared.borrow_mut();
            remove_tray_icon_inner(&mut s, hwnd);
            std::mem::replace(&mut s.subclass_ref_held, false)
        };
        if release_subclass {
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(tray_subclass_proc), TRAY_SUBCLASS_ID);
                // `shared` must not be touched after this: it may be the last ref.
                drop(Rc::from_raw(ref_data as *const RefCell<TrayShared>));
            }
        }
        return result;
    }

    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Right-click popup: Settings / Exit. `TPM_RETURNCMD | TPM_NONOTIFY` makes
/// the selection the synchronous return value, so no `WM_COMMAND` routing is
/// needed. `SetForegroundWindow` first (plus the `WM_NULL` nudge after) so the
/// menu dismisses correctly when the user clicks elsewhere.
fn show_context_menu(hwnd: HWND, sink: &Rc<dyn Fn(TrayEvent)>) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, w!("Settings"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            IDM_SETTINGS => sink(TrayEvent::MenuSettings),
            IDM_EXIT => sink(TrayEvent::MenuExit),
            _ => {} // dismissed without a choice
        }
    }
}

/// Build the icon at runtime: light circle on a dark square. Sized to the
/// system small-icon metrics (16 px fallback, clamped to a sane range).
fn create_app_icon() -> Result<HICON> {
    let w = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let h = unsafe { GetSystemMetrics(SM_CYSMICON) };
    let size = if w > 0 && w == h { w as usize } else { 16 };
    let size = size.clamp(16, 64);

    let (and_mask, xor_mask) = build_icon_masks(size);
    unsafe {
        CreateIcon(
            None,
            size as i32,
            size as i32,
            1,
            32,
            and_mask.as_ptr(),
            xor_mask.as_ptr(),
        )
    }
    .map_err(|e| anyhow!("CreateIcon failed: {e}"))
}

/// Pure: AND mask (1 bpp, 0 = opaque → all zero) + XOR mask (32 bpp BGRX,
/// top-down rows) for a `size`×`size` icon showing a light circle centered on
/// a dark square. Kept Win32-free so it is unit-testable headless.
fn build_icon_masks(size: usize) -> (Vec<u8>, Vec<u8>) {
    let and_stride = size.div_ceil(16) * 2; // monochrome DDB rows are WORD-aligned
    let and_mask = vec![0u8; and_stride * size]; // all opaque
    let mut xor_mask = vec![0u8; size * size * 4];

    const DARK: [u8; 3] = [32, 32, 32]; // B, G, R
    const LIGHT: [u8; 3] = [245, 245, 245];
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.32;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let [b, g, r] = if dx * dx + dy * dy <= radius * radius {
                LIGHT
            } else {
                DARK
            };
            let off = (y * size + x) * 4;
            xor_mask[off] = b;
            xor_mask[off + 1] = g;
            xor_mask[off + 2] = r;
            // xor_mask[off + 3] stays 0; the AND mask defines opacity.
        }
    }
    (and_mask, xor_mask)
}

/// Copy `text` into the 128-wide `szTip` field as UTF-16, truncating to the
/// 127-code-unit limit and always NUL-terminating. Pure helper.
fn write_tooltip(dst: &mut [u16; 128], text: &str) {
    dst.fill(0);
    let wide: Vec<u16> = text.encode_utf16().take(127).collect();
    dst[..wide.len()].copy_from_slice(&wide);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read the `[B, G, R]` bytes of pixel `(x, y)` from an XOR mask.
    fn xor_rgb(xor: &[u8], size: usize, x: usize, y: usize) -> [u8; 3] {
        let off = (y * size + x) * 4;
        [xor[off], xor[off + 1], xor[off + 2]]
    }

    #[test]
    fn icon_masks_have_ddb_layout() {
        for size in [16usize, 24, 32, 48] {
            let (and_mask, xor_mask) = build_icon_masks(size);
            assert_eq!(and_mask.len(), size.div_ceil(16) * 2 * size);
            assert_eq!(xor_mask.len(), size * size * 4);
            // AND mask all zero → every pixel opaque.
            assert!(and_mask.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn icon_pattern_is_light_circle_on_dark_square() {
        let size = 16usize;
        let (_and, xor) = build_icon_masks(size);
        // Center of the icon is inside the circle → light.
        assert_eq!(xor_rgb(&xor, size, 8, 8), [245, 245, 245]);
        // All four corners are outside the circle → dark.
        for (x, y) in [(0, 0), (15, 0), (0, 15), (15, 15)] {
            assert_eq!(xor_rgb(&xor, size, x, y), [32, 32, 32]);
        }
    }

    #[test]
    fn icon_circle_is_centered_symmetrically() {
        let size = 32usize;
        let (_and, xor) = build_icon_masks(size);
        // Mirrored pixels across the center must share the same color.
        for y in 0..size {
            for x in 0..size / 2 {
                assert_eq!(
                    xor_rgb(&xor, size, x, y),
                    xor_rgb(&xor, size, size - 1 - x, y),
                    "horizontal asymmetry at ({x}, {y})"
                );
            }
        }
        for y in 0..size / 2 {
            for x in 0..size {
                assert_eq!(
                    xor_rgb(&xor, size, x, y),
                    xor_rgb(&xor, size, x, size - 1 - y),
                    "vertical asymmetry at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn tooltip_is_copied_and_nul_terminated() {
        let mut tip = [1u16; 128];
        write_tooltip(&mut tip, "SpotFreeze");
        assert_eq!(&tip[..10], &"SpotFreeze".encode_utf16().collect::<Vec<_>>()[..]);
        assert_eq!(tip[10], 0);
        assert!(tip[11..].iter().all(|&u| u == 0));
    }

    #[test]
    fn tooltip_truncates_to_127_code_units() {
        let mut tip = [0u16; 128];
        write_tooltip(&mut tip, &"x".repeat(300));
        assert!(tip[..127].iter().all(|&u| u == 'x' as u16));
        assert_eq!(tip[127], 0);
    }

    #[test]
    fn tooltip_handles_empty_and_exact_fit() {
        let mut tip = [1u16; 128];
        write_tooltip(&mut tip, "");
        assert!(tip.iter().all(|&u| u == 0));

        let exact = "y".repeat(127);
        write_tooltip(&mut tip, &exact);
        assert!(tip[..127].iter().all(|&u| u == 'y' as u16));
        assert_eq!(tip[127], 0);
    }
}
