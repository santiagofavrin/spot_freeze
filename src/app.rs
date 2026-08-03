//! Application wiring: single instance, DPI awareness, hidden message window,
//! global hotkeys, tray, settings, overlay controller, and the message loop.
//!
//! Win32-only glue: every path creates windows, registers real hotkeys, shows
//! dialogs, or captures the screen — nothing a headless test could exercise.
//! The pure frozen-mode registration planner lives in
//! [`crate::hotkeys::frozen`].
//!
//! Threading model: everything runs on the single UI thread. The window proc
//! reaches [`AppState`] through `GWLP_USERDATA`. Cross-component callbacks
//! (tray sink, settings-window callbacks) never touch `AppState` directly —
//! they [`PostMessageW`] to the hidden window and the proc applies them, which
//! keeps reentrancy (nested loops from `TrackPopupMenu`/`MessageBoxW`) safe.

use crate::capture::GdiCapturer;
use crate::hotkeys::frozen::{FrozenAction, plan_frozen_registrations};
use crate::hotkeys::manager::{HotkeyId, HotkeyManager};
use crate::overlay::controller::OverlayController;
use crate::overlay::modes::ModeKind;
use crate::platform::windows::WindowsServices;
use crate::platform::{PlatformServices, SurfaceFactory};
use crate::settings::model::AppSettings;
use crate::settings::store;
use crate::tray::{TrayEvent, TrayIcon};
use crate::ui::settings_window::{self, SettingsCallbacks};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::rc::Rc;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE,
    HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK,
    MB_TOPMOST, MB_YESNO, MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, TranslateMessage, WM_APP, WM_DESTROY, WM_HOTKEY, WM_NCCREATE, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Posted by the tray sink; `wParam` = `TrayEvent as usize`.
const WM_APP_TRAY_EVENT: u32 = WM_APP + 2;
/// Posted by the settings window's `on_saved`; `lParam` = leaked
/// `Box<AppSettings>` pointer reclaimed by the proc.
const WM_APP_SETTINGS_SAVED: u32 = WM_APP + 3;
/// Posted by the settings window's `on_exit_requested`.
const WM_APP_EXIT_REQUESTED: u32 = WM_APP + 4;

/// Whole application state; owned by [`run`]'s stack frame for the lifetime of
/// the message loop and referenced from the window proc via `GWLP_USERDATA`.
struct AppState {
    /// Current settings (updated in place on every settings-window save).
    settings: AppSettings,
    settings_path: PathBuf,
    controller: OverlayController,
    capturer: GdiCapturer,
    services: WindowsServices,
    /// `Some` once the hidden window exists.
    hotkeys: Option<HotkeyManager>,
    tray: Option<TrayIcon>,
    freeze_id: Option<HotkeyId>,
    /// Frozen-mode registrations, only while frozen.
    frozen_ids: Vec<(HotkeyId, FrozenAction)>,
}

