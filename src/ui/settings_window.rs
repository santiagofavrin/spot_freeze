//! Settings window (Win32 common controls, no framework): one row per
//! rebindable hotkey with a capture button, conflict validation, Save/Cancel,
//! and an Exit button whose confirmation the APP owns. Win32-only module.
//!
//! # Implementation notes
//!
//! * `open()` is **non-blocking**: it creates the window, stores its `HWND` in
//!   a process-wide slot (single settings window per app instance), and returns.
//!   The app's normal message loop dispatches messages to it. All window state
//!   lives in a `Box` owned by the window itself (`GWLP_USERDATA`), reclaimed in
//!   `WM_NCDESTROY`, so no global mutable state and no leaks.
//! * Thread affinity: `open()` must be called on the app's UI thread (the one
//!   running the message loop). Every callback fires on that same thread.
//! * All validation logic (gesture conflicts, radius-modifier rule, numeric
//!   ranges) is implemented as **pure functions** with no Win32 dependency so it
//!   is fully unit-testable headless (see the `tests` module at the bottom).

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::settings::model::{AppSettings, HotkeySettings};
use anyhow::{Context, Result, anyhow};
use std::sync::Once;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, GetSysColorBrush, HDC, HFONT, SetTextColor,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, WC_BUTTONW, WC_EDITW, WC_STATICW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_PUSHBUTTON,
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, EN_CHANGE,
    EN_SETFOCUS, ES_AUTOHSCROLL, ES_CENTER, ES_NUMBER, ES_READONLY, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IDC_ARROW, IsWindow,
    LoadCursorW, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC,
    WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY, WM_SETFONT, WM_SYSKEYDOWN, WNDCLASS_STYLES,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

/// How the settings window talks back to the app.
pub struct SettingsCallbacks {
    /// User pressed Save with a VALID, conflict-free settings copy. The window
    /// validates before calling: every gesture [`crate::hotkeys::gesture::HotkeyGesture::is_registerable`],
    /// no duplicate gestures across all rebindable entries (the modifier-only
    /// spotlight radius modifier is checked separately against full gestures).
    ///
    /// The app MUST persist the copy ([`crate::settings::store::save`]) and
    /// re-register the global hotkeys.
    pub on_saved: Box<dyn FnMut(&AppSettings)>,
    /// User pressed Exit. The window shows NO confirmation itself — the app
    /// runs its single Yes/No confirm-and-quit flow (same one the tray uses).
    pub on_exit_requested: Box<dyn FnMut()>,
}

/// Open the settings window seeded from `settings`.
///
/// * If a settings window is already open, focus it and return `Ok(())`
///   (single settings window per app instance).
/// * The window edits its OWN copy; `settings` is only READ for seeding here —
///   write-back reaches the app exclusively through [`SettingsCallbacks::on_saved`].
/// * `parent`: the app's hidden message window (or `None`).
/// * Rebindable entries: every field of [`crate::settings::model::HotkeySettings`]
///   (freeze toggle, three mode keys, spotlight radius modifier, snip copy,
///   cancel, reset zoom), plus the numeric radius/zoom/dim fields.
///
/// Non-blocking: creates the window and returns immediately; the caller's
/// message loop drives it. Must be called on the UI thread.
pub fn open(
    parent: Option<HWND>,
    settings: &mut AppSettings,
    callbacks: SettingsCallbacks,
) -> Result<()> {
    // Single instance: focus the existing window if it is still alive.
    let existing = OPEN_HWND.load(Ordering::Acquire);
    if existing != 0 {
        let hwnd = hwnd_from_raw(existing);
        // SAFETY: `hwnd` came from a live HWND we stored ourselves; IsWindow
        // re-validates it before any use.
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                return Ok(());
            }
        }
        OPEN_HWND.store(0, Ordering::Release);
    }

    init_common_controls();
    let hinst = module_handle()?;
    ensure_window_class(hinst)?;

    let state = Box::new(SettingsWindowState {
        hwnd: HWND::default(),
        settings: settings.clone(),
        callbacks,
        gesture_edits: [HWND::default(); GESTURE_ROW_COUNT],
        rebind_buttons: [HWND::default(); GESTURE_ROW_COUNT],
        radius_checks: [HWND::default(); RADIUS_MOD_COUNT],
        numeric_edits: [HWND::default(); NUMERIC_FIELD_COUNT],
        hint_label: HWND::default(),
        save_button: HWND::default(),
        capture_row: None,
    });
    let state_ptr = Box::into_raw(state);

    // Compute the outer window size that yields the desired client area.
    // (Computed at 96 DPI; `fixup_client_size` in WM_CREATE corrects for the
    // actual window DPI.)
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: CLIENT_W,
        bottom: CLIENT_H,
    };
    unsafe { AdjustWindowRectEx(&mut rect, WINDOW_STYLE_FLAGS, false, WINDOW_EX_STYLE_FLAGS) }
        .context("AdjustWindowRectEx failed")?;

    let create = || unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE_FLAGS,
            w!("SpotFreezeSettingsWindow"),
            w!("SpotFreeze Settings"),
            WINDOW_STYLE_FLAGS,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            parent,
            None,
            Some(hinst),
            Some(state_ptr.cast()),
        )
    };

    // An overlapped window cannot be owned by a message-only window; retry
    // unowned if the parent HWND is rejected.
    let hwnd = match create() {
        Ok(hwnd) => hwnd,
        Err(first_err) => {
            if parent.is_some() {
                create().with_context(|| {
                    format!("CreateWindowExW failed (with and without parent): {first_err}")
                })?
            } else {
                // Window creation failed; WM_NCDESTROY already reclaimed the
                // state box if WM_CREATE had begun, otherwise reclaim it here.
                unsafe { reclaim_state(state_ptr) };
                return Err(anyhow!("CreateWindowExW failed: {first_err}"));
            }
        }
    };

    // SAFETY: `hwnd` is a valid window handle just returned by CreateWindowExW.
    unsafe {
        OPEN_HWND.store(hwnd.0 as isize, Ordering::Release);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure validation logic (no Win32) — unit-tested at the bottom of this file.
// ---------------------------------------------------------------------------

/// The 7 full-gesture bindings, in display-row order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GestureField {
    FreezeToggle,
    ModeSpotlight,
    ModeZoom,
    ModeSnip,
    SnipCopy,
    Cancel,
    ResetZoom,
}

