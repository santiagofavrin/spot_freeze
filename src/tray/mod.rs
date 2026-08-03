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
//! * The icon image is generated at runtime with `CreateIcon` (the "frost
//!   spotlight" motif: a white disc with a sky ring on a navy rounded square)
//!   — no resource files, no extra crates.

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
    GetSystemMetrics, HICON, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
    SM_CXSMICON, SM_CYSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};
use windows::core::{PCWSTR, w};

/// User intents reported by the tray icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayEvent {
    /// "Spotlight" chosen from the popup menu: freeze into spotlight mode, or
    /// switch to the spotlight layer when already frozen.
    MenuSpotlight,
    /// "Screenshot" chosen from the popup menu: freeze first when unfrozen,
    /// then enter snip/capture mode.
    MenuScreenshot,
    /// "Reload Settings" chosen from the popup menu: re-read the JSONC file
    /// immediately (a changed freeze binding is re-registered on the spot).
    MenuReloadSettings,
    /// "Settings…" chosen from the popup menu.
    MenuSettings,
    /// "Open settings folder" chosen from the popup menu: reveal the folder
    /// containing `spotfreeze.jsonc`, with the file selected, in Explorer.
    MenuOpenSettingsFolder,
    /// "Exit" chosen from the popup menu. The tray itself NEVER
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
const IDM_SPOTLIGHT: usize = 1;
const IDM_SCREENSHOT: usize = 2;
const IDM_RELOAD_SETTINGS: usize = 3;
const IDM_SETTINGS: usize = 4;
const IDM_OPEN_FOLDER: usize = 5;
const IDM_EXIT: usize = 6;

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
    /// Where menu choices are forwarded.
    sink: Rc<dyn Fn(TrayEvent)>,
}