/// Run SpotFreeze until the user exits. Responsibilities, in order:
///
/// 1. **Single instance**: `CreateMutexW("Local\\SpotFreeze.SingleInstance")`.
///    A second instance exits `Ok(())` immediately WITHOUT touching the desktop.
/// 2. **DPI**: `SetProcessDpiAwarenessContext(PerMonitorV2)` BEFORE any window
///    is created (all overlay pixels are physical).
/// 3. **Settings**: load via [`crate::settings::store::load`] (creates
///    `spotfreeze.jsonc` with defaults on first run; malformed file → defaults),
///    then reconcile the auto-start registration with the loaded setting so a
///    hand-edited JSONC takes effect on this launch.
/// 4. **Hidden message window**: owns the `WM_HOTKEY` registrations
///    ([`crate::hotkeys::manager::HotkeyManager`]) and the tray icon
///    ([`crate::tray::TrayIcon`]).
/// 5. **Global hotkeys** (re-registered after every settings save):
///    * `freeze_toggle` (always active) → freeze/unfreeze;
///    * while frozen only: `mode_spotlight` → `OverlayController::toggle_mode`
///      (spotlight on/off), `mode_snip` → `OverlayController::set_mode`
///      (enter capture mode, re-basing the freeze), plus `cancel` → unfreeze
///      (in capture mode: exit capture back to the pre-capture view),
///      `snip_copy` → `OverlayController::snip_copy_and_close`, `reset_zoom`
///      → `OverlayController::reset_view`: five registrations (see
///      [`plan_frozen_registrations`]). Zoom has no hotkey — it is the
///      implicit zoom-modifier wheel chord.
/// 6. **Message loop**: standard `GetMessage`/`DispatchMessage`, routing
///    `WM_HOTKEY`, tray callbacks, and overlay events to the controller.
/// 7. **Exit** (tray menu AND settings window Exit button): ALWAYS confirm via
///    `MessageBoxW(MB_YESNO | MB_ICONQUESTION)`; on Yes: unregister all hotkeys,
///    remove the tray icon, unfreeze, and quit.
pub fn run() -> Result<()> {
    // 1. Single instance. A second instance exits silently, before any other
    //    side effect. `_mutex` holds the handle until this function returns.
    let Some(_mutex) = InstanceMutex::acquire()? else {
        return Ok(()); // already running: exit silently, desktop untouched
    };

    // 2. PerMonitorV2 DPI awareness BEFORE any window is created. Failure is
    //    not fatal (worst case the OS bitmap-scales us), so only best effort.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 3. Settings: malformed JSONC → defaults and keep running (per contract).
    let settings_path = store::default_settings_path().context("locating spotfreeze.jsonc")?;
    store::migrate_legacy_settings(&settings_path);
    let settings = store::load(&settings_path).unwrap_or_default();

    // 3b. Auto-start: reconcile the Run-key registration with the setting
    //     (covers hand-edited JSONC and an exe that moved since the entry was
    //     written). Failure is reported but never fatal — the tray and hotkey
    //     must always come up.
    if let Err(e) = crate::platform::windows::apply_auto_start(settings.auto_start) {
        show_error(
            None,
            &format!("Could not update the auto-start registration:\n{e:#}"),
        );
    }

    // 4. State + hidden message window.
    let mut state = Box::new(AppState {
        settings,
        settings_path,
        controller: OverlayController::new(),
        capturer: GdiCapturer::new(),
        services: WindowsServices,
        hotkeys: None,
        tray: None,
        freeze_id: None,
        frozen_ids: Vec::new(),
    });
    let hwnd = create_hidden_window(&mut state)?;

    // 5. Hotkeys + tray. Failures here are reported but never fatal: the user
    //    must always be able to reach the tray to exit.
    state.hotkeys = Some(HotkeyManager::new(hwnd));
    register_freeze_hotkey(&mut state, hwnd);
    let tray_sink = make_tray_sink(hwnd);
    match TrayIcon::create(hwnd, &tooltip_text(&state.settings), tray_sink) {
        Ok(tray) => state.tray = Some(tray),
        Err(e) => show_error(
            Some(hwnd),
            &format!("Could not create the tray icon:\n{e:#}\n\nThe freeze hotkey still works."),
        ),
    }

    // 6. Message loop.
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break; // 0 = WM_QUIT; -1 = error (treated as quit: nothing else to do)
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // 7. Safety net: cleanup is idempotent and normally already ran from the
    //    exit flow / WM_DESTROY.
    cleanup(&mut state);
    Ok(())
}

/// Closes the single-instance mutex handle on drop.
struct InstanceMutex(HANDLE);

// The frozen Cargo.toml feature set does not include `Win32_Security`, which
// gates `windows::Win32::System::Threading::CreateMutexW` (its signature names
// `SECURITY_ATTRIBUTES`). We always pass NULL attributes, so the prototype is
// declared here instead — kernel32.lib is linked on every Windows target, so
// this adds no dependency. `binitialowner` is the Win32 `BOOL` (4 bytes).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(
        lpmutexattributes: *const core::ffi::c_void,
        binitialowner: i32,
        lpname: PCWSTR,
    ) -> HANDLE;
}

impl InstanceMutex {
    /// `Ok(None)` when another instance already holds the mutex (the caller
    /// then exits silently). The handle is kept alive until drop.
    fn acquire() -> Result<Option<Self>> {
        let handle =
            unsafe { CreateMutexW(std::ptr::null(), 1, w!("Local\\SpotFreeze.SingleInstance")) };
        if handle.0.is_null() {
            return Err(anyhow!("CreateMutexW failed"));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Register the window class (idempotent across instances of this process) and
/// create the hidden, message-only owner window.
fn create_hidden_window(state: &mut AppState) -> Result<HWND> {
    let hinstance = HINSTANCE(
        unsafe { GetModuleHandleW(None) }
            .context("GetModuleHandleW")?
            .0,
    );
    let class = WNDCLASSW {
        lpfnWndProc: Some(hidden_wndproc),
        hInstance: hinstance,
        lpszClassName: w!("SpotFreeze.MessageWindow"),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return Err(anyhow!("RegisterClassW failed"));
    }

    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("SpotFreeze.MessageWindow"),
            w!("SpotFreeze"),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE), // message-only: never visible, no taskbar entry
            None,
            Some(hinstance),
            Some(state as *mut AppState as *const core::ffi::c_void),
        )
    }
    .context("CreateWindowExW failed")
}