const GESTURE_ROW_COUNT: usize = 7;

impl GestureField {
    const ALL: [Self; GESTURE_ROW_COUNT] = [
        Self::FreezeToggle,
        Self::ModeSpotlight,
        Self::ModeZoom,
        Self::ModeSnip,
        Self::SnipCopy,
        Self::Cancel,
        Self::ResetZoom,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::FreezeToggle => "Freeze toggle (global)",
            Self::ModeSpotlight => "Mode: Spotlight",
            Self::ModeZoom => "Mode: Zoom",
            Self::ModeSnip => "Mode: Snip",
            Self::SnipCopy => "Snip: copy to clipboard",
            Self::Cancel => "Cancel / unfreeze",
            Self::ResetZoom => "Zoom: reset to 100%",
        }
    }

    fn get(self, hotkeys: &HotkeySettings) -> HotkeyGesture {
        match self {
            Self::FreezeToggle => hotkeys.freeze_toggle,
            Self::ModeSpotlight => hotkeys.mode_spotlight,
            Self::ModeZoom => hotkeys.mode_zoom,
            Self::ModeSnip => hotkeys.mode_snip,
            Self::SnipCopy => hotkeys.snip_copy,
            Self::Cancel => hotkeys.cancel,
            Self::ResetZoom => hotkeys.reset_zoom,
        }
    }

    fn set(self, hotkeys: &mut HotkeySettings, gesture: HotkeyGesture) {
        match self {
            Self::FreezeToggle => hotkeys.freeze_toggle = gesture,
            Self::ModeSpotlight => hotkeys.mode_spotlight = gesture,
            Self::ModeZoom => hotkeys.mode_zoom = gesture,
            Self::ModeSnip => hotkeys.mode_snip = gesture,
            Self::SnipCopy => hotkeys.snip_copy = gesture,
            Self::Cancel => hotkeys.cancel = gesture,
            Self::ResetZoom => hotkeys.reset_zoom = gesture,
        }
    }
}

/// The 4 modifier checkboxes of the spotlight radius modifier, left to right.
const RADIUS_MOD_COUNT: usize = 4;
const RADIUS_MODS: [Modifiers; RADIUS_MOD_COUNT] = [
    Modifiers::CTRL,
    Modifiers::ALT,
    Modifiers::SHIFT,
    Modifiers::WIN,
];
const RADIUS_MOD_LABELS: [&str; RADIUS_MOD_COUNT] = ["Ctrl", "Alt", "Shift", "Win"];

/// The numeric option fields, in display-row order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NumericField {
    SpotlightRadius,
    ZoomStep,
    ZoomMin,
    ZoomMax,
    DimOpacity,
}

const NUMERIC_FIELD_COUNT: usize = 5;

impl NumericField {
    const ALL: [Self; NUMERIC_FIELD_COUNT] = [
        Self::SpotlightRadius,
        Self::ZoomStep,
        Self::ZoomMin,
        Self::ZoomMax,
        Self::DimOpacity,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::SpotlightRadius => "Spotlight default radius (px)",
            Self::ZoomStep => "Zoom step factor (e.g. 1.25)",
            Self::ZoomMin => "Zoom minimum",
            Self::ZoomMax => "Zoom maximum",
            Self::DimOpacity => "Overlay dim opacity (0-255)",
        }
    }

    /// Seed text from a settings object.
    fn seed_text(self, settings: &AppSettings) -> String {
        match self {
            Self::SpotlightRadius => settings.spotlight.default_radius.to_string(),
            Self::ZoomStep => settings.zoom.step_factor.to_string(),
            Self::ZoomMin => settings.zoom.min.to_string(),
            Self::ZoomMax => settings.zoom.max.to_string(),
            Self::DimOpacity => settings.overlay.dim_opacity.to_string(),
        }
    }
}

/// Accepted ranges for the numeric fields (documented, "sensible defaults").
const RADIUS_MIN: u32 = 10;
const RADIUS_MAX: u32 = 2000;
const ZOOM_STEP_MAX: f32 = 4.0; // min is > 1.0 (exclusive), per ZoomSettings contract
const ZOOM_LIMIT_MIN: f32 = 1.0;
const ZOOM_LIMIT_MAX: f32 = 64.0;
const DIM_MIN: u32 = 0;
const DIM_MAX: u32 = 255;

/// Raw text of the numeric option fields, as typed by the user.
#[derive(Clone, Debug, PartialEq)]
struct NumericDraft {
    spotlight_radius: String,
    zoom_step: String,
    zoom_min: String,
    zoom_max: String,
    dim_opacity: String,
}

/// Everything the window needs to validate before enabling Save.
#[derive(Clone, Debug)]
struct SettingsDraft {
    hotkeys: HotkeySettings,
    numerics: NumericDraft,
}

/// Parsed + range-checked numeric values, ready to write into `AppSettings`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ParsedNumerics {
    spotlight_radius: u32,
    zoom_step: f32,
    zoom_min: f32,
    zoom_max: f32,
    dim_opacity: u8,
}

/// Returns the indices of the first exact duplicate pair, if any.
/// `HotkeyGesture` equality (modifiers + vk) IS the conflict definition.
fn find_duplicate_gesture(gestures: &[HotkeyGesture]) -> Option<(usize, usize)> {
    for i in 0..gestures.len() {
        for j in (i + 1)..gestures.len() {
            if gestures[i] == gestures[j] {
                return Some((i, j));
            }
        }
    }
    None
}

/// Parse a whole-number field and range-check it (inclusive bounds).
fn parse_u32_field(label: &str, text: &str, min: u32, max: u32) -> Result<u32, String> {
    let trimmed = text.trim();
    let value: u32 = trimmed
        .parse()
        .map_err(|_| format!("{label}: \"{trimmed}\" is not a whole number"))?;
    if value < min || value > max {
        return Err(format!("{label}: must be between {min} and {max}"));
    }
    Ok(value)
}