/// Tray icon with tooltip. Either mouse button shows the popup menu (a
/// disabled version line, then Spotlight / Screenshot / Reload Settings /
/// Settings… / Open settings folder / Exit); menu choices are forwarded to
/// the sink.
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

        if !unsafe { SetWindowSubclass(hwnd, Some(tray_subclass_proc), TRAY_SUBCLASS_ID, refdata) }
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
        // Callback version 0: lParam carries the raw mouse message. Either
        // button opens the same context menu.
        match lparam.0 as u32 {
            WM_LBUTTONUP | WM_RBUTTONUP => {
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

/// Popup menu shown for either mouse button: a disabled version line, then
/// Spotlight / Screenshot / Reload Settings / Settings… / Open settings
/// folder / Exit. `TPM_RETURNCMD | TPM_NONOTIFY` makes the selection the
/// synchronous return value, so no `WM_COMMAND` routing is needed.
/// `SetForegroundWindow` first (plus the `WM_NULL` nudge after) so the menu
/// dismisses correctly when the user clicks elsewhere.
fn show_context_menu(hwnd: HWND, sink: &Rc<dyn Fn(TrayEvent)>) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        // The version label is dynamic, so its buffer must outlive
        // TrackPopupMenu below; it does, as a local of this function.
        let version = format!("SpotFreeze v{}", env!("CARGO_PKG_VERSION"));
        let version_wide: Vec<u16> = version.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED | MF_DISABLED,
            0,
            PCWSTR(version_wide.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_SPOTLIGHT, w!("Spotlight"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_SCREENSHOT, w!("Screenshot"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_RELOAD_SETTINGS, w!("Reload Settings"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, w!("Settings…"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_FOLDER, w!("Open settings folder"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
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
            IDM_SPOTLIGHT => sink(TrayEvent::MenuSpotlight),
            IDM_SCREENSHOT => sink(TrayEvent::MenuScreenshot),
            IDM_RELOAD_SETTINGS => sink(TrayEvent::MenuReloadSettings),
            IDM_SETTINGS => sink(TrayEvent::MenuSettings),
            IDM_OPEN_FOLDER => sink(TrayEvent::MenuOpenSettingsFolder),
            IDM_EXIT => sink(TrayEvent::MenuExit),
            _ => {} // dismissed without a choice
        }
    }
}

/// Build the icon at runtime: the "frost spotlight" motif (see
/// [`motif_bgr`]). Sized to the system small-icon metrics (16 px fallback,
/// clamped to a sane range).
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

/// Motif geometry, all fractions of the icon edge.
const CORNER_RADIUS_FRAC: f32 = 0.22;
const SPOTLIGHT_RADIUS_FRAC: f32 = 0.30;
const RING_RADIUS_FRAC: f32 = 0.43; // stroke center
const RING_WIDTH_FRAC: f32 = 0.06;
const SPARKLE_ARM_FRAC: f32 = 0.12;
/// The sparkle is dropped below this edge length (illegible at 16 px).
const SPARKLE_MIN_SIZE: usize = 24;
/// Concavity of the sparkle's 4-point star (superellipse exponent < 1; 0.5 is
/// the classic astroid sparkle).
const SPARKLE_STAR_K: f32 = 0.5;

/// Motif colors as `[B, G, R]` (XOR-mask channel order).
const NAVY: [u8; 3] = [0x2A, 0x17, 0x0F]; // #0F172A
const WHITE: [u8; 3] = [0xFC, 0xFA, 0xF8]; // #F8FAFC
const SKY: [u8; 3] = [0xF8, 0xBD, 0x38]; // #38BDF8

/// The "frost spotlight" motif at pixel `(x, y)` of a `size`×`size` icon:
/// a navy rounded square (transparent outside), a centered white spotlight
/// disc, a sky ring around it, and — for edges >= [`SPARKLE_MIN_SIZE`] — a
/// small 4-point sparkle on the ring at 45° upper-right. `None` = transparent.
/// Sampled at pixel centers with hard edges (the AND mask is 1 bpp, so
/// antialiasing is impossible anyway). Pure, so it is unit-testable headless.
fn motif_bgr(size: usize, x: usize, y: usize) -> Option<[u8; 3]> {
    let edge = size as f32;
    let px = x as f32 + 0.5;
    let py = y as f32 + 0.5;
    let c = edge / 2.0;

    // Rounded-square tile: outside when the distance past the corner square
    // exceeds the corner radius.
    let corner = edge * CORNER_RADIUS_FRAC;
    let qx = ((px - c).abs() - (c - corner)).max(0.0);
    let qy = ((py - c).abs() - (c - corner)).max(0.0);
    if qx.hypot(qy) > corner {
        return None;
    }

    // Sparkle: |u|^k + |v|^k <= arm^k centered on the ring at 45° upper-right.
    if size >= SPARKLE_MIN_SIZE {
        let diag = edge * RING_RADIUS_FRAC * std::f32::consts::FRAC_1_SQRT_2;
        let u = px - (c + diag);
        let v = py - (c - diag);
        let arm = edge * SPARKLE_ARM_FRAC;
        if u.abs().powf(SPARKLE_STAR_K) + v.abs().powf(SPARKLE_STAR_K) <= arm.powf(SPARKLE_STAR_K) {
            return Some(SKY);
        }
    }

    let d = (px - c).hypot(py - c);
    if d <= edge * SPOTLIGHT_RADIUS_FRAC {
        return Some(WHITE);
    }
    if (d - edge * RING_RADIUS_FRAC).abs() <= edge * RING_WIDTH_FRAC / 2.0 {
        return Some(SKY);
    }
    Some(NAVY)
}

/// Pure: AND mask (1 bpp, 1 = transparent → only the corners outside the
/// rounded square; MSB-first, WORD-aligned rows) + XOR mask (32 bpp BGRX,
/// top-down rows) for a `size`×`size` icon showing [`motif_bgr`]. Kept
/// Win32-free so it is unit-testable headless.
fn build_icon_masks(size: usize) -> (Vec<u8>, Vec<u8>) {
    let and_stride = size.div_ceil(16) * 2; // monochrome DDB rows are WORD-aligned
    let mut and_mask = vec![0u8; and_stride * size]; // opaque by default
    let mut xor_mask = vec![0u8; size * size * 4];

    for y in 0..size {
        for x in 0..size {
            let Some([b, g, r]) = motif_bgr(size, x, y) else {
                and_mask[y * and_stride + x / 8] |= 0x80 >> (x % 8);
                continue;
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

    /// Read the AND-mask bit of pixel `(x, y)` (1 = transparent).
    fn and_bit(and: &[u8], size: usize, x: usize, y: usize) -> bool {
        let stride = size.div_ceil(16) * 2;
        and[y * stride + x / 8] & (0x80 >> (x % 8)) != 0
    }

    #[test]
    fn icon_masks_have_ddb_layout() {
        for size in [16usize, 24, 32, 48] {
            let (and_mask, xor_mask) = build_icon_masks(size);
            assert_eq!(and_mask.len(), size.div_ceil(16) * 2 * size);
            assert_eq!(xor_mask.len(), size * size * 4);
            // Corners are transparent, the tile's edge midpoints opaque.
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert!(
                    and_bit(&and_mask, size, x, y),
                    "corner ({x}, {y}) not transparent at {size}"
                );
            }
            for (x, y) in [
                (size / 2, 0),
                (size / 2, size - 1),
                (0, size / 2),
                (size - 1, size / 2),
            ] {
                assert!(
                    !and_bit(&and_mask, size, x, y),
                    "edge midpoint ({x}, {y}) transparent at {size}"
                );
            }
        }
    }

    #[test]
    fn icon_pattern_is_frost_spotlight() {
        let size = 16usize;
        let (and, xor) = build_icon_masks(size);
        // Center of the icon is inside the spotlight disc → white.
        assert_eq!(xor_rgb(&xor, size, 8, 8), WHITE);
        // The ring sits at 43% of the edge on the horizontal axis → sky.
        assert_eq!(xor_rgb(&xor, size, 14, 8), SKY);
        // Between disc and ring the navy tile shows through.
        assert_eq!(xor_rgb(&xor, size, 13, 8), NAVY);
        // All four corners are outside the rounded square → transparent.
        for (x, y) in [(0, 0), (15, 0), (0, 15), (15, 15)] {
            assert!(and_bit(&and, size, x, y));
        }
    }

    #[test]
    fn icon_sparkle_only_on_large_icons() {
        // On the ring at 45° upper-right, then ~70% of the arm length further
        // out: beyond the ring's outer edge, so only the sparkle paints sky.
        let size = 32usize;
        let (_and, xor) = build_icon_masks(size);
        assert_eq!(xor_rgb(&xor, size, 27, 6), SKY);
        // At 16 px the sparkle is dropped: the analogous pixel stays navy.
        let size = 16usize;
        let (_and, xor) = build_icon_masks(size);
        assert_eq!(xor_rgb(&xor, size, 14, 3), NAVY);
    }

    #[test]
    fn icon_motif_is_centered_symmetrically() {
        // Only testable at 16 px: larger sizes carry the upper-right sparkle.
        let size = 16usize;
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
        assert_eq!(
            &tip[..10],
            &"SpotFreeze".encode_utf16().collect::<Vec<_>>()[..]
        );
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