/// Window proc of the hidden message window. Every handler runs on the UI
/// thread.
///
/// # Safety
/// `GWLP_USERDATA` holds `*mut AppState` pointing into [`run`]'s stack frame,
/// which outlives the window (cleanup runs before `run` returns and the window
/// is destroyed first via `WM_DESTROY`). Nested message loops (`MessageBoxW`,
/// the tray's `TrackPopupMenu`) can re-enter this proc while an outer handler
/// still uses its `&mut AppState`; this is the standard single-threaded Win32
/// pattern — handlers never hold the reference across a yield other than
/// these synchronous nested loops, and there is no data race (one thread).
unsafe extern "system" fn hidden_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut AppState;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_HOTKEY => {
            on_hotkey(state, hwnd, wparam);
            LRESULT(0)
        }
        WM_APP_TRAY_EVENT => {
            on_tray_event(state, hwnd, wparam);
            LRESULT(0)
        }
        WM_APP_SETTINGS_SAVED => {
            // Ownership of the boxed copy posted by `on_saved` transfers here.
            let saved = unsafe { Box::from_raw(lparam.0 as *mut AppSettings) };
            apply_saved_settings(state, hwnd, *saved);
            LRESULT(0)
        }
        WM_APP_EXIT_REQUESTED => {
            confirm_exit(state, hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            cleanup(state);
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Register (and report failure of) the always-active global freeze hotkey.
fn register_freeze_hotkey(state: &mut AppState, hwnd: HWND) {
    let gesture = state.settings.hotkeys.freeze_toggle;
    let Some(hk) = state.hotkeys.as_mut() else {
        return;
    };
    match hk.register(gesture) {
        Ok(id) => state.freeze_id = Some(id),
        Err(e) => show_error(
            Some(hwnd),
            &format!(
                "Could not register the freeze hotkey {}:\n{e:#}\n\nThe tray icon still works.",
                gesture.to_display()
            ),
        ),
    }
}

/// Rebind the freeze hotkey after a settings save, REGISTER-FIRST: the new
/// gesture is registered while the old one is still active, and the old
/// registration is dropped only after the new one succeeds. On failure the
/// old binding stays registered and working (and the error says so) — unlike
/// unregister-first, which would leave no freeze hotkey at all. Returns
/// `true` when the new binding is live.
fn rebind_freeze_hotkey(state: &mut AppState, hwnd: HWND) -> bool {
    let new_gesture = state.settings.hotkeys.freeze_toggle;
    let Some(hk) = state.hotkeys.as_mut() else {
        return false;
    };
    match hk.register(new_gesture) {
        Ok(new_id) => {
            // Best-effort unregister of the old gesture: a failure only leaks
            // one OS hotkey until exit, and the manager's bookkeeping stays
            // consistent either way.
            if let Some(old_id) = state.freeze_id.replace(new_id) {
                let _ = hk.unregister(old_id);
            }
            true
        }
        Err(e) => {
            // Name the still-active binding so the user knows what works.
            let still = state
                .freeze_id
                .and_then(|id| hk.gesture(id))
                .map(|g| format!("\n\nThe previous binding {} still works.", g.to_display()))
                .unwrap_or_default();
            show_error(
                Some(hwnd),
                &format!(
                    "Could not register the freeze hotkey {}:\n{e:#}{still}",
                    new_gesture.to_display()
                ),
            );
            false
        }
    }
}

/// Register the frozen-mode hotkeys planned from the CURRENT settings:
/// spotlight toggle, capture-mode switch, plus `reset_zoom`, `snip_copy`,
/// `cancel` — five registrations (zoom has no hotkey: it is the implicit
/// zoom-modifier wheel chord). Individual
/// failures (e.g. a combo owned by another app, or a duplicate in a
/// hand-edited settings file) are collected into one message box; whatever
/// registered stays active.
fn register_frozen_hotkeys(state: &mut AppState, hwnd: HWND) {
    let plan = plan_frozen_registrations(&state.settings.hotkeys);
    let mut failures = String::new();
    if let Some(hk) = state.hotkeys.as_mut() {
        for registration in plan {
            match hk.register(registration.gesture) {
                Ok(id) => state.frozen_ids.push((id, registration.action)),
                Err(e) => {
                    let _ = std::fmt::Write::write_fmt(
                        &mut failures,
                        format_args!("\n{}: {e:#}", registration.gesture.to_display()),
                    );
                }
            }
        }
    }
    if !failures.is_empty() {
        show_error(
            Some(hwnd),
            &format!("Some frozen-mode hotkeys could not be registered:{failures}"),
        );
    }
}

fn unregister_frozen_hotkeys(state: &mut AppState) {
    if let Some(hk) = state.hotkeys.as_mut() {
        for (id, _) in state.frozen_ids.drain(..) {
            let _ = hk.unregister(id);
        }
    } else {
        state.frozen_ids.clear();
    }
}

/// `WM_HOTKEY`: resolve the id and dispatch.
fn on_hotkey(state: &mut AppState, hwnd: HWND, wparam: WPARAM) {
    let resolved = state
        .hotkeys
        .as_ref()
        .and_then(|hk| hk.handle_wm_hotkey(wparam));
    let Some((id, _gesture)) = resolved else {
        return;
    };

    if state.freeze_id == Some(id) {
        toggle_freeze(state, hwnd);
        return;
    }

    let action = state
        .frozen_ids
        .iter()
        .find(|(fid, _)| *fid == id)
        .map(|(_, action)| *action);
    let Some(action) = action else { return };

    match action {
        FrozenAction::SetMode(kind) => state.controller.set_mode(kind, &state.services),
        FrozenAction::ToggleMode(kind) => state.controller.toggle_mode(kind, &state.services),
        FrozenAction::AddMode(kind) => state.controller.add_mode(kind, &state.services),
        FrozenAction::Copy => {
            if let Err(e) = state.controller.snip_copy_and_close(&state.services) {
                show_error(Some(hwnd), &format!("Could not copy the snip:\n{e:#}"));
            }
        }
        FrozenAction::Cancel => state.controller.unfreeze(),
        FrozenAction::ResetZoom => reset_zoom(state),
    }
    // The controller may have unfrozen itself (snip copy, or a mode asking to
    // exit): drop the frozen-mode hotkeys in that case.
    reconcile_frozen_state(state);
}

/// Freeze/unfreeze toggle on the global hotkey.
fn toggle_freeze(state: &mut AppState, hwnd: HWND) {
    if state.controller.is_frozen() {
        state.controller.unfreeze();
        reconcile_frozen_state(state);
    } else {
        freeze(state, hwnd);
    }
}

/// Capture the screen and freeze it (the freeze contract lands in spotlight
/// mode), registering the frozen-mode hotkeys on success.
fn freeze(state: &mut AppState, hwnd: HWND) {
    let surfaces: &SurfaceFactory = &crate::platform::windows::create_overlay_surface;
    let services: &dyn PlatformServices = &state.services;
    match state
        .controller
        .freeze(&state.capturer, &state.settings, surfaces, services)
    {
        Ok(()) => register_frozen_hotkeys(state, hwnd),
        Err(e) => show_error(Some(hwnd), &format!("Could not freeze the screen:\n{e:#}")),
    }
    reconcile_frozen_state(state);
}

/// Reset-zoom hotkey: forward straight to the controller's dedicated
/// [`OverlayController::reset_view`], which invokes the active mode's
/// `reset_view` and applies its repaint effect. (The former implementation
/// synthesized a `KeyDown` overlay event instead — dead code at runtime,
/// because every mode's `on_key` is a documented no-op.)
fn reset_zoom(state: &mut AppState) {
    state.controller.reset_view();
}

/// Keep app-side frozen state in sync with the controller: whenever the
/// overlay is gone, the frozen-mode hotkeys go too.
fn reconcile_frozen_state(state: &mut AppState) {
    if !state.controller.is_frozen() {
        unregister_frozen_hotkeys(state);
    }
}

/// Tray menu intents: "Spotlight"/"Screenshot" drive the overlay directly,
/// "Reload Settings" re-reads the JSONC file, "Settings…" opens the settings
/// window, "Open settings folder" reveals it in Explorer, "Exit" starts the
/// shared confirm-and-quit flow.
fn on_tray_event(state: &mut AppState, hwnd: HWND, wparam: WPARAM) {
    match wparam.0 {
        x if x == TrayEvent::MenuSpotlight as usize => tray_spotlight(state, hwnd),
        x if x == TrayEvent::MenuScreenshot as usize => tray_screenshot(state, hwnd),
        x if x == TrayEvent::MenuReloadSettings as usize => reload_settings(state, hwnd),
        x if x == TrayEvent::MenuSettings as usize => open_settings(state, hwnd),
        x if x == TrayEvent::MenuOpenSettingsFolder as usize => open_settings_folder(state, hwnd),
        x if x == TrayEvent::MenuExit as usize => confirm_exit(state, hwnd),
        _ => {}
    }
}

/// Tray "Spotlight": freeze when unfrozen (the freeze contract lands in
/// spotlight mode); when already frozen, switch to the spotlight layer.
fn tray_spotlight(state: &mut AppState, hwnd: HWND) {
    if state.controller.is_frozen() {
        state
            .controller
            .set_mode(ModeKind::Spotlight, &state.services);
    } else {
        freeze(state, hwnd);
    }
}

/// Tray "Screenshot": freeze first when unfrozen, then enter snip/capture
/// mode (`set_mode` is a documented no-op when the freeze failed).
fn tray_screenshot(state: &mut AppState, hwnd: HWND) {
    if !state.controller.is_frozen() {
        freeze(state, hwnd);
    }
    state.controller.set_mode(ModeKind::Snip, &state.services);
}

/// Tray "Reload Settings": re-read spotfreeze.jsonc (edited externally) and
/// apply it exactly like a settings-window save, minus the save itself (the
/// file is the source). A malformed file keeps the previous settings.
fn reload_settings(state: &mut AppState, hwnd: HWND) {
    match store::load(&state.settings_path) {
        Ok(loaded) => apply_new_settings(state, hwnd, loaded),
        Err(e) => show_error(
            Some(hwnd),
            &format!(
                "Could not read {}:\n{e:#}\n\nKeeping the previous settings.",
                state.settings_path.display()
            ),
        ),
    }
}

/// The tray sink must be callable from within the tray's subclass proc (with
/// its nested menu loop), so it never touches `AppState` — it posts.
fn make_tray_sink(hwnd: HWND) -> Rc<dyn Fn(TrayEvent)> {
    Rc::new(move |event| {
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_APP_TRAY_EVENT,
                WPARAM(event as usize),
                LPARAM(0),
            )
        };
    })
}