/// Parse a decimal field and range-check it. `min_exclusive` implements the
/// "must be > 1.0" rule of the zoom step factor.
fn parse_f32_field(
    label: &str,
    text: &str,
    min: f32,
    min_exclusive: bool,
    max: f32,
) -> Result<f32, String> {
    let trimmed = text.trim();
    let value: f32 = trimmed
        .parse()
        .map_err(|_| format!("{label}: \"{trimmed}\" is not a number"))?;
    if !value.is_finite() {
        return Err(format!("{label}: must be a finite number"));
    }
    let below = if min_exclusive {
        value <= min
    } else {
        value < min
    };
    if below || value > max {
        let lo = if min_exclusive {
            format!("greater than {min}")
        } else {
            format!("at least {min}")
        };
        return Err(format!("{label}: must be {lo} and at most {max}"));
    }
    Ok(value)
}

/// Parse and cross-check all numeric fields.
fn parse_numeric_fields(numerics: &NumericDraft) -> Result<ParsedNumerics, String> {
    let spotlight_radius = parse_u32_field(
        "Spotlight default radius",
        &numerics.spotlight_radius,
        RADIUS_MIN,
        RADIUS_MAX,
    )?;
    let zoom_step = parse_f32_field("Zoom step factor", &numerics.zoom_step, 1.0, true, ZOOM_STEP_MAX)?;
    let zoom_min = parse_f32_field("Zoom minimum", &numerics.zoom_min, ZOOM_LIMIT_MIN, false, ZOOM_LIMIT_MAX)?;
    let zoom_max = parse_f32_field("Zoom maximum", &numerics.zoom_max, ZOOM_LIMIT_MIN, false, ZOOM_LIMIT_MAX)?;
    if zoom_min >= zoom_max {
        return Err("Zoom minimum must be smaller than zoom maximum".to_string());
    }
    let dim_opacity = parse_u32_field("Overlay dim opacity", &numerics.dim_opacity, DIM_MIN, DIM_MAX)?;
    Ok(ParsedNumerics {
        spotlight_radius,
        zoom_step,
        zoom_min,
        zoom_max,
        dim_opacity: dim_opacity as u8,
    })
}

/// Full pre-Save validation per the `SettingsCallbacks::on_saved` contract:
/// every gesture registerable, no duplicate full gestures, and the
/// modifier-only radius binding valid. Returns the parsed numerics on success,
/// or a human-readable message for the inline red hint.
fn validate_draft(draft: &SettingsDraft) -> Result<ParsedNumerics, String> {
    let gestures: Vec<HotkeyGesture> = GestureField::ALL
        .iter()
        .map(|field| field.get(&draft.hotkeys))
        .collect();

    // 1. Every full gesture must be acceptable to RegisterHotKey.
    for (field, gesture) in GestureField::ALL.iter().zip(gestures.iter()) {
        if !gesture.is_registerable() {
            return Err(format!(
                "{}: \"{}\" is not a usable hotkey",
                field.label(),
                gesture.to_display()
            ));
        }
    }

    // 2. No duplicate gestures across all full-gesture bindings.
    if let Some((i, j)) = find_duplicate_gesture(&gestures) {
        return Err(format!(
            "{} and {} both use \"{}\"",
            GestureField::ALL[i].label(),
            GestureField::ALL[j].label(),
            gestures[i].to_display()
        ));
    }

    // 3. The modifier-only spotlight radius modifier is checked *separately*
    //    from full gestures: it must name at least one modifier. Cross-domain
    //    duplicates are impossible by construction — a `HotkeyGesture` always
    //    contains a non-modifier key, a bare `Modifiers` never does — so no
    //    gesture/modifier pair can ever compare equal. (Note this also means
    //    the out-of-box defaults `Ctrl` + `Ctrl+C` stay valid, as intended.)
    if draft.hotkeys.spotlight_radius_modifier.is_empty() {
        return Err(
            "Spotlight radius modifier: tick at least one of Ctrl / Alt / Shift / Win".to_string(),
        );
    }

    // 4. Numeric fields.
    parse_numeric_fields(&draft.numerics)
}

// ---------------------------------------------------------------------------
// Win32 implementation
// ---------------------------------------------------------------------------

/// `HWND` of the open settings window (0 = closed). Touched only on the UI
/// thread; atomic purely to satisfy the compiler about shared access.
static OPEN_HWND: AtomicIsize = AtomicIsize::new(0);

// Child-control IDs (travel in the low word of WM_COMMAND's wParam).
const ID_REBIND_BASE: i32 = 100; // + gesture row index
const ID_RADIUS_CHECK_BASE: i32 = 300; // + modifier index
const ID_NUMERIC_BASE: i32 = 400; // + numeric field index
const ID_SAVE: i32 = 500;
const ID_CANCEL: i32 = 501;
const ID_EXIT: i32 = 502;

// Layout metrics in 96-DPI units (scaled by `px()` at creation).
const MARGIN: i32 = 12;
const ROW_H: i32 = 22;
const ROW_PITCH: i32 = 28;
const GAP: i32 = 8;
const LABEL_W: i32 = 210;
const GESTURE_EDIT_W: i32 = 140;
const REBIND_BTN_W: i32 = 74;
const NUMERIC_LABEL_W: i32 = 300;
const NUMERIC_EDIT_W: i32 = 80;
const CHECK_W: i32 = 96;
const CHECK_PITCH: i32 = 106;
const CLIENT_W: i32 = 470;
const CLIENT_H: i32 = 514;

/// Fixed-size, dialog-style: caption + system menu (close button), no sizing
/// border, no minimize/maximize.
const WINDOW_STYLE_FLAGS: WINDOW_STYLE =
    WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0);
const WINDOW_EX_STYLE_FLAGS: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

const HINT_RED: u32 = 0x0000_00C8; // COLORREF 0x00BBGGRR — dark red