/// Open (or focus) the settings window. Its callbacks post back to the hidden
/// window, so the window module never borrows `AppState`.
fn open_settings(state: &mut AppState, hwnd: HWND) {
    let callbacks = SettingsCallbacks {
        on_saved: Box::new(move |saved: &AppSettings| {
            let boxed = Box::new(saved.clone());
            // On PostMessage failure the box leaks — tiny and near-impossible.
            let _ = unsafe {
                PostMessageW(
                    Some(hwnd),
                    WM_APP_SETTINGS_SAVED,
                    WPARAM(0),
                    LPARAM(Box::into_raw(boxed) as isize),
                )
            };
        }),
        on_exit_requested: Box::new(move || {
            let _ =
                unsafe { PostMessageW(Some(hwnd), WM_APP_EXIT_REQUESTED, WPARAM(0), LPARAM(0)) };
        }),
    };
    if let Err(e) = settings_window::open(Some(hwnd), &mut state.settings, callbacks) {
        show_error(
            Some(hwnd),
            &format!("Could not open the settings window:\n{e:#}"),
        );
    }
}

/// Tray "Open settings folder": reveal `spotfreeze.jsonc`'s folder in
/// Explorer with the file pre-selected. Spawned detached and never waited
/// on: explorer.exe often exits non-zero even on success, so waiting on it
/// would misreport failures.
fn open_settings_folder(state: &mut AppState, hwnd: HWND) {
    let arg = format!("/select,{}", state.settings_path.display());
    if let Err(e) = std::process::Command::new("explorer").arg(arg).spawn() {
        show_error(
            Some(hwnd),
            &format!("Could not open the settings folder:\n{e:#}"),
        );
    }
}

/// A validated settings copy arrived from the settings window: persist it,
/// then apply it (hotkeys, tooltip, auto-start).
fn apply_saved_settings(state: &mut AppState, hwnd: HWND, new_settings: AppSettings) {
    if let Err(e) = store::save(&state.settings_path, &new_settings) {
        show_error(
            Some(hwnd),
            &format!("Could not save {}:\n{e:#}", state.settings_path.display()),
        );
    }
    apply_new_settings(state, hwnd, new_settings);
}

/// Swap in `new_settings` and re-register whatever hotkey bindings changed.
/// Shared by the settings-window save path and the tray's "Reload Settings"
/// (which reads the file itself and must not save it back).
fn apply_new_settings(state: &mut AppState, hwnd: HWND, new_settings: AppSettings) {
    let old = std::mem::replace(&mut state.settings, new_settings);

    if old.hotkeys.freeze_toggle != state.settings.hotkeys.freeze_toggle {
        // Register the NEW gesture first; only on success is the old
        // registration dropped (see `rebind_freeze_hotkey`), so a failed
        // rebind can never leave NO freeze hotkey registered.
        if rebind_freeze_hotkey(state, hwnd)
            && let Some(tray) = state.tray.as_mut()
        {
            // The tooltip follows the binding that is actually live; on
            // failure it keeps showing the previous (still-working) gesture.
            let _ = tray.set_tooltip(&tooltip_text(&state.settings));
        }
    }

    // Rebind frozen-mode hotkeys live when the user saves while frozen. The
    // controller keeps its freeze-time snapshot (per its contract); only the
    // OS-level registrations follow the new bindings.
    if state.controller.is_frozen() && old.hotkeys != state.settings.hotkeys {
        unregister_frozen_hotkeys(state);
        register_frozen_hotkeys(state, hwnd);
    }

    // Auto-start toggled via the settings-window checkbox: apply it to the
    // registry now (hand-edited JSONC is covered by startup reconciliation).
    if old.auto_start != state.settings.auto_start
        && let Err(e) = crate::platform::windows::apply_auto_start(state.settings.auto_start)
    {
        show_error(
            Some(hwnd),
            &format!("Could not update the auto-start registration:\n{e:#}"),
        );
    }
}