/// All mutable state of one settings window. Owned by the window itself via
/// `GWLP_USERDATA`; created in `open()`, freed in `WM_NCDESTROY`.
struct SettingsWindowState {
    hwnd: HWND,
    /// Working copy — seeded from the caller, written back only via on_saved.
    settings: AppSettings,
    callbacks: SettingsCallbacks,
    gesture_edits: [HWND; GESTURE_ROW_COUNT],
    rebind_buttons: [HWND; GESTURE_ROW_COUNT],
    radius_checks: [HWND; RADIUS_MOD_COUNT],
    numeric_edits: [HWND; NUMERIC_FIELD_COUNT],
    hint_label: HWND,
    save_button: HWND,
    /// Row currently in modal key-capture state ("press keys…"), if any.
    capture_row: Option<usize>,
}

fn hwnd_from_raw(raw: isize) -> HWND {
    HWND(std::ptr::with_exposed_provenance_mut::<core::ffi::c_void>(
        raw as usize,
    ))
}

/// SAFETY: reclaims the state box installed in `GWLP_USERDATA`, if any.
/// Idempotent: clears the slot so a later WM_NCDESTROY is a no-op.
unsafe fn reclaim_state(hwnd_or_ptr: *mut SettingsWindowState) {
    if !hwnd_or_ptr.is_null() {
        // SAFETY: caller guarantees this pointer came from Box::into_raw and
        // has not been reclaimed yet.
        unsafe { drop(Box::from_raw(hwnd_or_ptr)) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Scale a 96-DPI layout unit to the window's actual DPI.
fn px(dpi: u32, value: i32) -> i32 {
    (value as i64 * dpi as i64 / 96) as i32
}

fn init_common_controls() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        };
        // SAFETY: points to a valid, correctly-sized INITCOMMONCONTROLSEX.
        let _ = unsafe { InitCommonControlsEx(&icc) };
    });
}

fn module_handle() -> Result<HINSTANCE> {
    // SAFETY: null module name → this process's executable module.
    let hmodule = unsafe { GetModuleHandleW(PCWSTR::null()) }.context("GetModuleHandleW failed")?;
    Ok(HINSTANCE(hmodule.0))
}

fn ensure_window_class(hinst: HINSTANCE) -> Result<()> {
    static ONCE: Once = Once::new();
    let mut outcome: Result<()> = Ok(());
    ONCE.call_once(|| {
        let class = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(settings_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: Default::default(),
            // SAFETY: IDC_ARROW is a valid system cursor resource ID.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            // SAFETY: COLOR_WINDOW is a valid system color index.
            hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("SpotFreezeSettingsWindow"),
        };
        // SAFETY: `class` is fully initialized; the class name is distinct.
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            outcome = Err(anyhow!("RegisterClassW failed for SpotFreezeSettingsWindow"));
        }
    });
    outcome
}