/// The single Yes/No exit confirmation used by BOTH the tray menu and the
/// settings window's Exit button. `MB_TOPMOST` is added so the dialog cannot
/// hide behind the topmost overlay windows while frozen.
///
/// Keyboard safety: a settings-window rebind capture can never outlive this
/// dialog's arrival — the window's Exit button disarms the capture BEFORE
/// posting the exit request, and every other path (tray menu, error box) makes
/// the settings window lose activation first, which disarms the capture via
/// its `WM_ACTIVATE` handler (D2). So the modal dialog always gets keys.
fn confirm_exit(state: &mut AppState, hwnd: HWND) {
    let answer = unsafe {
        MessageBoxW(
            Some(hwnd),
            w!("Exit SpotFreeze?"),
            w!("SpotFreeze"),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST,
        )
    };
    if answer == IDYES {
        cleanup(state);
        // WM_DESTROY repeats the (idempotent) cleanup and posts WM_QUIT.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Unregister every hotkey, remove the tray icon, drop the overlay. Every
/// piece is idempotent — safe to call from the exit flow AND `WM_DESTROY`.
fn cleanup(state: &mut AppState) {
    unregister_frozen_hotkeys(state);
    if let Some(hk) = state.hotkeys.as_mut()
        && let Some(id) = state.freeze_id.take()
    {
        let _ = hk.unregister(id);
    }
    if let Some(tray) = state.tray.as_mut() {
        tray.remove();
    }
    state.controller.unfreeze();
}

/// Tray tooltip: app name, version, and the current freeze binding.
fn tooltip_text(settings: &AppSettings) -> String {
    format!(
        "SpotFreeze v{} — freeze: {}",
        env!("CARGO_PKG_VERSION"),
        settings.hotkeys.freeze_toggle.to_display()
    )
}

/// Non-fatal error dialog. Topmost so it is visible even above the overlay.
/// (Keyboard safety same as [`confirm_exit`]: an armed settings-window rebind
/// capture is disarmed on deactivation before this box can take foreground —
/// D2 — so its keystrokes always land here.)
fn show_error(hwnd: Option<HWND>, message: &str) {
    let text = HSTRING::from(message);
    unsafe {
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(text.as_ptr()),
            w!("SpotFreeze"),
            MB_OK | MB_ICONERROR | MB_TOPMOST,
        );
    }
}