/// Window procedure. State access is through `GWLP_USERDATA`, installed during
/// `WM_NCCREATE` and reclaimed during `WM_NCDESTROY`.
unsafe extern "system" fn settings_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // SAFETY: lParam points to the CREATESTRUCTW for this creation.
            unsafe {
                let create_struct = lparam.0 as *const CREATESTRUCTW;
                if !create_struct.is_null() {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        (*create_struct).lpCreateParams as isize,
                    );
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CREATE => {
            // SAFETY: GWLP_USERDATA holds the state pointer (set in NCCREATE).
            unsafe {
                let state = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA)
                    as *mut SettingsWindowState);
                state.hwnd = hwnd;
                match build_ui(state) {
                    Ok(()) => LRESULT(0),
                    Err(_) => LRESULT(-1), // abort window creation
                }
            }
        }
        WM_COMMAND => {
            // SAFETY: GWLP_USERDATA holds the state pointer.
            unsafe {
                let state =
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !state.is_null() {
                    on_command(state, wparam);
                }
                LRESULT(0)
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // SAFETY: GWLP_USERDATA holds the state pointer.
            unsafe {
                let state =
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !state.is_null() && (*state).capture_row.is_some() {
                    on_capture_key(state, wparam);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
        WM_CTLCOLORSTATIC => {
            // SAFETY: GWLP_USERDATA holds the state pointer; wParam is the HDC
            // of the static control and lParam its HWND, per Win32 docs.
            unsafe {
                let state =
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !state.is_null()
                    && HWND(lparam.0 as *mut core::ffi::c_void) == (*state).hint_label
                {
                    SetTextColor(
                        HDC(wparam.0 as *mut core::ffi::c_void),
                        COLORREF(HINT_RED),
                    );
                    return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CLOSE => {
            // SAFETY: hwnd is this window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: reclaim the state box exactly once, then clear the
            // single-instance slot if it still points at this window.
            unsafe {
                let ptr =
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                }
                let _ = OPEN_HWND.compare_exchange(
                    hwnd.0 as isize,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Create every child control, lay it out, seed it from the working settings
/// copy, and run the first validation pass. Called once from WM_CREATE.
///
/// SAFETY: `state.hwnd` must be a valid window during its WM_CREATE.
unsafe fn build_ui(state: &mut SettingsWindowState) -> Result<()> {
    // SAFETY: the whole body touches raw Win32 handles owned by `state.hwnd`
    // (valid during WM_CREATE) and stock objects; each step's preconditions
    // are documented at its call site.
    unsafe {
    let dpi = GetDpiForWindow(state.hwnd).max(96);
    fixup_client_size(state, dpi);

    let hinst = module_handle()?;
    // DEFAULT_GUI_FONT is a valid stock-object index; stock objects
    // are owned by the system and must NOT be deleted.
    let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
    let parent = state.hwnd;

    let label_style = WS_CHILD.0 | WS_VISIBLE.0;
    let edit_base = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32;
    let button_style =
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32;
    let checkbox_style =
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32;

    let mut y = MARGIN;

    // --- Section: hotkeys -------------------------------------------------
    create_child(
        WC_STATICW,
        "Hotkeys",
        label_style,
        px(dpi, MARGIN),
        px(dpi, y),
        px(dpi, CLIENT_W - 2 * MARGIN),
        px(dpi, ROW_H),
        0,
        parent,
        hinst,
        font,
    )?;
    y += ROW_PITCH;

    for (row, field) in GestureField::ALL.iter().enumerate() {
        let row_y = px(dpi, y);
        create_child(
            WC_STATICW,
            field.label(),
            label_style,
            px(dpi, MARGIN),
            row_y,
            px(dpi, LABEL_W),
            px(dpi, ROW_H),
            0,
            parent,
            hinst,
            font,
        )?;
        let edit_x = MARGIN + LABEL_W + GAP;
        state.gesture_edits[row] = create_child(
            WC_EDITW,
            "",
            edit_base | ES_READONLY as u32 | ES_CENTER as u32,
            px(dpi, edit_x),
            row_y,
            px(dpi, GESTURE_EDIT_W),
            px(dpi, ROW_H),
            0,
            parent,
            hinst,
            font,
        )?;
        let btn_x = edit_x + GESTURE_EDIT_W + GAP;
        state.rebind_buttons[row] = create_child(
            WC_BUTTONW,
            "Rebind",
            button_style,
            px(dpi, btn_x),
            row_y,
            px(dpi, REBIND_BTN_W),
            px(dpi, ROW_H),
            ID_REBIND_BASE + row as i32,
            parent,
            hinst,
            font,
        )?;
        y += ROW_PITCH;
    }

    // --- Section: spotlight radius modifier -------------------------------
    y += GAP;
    create_child(
        WC_STATICW,
        "Spotlight radius modifier (hold + scroll wheel to resize)",
        label_style,
        px(dpi, MARGIN),
        px(dpi, y),
        px(dpi, CLIENT_W - 2 * MARGIN),
        px(dpi, ROW_H),
        0,
        parent,
        hinst,
        font,
    )?;
    y += ROW_PITCH;
    for (i, label) in RADIUS_MOD_LABELS.iter().enumerate() {
        state.radius_checks[i] = create_child(
            WC_BUTTONW,
            label,
            checkbox_style,
            px(dpi, MARGIN + i as i32 * CHECK_PITCH),
            px(dpi, y),
            px(dpi, CHECK_W),
            px(dpi, ROW_H),
            ID_RADIUS_CHECK_BASE + i as i32,
            parent,
            hinst,
            font,
        )?;
    }
    y += ROW_PITCH;

    // --- Section: numeric options -----------------------------------------
    y += GAP;
    create_child(
        WC_STATICW,
        "Options",
        label_style,
        px(dpi, MARGIN),
        px(dpi, y),
        px(dpi, CLIENT_W - 2 * MARGIN),
        px(dpi, ROW_H),
        0,
        parent,
        hinst,
        font,
    )?;
    y += ROW_PITCH;

    for (i, field) in NumericField::ALL.iter().enumerate() {
        let row_y = px(dpi, y);
        create_child(
            WC_STATICW,
            field.label(),
            label_style,
            px(dpi, MARGIN),
            row_y,
            px(dpi, NUMERIC_LABEL_W),
            px(dpi, ROW_H),
            0,
            parent,
            hinst,
            font,
        )?;
        state.numeric_edits[i] = create_child(
            WC_EDITW,
            "",
            edit_base | WS_TABSTOP.0 | ES_NUMBER as u32,
            px(dpi, MARGIN + NUMERIC_LABEL_W + GAP),
            row_y,
            px(dpi, NUMERIC_EDIT_W),
            px(dpi, ROW_H),
            ID_NUMERIC_BASE + i as i32,
            parent,
            hinst,
            font,
        )?;
        y += ROW_PITCH;
    }

    // --- Validation hint (red) ---------------------------------------------
    y += GAP;
    state.hint_label = create_child(
        WC_STATICW,
        "",
        label_style,
        px(dpi, MARGIN),
        px(dpi, y),
        px(dpi, CLIENT_W - 2 * MARGIN),
        px(dpi, ROW_H),
        0,
        parent,
        hinst,
        font,
    )?;
    y += ROW_PITCH;

    // --- Bottom buttons -----------------------------------------------------
    y += GAP;
    state.save_button = create_child(
        WC_BUTTONW,
        "Save",
        button_style,
        px(dpi, MARGIN),
        px(dpi, y),
        px(dpi, 80),
        px(dpi, 26),
        ID_SAVE,
        parent,
        hinst,
        font,
    )?;
    create_child(
        WC_BUTTONW,
        "Cancel",
        button_style,
        px(dpi, MARGIN + 88),
        px(dpi, y),
        px(dpi, 80),
        px(dpi, 26),
        ID_CANCEL,
        parent,
        hinst,
        font,
    )?;
    create_child(
        WC_BUTTONW,
        "Exit SpotFreeze",
        button_style,
        px(dpi, CLIENT_W - MARGIN - 132),
        px(dpi, y),
        px(dpi, 132),
        px(dpi, 26),
        ID_EXIT,
        parent,
        hinst,
        font,
    )?;

    seed_controls(state);
    refresh_validation(state);
    Ok(())
    }
}

/// Resize the window so its CLIENT area is exactly CLIENT_W x CLIENT_H at the
/// window's DPI (MoveWindow-style math, keeping the position Windows chose).
///
/// SAFETY: `state.hwnd` must be valid.
unsafe fn fixup_client_size(state: &SettingsWindowState, dpi: u32) {
    unsafe {
        let want_w = px(dpi, CLIENT_W);
        let want_h = px(dpi, CLIENT_H);
        let mut client = RECT::default();
        if GetClientRect(state.hwnd, &mut client).is_err() {
            return;
        }
        let dw = want_w - (client.right - client.left);
        let dh = want_h - (client.bottom - client.top);
        if dw == 0 && dh == 0 {
            return;
        }
        let mut window = RECT::default();
        if GetWindowRect(state.hwnd, &mut window).is_err() {
            return;
        }
        let _ = SetWindowPos(
            state.hwnd,
            None,
            0,
            0,
            (window.right - window.left) + dw,
            (window.bottom - window.top) + dh,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Create one child control and hand it the default GUI font.
///
/// SAFETY: `parent` must be a valid window; `hinst` our module handle.
#[allow(clippy::too_many_arguments)]
unsafe fn create_child(
    class: PCWSTR,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
) -> Result<HWND> {
    let text_wide = wide(text);
    // SAFETY: all parameters valid; `text_wide` outlives the call.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(text_wide.as_ptr()),
            WINDOW_STYLE(style),
            x,
            y,
            width,
            height,
            Some(parent),
            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                std::ptr::with_exposed_provenance_mut::<core::ffi::c_void>(id as usize),
            )),
            Some(hinst),
            None,
        )
    }
    .with_context(|| format!("CreateWindowExW failed for \"{text}\""))?;
    // SAFETY: hwnd is a live child control; font is a stock object.
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)), // redraw immediately
        );
    }
    Ok(hwnd)
}

/// Fill every control from the working settings copy.
fn seed_controls(state: &SettingsWindowState) {
    for (row, field) in GestureField::ALL.iter().enumerate() {
        set_text(
            state.gesture_edits[row],
            &field.get(&state.settings.hotkeys).to_display(),
        );
    }
    let radius_mod = state.settings.hotkeys.spotlight_radius_modifier;
    for (i, checkbox) in state.radius_checks.iter().enumerate() {
        set_checkbox(*checkbox, radius_mod.contains(RADIUS_MODS[i]));
    }
    for (i, field) in NumericField::ALL.iter().enumerate() {
        set_text(state.numeric_edits[i], &field.seed_text(&state.settings));
    }
}

fn set_text(hwnd: HWND, text: &str) {
    let text_wide = wide(text);
    // SAFETY: hwnd is a live control; `text_wide` outlives the call.
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(text_wide.as_ptr()));
    }
}

fn read_text(hwnd: HWND) -> String {
    // SAFETY: hwnd is a live control; buffer sized length+1 per Win32 docs.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

fn set_checkbox(hwnd: HWND, checked: bool) {
    // SAFETY: hwnd is a live checkbox control.
    unsafe {
        SendMessageW(
            hwnd,
            BM_SETCHECK,
            Some(WPARAM(usize::from(checked))),
            None,
        );
    }
}

fn checkbox_checked(hwnd: HWND) -> bool {
    // SAFETY: hwnd is a live checkbox control.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 != 0 }
}

/// Read the current UI contents into a validation draft.
fn collect_draft(state: &SettingsWindowState) -> SettingsDraft {
    let mut hotkeys = state.settings.hotkeys.clone();

    let mut radius_modifier = Modifiers::NONE;
    for (i, checkbox) in state.radius_checks.iter().enumerate() {
        if checkbox_checked(*checkbox) {
            radius_modifier = radius_modifier | RADIUS_MODS[i];
        }
    }
    hotkeys.spotlight_radius_modifier = radius_modifier;

    SettingsDraft {
        hotkeys,
        numerics: NumericDraft {
            spotlight_radius: read_text(state.numeric_edits[0]),
            zoom_step: read_text(state.numeric_edits[1]),
            zoom_min: read_text(state.numeric_edits[2]),
            zoom_max: read_text(state.numeric_edits[3]),
            dim_opacity: read_text(state.numeric_edits[4]),
        },
    }
}

/// Re-validate the UI, update the red inline hint, and enable/disable Save.
/// Returns whether the draft is currently valid.
fn refresh_validation(state: &SettingsWindowState) -> bool {
    let (hint, valid) = match validate_draft(&collect_draft(state)) {
        Ok(_) => (String::new(), true),
        Err(message) => (message, false),
    };
    set_text(state.hint_label, &hint);
    // SAFETY: save_button is a live control.
    unsafe {
        let _ = EnableWindow(state.save_button, valid);
    }
    valid
}

/// Write a validated draft back into the working settings copy.
fn apply_valid_draft(state: &mut SettingsWindowState, draft: &SettingsDraft, parsed: ParsedNumerics) {
    state.settings.hotkeys = draft.hotkeys.clone();
    state.settings.spotlight.default_radius = parsed.spotlight_radius;
    state.settings.zoom.step_factor = parsed.zoom_step;
    state.settings.zoom.min = parsed.zoom_min;
    state.settings.zoom.max = parsed.zoom_max;
    state.settings.overlay.dim_opacity = parsed.dim_opacity;
}

/// Enter the modal key-capture state for one gesture row.
fn begin_capture(state: &SettingsWindowState, row: usize) {
    set_text(state.gesture_edits[row], "press keys\u{2026}"); // "press keys…"
    // SAFETY: both handles are live; moving focus to the main window makes
    // raw WM_KEYDOWN arrive at our own window procedure.
    unsafe {
        let _ = SetFocus(Some(state.hwnd));
    }
}

/// Leave capture mode, restoring the row's display to its bound gesture.
fn end_capture_restore(state: &mut SettingsWindowState) {
    if let Some(row) = state.capture_row.take() {
        let gesture = GestureField::ALL[row].get(&state.settings.hotkeys);
        set_text(state.gesture_edits[row], &gesture.to_display());
    }
}

/// WM_COMMAND dispatch.
///
/// SAFETY: `state` is the live state pointer from GWLP_USERDATA.
unsafe fn on_command(state: *mut SettingsWindowState, wparam: WPARAM) {
    let id = (wparam.0 & 0xFFFF) as i32;
    let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;

    // SAFETY: pointer is valid for the window's lifetime (freed only in
    // WM_NCDESTROY, after which no WM_COMMAND can arrive).
    let state = unsafe { &mut *state };

    match notification {
        EN_CHANGE if (ID_NUMERIC_BASE..ID_NUMERIC_BASE + NUMERIC_FIELD_COUNT as i32).contains(&id) => {
            refresh_validation(state);
        }
        EN_SETFOCUS if (ID_NUMERIC_BASE..ID_NUMERIC_BASE + NUMERIC_FIELD_COUNT as i32).contains(&id) => {
            // Clicking into another field abandons an ongoing capture.
            end_capture_restore(state);
        }
        BN_CLICKED => match id {
            i if (ID_REBIND_BASE..ID_REBIND_BASE + GESTURE_ROW_COUNT as i32).contains(&i) => {
                let row = (i - ID_REBIND_BASE) as usize;
                if state.capture_row == Some(row) {
                    // Clicking the same Rebind button again cancels capture.
                    end_capture_restore(state);
                } else {
                    end_capture_restore(state);
                    state.capture_row = Some(row);
                    begin_capture(state, row);
                }
            }
            i if (ID_RADIUS_CHECK_BASE..ID_RADIUS_CHECK_BASE + RADIUS_MOD_COUNT as i32).contains(&i) => {
                end_capture_restore(state);
                refresh_validation(state);
            }
            ID_SAVE => try_save(state),
            ID_CANCEL => {
                let hwnd = state.hwnd;
                // SAFETY: hwnd is the live settings window. Destroys children
                // and triggers WM_NCDESTROY, which frees `state`.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            ID_EXIT => {
                // The APP owns confirmation and quitting — we just forward.
                (state.callbacks.on_exit_requested)();
            }
            _ => {}
        },
        _ => {}
    }
}

/// Validate + persist via callback, then close. Save is normally disabled
/// while invalid, but re-check anyway (defense in depth).
fn try_save(state: &mut SettingsWindowState) {
    end_capture_restore(state);
    let draft = collect_draft(state);
    match validate_draft(&draft) {
        Ok(parsed) => {
            apply_valid_draft(state, &draft, parsed);
            refresh_validation(state);
            (state.callbacks.on_saved)(&state.settings);
            let hwnd = state.hwnd;
            // SAFETY: hwnd is the live settings window; `state` is not
            // touched after DestroyWindow (it is freed in WM_NCDESTROY).
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Err(message) => {
            set_text(state.hint_label, &message);
            // SAFETY: save_button is a live control.
            unsafe {
                let _ = EnableWindow(state.save_button, false);
            }
        }
    }
}

/// Key arrived while a row is in modal capture state: convert it into a
/// `HotkeyGesture` from GetKeyState modifiers + the pressed VK.
///
/// SAFETY: `state` is the live state pointer from GWLP_USERDATA.
unsafe fn on_capture_key(state: *mut SettingsWindowState, wparam: WPARAM) {
    // SAFETY: valid for the window's lifetime (see on_command).
    let state = unsafe { &mut *state };
    let Some(row) = state.capture_row else {
        return;
    };

    let vk = wparam.0 as u32;
    if is_modifier_vk(vk) {
        // Bare modifier press: keep waiting for the actual key.
        return;
    }

    let gesture = HotkeyGesture::new(current_modifiers(), vk);
    GestureField::ALL[row].set(&mut state.settings.hotkeys, gesture);
    set_text(state.gesture_edits[row], &gesture.to_display());
    state.capture_row = None;
    refresh_validation(state);
}

/// True for VK codes that ARE modifiers (they build the modifier set, they
/// never become the gesture's key).
fn is_modifier_vk(vk: u32) -> bool {
    [
        VK_SHIFT.0,
        VK_CONTROL.0,
        VK_MENU.0,
        VK_LSHIFT.0,
        VK_RSHIFT.0,
        VK_LCONTROL.0,
        VK_RCONTROL.0,
        VK_LMENU.0,
        VK_RMENU.0,
        VK_LWIN.0,
        VK_RWIN.0,
    ]
    .contains(&(vk as u16))
}

/// Snapshot the physically-held modifiers via GetKeyState.
fn current_modifiers() -> Modifiers {
    let mut modifiers = Modifiers::NONE;
    // SAFETY: GetKeyState is safe to call for any VK on the UI thread.
    unsafe {
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            modifiers = modifiers | Modifiers::SHIFT;
        }
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            modifiers = modifiers | Modifiers::CTRL;
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            modifiers = modifiers | Modifiers::ALT;
        }
        if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
            modifiers = modifiers | Modifiers::WIN;
        }
    }
    modifiers
}

// ---------------------------------------------------------------------------
// Tests — headless, std-only, no windows/hotkeys/clipboard/capture.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gesture without going through `HotkeyGesture::new` so these
    /// tests exercise only THIS module's logic.
    fn gesture(modifiers: Modifiers, vk: u32) -> HotkeyGesture {
        HotkeyGesture { modifiers, vk }
    }

    // --- find_duplicate_gesture -------------------------------------------

    #[test]
    fn duplicate_pair_is_found() {
        let gestures = [
            gesture(Modifiers::CTRL, 0x46),               // Ctrl+F
            gesture(Modifiers::NONE, 0x31),               // 1
            gesture(Modifiers::CTRL | Modifiers::NONE, 0x46), // == gestures[0]
        ];
        assert_eq!(find_duplicate_gesture(&gestures), Some((0, 2)));
    }

    #[test]
    fn same_key_with_different_modifiers_is_not_a_duplicate() {
        let gestures = [
            gesture(Modifiers::CTRL, 0x43),                       // Ctrl+C
            gesture(Modifiers::CTRL | Modifiers::SHIFT, 0x43),    // Ctrl+Shift+C
            gesture(Modifiers::NONE, 0x43),                       // C
        ];
        assert_eq!(find_duplicate_gesture(&gestures), None);
    }

    #[test]
    fn all_distinct_gestures_pass() {
        // Mirrors the out-of-box defaults: F, 1, 2, 3, Ctrl+C, Esc, 0.
        let gestures = [
            gesture(Modifiers::CTRL | Modifiers::ALT, 0x46),
            gesture(Modifiers::NONE, 0x31),
            gesture(Modifiers::NONE, 0x32),
            gesture(Modifiers::NONE, 0x33),
            gesture(Modifiers::CTRL, 0x43),
            gesture(Modifiers::NONE, 0x1B),
            gesture(Modifiers::NONE, 0x30),
        ];
        assert_eq!(find_duplicate_gesture(&gestures), None);
    }

    // --- numeric field parsing ---------------------------------------------

    #[test]
    fn u32_field_accepts_in_range_and_trims_whitespace() {
        assert_eq!(parse_u32_field("Radius", " 150 ", 10, 2000), Ok(150));
        assert_eq!(parse_u32_field("Radius", "10", 10, 2000), Ok(10));
        assert_eq!(parse_u32_field("Radius", "2000", 10, 2000), Ok(2000));
    }

    #[test]
    fn u32_field_rejects_garbage_and_out_of_range() {
        assert!(parse_u32_field("Radius", "", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "abc", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "15.5", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "-20", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "9", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "2001", 10, 2000).is_err());
        assert!(parse_u32_field("Radius", "99999999999", 10, 2000).is_err());
    }

    #[test]
    fn dim_opacity_bounds_are_0_to_255() {
        assert_eq!(parse_u32_field("Overlay dim opacity", "0", 0, 255), Ok(0));
        assert_eq!(parse_u32_field("Overlay dim opacity", "255", 0, 255), Ok(255));
        assert!(parse_u32_field("Overlay dim opacity", "256", 0, 255).is_err());
    }

    #[test]
    fn f32_step_factor_min_is_exclusive() {
        assert!(parse_f32_field("Zoom step factor", "1.0", 1.0, true, 4.0).is_err());
        assert!(parse_f32_field("Zoom step factor", "0.99", 1.0, true, 4.0).is_err());
        assert_eq!(parse_f32_field("Zoom step factor", "1.25", 1.0, true, 4.0), Ok(1.25));
        assert_eq!(parse_f32_field("Zoom step factor", "4.0", 1.0, true, 4.0), Ok(4.0));
        assert!(parse_f32_field("Zoom step factor", "4.01", 1.0, true, 4.0).is_err());
    }

    #[test]
    fn f32_field_rejects_nan_and_infinity() {
        assert!(parse_f32_field("Zoom minimum", "NaN", 1.0, false, 64.0).is_err());
        assert!(parse_f32_field("Zoom minimum", "inf", 1.0, false, 64.0).is_err());
        assert!(parse_f32_field("Zoom minimum", "-inf", 1.0, false, 64.0).is_err());
    }

    // --- parse_numeric_fields ----------------------------------------------

    fn default_numerics() -> NumericDraft {
        NumericDraft {
            spotlight_radius: "150".to_string(),
            zoom_step: "1.25".to_string(),
            zoom_min: "1.0".to_string(),
            zoom_max: "16".to_string(),
            dim_opacity: "160".to_string(),
        }
    }

    #[test]
    fn numeric_defaults_validate() {
        let parsed = parse_numeric_fields(&default_numerics()).expect("defaults must parse");
        assert_eq!(parsed.spotlight_radius, 150);
        assert_eq!(parsed.zoom_step, 1.25);
        assert_eq!(parsed.zoom_min, 1.0);
        assert_eq!(parsed.zoom_max, 16.0);
        assert_eq!(parsed.dim_opacity, 160);
    }

    #[test]
    fn zoom_min_must_be_strictly_below_zoom_max() {
        let mut numerics = default_numerics();
        numerics.zoom_min = "16".to_string();
        numerics.zoom_max = "16".to_string();
        assert!(parse_numeric_fields(&numerics).is_err());

        numerics.zoom_min = "20".to_string();
        assert!(parse_numeric_fields(&numerics).is_err());

        numerics.zoom_min = "2".to_string();
        assert!(parse_numeric_fields(&numerics).is_ok());
    }

    #[test]
    fn first_invalid_numeric_field_is_reported() {
        let mut numerics = default_numerics();
        numerics.spotlight_radius = "5".to_string();
        numerics.dim_opacity = "300".to_string();
        let err = parse_numeric_fields(&numerics).unwrap_err();
        assert!(err.contains("radius"), "unexpected error: {err}");
    }

    // --- validate_draft (needs gesture.rs: is_registerable/to_display) -----

    fn default_like_draft() -> SettingsDraft {
        SettingsDraft {
            hotkeys: HotkeySettings {
                freeze_toggle: gesture(Modifiers::CTRL | Modifiers::ALT, 0x46),
                mode_spotlight: gesture(Modifiers::NONE, 0x31),
                mode_zoom: gesture(Modifiers::NONE, 0x32),
                mode_snip: gesture(Modifiers::NONE, 0x33),
                spotlight_radius_modifier: Modifiers::CTRL,
                snip_copy: gesture(Modifiers::CTRL, 0x43),
                cancel: gesture(Modifiers::NONE, 0x1B),
                reset_zoom: gesture(Modifiers::NONE, 0x30),
            },
            numerics: default_numerics(),
        }
    }

    #[test]
    fn out_of_box_like_settings_validate() {
        validate_draft(&default_like_draft()).expect("default-like settings must validate");
    }

    #[test]
    fn radius_modifier_may_share_modifiers_with_a_full_gesture() {
        // Contract nuance: the modifier-only binding is checked *separately*;
        // Ctrl (radius) + Ctrl+C (snip copy) are BOTH defaults and must pass.
        let draft = default_like_draft();
        assert_eq!(draft.hotkeys.spotlight_radius_modifier, Modifiers::CTRL);
        assert_eq!(draft.hotkeys.snip_copy.modifiers, Modifiers::CTRL);
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn duplicate_full_gestures_are_rejected() {
        let mut draft = default_like_draft();
        draft.hotkeys.cancel = draft.hotkeys.freeze_toggle;
        let err = validate_draft(&draft).unwrap_err();
        assert!(err.contains("both use"), "unexpected error: {err}");
    }

    #[test]
    fn empty_radius_modifier_is_rejected() {
        let mut draft = default_like_draft();
        draft.hotkeys.spotlight_radius_modifier = Modifiers::NONE;
        let err = validate_draft(&draft).unwrap_err();
        assert!(err.contains("radius modifier"), "unexpected error: {err}");
    }

    #[test]
    fn multi_modifier_radius_modifier_is_accepted() {
        let mut draft = default_like_draft();
        draft.hotkeys.spotlight_radius_modifier = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn invalid_numeric_disables_save_via_validation_error() {
        let mut draft = default_like_draft();
        draft.numerics.zoom_step = "1.0".to_string(); // must be > 1.0
        assert!(validate_draft(&draft).is_err());
    }
}
