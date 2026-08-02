//! Settings window (Win32 common controls, no framework), styled after a
//! modern system-settings panel: a light neutral window background, one
//! rounded white card per section, a three-level font hierarchy, and the
//! accent color reserved for the primary Save action. One row per rebindable
//! hotkey with a capture button, conflict validation, Save/Cancel, and an
//! Exit button whose confirmation the APP owns. Win32-only module.
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
//! * All validation logic (gesture conflicts, modifier-only rules, numeric
//!   ranges, overlay color hex) plus the rebind-capture decision state machine
//!   are implemented as **pure functions** with no Win32 dependency so they are
//!   fully unit-testable headless (see the `tests` module at the bottom).
//! * Hotkey rebinding uses a temporary `WH_KEYBOARD_LL` low-level keyboard
//!   hook, installed only while a capture is armed. This captures chords the
//!   shell would otherwise eat (e.g. `Win+F`) because LL hooks run before the
//!   shell's hotkey processing. The hook handle is held in an RAII guard
//!   ([`CaptureKeyboardHook`]) inside the window state, so every exit path —
//!   successful capture, Esc cancel, window close, `WM_NCDESTROY` — uninstalls
//!   it automatically; a leaked system-wide hook is impossible by construction.
//!   While armed the hook swallows ALL system keyboard input, so the capture is
//!   also disarmed (old binding text restored) the moment the window loses
//!   activation (`WM_ACTIVATE` / `WA_INACTIVE`) — walking away from the window
//!   can never leave the rest of the desktop keyboard-dead.
//! * Visual layer: the section cards and all push buttons are owner-drawn GDI
//!   (rounded rectangles, hairline edges); labels, checkboxes, and edits get
//!   their colors through the `WM_CTLCOLOR*` handlers. The font hierarchy is
//!   derived from the system message-box font once per window and owned by
//!   the window state; the two background brushes are process-lifetime
//!   singletons created alongside the window class. Everything visual lives
//!   in `build_ui`, the `WM_CTLCOLOR*`/`WM_DRAWITEM` handlers, and the
//!   `draw_*` routines — the validation and capture logic is layout-agnostic.

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::settings::model::{AppSettings, HotkeySettings, Rgb};
use anyhow::{Context, Result, anyhow};
use std::sync::Once;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CreateFontIndirectW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET,
    DEFAULT_GUI_FONT, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawFocusRect, DrawTextW,
    FF_SWISS, FW_NORMAL, FW_SEMIBOLD, GetStockObject, HBRUSH, HDC, HFONT, InflateRect,
    InvalidateRect, LOGFONTW, PS_SOLID, RoundRect, SelectObject, SetBkColor, SetBkMode,
    SetTextColor, TRANSPARENT, VARIABLE_PITCH, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, ODS_DISABLED,
    ODS_FOCUS, ODS_SELECTED, WC_BUTTONW, WC_EDITW, WC_STATICW,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_OWNERDRAW,
    CREATESTRUCTW, CW_USEDEFAULT, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    EN_CHANGE, EN_SETFOCUS, ES_AUTOHSCROLL, ES_CENTER, ES_NUMBER, ES_READONLY, GWLP_USERDATA,
    GetClientRect, GetDlgCtrlID, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, HHOOK, HWND_BOTTOM, IDC_ARROW, IsWindow, KBDLLHOOKSTRUCT, LoadCursorW,
    NONCLIENTMETRICSW, RegisterClassW, SPI_GETNONCLIENTMETRICS, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, SetWindowsHookExW,
    ShowWindow, SystemParametersInfoW, UnhookWindowsHookEx, WA_INACTIVE, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DRAWITEM, WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY, WM_SETFONT,
    WM_SYSKEYDOWN, WNDCLASS_STYLES, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
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
///   — the full gestures (freeze toggle, mode keys, zoom hold, snip copy,
///   cancel, reset zoom) as gesture rows and the two modifier-only chords
///   (spotlight radius, zoom) as checkbox rows — plus the numeric
///   radius/zoom/dim fields, the overlay color row, and the auto-start
///   checkbox.
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
        radius_checks: [HWND::default(); MOD_CHECK_COUNT],
        zoom_checks: [HWND::default(); MOD_CHECK_COUNT],
        numeric_edits: [HWND::default(); NUMERIC_FIELD_COUNT],
        color_swatch: HWND::default(),
        color_hex_edit: HWND::default(),
        auto_start_check: HWND::default(),
        hint_label: HWND::default(),
        save_button: HWND::default(),
        dpi: 96,
        fonts: UiFonts::default(),
        capture_row: None,
        capture_hook: None,
        capture_modifiers: HeldModifiers::default(),
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
    ModeSnip,
    ZoomHold,
    SnipCopy,
    Cancel,
    ResetZoom,
}

const GESTURE_ROW_COUNT: usize = 7;

impl GestureField {
    const ALL: [Self; GESTURE_ROW_COUNT] = [
        Self::FreezeToggle,
        Self::ModeSpotlight,
        Self::ModeSnip,
        Self::ZoomHold,
        Self::SnipCopy,
        Self::Cancel,
        Self::ResetZoom,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::FreezeToggle => "Freeze toggle (global)",
            Self::ModeSpotlight => "Mode: Spotlight",
            Self::ModeSnip => "Mode: Capture",
            Self::ZoomHold => "Zoom hold toggle",
            Self::SnipCopy => "Snip: copy to clipboard",
            Self::Cancel => "Cancel / unfreeze",
            Self::ResetZoom => "Zoom: reset to 100%",
        }
    }

    fn get(self, hotkeys: &HotkeySettings) -> HotkeyGesture {
        match self {
            Self::FreezeToggle => hotkeys.freeze_toggle,
            Self::ModeSpotlight => hotkeys.mode_spotlight,
            Self::ModeSnip => hotkeys.mode_snip,
            Self::ZoomHold => hotkeys.zoom_hold,
            Self::SnipCopy => hotkeys.snip_copy,
            Self::Cancel => hotkeys.cancel,
            Self::ResetZoom => hotkeys.reset_zoom,
        }
    }

    fn set(self, hotkeys: &mut HotkeySettings, gesture: HotkeyGesture) {
        match self {
            Self::FreezeToggle => hotkeys.freeze_toggle = gesture,
            Self::ModeSpotlight => hotkeys.mode_spotlight = gesture,
            Self::ModeSnip => hotkeys.mode_snip = gesture,
            Self::ZoomHold => hotkeys.zoom_hold = gesture,
            Self::SnipCopy => hotkeys.snip_copy = gesture,
            Self::Cancel => hotkeys.cancel = gesture,
            Self::ResetZoom => hotkeys.reset_zoom = gesture,
        }
    }
}

/// The 4 modifier checkboxes of each modifier-only binding (spotlight radius
/// modifier and zoom modifier), left to right.
const MOD_CHECK_COUNT: usize = 4;
const MOD_CHECK_MODS: [Modifiers; MOD_CHECK_COUNT] = [
    Modifiers::CTRL,
    Modifiers::ALT,
    Modifiers::SHIFT,
    Modifiers::WIN,
];
const MOD_CHECK_LABELS: [&str; MOD_CHECK_COUNT] = ["Ctrl", "Alt", "Shift", "Win"];

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

    /// Row label. ZoomMax's goes unused: its edit shares ZoomMin's
    /// "Zoom range" row (see `build_ui`).
    fn label(self) -> &'static str {
        match self {
            Self::SpotlightRadius => "Spotlight default radius (px)",
            Self::ZoomStep => "Zoom step factor (e.g. 1.25)",
            Self::ZoomMin => "Zoom range (min / max)",
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
    /// Raw `"#RRGGBB"` text of the overlay color hex field, as typed.
    color_hex: String,
    /// The auto-start checkbox. A plain boolean: nothing to validate.
    auto_start: bool,
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

/// A fully validated draft: parsed numerics + parsed overlay color.
/// (Hotkeys ride through the draft unchanged — they need no parsing.)
#[derive(Clone, Copy, Debug, PartialEq)]
struct ParsedDraft {
    numerics: ParsedNumerics,
    overlay_color: Rgb,
}

// ---------------------------------------------------------------------------
// Pure rebind-capture decision logic (no Win32) — unit-tested at the bottom.
// ---------------------------------------------------------------------------

/// What the capture state machine decided for one keyboard event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CaptureDecision {
    /// Key-up event or bare modifier press: not a binding yet — keep waiting.
    KeepCapturing,
    /// `Esc` pressed with NO modifiers held: abort, keep the old binding.
    Cancel,
    /// A non-modifier key went down: adopt this gesture as the new binding.
    Bind(HotkeyGesture),
}

/// Decide what one keyboard event means for an armed rebind capture.
///
/// * `key_down == false` (key-up) and bare modifier presses never decide
///   anything — the capture stays armed so chords can be built up.
/// * `Esc` with an empty modifier set cancels instead of binding; `Esc` WITH
///   modifiers (e.g. `Ctrl+Esc`) is a legitimate binding and is bound.
/// * Anything else binds `modifiers + vk` verbatim. The modifier set is a
///   snapshot of the physically-held keys at the moment `vk` went down, so
///   shell-eaten chords like `Win+F` bind correctly (the LL hook sees them
///   first).
fn decide_capture(key_down: bool, vk: u32, modifiers: Modifiers) -> CaptureDecision {
    if !key_down || is_modifier_vk(vk) {
        return CaptureDecision::KeepCapturing;
    }
    if vk == VK_ESCAPE.0 as u32 && modifiers.is_empty() {
        return CaptureDecision::Cancel;
    }
    CaptureDecision::Bind(HotkeyGesture::new(modifiers, vk))
}

/// Assemble a [`Modifiers`] set from four physically-held flags. Pure; the
/// Win32 layer ([`HeldModifiers`]) only supplies the booleans.
fn modifiers_from_key_state(ctrl: bool, alt: bool, shift: bool, win: bool) -> Modifiers {
    let mut modifiers = Modifiers::NONE;
    if ctrl {
        modifiers = modifiers | Modifiers::CTRL;
    }
    if alt {
        modifiers = modifiers | Modifiers::ALT;
    }
    if shift {
        modifiers = modifiers | Modifiers::SHIFT;
    }
    if win {
        modifiers = modifiers | Modifiers::WIN;
    }
    modifiers
}

/// Physically-held modifier keys, one flag per side so releasing the left
/// side of a pair does not clear the still-held right side. The capture hook
/// maintains this from its own key events (seeded by
/// [`held_modifiers_snapshot`] when a capture arms); per MSDN the async key
/// state is updated only AFTER a `WH_KEYBOARD_LL` callback returns, so
/// `GetAsyncKeyState` cannot observe modifier presses from inside the hook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HeldModifiers {
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lshift: bool,
    rshift: bool,
    lwin: bool,
    rwin: bool,
}

impl HeldModifiers {
    /// Record one key event: `key_down == true` presses, `false` releases.
    /// Non-modifier vks are ignored; the generic `VK_SHIFT`/`VK_CONTROL`/
    /// `VK_MENU` codes (synthetic input) fold into the left side so their
    /// down/up pairs stay balanced.
    fn apply(&mut self, key_down: bool, vk: u32) {
        let side = match vk as u16 {
            v if v == VK_LSHIFT.0 || v == VK_SHIFT.0 => &mut self.lshift,
            v if v == VK_RSHIFT.0 => &mut self.rshift,
            v if v == VK_LCONTROL.0 || v == VK_CONTROL.0 => &mut self.lctrl,
            v if v == VK_RCONTROL.0 => &mut self.rctrl,
            v if v == VK_LMENU.0 || v == VK_MENU.0 => &mut self.lalt,
            v if v == VK_RMENU.0 => &mut self.ralt,
            v if v == VK_LWIN.0 => &mut self.lwin,
            v if v == VK_RWIN.0 => &mut self.rwin,
            _ => return,
        };
        *side = key_down;
    }

    /// The held set as [`Modifiers`]; either side of a pair sets the flag.
    fn modifiers(self) -> Modifiers {
        modifiers_from_key_state(
            self.lctrl || self.rctrl,
            self.lalt || self.ralt,
            self.lshift || self.rshift,
            self.lwin || self.rwin,
        )
    }
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
    let zoom_step = parse_f32_field(
        "Zoom step factor",
        &numerics.zoom_step,
        1.0,
        true,
        ZOOM_STEP_MAX,
    )?;
    let zoom_min = parse_f32_field(
        "Zoom minimum",
        &numerics.zoom_min,
        ZOOM_LIMIT_MIN,
        false,
        ZOOM_LIMIT_MAX,
    )?;
    let zoom_max = parse_f32_field(
        "Zoom maximum",
        &numerics.zoom_max,
        ZOOM_LIMIT_MIN,
        false,
        ZOOM_LIMIT_MAX,
    )?;
    if zoom_min >= zoom_max {
        return Err("Zoom minimum must be smaller than zoom maximum".to_string());
    }
    let dim_opacity = parse_u32_field(
        "Overlay dim opacity",
        &numerics.dim_opacity,
        DIM_MIN,
        DIM_MAX,
    )?;
    Ok(ParsedNumerics {
        spotlight_radius,
        zoom_step,
        zoom_min,
        zoom_max,
        dim_opacity: dim_opacity as u8,
    })
}

/// Full pre-Save validation per the `SettingsCallbacks::on_saved` contract:
/// every gesture registerable, no duplicate full gestures, both modifier-only
/// bindings valid and distinct from each other, numerics in range, and the
/// overlay color a valid `"#RRGGBB"`. Returns the parsed values on success,
/// or a human-readable message for the inline red hint.
fn validate_draft(draft: &SettingsDraft) -> Result<ParsedDraft, String> {
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

    // 3. The modifier-only bindings (spotlight radius, zoom) are checked
    //    *separately* from full gestures: each must name at least one modifier,
    //    and the two must differ from each other (holding the same chord would
    //    resize the spotlight AND zoom on the same wheel scroll). Cross-domain
    //    duplicates with full gestures are impossible by construction — a
    //    `HotkeyGesture` always contains a non-modifier key, a bare `Modifiers`
    //    never does — so no gesture/modifier pair can ever compare equal. (Note
    //    this also means the out-of-box defaults `Ctrl` + `Ctrl+C` stay valid,
    //    as intended.)
    if draft.hotkeys.spotlight_radius_modifier.is_empty() {
        return Err(
            "Spotlight radius modifier: tick at least one of Ctrl / Alt / Shift / Win".to_string(),
        );
    }
    if draft.hotkeys.zoom_modifier.is_empty() {
        return Err("Zoom modifier: tick at least one of Ctrl / Alt / Shift / Win".to_string());
    }
    if draft.hotkeys.zoom_modifier == draft.hotkeys.spotlight_radius_modifier {
        return Err(format!(
            "Zoom modifier and spotlight radius modifier both use \"{}\"",
            draft.hotkeys.zoom_modifier.to_display()
        ));
    }

    // 4. Numeric fields.
    let numerics = parse_numeric_fields(&draft.numerics)?;

    // 5. Overlay color hex field.
    let overlay_color =
        Rgb::parse_hex(&draft.color_hex).map_err(|e| format!("Overlay color: {e}"))?;

    Ok(ParsedDraft {
        numerics,
        overlay_color,
    })
}

// ---------------------------------------------------------------------------
// Win32 implementation
// ---------------------------------------------------------------------------

/// `HWND` of the open settings window (0 = closed). Touched only on the UI
/// thread; atomic purely to satisfy the compiler about shared access.
static OPEN_HWND: AtomicIsize = AtomicIsize::new(0);

/// Process-lifetime background brushes (window background, edit field
/// background), created once in `ensure_window_class` and deliberately never
/// deleted: the class brush must outlive every window of the class anyway.
static BG_BRUSH: AtomicIsize = AtomicIsize::new(0);
static EDIT_BRUSH: AtomicIsize = AtomicIsize::new(0);

fn brush_from_slot(slot: &AtomicIsize) -> HBRUSH {
    HBRUSH(std::ptr::with_exposed_provenance_mut::<core::ffi::c_void>(
        slot.load(Ordering::Acquire) as usize,
    ))
}

/// The white card background brush (a stock object — owned by the system).
fn card_brush() -> HBRUSH {
    // SAFETY: WHITE_BRUSH is a valid stock-object index; stock objects are
    // owned by the system and must NOT be deleted.
    unsafe { HBRUSH(GetStockObject(WHITE_BRUSH).0) }
}

// Child-control IDs (travel in the low word of WM_COMMAND's wParam).
const ID_REBIND_BASE: i32 = 100; // + gesture row index
const ID_RADIUS_CHECK_BASE: i32 = 300; // + modifier index
const ID_ZOOM_CHECK_BASE: i32 = 340; // + modifier index
const ID_NUMERIC_BASE: i32 = 400; // + numeric field index
const ID_COLOR_HEX: i32 = 450;
const ID_COLOR_SWATCH: i32 = 451;
const ID_AUTO_START_CHECK: i32 = 460;
const ID_SAVE: i32 = 500;
const ID_CANCEL: i32 = 501;
const ID_EXIT: i32 = 502;
// Statics send no commands; these IDs only steer the WM_DRAWITEM /
// WM_CTLCOLORSTATIC handlers. All card frames share one ID — each frame's
// DRAWITEMSTRUCT carries its own rect.
const ID_CARD_FRAME: i32 = 600;
const ID_CARD_CONTENT: i32 = 601;
const ID_CARD_SECONDARY: i32 = 602;

// Layout metrics in 96-DPI units (scaled by `px()` at creation). The layout
// trace lives in `build_ui`; CLIENT_H is its final y + BOTTOM_PAD.
const MARGIN: i32 = 20;
const CLIENT_W: i32 = 640;
const CLIENT_H: i32 = 754;
const CARD_W: i32 = CLIENT_W - 2 * MARGIN;
const CARD_PAD_X: i32 = 16;
const CARD_PAD_TOP: i32 = 10;
const CARD_PAD_BOTTOM: i32 = 10;
const CARD_TITLE_H: i32 = 16;
const CARD_TITLE_GAP: i32 = 6;
const CARD_GAP: i32 = 8;
const CARD_RADIUS: i32 = 8;
const CONTENT_X: i32 = MARGIN + CARD_PAD_X;
const CONTENT_W: i32 = CARD_W - 2 * CARD_PAD_X;
const WINDOW_TITLE_H: i32 = 22;
const WINDOW_TITLE_GAP: i32 = 8;
const ROW_H: i32 = 24;
const ROW_PITCH: i32 = 30;
const SECONDARY_H: i32 = 16;
const GAP: i32 = 8;
const SMALL_GAP: i32 = 4;
const LABEL_W: i32 = 250;
const GESTURE_EDIT_W: i32 = 130;
const REBIND_BTN_W: i32 = 84;
const MOD_LABEL_W: i32 = 200;
const CHECK_W: i32 = 78;
const CHECK_H: i32 = 16;
const CHECK_PITCH: i32 = 88;
const NUMERIC_LABEL_W: i32 = 330;
const NUMERIC_EDIT_W: i32 = 72;
const SWATCH_W: i32 = 44;
const HEX_EDIT_W: i32 = 92;
const HINT_H: i32 = 16;
const FOOTER_GAP: i32 = 6;
const BUTTON_H: i32 = 26;
const BUTTON_RADIUS: i32 = 5;
const SAVE_W: i32 = 92;
const CANCEL_W: i32 = 84;
const EXIT_W: i32 = 140;
const BOTTOM_PAD: i32 = 16;

/// Fixed-size, dialog-style: caption + system menu (close button), no sizing
/// border, no minimize/maximize.
const WINDOW_STYLE_FLAGS: WINDOW_STYLE =
    WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0);
const WINDOW_EX_STYLE_FLAGS: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

/// COLORREF (0x00BBGGRR) from 8-bit RGB channels.
const fn cref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

// The palette: a light neutral window, white cards with hairline edges,
// near-black primary text, and one accent blue reserved for Save.
const WINDOW_BG: COLORREF = cref(0xF5, 0xF5, 0xF7);
const CARD_BG: COLORREF = cref(0xFF, 0xFF, 0xFF);
const CARD_EDGE: COLORREF = cref(0xD2, 0xD2, 0xD7);
const TEXT_PRIMARY: COLORREF = cref(0x1D, 0x1D, 0x1F);
const TEXT_SECONDARY: COLORREF = cref(0x6E, 0x6E, 0x73);
const EDIT_BG: COLORREF = cref(0xF2, 0xF2, 0xF5);
const ACCENT: COLORREF = cref(0x00, 0x7A, 0xFF);
const ACCENT_DOWN: COLORREF = cref(0x00, 0x63, 0xE1);
const BUTTON_EDGE: COLORREF = cref(0xC7, 0xC7, 0xCC);
const SECONDARY_DOWN: COLORREF = cref(0xE8, 0xE8, 0xEC);
const DISABLED_FILL: COLORREF = cref(0xF0, 0xF0, 0xF3);
const DISABLED_TEXT: COLORREF = cref(0xAE, 0xAE, 0xB2);

const HINT_RED: u32 = 0x0000_00C8; // COLORREF 0x00BBGGRR — dark red

// winuser.h static-control styles the `windows` crate gates behind its
// (here disabled) Win32_System_SystemServices feature.
const SS_CENTERIMAGE_U32: u32 = 0x0200;
const SS_OWNERDRAW_U32: u32 = 0x000D;

/// All mutable state of one settings window. Owned by the window itself via
/// `GWLP_USERDATA`; created in `open()`, freed in `WM_NCDESTROY`.
struct SettingsWindowState {
    hwnd: HWND,
    /// Working copy — seeded from the caller, written back only via on_saved.
    settings: AppSettings,
    callbacks: SettingsCallbacks,
    gesture_edits: [HWND; GESTURE_ROW_COUNT],
    rebind_buttons: [HWND; GESTURE_ROW_COUNT],
    radius_checks: [HWND; MOD_CHECK_COUNT],
    zoom_checks: [HWND; MOD_CHECK_COUNT],
    numeric_edits: [HWND; NUMERIC_FIELD_COUNT],
    color_swatch: HWND,
    color_hex_edit: HWND,
    auto_start_check: HWND,
    hint_label: HWND,
    save_button: HWND,
    /// The window's DPI, captured in WM_CREATE; the owner-draw routines scale
    /// corner radii with it.
    dpi: u32,
    /// The font hierarchy, created in WM_CREATE.
    fonts: UiFonts,
    /// Row currently in modal key-capture state ("press keys…"), if any.
    capture_row: Option<usize>,
    /// The temporary LL keyboard hook, alive EXACTLY while `capture_row` is
    /// `Some`. RAII: dropping this guard uninstalls the hook, so no exit path
    /// (capture, cancel, close, destroy) can leak a system-wide hook.
    capture_hook: Option<CaptureKeyboardHook>,
    /// Modifier keys currently held, maintained by the capture hook from its
    /// own key events; re-seeded every time a capture arms.
    capture_modifiers: HeldModifiers,
}

impl Drop for SettingsWindowState {
    fn drop(&mut self) {
        for font in self.fonts.owned.drain(..) {
            // SAFETY: every handle in `owned` came from CreateFontIndirectW
            // (pushed at creation) and is deleted exactly once, here.
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    }
}

/// The window's font hierarchy, derived from the system message-box font
/// (Segoe UI on any supported Windows). Each font falls back to the stock
/// GUI font independently, so one failed creation cannot break the window;
/// only created fonts land in `owned` and are deleted with the state.
struct UiFonts {
    /// Control labels, edits, buttons, checkboxes.
    base: HFONT,
    /// The large window title.
    title: HFONT,
    /// Card section titles.
    section: HFONT,
    /// The created (non-stock) fonts, for Drop.
    owned: Vec<HFONT>,
}

impl Default for UiFonts {
    fn default() -> Self {
        let null = HFONT(std::ptr::null_mut());
        Self {
            base: null,
            title: null,
            section: null,
            owned: Vec::new(),
        }
    }
}

impl UiFonts {
    /// Create the hierarchy for `dpi`: base at the message-box size, the
    /// window title at 2x semibold, card section titles at 1.25x semibold.
    fn create(dpi: u32) -> Self {
        let mut fonts = Self::default();
        let base_lf = message_box_log_font(dpi);

        let mut title_lf = base_lf;
        title_lf.lfHeight = scale_font_height(base_lf.lfHeight, 2, 1);
        title_lf.lfWeight = FW_SEMIBOLD.0 as i32;

        let mut section_lf = base_lf;
        section_lf.lfHeight = scale_font_height(base_lf.lfHeight, 5, 4);
        section_lf.lfWeight = FW_SEMIBOLD.0 as i32;

        fonts.base = create_or_stock(&base_lf, &mut fonts.owned);
        fonts.title = create_or_stock(&title_lf, &mut fonts.owned);
        fonts.section = create_or_stock(&section_lf, &mut fonts.owned);
        fonts
    }
}

/// Scale a signed GDI font height (negative = em height) by `num / den`,
/// preserving the sign and never returning zero.
fn scale_font_height(height: i32, num: u32, den: u32) -> i32 {
    let magnitude =
        (i64::from(height.unsigned_abs()) * i64::from(num) / i64::from(den)).max(1) as i32;
    if height < 0 { -magnitude } else { magnitude }
}

/// The user's message-box font scaled to `dpi` (SystemParametersInfoW
/// reports it scaled to the SYSTEM dpi, so rescale by the ratio). Falls back
/// to a hand-built Segoe UI 9pt description if the metrics call fails.
fn message_box_log_font(dpi: u32) -> LOGFONTW {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is valid for writes of `cbSize` bytes, as the API
    // requires.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if result.is_err() {
        return fallback_log_font(dpi);
    }
    // SAFETY: GetDpiForSystem is safe to call from any thread.
    let system_dpi = unsafe { GetDpiForSystem() }.max(96);
    let mut lf = metrics.lfMessageFont;
    lf.lfHeight = scale_font_height(lf.lfHeight, dpi, system_dpi);
    lf
}

/// Segoe UI 9pt at `dpi`, used only when the system metrics are unavailable.
fn fallback_log_font(dpi: u32) -> LOGFONTW {
    let mut lf = LOGFONTW {
        lfHeight: scale_font_height(-12, dpi, 96),
        lfWeight: FW_NORMAL.0 as i32,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: CLEARTYPE_QUALITY,
        lfPitchAndFamily: VARIABLE_PITCH.0 | FF_SWISS.0,
        ..Default::default()
    };
    let face = wide("Segoe UI");
    lf.lfFaceName[..face.len()].copy_from_slice(&face);
    lf
}

/// Create one GDI font, recording it for Drop; on failure return the stock
/// GUI font (system-owned: never deleted, and NOT recorded).
fn create_or_stock(lf: &LOGFONTW, owned: &mut Vec<HFONT>) -> HFONT {
    // SAFETY: `lf` points to a valid LOGFONTW; DEFAULT_GUI_FONT is a valid
    // stock-object index.
    unsafe {
        let font = CreateFontIndirectW(lf);
        if font.is_invalid() {
            HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
        } else {
            owned.push(font);
            font
        }
    }
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
        // SAFETY: CreateSolidBrush cannot fail for a valid COLORREF. Both
        // brushes are process-lifetime by design (see BG_BRUSH).
        let (bg_brush, edit_brush) =
            unsafe { (CreateSolidBrush(WINDOW_BG), CreateSolidBrush(EDIT_BG)) };
        BG_BRUSH.store(bg_brush.0 as isize, Ordering::Release);
        EDIT_BRUSH.store(edit_brush.0 as isize, Ordering::Release);
        let class = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(settings_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: Default::default(),
            // SAFETY: IDC_ARROW is a valid system cursor resource ID.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            hbrBackground: bg_brush,
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("SpotFreezeSettingsWindow"),
        };
        // SAFETY: `class` is fully initialized; the class name is distinct.
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            outcome = Err(anyhow!(
                "RegisterClassW failed for SpotFreezeSettingsWindow"
            ));
        }
    });
    outcome
}

/// Pure decision behind the wndproc's `WM_ACTIVATE` arm: `wParam`'s low word
/// is the activation state — only `WA_INACTIVE` (the window is LOSING
/// activation) means an armed rebind capture must be disarmed; gaining
/// activation (`WA_ACTIVE` / `WA_CLICKACTIVE`, 1/2) must not touch it.
/// Factored out (and unit-tested below) so the deactivation→disarm mapping is
/// pinned without a window.
fn wm_activate_is_deactivation(wparam_low_word: u32) -> bool {
    wparam_low_word == WA_INACTIVE
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
        WM_ACTIVATE => {
            // D2: losing activation DISARMS an armed rebind capture. While a
            // capture is armed the temporary WH_KEYBOARD_LL hook swallows ALL
            // system keyboard input (and typed letters would bind invisibly),
            // so walking away from the window — alt-tab, clicking the tray,
            // or the app's own Exit-confirm / error MessageBox taking
            // foreground — must release it. end_capture_restore also restores
            // the row's old binding text and is a no-op when nothing is armed.
            // (ChooseColorW and the window's own Exit path already disarm
            // first; this arm is the catch-all for every other deactivation.)
            // SAFETY: GWLP_USERDATA holds the state pointer.
            unsafe {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !state.is_null() && wm_activate_is_deactivation((wparam.0 & 0xFFFF) as u32) {
                    end_capture_restore(&mut *state);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CREATE => {
            // SAFETY: GWLP_USERDATA holds the state pointer (set in NCCREATE).
            unsafe {
                let state =
                    &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState);
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
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if !state.is_null() {
                    on_command(state, wparam);
                }
                LRESULT(0)
            }
        }
        WM_DRAWITEM => {
            // SAFETY: GWLP_USERDATA holds the state pointer; with a control
            // ID in wParam, lParam points to a valid DRAWITEMSTRUCT.
            unsafe {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                let dis = lparam.0 as *const DRAWITEMSTRUCT;
                if state.is_null() || dis.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let id = (*dis).CtlID as i32;
                let handled = if id == ID_COLOR_SWATCH {
                    draw_color_swatch(&*state, &*dis);
                    true
                } else if id == ID_CARD_FRAME {
                    draw_card_frame(&*state, &*dis);
                    true
                } else if id == ID_SAVE {
                    draw_push_button(&*state, &*dis, ButtonRole::Accent);
                    true
                } else if id == ID_CANCEL
                    || id == ID_EXIT
                    || (ID_REBIND_BASE..ID_REBIND_BASE + GESTURE_ROW_COUNT as i32).contains(&id)
                {
                    draw_push_button(&*state, &*dis, ButtonRole::Secondary);
                    true
                } else {
                    false
                };
                if handled {
                    LRESULT(1) // we handled the drawing
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
        WM_CTLCOLOREDIT => {
            // Every edit (including the read-only gesture fields) is an inset
            // field: light gray fill, primary text.
            // SAFETY: wParam is the edit control's HDC, per Win32 docs.
            unsafe {
                let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
                SetTextColor(hdc, TEXT_PRIMARY);
                SetBkColor(hdc, EDIT_BG);
                LRESULT(brush_from_slot(&EDIT_BRUSH).0 as isize)
            }
        }
        WM_CTLCOLORSTATIC => {
            // SAFETY: GWLP_USERDATA holds the state pointer; wParam is the
            // HDC of the static (or checkbox) control and lParam its HWND,
            // per Win32 docs.
            unsafe {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
                if state.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
                let control = HWND(lparam.0 as *mut core::ffi::c_void);
                if control == (*state).hint_label {
                    SetTextColor(hdc, COLORREF(HINT_RED));
                    SetBkColor(hdc, WINDOW_BG);
                    return LRESULT(brush_from_slot(&BG_BRUSH).0 as isize);
                }
                // Checkboxes report through WM_CTLCOLORSTATIC too; everything
                // living on a card gets the card background, with the
                // secondary-text gray for ID_CARD_SECONDARY labels.
                let id = GetDlgCtrlID(control);
                let on_card = id == ID_CARD_CONTENT
                    || id == ID_CARD_SECONDARY
                    || (ID_RADIUS_CHECK_BASE..ID_RADIUS_CHECK_BASE + MOD_CHECK_COUNT as i32)
                        .contains(&id)
                    || (ID_ZOOM_CHECK_BASE..ID_ZOOM_CHECK_BASE + MOD_CHECK_COUNT as i32)
                        .contains(&id)
                    || id == ID_AUTO_START_CHECK;
                if on_card {
                    let text_color = if id == ID_CARD_SECONDARY {
                        TEXT_SECONDARY
                    } else {
                        TEXT_PRIMARY
                    };
                    SetTextColor(hdc, text_color);
                    SetBkColor(hdc, CARD_BG);
                    return LRESULT(card_brush().0 as isize);
                }
                // Window-level statics (the window title): primary text on
                // the window background.
                SetTextColor(hdc, TEXT_PRIMARY);
                SetBkColor(hdc, WINDOW_BG);
                LRESULT(brush_from_slot(&BG_BRUSH).0 as isize)
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
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindowState;
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

/// Create every child control, lay it out in card sections, seed it from the
/// working settings copy, and run the first validation pass. Called once from
/// WM_CREATE.
///
/// The layout runs in 96-DPI units, scaled by `px()` at creation; `y` walks
/// top to bottom and ends exactly at CLIENT_H - BOTTOM_PAD (fixup_client_size
/// has already sized the client area to CLIENT_W x CLIENT_H at this DPI).
/// Each card's frame static is created AFTER the card's controls — when its
/// exact bounds are known — and pushed beneath them (see create_card_frame).
///
/// SAFETY: `state.hwnd` must be a valid window during its WM_CREATE.
unsafe fn build_ui(state: &mut SettingsWindowState) -> Result<()> {
    // SAFETY: the whole body touches raw Win32 handles owned by `state.hwnd`
    // (valid during WM_CREATE) and stock objects; each step's preconditions
    // are documented at its call site.
    unsafe {
        let dpi = GetDpiForWindow(state.hwnd).max(96);
        state.dpi = dpi;
        fixup_client_size(state, dpi);

        let hinst = module_handle()?;
        state.fonts = UiFonts::create(dpi);
        let base_font = state.fonts.base;
        let parent = state.hwnd;

        let edit_style = WS_CHILD.0 | WS_VISIBLE.0 | ES_AUTOHSCROLL as u32;
        let button_style = WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_OWNERDRAW as u32;
        let checkbox_style = WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32;

        let mut y = MARGIN;

        // --- Window title -------------------------------------------------------
        create_label(
            "Settings",
            0,
            px(dpi, MARGIN),
            px(dpi, y),
            px(dpi, CLIENT_W - 2 * MARGIN),
            px(dpi, WINDOW_TITLE_H),
            parent,
            hinst,
            state.fonts.title,
        )?;
        y += WINDOW_TITLE_H + WINDOW_TITLE_GAP;

        // --- Card: Hotkeys ------------------------------------------------------
        let card_top = y;
        y += CARD_PAD_TOP;
        create_label(
            "Hotkeys",
            ID_CARD_CONTENT,
            px(dpi, CONTENT_X),
            px(dpi, y),
            px(dpi, CONTENT_W),
            px(dpi, CARD_TITLE_H),
            parent,
            hinst,
            state.fonts.section,
        )?;
        y += CARD_TITLE_H + CARD_TITLE_GAP;

        for (row, field) in GestureField::ALL.iter().enumerate() {
            let row_y = px(dpi, y);
            create_label(
                field.label(),
                ID_CARD_CONTENT,
                px(dpi, CONTENT_X),
                row_y,
                px(dpi, LABEL_W),
                px(dpi, ROW_H),
                parent,
                hinst,
                base_font,
            )?;
            let edit_x = CONTENT_X + LABEL_W + GAP;
            state.gesture_edits[row] = create_child(
                WC_EDITW,
                "",
                edit_style | ES_READONLY as u32 | ES_CENTER as u32,
                px(dpi, edit_x),
                row_y,
                px(dpi, GESTURE_EDIT_W),
                px(dpi, ROW_H),
                0,
                parent,
                hinst,
                base_font,
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
                base_font,
            )?;
            y += ROW_PITCH;
        }

        y += SMALL_GAP;
        create_label(
            "Tip: S toggles the spotlight, F toggles zoom hold, C enters capture, Esc backs out.",
            ID_CARD_SECONDARY,
            px(dpi, CONTENT_X),
            px(dpi, y),
            px(dpi, CONTENT_W),
            px(dpi, SECONDARY_H),
            parent,
            hinst,
            base_font,
        )?;
        y += SECONDARY_H + CARD_PAD_BOTTOM;
        create_card_frame(dpi, card_top, y, parent, hinst)?;
        y += CARD_GAP;

        // --- Card: Scroll-wheel modifiers ---------------------------------------
        let card_top = y;
        y += CARD_PAD_TOP;
        create_label(
            "Scroll-wheel modifiers",
            ID_CARD_CONTENT,
            px(dpi, CONTENT_X),
            px(dpi, y),
            px(dpi, CONTENT_W),
            px(dpi, CARD_TITLE_H),
            parent,
            hinst,
            state.fonts.section,
        )?;
        y += CARD_TITLE_H + CARD_TITLE_GAP;
        create_label(
            "Hold and scroll the wheel to resize the spotlight or zoom.",
            ID_CARD_SECONDARY,
            px(dpi, CONTENT_X),
            px(dpi, y),
            px(dpi, CONTENT_W),
            px(dpi, SECONDARY_H),
            parent,
            hinst,
            base_font,
        )?;
        y += SECONDARY_H + SMALL_GAP;

        let row_y = px(dpi, y);
        create_label(
            "Spotlight radius",
            ID_CARD_CONTENT,
            px(dpi, CONTENT_X),
            row_y,
            px(dpi, MOD_LABEL_W),
            px(dpi, ROW_H),
            parent,
            hinst,
            base_font,
        )?;
        for (i, label) in MOD_CHECK_LABELS.iter().enumerate() {
            state.radius_checks[i] = create_child(
                WC_BUTTONW,
                label,
                checkbox_style,
                px(dpi, CONTENT_X + MOD_LABEL_W + GAP + i as i32 * CHECK_PITCH),
                row_y + px(dpi, (ROW_H - CHECK_H) / 2),
                px(dpi, CHECK_W),
                px(dpi, CHECK_H),
                ID_RADIUS_CHECK_BASE + i as i32,
                parent,
                hinst,
                base_font,
            )?;
        }
        y += ROW_PITCH;

        let row_y = px(dpi, y);
        create_label(
            "Zoom",
            ID_CARD_CONTENT,
            px(dpi, CONTENT_X),
            row_y,
            px(dpi, MOD_LABEL_W),
            px(dpi, ROW_H),
            parent,
            hinst,
            base_font,
        )?;
        for (i, label) in MOD_CHECK_LABELS.iter().enumerate() {
            state.zoom_checks[i] = create_child(
                WC_BUTTONW,
                label,
                checkbox_style,
                px(dpi, CONTENT_X + MOD_LABEL_W + GAP + i as i32 * CHECK_PITCH),
                row_y + px(dpi, (ROW_H - CHECK_H) / 2),
                px(dpi, CHECK_W),
                px(dpi, CHECK_H),
                ID_ZOOM_CHECK_BASE + i as i32,
                parent,
                hinst,
                base_font,
            )?;
        }
        y += ROW_PITCH + CARD_PAD_BOTTOM;
        create_card_frame(dpi, card_top, y, parent, hinst)?;
        y += CARD_GAP;

        // --- Card: Options ------------------------------------------------------
        let card_top = y;
        y += CARD_PAD_TOP;
        create_label(
            "Options",
            ID_CARD_CONTENT,
            px(dpi, CONTENT_X),
            px(dpi, y),
            px(dpi, CONTENT_W),
            px(dpi, CARD_TITLE_H),
            parent,
            hinst,
            state.fonts.section,
        )?;
        y += CARD_TITLE_H + CARD_TITLE_GAP;

        for (i, field) in NumericField::ALL.iter().enumerate() {
            // ZoomMax's edit rides on ZoomMin's "Zoom range" row (created just
            // below), so it gets no row of its own.
            if *field == NumericField::ZoomMax {
                continue;
            }
            let row_y = px(dpi, y);
            create_label(
                field.label(),
                ID_CARD_CONTENT,
                px(dpi, CONTENT_X),
                row_y,
                px(dpi, NUMERIC_LABEL_W),
                px(dpi, ROW_H),
                parent,
                hinst,
                base_font,
            )?;
            let edit_x = CONTENT_X + NUMERIC_LABEL_W + GAP;
            state.numeric_edits[i] = create_child(
                WC_EDITW,
                "",
                edit_style | WS_TABSTOP.0 | ES_NUMBER as u32,
                px(dpi, edit_x),
                row_y,
                px(dpi, NUMERIC_EDIT_W),
                px(dpi, ROW_H),
                ID_NUMERIC_BASE + i as i32,
                parent,
                hinst,
                base_font,
            )?;
            if *field == NumericField::ZoomMin {
                // ZoomMax is the 4th field (index 3) in NumericField::ALL.
                state.numeric_edits[3] = create_child(
                    WC_EDITW,
                    "",
                    edit_style | WS_TABSTOP.0 | ES_NUMBER as u32,
                    px(dpi, edit_x + NUMERIC_EDIT_W + GAP),
                    row_y,
                    px(dpi, NUMERIC_EDIT_W),
                    px(dpi, ROW_H),
                    ID_NUMERIC_BASE + 3,
                    parent,
                    hinst,
                    base_font,
                )?;
            }
            y += ROW_PITCH;
        }

        // --- Overlay color: swatch button + live-parsed hex field ---------------
        {
            let row_y = px(dpi, y);
            create_label(
                "Overlay color (#RRGGBB)",
                ID_CARD_CONTENT,
                px(dpi, CONTENT_X),
                row_y,
                px(dpi, NUMERIC_LABEL_W),
                px(dpi, ROW_H),
                parent,
                hinst,
                base_font,
            )?;
            let swatch_x = CONTENT_X + NUMERIC_LABEL_W + GAP;
            state.color_swatch = create_child(
                WC_BUTTONW,
                "",
                button_style,
                px(dpi, swatch_x),
                row_y,
                px(dpi, SWATCH_W),
                px(dpi, ROW_H),
                ID_COLOR_SWATCH,
                parent,
                hinst,
                base_font,
            )?;
            state.color_hex_edit = create_child(
                WC_EDITW,
                "",
                edit_style | WS_TABSTOP.0,
                px(dpi, swatch_x + SWATCH_W + GAP),
                row_y,
                px(dpi, HEX_EDIT_W),
                px(dpi, ROW_H),
                ID_COLOR_HEX,
                parent,
                hinst,
                base_font,
            )?;
            y += ROW_PITCH;
        }

        state.auto_start_check = create_child(
            WC_BUTTONW,
            "Launch SpotFreeze at login (auto-start)",
            checkbox_style,
            px(dpi, CONTENT_X),
            px(dpi, y) + px(dpi, (ROW_H - CHECK_H) / 2),
            px(dpi, CONTENT_W),
            px(dpi, CHECK_H),
            ID_AUTO_START_CHECK,
            parent,
            hinst,
            base_font,
        )?;
        y += ROW_PITCH + CARD_PAD_BOTTOM;
        create_card_frame(dpi, card_top, y, parent, hinst)?;
        y += CARD_GAP;

        // --- Validation hint (red) ---------------------------------------------
        state.hint_label = create_label(
            "",
            0,
            px(dpi, MARGIN),
            px(dpi, y),
            px(dpi, CLIENT_W - 2 * MARGIN),
            px(dpi, HINT_H),
            parent,
            hinst,
            base_font,
        )?;
        y += HINT_H + FOOTER_GAP;

        // --- Footer: Exit subdued at the left, Cancel + the accent Save right ---
        let buttons_y = px(dpi, y);
        state.save_button = create_child(
            WC_BUTTONW,
            "Save",
            button_style,
            px(dpi, CLIENT_W - MARGIN - SAVE_W),
            buttons_y,
            px(dpi, SAVE_W),
            px(dpi, BUTTON_H),
            ID_SAVE,
            parent,
            hinst,
            base_font,
        )?;
        create_child(
            WC_BUTTONW,
            "Cancel",
            button_style,
            px(dpi, CLIENT_W - MARGIN - SAVE_W - GAP - CANCEL_W),
            buttons_y,
            px(dpi, CANCEL_W),
            px(dpi, BUTTON_H),
            ID_CANCEL,
            parent,
            hinst,
            base_font,
        )?;
        create_child(
            WC_BUTTONW,
            "Exit SpotFreeze",
            button_style,
            px(dpi, MARGIN),
            buttons_y,
            px(dpi, EXIT_W),
            px(dpi, BUTTON_H),
            ID_EXIT,
            parent,
            hinst,
            base_font,
        )?;

        debug_assert_eq!(
            y + BUTTON_H + BOTTOM_PAD,
            CLIENT_H,
            "the layout trace must fill the client area exactly"
        );

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

/// Create one child control and hand it `font`.
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
    // SAFETY: hwnd is a live child control; the font is state-owned and
    // outlives every control (deleted only in WM_NCDESTROY).
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

/// Create one single-line, vertically-centered label static. `id` steers its
/// colors in WM_CTLCOLORSTATIC (ID_CARD_CONTENT / ID_CARD_SECONDARY on cards,
/// 0 on the window background).
///
/// SAFETY: same contract as `create_child`.
#[allow(clippy::too_many_arguments)]
unsafe fn create_label(
    text: &str,
    id: i32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
) -> Result<HWND> {
    // SAFETY: same contract as `create_child`, upheld by the caller.
    unsafe {
        create_child(
            WC_STATICW,
            text,
            WS_CHILD.0 | WS_VISIBLE.0 | SS_CENTERIMAGE_U32,
            x,
            y,
            width,
            height,
            id,
            parent,
            hinst,
            font,
        )
    }
}

/// Create the owner-drawn card background spanning [card_top, card_bottom]
/// (96-DPI units) and push it to the bottom of the Z-order: it is created
/// AFTER the card's controls (when its exact bounds are known) but must paint
/// and hit-test beneath them.
///
/// SAFETY: `parent` must be a valid window; `hinst` our module handle.
unsafe fn create_card_frame(
    dpi: u32,
    card_top: i32,
    card_bottom: i32,
    parent: HWND,
    hinst: HINSTANCE,
) -> Result<()> {
    // SAFETY: `parent` and `hinst` are valid, per the caller's contract.
    let frame = unsafe {
        create_child(
            WC_STATICW,
            "",
            WS_CHILD.0 | WS_VISIBLE.0 | SS_OWNERDRAW_U32,
            px(dpi, MARGIN),
            px(dpi, card_top),
            px(dpi, CARD_W),
            px(dpi, card_bottom) - px(dpi, card_top),
            ID_CARD_FRAME,
            parent,
            hinst,
            HFONT(std::ptr::null_mut()),
        )
    }?;
    // SAFETY: `frame` is a live child window of `parent`.
    unsafe {
        let _ = SetWindowPos(
            frame,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    Ok(())
}

/// Fill every control from the working settings copy.
fn seed_controls(state: &SettingsWindowState) {
    for (row, field) in GestureField::ALL.iter().enumerate() {
        set_text(
            state.gesture_edits[row],
            &field.get(&state.settings.hotkeys).to_display(),
        );
    }
    seed_modifier_checks(
        &state.radius_checks,
        state.settings.hotkeys.spotlight_radius_modifier,
    );
    seed_modifier_checks(&state.zoom_checks, state.settings.hotkeys.zoom_modifier);
    for (i, field) in NumericField::ALL.iter().enumerate() {
        set_text(state.numeric_edits[i], &field.seed_text(&state.settings));
    }
    set_text(state.color_hex_edit, &state.settings.overlay.color.to_hex());
    repaint_swatch(state);
    set_checkbox(state.auto_start_check, state.settings.auto_start);
}

/// Tick a modifier-checkbox row to match a [`Modifiers`] set.
fn seed_modifier_checks(checks: &[HWND; MOD_CHECK_COUNT], modifiers: Modifiers) {
    for (i, checkbox) in checks.iter().enumerate() {
        set_checkbox(*checkbox, modifiers.contains(MOD_CHECK_MODS[i]));
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
        SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(usize::from(checked))), None);
    }
}

fn checkbox_checked(hwnd: HWND) -> bool {
    // SAFETY: hwnd is a live checkbox control.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 != 0 }
}

/// Read the current UI contents into a validation draft.
fn collect_draft(state: &SettingsWindowState) -> SettingsDraft {
    let mut hotkeys = state.settings.hotkeys.clone();

    hotkeys.spotlight_radius_modifier = read_modifier_checks(&state.radius_checks);
    hotkeys.zoom_modifier = read_modifier_checks(&state.zoom_checks);

    SettingsDraft {
        hotkeys,
        numerics: NumericDraft {
            spotlight_radius: read_text(state.numeric_edits[0]),
            zoom_step: read_text(state.numeric_edits[1]),
            zoom_min: read_text(state.numeric_edits[2]),
            zoom_max: read_text(state.numeric_edits[3]),
            dim_opacity: read_text(state.numeric_edits[4]),
        },
        color_hex: read_text(state.color_hex_edit),
        auto_start: checkbox_checked(state.auto_start_check),
    }
}

/// Combine a modifier-checkbox row into a [`Modifiers`] set.
fn read_modifier_checks(checks: &[HWND; MOD_CHECK_COUNT]) -> Modifiers {
    let mut modifiers = Modifiers::NONE;
    for (i, checkbox) in checks.iter().enumerate() {
        if checkbox_checked(*checkbox) {
            modifiers = modifiers | MOD_CHECK_MODS[i];
        }
    }
    modifiers
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
fn apply_valid_draft(settings: &mut AppSettings, draft: &SettingsDraft, parsed: ParsedDraft) {
    settings.hotkeys = draft.hotkeys.clone();
    settings.spotlight.default_radius = parsed.numerics.spotlight_radius;
    settings.zoom.step_factor = parsed.numerics.zoom_step;
    settings.zoom.min = parsed.numerics.zoom_min;
    settings.zoom.max = parsed.numerics.zoom_max;
    settings.overlay.dim_opacity = parsed.numerics.dim_opacity;
    settings.overlay.color = parsed.overlay_color;
    settings.auto_start = draft.auto_start;
}

// ---------------------------------------------------------------------------
// Rebind capture: temporary WH_KEYBOARD_LL hook (RAII-guarded)
// ---------------------------------------------------------------------------

/// RAII guard for the temporary low-level keyboard hook. The hook is
/// installed when a rebind capture is armed and uninstalled by `Drop`, so
/// EVERY exit path — successful capture, Esc cancel, window close,
/// `WM_NCDESTROY` — releases it. Holding the system-wide keyboard hooked
/// after the settings window is gone is impossible while this guard exists.
struct CaptureKeyboardHook {
    hhook: HHOOK,
}

impl CaptureKeyboardHook {
    /// Install a process-wide `WH_KEYBOARD_LL` hook on this thread.
    ///
    /// SAFETY/Win32 notes: for low-level hooks the callback runs in the
    /// INSTALLING thread's context (no DLL injection), so our own module
    /// handle is a valid `hMod`, and `dwThreadId == 0` hooks all threads.
    fn install(hinst: HINSTANCE) -> Result<Self> {
        // SAFETY: `capture_keyboard_proc` is a valid HOOKPROC that lives for
        // the whole process; `hinst` is our own module handle.
        let hhook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(capture_keyboard_proc), Some(hinst), 0)
        }
        .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;
        Ok(Self { hhook })
    }
}

impl Drop for CaptureKeyboardHook {
    fn drop(&mut self) {
        // SAFETY: `hhook` came from SetWindowsHookExW and is unhooked exactly
        // once, here. Safe to call from within the hook callback itself.
        unsafe {
            let _ = UnhookWindowsHookEx(self.hhook);
        }
    }
}

/// Low-level keyboard hook procedure, active only while a capture is armed.
///
/// Runs on the settings window's UI thread BEFORE the shell or any app sees
/// the key, which is what makes shell-reserved chords (`Win+F`, …) capturable.
/// While armed it swallows every keyboard event (modal capture) so the chord
/// never reaches the shell, the Start menu, or the focused app.
unsafe extern "system" fn capture_keyboard_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if ncode >= 0 {
        // SAFETY: with nCode >= 0, lParam points to a valid KBDLLHOOKSTRUCT.
        let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

        // Find the settings window's state. The hook is only installed while
        // a capture is armed, so a missing state means we were already
        // disarmed (race with teardown): pass the event through untouched.
        let hwnd_raw = OPEN_HWND.load(Ordering::Acquire);
        let state = if hwnd_raw != 0 {
            // SAFETY: `hwnd_raw` is our own settings window (it wrote the
            // slot); GWLP_USERDATA holds the state box for its lifetime.
            unsafe {
                GetWindowLongPtrW(hwnd_from_raw(hwnd_raw), GWLP_USERDATA)
                    as *mut SettingsWindowState
            }
        } else {
            std::ptr::null_mut()
        };
        // SAFETY: checked for null above and for capture_row below.
        if state.is_null() || unsafe { (*state).capture_row.is_none() } {
            return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
        }

        let key_down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
        // SAFETY: `state` is valid for the window's lifetime (freed only in
        // WM_NCDESTROY); we are on the UI thread.
        unsafe {
            (*state).capture_modifiers.apply(key_down, kbd.vkCode);
            let decision =
                decide_capture(key_down, kbd.vkCode, (*state).capture_modifiers.modifiers());
            match decision {
                CaptureDecision::KeepCapturing => {}
                CaptureDecision::Cancel => end_capture_restore(&mut *state),
                CaptureDecision::Bind(gesture) => end_capture_bind(&mut *state, gesture),
            }
        }
        // Modal capture: swallow every keyboard event while armed.
        return LRESULT(1);
    }
    // Contract: negative nCode must be passed straight on.
    unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
}

/// Enter the modal key-capture state for one gesture row: install the LL
/// hook and arm the row. On hook-install failure the row is NOT armed and the
/// red hint explains why — the previous binding stays untouched.
fn begin_capture(state: &mut SettingsWindowState, row: usize) {
    // Defensive: never stack two hooks.
    state.capture_hook = None;
    let hook = match module_handle().and_then(CaptureKeyboardHook::install) {
        Ok(hook) => hook,
        Err(err) => {
            state.capture_row = None;
            let gesture = GestureField::ALL[row].get(&state.settings.hotkeys);
            set_text(state.gesture_edits[row], &gesture.to_display());
            set_text(
                state.hint_label,
                &format!("could not start key capture: {err:#}"),
            );
            return;
        }
    };
    state.capture_modifiers = held_modifiers_snapshot();
    state.capture_hook = Some(hook);
    state.capture_row = Some(row);
    set_text(state.gesture_edits[row], "press keys\u{2026}"); // "press keys…"
}

/// Disarm the capture: take the row and drop the hook guard (uninstalling
/// the LL hook). Returns the row that was being captured, if any.
fn disarm_capture(state: &mut SettingsWindowState) -> Option<usize> {
    state.capture_hook = None; // Drop uninstalls the hook — this cannot leak.
    state.capture_row.take()
}

/// Leave capture mode WITHOUT rebinding, restoring the row's display.
fn end_capture_restore(state: &mut SettingsWindowState) {
    if let Some(row) = disarm_capture(state) {
        let gesture = GestureField::ALL[row].get(&state.settings.hotkeys);
        set_text(state.gesture_edits[row], &gesture.to_display());
    }
}

/// Leave capture mode by adopting `gesture` as the row's new binding.
fn end_capture_bind(state: &mut SettingsWindowState, gesture: HotkeyGesture) {
    if let Some(row) = disarm_capture(state) {
        GestureField::ALL[row].set(&mut state.settings.hotkeys, gesture);
        set_text(state.gesture_edits[row], &gesture.to_display());
        refresh_validation(state);
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
        EN_CHANGE
            if (ID_NUMERIC_BASE..ID_NUMERIC_BASE + NUMERIC_FIELD_COUNT as i32).contains(&id) =>
        {
            refresh_validation(state);
        }
        EN_CHANGE if id == ID_COLOR_HEX => {
            // Live hex parse: valid text repaints the swatch; either way the
            // draft re-validates (invalid hex => red hint + Save disabled).
            repaint_swatch(state);
            refresh_validation(state);
        }
        EN_SETFOCUS
            if (ID_NUMERIC_BASE..ID_NUMERIC_BASE + NUMERIC_FIELD_COUNT as i32).contains(&id) =>
        {
            // Clicking into another field abandons an ongoing capture.
            end_capture_restore(state);
        }
        EN_SETFOCUS if id == ID_COLOR_HEX => {
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
                    begin_capture(state, row);
                }
            }
            i if (ID_RADIUS_CHECK_BASE..ID_RADIUS_CHECK_BASE + MOD_CHECK_COUNT as i32)
                .contains(&i) =>
            {
                end_capture_restore(state);
                refresh_validation(state);
            }
            i if (ID_ZOOM_CHECK_BASE..ID_ZOOM_CHECK_BASE + MOD_CHECK_COUNT as i32).contains(&i) => {
                end_capture_restore(state);
                refresh_validation(state);
            }
            ID_AUTO_START_CHECK => {
                end_capture_restore(state);
                refresh_validation(state);
            }
            ID_COLOR_SWATCH => {
                // ChooseColorW is modal and pumps its own messages on this
                // thread: the LL hook MUST be disarmed first, or it would
                // swallow the dialog's keystrokes.
                end_capture_restore(state);
                pick_overlay_color(state);
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
                // The app shows its own modal confirmation; disarm the hook
                // first so its keystrokes reach that dialog.
                end_capture_restore(state);
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
            apply_valid_draft(&mut state.settings, &draft, parsed);
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

/// Snapshot the physically-held modifiers via GetAsyncKeyState, seeding the
/// capture's [`HeldModifiers`] when a capture arms. Called on the UI thread
/// OUTSIDE the hook callback, where the async key state is accurate (per
/// MSDN it cannot be sampled from inside a `WH_KEYBOARD_LL` callback); it
/// covers modifiers already held before the hook was installed.
fn held_modifiers_snapshot() -> HeldModifiers {
    // SAFETY: GetAsyncKeyState is safe to call for any VK on any thread.
    unsafe {
        HeldModifiers {
            lctrl: GetAsyncKeyState(VK_LCONTROL.0 as i32) < 0,
            rctrl: GetAsyncKeyState(VK_RCONTROL.0 as i32) < 0,
            lalt: GetAsyncKeyState(VK_LMENU.0 as i32) < 0,
            ralt: GetAsyncKeyState(VK_RMENU.0 as i32) < 0,
            lshift: GetAsyncKeyState(VK_LSHIFT.0 as i32) < 0,
            rshift: GetAsyncKeyState(VK_RSHIFT.0 as i32) < 0,
            lwin: GetAsyncKeyState(VK_LWIN.0 as i32) < 0,
            rwin: GetAsyncKeyState(VK_RWIN.0 as i32) < 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Owner-drawn visuals: card frames and push buttons (GDI)
// ---------------------------------------------------------------------------

/// Accent = the primary action (Save); Secondary = subdued actions (Cancel,
/// Exit, the Rebind buttons).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ButtonRole {
    Accent,
    Secondary,
}

/// Owner-draw routine for a card frame static: a white rounded card with a
/// 1px hairline edge.
///
/// SAFETY: `dis` is the DRAWITEMSTRUCT for a card frame static.
unsafe fn draw_card_frame(state: &SettingsWindowState, dis: &DRAWITEMSTRUCT) {
    // SAFETY: `dis.hDC`/`dis.rcItem` are valid for the duration of the draw;
    // the edge pen is created and deleted within this call, and the fill is
    // a stock brush (owned by the system, never deleted).
    unsafe {
        let edge = CreatePen(PS_SOLID, 1, CARD_EDGE);
        let old_pen = SelectObject(dis.hDC, edge.into());
        let old_brush = SelectObject(dis.hDC, GetStockObject(WHITE_BRUSH));
        let diameter = 2 * px(state.dpi, CARD_RADIUS);
        let _ = RoundRect(
            dis.hDC,
            dis.rcItem.left,
            dis.rcItem.top,
            dis.rcItem.right,
            dis.rcItem.bottom,
            diameter,
            diameter,
        );
        SelectObject(dis.hDC, old_pen);
        SelectObject(dis.hDC, old_brush);
        let _ = DeleteObject(edge.into());
    }
}

/// Owner-draw routine for the push buttons: a rounded rect — solid accent
/// blue for the primary action, white with a hairline edge for secondary
/// ones, grayed when disabled — with the label centered.
///
/// SAFETY: `dis` is the DRAWITEMSTRUCT for one of our owner-drawn buttons.
unsafe fn draw_push_button(state: &SettingsWindowState, dis: &DRAWITEMSTRUCT, role: ButtonRole) {
    let pressed = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let disabled = dis.itemState.0 & ODS_DISABLED.0 != 0;
    let focused = dis.itemState.0 & ODS_FOCUS.0 != 0;

    let (fill, text, edge) = match (role, disabled, pressed) {
        (ButtonRole::Accent, true, _) => (DISABLED_FILL, DISABLED_TEXT, DISABLED_FILL),
        (ButtonRole::Accent, false, true) => (ACCENT_DOWN, CARD_BG, ACCENT_DOWN),
        (ButtonRole::Accent, false, false) => (ACCENT, CARD_BG, ACCENT),
        (ButtonRole::Secondary, true, _) => (CARD_BG, DISABLED_TEXT, BUTTON_EDGE),
        (ButtonRole::Secondary, false, true) => (SECONDARY_DOWN, TEXT_PRIMARY, BUTTON_EDGE),
        (ButtonRole::Secondary, false, false) => (CARD_BG, TEXT_PRIMARY, BUTTON_EDGE),
    };

    // SAFETY: `dis.hDC`/`dis.rcItem` are valid for the duration of the draw;
    // the pen and brush are created, selected out, and deleted within this
    // call, and the state-owned font is only borrowed (never deleted here).
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, edge);
        let brush = CreateSolidBrush(fill);
        let old_pen = SelectObject(dis.hDC, pen.into());
        let old_brush = SelectObject(dis.hDC, brush.into());
        let diameter = 2 * px(state.dpi, BUTTON_RADIUS);
        let _ = RoundRect(
            dis.hDC,
            dis.rcItem.left,
            dis.rcItem.top,
            dis.rcItem.right,
            dis.rcItem.bottom,
            diameter,
            diameter,
        );
        SelectObject(dis.hDC, old_pen);
        SelectObject(dis.hDC, old_brush);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());

        let old_font = SelectObject(dis.hDC, state.fonts.base.into());
        SetBkMode(dis.hDC, TRANSPARENT);
        SetTextColor(dis.hDC, text);
        let label = read_text(dis.hwndItem);
        let mut label_wide = wide(&label);
        let label_len = label_wide.len().saturating_sub(1); // strip the NUL
        let mut rect = dis.rcItem;
        let _ = DrawTextW(
            dis.hDC,
            &mut label_wide[..label_len],
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(dis.hDC, old_font);

        if focused {
            let mut focus_rect = dis.rcItem;
            let inset = px(state.dpi, 3);
            let _ = InflateRect(&mut focus_rect, -inset, -inset);
            let _ = DrawFocusRect(dis.hDC, &focus_rect);
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay color: swatch, hex field, ChooseColorW picker
// ---------------------------------------------------------------------------

fn rgb_to_colorref(rgb: Rgb) -> COLORREF {
    // COLORREF is 0x00BBGGRR.
    COLORREF(u32::from(rgb.r) | (u32::from(rgb.g) << 8) | (u32::from(rgb.b) << 16))
}

fn colorref_to_rgb(color: COLORREF) -> Rgb {
    Rgb {
        r: (color.0 & 0xFF) as u8,
        g: ((color.0 >> 8) & 0xFF) as u8,
        b: ((color.0 >> 16) & 0xFF) as u8,
    }
}

/// The color currently described by the hex field, if it parses.
fn current_draft_color(state: &SettingsWindowState) -> Option<Rgb> {
    Rgb::parse_hex(&read_text(state.color_hex_edit)).ok()
}

/// Ask Windows to redraw the swatch (triggers WM_DRAWITEM).
fn repaint_swatch(state: &SettingsWindowState) {
    // SAFETY: color_swatch is a live child control.
    unsafe {
        let _ = InvalidateRect(Some(state.color_swatch), None, true);
    }
}

/// Owner-draw routine for the swatch button: a rounded chip filled with the
/// draft color (or the last valid color while the hex text is invalid), with
/// a hairline edge so light colors stay visible on the white card.
///
/// SAFETY: `dis` is the DRAWITEMSTRUCT for our swatch button.
unsafe fn draw_color_swatch(state: &SettingsWindowState, dis: &DRAWITEMSTRUCT) {
    let color = current_draft_color(state).unwrap_or(state.settings.overlay.color);
    // SAFETY: `dis.hDC`/`dis.rcItem` are valid for the duration of the draw;
    // the pen and brush are created, selected out, and deleted within this
    // call.
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, BUTTON_EDGE);
        let brush = CreateSolidBrush(rgb_to_colorref(color));
        let old_pen = SelectObject(dis.hDC, pen.into());
        let old_brush = SelectObject(dis.hDC, brush.into());
        let diameter = 2 * px(state.dpi, BUTTON_RADIUS);
        let _ = RoundRect(
            dis.hDC,
            dis.rcItem.left,
            dis.rcItem.top,
            dis.rcItem.right,
            dis.rcItem.bottom,
            diameter,
            diameter,
        );
        SelectObject(dis.hDC, old_pen);
        SelectObject(dis.hDC, old_brush);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());
    }
}

/// Open the common color picker seeded from the current draft color. On OK,
/// write the canonical hex into the hex edit — its EN_CHANGE handler then
/// repaints the swatch and re-validates, so picker→hex→swatch stays on one
/// sync path.
fn pick_overlay_color(state: &mut SettingsWindowState) {
    let current = current_draft_color(state).unwrap_or(state.settings.overlay.color);
    let mut custom_colors = [COLORREF(0); 16];
    let mut chooser = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: state.hwnd,
        rgbResult: rgb_to_colorref(current),
        lpCustColors: custom_colors.as_mut_ptr(),
        Flags: CC_FULLOPEN | CC_RGBINIT,
        ..Default::default()
    };
    // SAFETY: `chooser` is fully initialized; `custom_colors` outlives the
    // call, as required by CHOOSECOLORW.
    if unsafe { ChooseColorW(&mut chooser) }.as_bool() {
        let picked = colorref_to_rgb(chooser.rgbResult);
        set_text(state.color_hex_edit, &picked.to_hex());
    }
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
            gesture(Modifiers::CTRL, 0x46),                   // Ctrl+F
            gesture(Modifiers::NONE, 0x31),                   // 1
            gesture(Modifiers::CTRL | Modifiers::NONE, 0x46), // == gestures[0]
        ];
        assert_eq!(find_duplicate_gesture(&gestures), Some((0, 2)));
    }

    #[test]
    fn same_key_with_different_modifiers_is_not_a_duplicate() {
        let gestures = [
            gesture(Modifiers::CTRL, 0x43),                    // Ctrl+C
            gesture(Modifiers::CTRL | Modifiers::SHIFT, 0x43), // Ctrl+Shift+C
            gesture(Modifiers::NONE, 0x43),                    // C
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

    // --- GestureField rows cover every HotkeySettings gesture field ---------

    #[test]
    fn every_hotkey_gesture_field_has_a_row() {
        // One distinct sentinel per HotkeyGesture field of HotkeySettings.
        // Adding a field to HotkeySettings breaks this literal at compile
        // time until the field is listed here; listing it without a matching
        // GestureField row then fails the comparison below.
        let hotkeys = HotkeySettings {
            freeze_toggle: gesture(Modifiers::NONE, 0x70), // F1 … F7
            mode_spotlight: gesture(Modifiers::NONE, 0x71),
            mode_snip: gesture(Modifiers::NONE, 0x72),
            zoom_hold: gesture(Modifiers::NONE, 0x73),
            spotlight_radius_modifier: Modifiers::CTRL,
            zoom_modifier: Modifiers::SHIFT,
            snip_copy: gesture(Modifiers::NONE, 0x74),
            cancel: gesture(Modifiers::NONE, 0x75),
            reset_zoom: gesture(Modifiers::NONE, 0x76),
        };
        let field_gestures = [
            hotkeys.freeze_toggle,
            hotkeys.mode_spotlight,
            hotkeys.mode_snip,
            hotkeys.zoom_hold,
            hotkeys.snip_copy,
            hotkeys.cancel,
            hotkeys.reset_zoom,
        ];
        let row_gestures: Vec<HotkeyGesture> = GestureField::ALL
            .iter()
            .map(|field| field.get(&hotkeys))
            .collect();
        assert_eq!(
            row_gestures.len(),
            field_gestures.len(),
            "one GestureField row per HotkeyGesture field"
        );
        for field_gesture in field_gestures {
            assert!(
                row_gestures.contains(&field_gesture),
                "no GestureField row reads {field_gesture:?}"
            );
        }
        // Every row writes back to the field it reads (no crossed get/set arms).
        for field in GestureField::ALL {
            let sentinel = gesture(Modifiers::ALT, 0x77); // Alt+F8
            let mut h = hotkeys.clone();
            field.set(&mut h, sentinel);
            assert_eq!(field.get(&h), sentinel, "{field:?} set/get mismatch");
        }
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
        assert_eq!(
            parse_u32_field("Overlay dim opacity", "255", 0, 255),
            Ok(255)
        );
        assert!(parse_u32_field("Overlay dim opacity", "256", 0, 255).is_err());
    }

    #[test]
    fn f32_step_factor_min_is_exclusive() {
        assert!(parse_f32_field("Zoom step factor", "1.0", 1.0, true, 4.0).is_err());
        assert!(parse_f32_field("Zoom step factor", "0.99", 1.0, true, 4.0).is_err());
        assert_eq!(
            parse_f32_field("Zoom step factor", "1.25", 1.0, true, 4.0),
            Ok(1.25)
        );
        assert_eq!(
            parse_f32_field("Zoom step factor", "4.0", 1.0, true, 4.0),
            Ok(4.0)
        );
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
                mode_snip: gesture(Modifiers::NONE, 0x33),
                zoom_hold: gesture(Modifiers::NONE, 0x32),
                spotlight_radius_modifier: Modifiers::CTRL,
                zoom_modifier: Modifiers::SHIFT,
                snip_copy: gesture(Modifiers::CTRL, 0x43),
                cancel: gesture(Modifiers::NONE, 0x1B),
                reset_zoom: gesture(Modifiers::NONE, 0x30),
            },
            numerics: default_numerics(),
            color_hex: "#000000".to_string(),
            auto_start: false,
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
    fn zoom_hold_participates_in_duplicate_validation() {
        let mut draft = default_like_draft();
        draft.hotkeys.zoom_hold = draft.hotkeys.cancel;
        let err = validate_draft(&draft).unwrap_err();
        assert!(err.contains("both use"), "unexpected error: {err}");
        assert!(err.contains("Zoom hold toggle"), "names the row: {err}");
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

    // --- zoom modifier + overlay color validation --------------------------

    #[test]
    fn empty_zoom_modifier_is_rejected() {
        let mut draft = default_like_draft();
        draft.hotkeys.zoom_modifier = Modifiers::NONE;
        let err = validate_draft(&draft).unwrap_err();
        assert!(err.contains("Zoom modifier"), "unexpected error: {err}");
    }

    #[test]
    fn zoom_modifier_may_share_modifiers_with_a_full_gesture() {
        // Same contract nuance as the radius modifier: modifier-only bindings
        // are checked separately from full gestures.
        let draft = default_like_draft();
        assert_eq!(draft.hotkeys.zoom_modifier, Modifiers::SHIFT);
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn zoom_modifier_identical_to_radius_modifier_is_rejected() {
        // The two modifier-only bindings must differ: the same chord would
        // resize the spotlight AND zoom on one wheel scroll.
        let mut draft = default_like_draft();
        draft.hotkeys.zoom_modifier = draft.hotkeys.spotlight_radius_modifier;
        let err = validate_draft(&draft).unwrap_err();
        assert!(err.contains("both use"), "unexpected error: {err}");

        // Multi-modifier sets compare as sets, not per-flag.
        let mut draft = default_like_draft();
        draft.hotkeys.spotlight_radius_modifier = Modifiers::CTRL | Modifiers::SHIFT;
        draft.hotkeys.zoom_modifier = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(validate_draft(&draft).is_err());

        // Overlapping but DISTINCT sets are fine.
        let mut draft = default_like_draft();
        draft.hotkeys.spotlight_radius_modifier = Modifiers::CTRL;
        draft.hotkeys.zoom_modifier = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn multi_modifier_zoom_modifier_is_accepted() {
        let mut draft = default_like_draft();
        draft.hotkeys.zoom_modifier = Modifiers::ALT | Modifiers::SHIFT;
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn overlay_color_hex_is_validated() {
        let mut draft = default_like_draft();
        draft.color_hex = "#1A2B3C".to_string();
        let parsed = validate_draft(&draft).expect("valid hex must pass");
        assert_eq!(
            parsed.overlay_color,
            Rgb {
                r: 0x1A,
                g: 0x2B,
                b: 0x3C
            }
        );

        // Lowercase is accepted too (Rgb::parse_hex is case-insensitive).
        draft.color_hex = "#1a2b3c".to_string();
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn invalid_overlay_color_hex_disables_save() {
        for bad in ["", "black", "#12345", "#GG0000", "123456", "#1234567"] {
            let mut draft = default_like_draft();
            draft.color_hex = bad.to_string();
            let err = validate_draft(&draft).unwrap_err();
            assert!(
                err.contains("Overlay color"),
                "unexpected error for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn non_ascii_overlay_color_hex_disables_save_without_panicking() {
        // Regression (D1): multi-byte UTF-8 straddling an even byte boundary
        // used to panic Rgb::parse_hex through this exact validation path —
        // a hard UI crash on every keystroke producing such a string.
        for bad in ["#aébcd", "#abécd", "#abcdé", "#１２３４５６"] {
            let mut draft = default_like_draft();
            draft.color_hex = bad.to_string();
            let err = validate_draft(&draft).unwrap_err();
            assert!(
                err.contains("Overlay color"),
                "unexpected error for {bad:?}: {err}"
            );
        }
    }

    // --- auto_start checkbox (plain bool passthrough) ------------------------

    #[test]
    fn auto_start_does_not_affect_validation() {
        let mut draft = default_like_draft();
        assert!(validate_draft(&draft).is_ok());
        draft.auto_start = true;
        assert!(validate_draft(&draft).is_ok());
    }

    #[test]
    fn apply_valid_draft_transfers_auto_start_both_ways() {
        let checked = SettingsDraft {
            auto_start: true,
            ..default_like_draft()
        };
        let parsed = validate_draft(&checked).expect("draft validates");
        let mut settings = AppSettings::default();
        assert!(!settings.auto_start);

        apply_valid_draft(&mut settings, &checked, parsed);
        assert!(
            settings.auto_start,
            "ticked checkbox reaches the settings copy"
        );
        // Spot-check that the validated fields still transfer alongside.
        assert_eq!(settings.spotlight.default_radius, 150);
        assert_eq!(settings.overlay.color, Rgb::BLACK);

        apply_valid_draft(&mut settings, &default_like_draft(), parsed);
        assert!(
            !settings.auto_start,
            "unticked writes false too (toggle off)"
        );
    }

    // --- capture decision state machine (pure, no hook installed) ----------

    #[test]
    fn key_up_events_keep_capturing() {
        assert_eq!(
            decide_capture(false, 0x46, Modifiers::NONE),
            CaptureDecision::KeepCapturing
        );
        // Even Esc's key-UP must not cancel.
        assert_eq!(
            decide_capture(false, 0x1B, Modifiers::NONE),
            CaptureDecision::KeepCapturing
        );
    }

    #[test]
    fn bare_modifier_presses_keep_capturing() {
        for vk in [
            0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
        ] {
            assert_eq!(
                decide_capture(true, vk, Modifiers::NONE),
                CaptureDecision::KeepCapturing,
                "modifier vk {vk:#04X} must keep capturing"
            );
        }
    }

    #[test]
    fn esc_alone_cancels_capture() {
        assert_eq!(
            decide_capture(true, 0x1B, Modifiers::NONE),
            CaptureDecision::Cancel
        );
    }

    #[test]
    fn esc_with_a_modifier_binds_instead_of_cancelling() {
        assert_eq!(
            decide_capture(true, 0x1B, Modifiers::CTRL),
            CaptureDecision::Bind(gesture(Modifiers::CTRL, 0x1B))
        );
        assert_eq!(
            decide_capture(true, 0x1B, Modifiers::WIN),
            CaptureDecision::Bind(gesture(Modifiers::WIN, 0x1B))
        );
    }

    #[test]
    fn win_chords_bind_with_the_win_modifier() {
        // The headline bug: Win+F is shell-eaten for focus-based capture, but
        // the LL hook sees it and binds Win+F.
        assert_eq!(
            decide_capture(true, 0x46, Modifiers::WIN),
            CaptureDecision::Bind(gesture(Modifiers::WIN, 0x46))
        );
        let bound = decide_capture(true, 0x46, Modifiers::WIN | Modifiers::SHIFT);
        assert_eq!(
            bound,
            CaptureDecision::Bind(gesture(Modifiers::WIN | Modifiers::SHIFT, 0x46))
        );
        if let CaptureDecision::Bind(g) = bound {
            assert_eq!(g.to_display(), "Shift+Win+F");
        } else {
            panic!("expected Bind, got {bound:?}");
        }
    }

    #[test]
    fn plain_key_down_binds_verbatim() {
        assert_eq!(
            decide_capture(true, 0x53, Modifiers::NONE), // 'S'
            CaptureDecision::Bind(gesture(Modifiers::NONE, 0x53))
        );
        assert_eq!(
            decide_capture(true, 0x43, Modifiers::CTRL), // Ctrl+C
            CaptureDecision::Bind(gesture(Modifiers::CTRL, 0x43))
        );
    }

    // --- WM_ACTIVATE deactivation → disarm decision (D2) --------------------

    #[test]
    fn only_wa_inactive_is_a_deactivation() {
        // wParam's low word: 0 = WA_INACTIVE, 1 = WA_ACTIVE, 2 = WA_CLICKACTIVE.
        assert!(wm_activate_is_deactivation(0), "WA_INACTIVE must disarm");
        assert!(
            !wm_activate_is_deactivation(1),
            "WA_ACTIVE (gained focus) must not"
        );
        assert!(
            !wm_activate_is_deactivation(2),
            "WA_CLICKACTIVE (clicked) must not"
        );
        // The named constant the wndproc matches against is 0.
        assert_eq!(WA_INACTIVE, 0);
    }

    // --- modifiers_from_key_state ------------------------------------------

    #[test]
    fn modifier_state_assembles_all_combinations() {
        assert_eq!(
            modifiers_from_key_state(false, false, false, false),
            Modifiers::NONE
        );
        assert_eq!(
            modifiers_from_key_state(true, false, false, false),
            Modifiers::CTRL
        );
        assert_eq!(
            modifiers_from_key_state(false, true, false, false),
            Modifiers::ALT
        );
        assert_eq!(
            modifiers_from_key_state(false, false, true, false),
            Modifiers::SHIFT
        );
        assert_eq!(
            modifiers_from_key_state(false, false, false, true),
            Modifiers::WIN
        );
        assert_eq!(
            modifiers_from_key_state(true, true, true, true),
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::WIN
        );
        assert_eq!(
            modifiers_from_key_state(true, false, false, true),
            Modifiers::CTRL | Modifiers::WIN
        );
    }

    // --- HeldModifiers (the capture hook's own key-state tracking) ----------

    #[test]
    fn held_modifiers_press_and_release_each_modifier() {
        for (down_vk, up_vk, expected) in [
            (VK_LSHIFT.0, VK_RSHIFT.0, Modifiers::SHIFT),
            (VK_LCONTROL.0, VK_RCONTROL.0, Modifiers::CTRL),
            (VK_LMENU.0, VK_RMENU.0, Modifiers::ALT),
            (VK_LWIN.0, VK_RWIN.0, Modifiers::WIN),
        ] {
            for vk in [down_vk, up_vk] {
                let mut held = HeldModifiers::default();
                assert_eq!(held.modifiers(), Modifiers::NONE);
                held.apply(true, vk as u32);
                assert_eq!(held.modifiers(), expected, "press of vk {vk:#04X}");
                held.apply(false, vk as u32);
                assert_eq!(held.modifiers(), Modifiers::NONE, "release of vk {vk:#04X}");
            }
        }
    }

    #[test]
    fn held_modifiers_track_left_and_right_independently() {
        // Both sides down, one side up: the family flag must stay set.
        let mut held = HeldModifiers::default();
        held.apply(true, VK_LCONTROL.0 as u32);
        held.apply(true, VK_RCONTROL.0 as u32);
        held.apply(false, VK_LCONTROL.0 as u32);
        assert_eq!(held.modifiers(), Modifiers::CTRL);
        held.apply(false, VK_RCONTROL.0 as u32);
        assert_eq!(held.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn held_modifiers_accept_generic_vks_balanced() {
        // Synthetic input may use the side-less codes; down/up pairs stay
        // balanced by folding them into the left side.
        for (vk, expected) in [
            (VK_SHIFT.0, Modifiers::SHIFT),
            (VK_CONTROL.0, Modifiers::CTRL),
            (VK_MENU.0, Modifiers::ALT),
        ] {
            let mut held = HeldModifiers::default();
            held.apply(true, vk as u32);
            assert_eq!(held.modifiers(), expected, "press of vk {vk:#04X}");
            held.apply(false, vk as u32);
            assert_eq!(held.modifiers(), Modifiers::NONE, "release of vk {vk:#04X}");
        }
    }

    #[test]
    fn held_modifiers_ignore_non_modifier_vks() {
        let mut held = HeldModifiers::default();
        held.apply(true, 0x46); // 'F'
        held.apply(true, 0x1B); // Esc
        held.apply(false, 0x46);
        assert_eq!(held.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn tracked_modifiers_build_chords_for_decide_capture() {
        // The hook's event order: modifiers down, then the key. The tracker
        // snapshot at key-down time is the chord's modifier set.
        let mut held = HeldModifiers::default();
        held.apply(true, VK_LCONTROL.0 as u32);
        held.apply(true, VK_LWIN.0 as u32);
        assert_eq!(
            decide_capture(true, 0x46, held.modifiers()),
            CaptureDecision::Bind(gesture(Modifiers::CTRL | Modifiers::WIN, 0x46))
        );
        // Releasing a modifier before the key drops it from the chord.
        held.apply(false, VK_LWIN.0 as u32);
        assert_eq!(
            decide_capture(true, 0x46, held.modifiers()),
            CaptureDecision::Bind(gesture(Modifiers::CTRL, 0x46))
        );
        // Modifier key events themselves still never bind.
        assert_eq!(
            decide_capture(true, VK_LSHIFT.0 as u32, held.modifiers()),
            CaptureDecision::KeepCapturing
        );
    }

    #[test]
    fn esc_cancels_only_when_no_modifier_is_tracked() {
        let mut held = HeldModifiers::default();
        held.apply(true, VK_RSHIFT.0 as u32);
        assert_eq!(
            decide_capture(true, 0x1B, held.modifiers()),
            CaptureDecision::Bind(gesture(Modifiers::SHIFT, 0x1B))
        );
        held.apply(false, VK_RSHIFT.0 as u32);
        assert_eq!(
            decide_capture(true, 0x1B, held.modifiers()),
            CaptureDecision::Cancel
        );
    }

    // --- COLORREF <-> Rgb ---------------------------------------------------

    #[test]
    fn colorref_round_trip_preserves_channels() {
        for c in [
            Rgb::BLACK,
            Rgb {
                r: 255,
                g: 255,
                b: 255,
            },
            Rgb {
                r: 0x1A,
                g: 0x2B,
                b: 0x3C,
            },
        ] {
            assert_eq!(colorref_to_rgb(rgb_to_colorref(c)), c);
        }
        // Spot-check the 0x00BBGGRR byte order.
        assert_eq!(
            rgb_to_colorref(Rgb {
                r: 0x11,
                g: 0x22,
                b: 0x33
            })
            .0,
            0x0033_2211
        );
    }

    // --- scale_font_height (signed GDI font heights) -------------------------

    #[test]
    fn font_height_scaling_preserves_the_em_height_sign() {
        // Negative = em height (what CreateFontIndirectW wants); the sign
        // must survive DPI and ratio scaling.
        assert_eq!(scale_font_height(-12, 144, 96), -18);
        assert_eq!(scale_font_height(-12, 96, 96), -12);
        assert_eq!(scale_font_height(-12, 2, 1), -24);
        assert_eq!(scale_font_height(12, 144, 96), 18);
    }

    #[test]
    fn font_height_scaling_never_returns_zero() {
        // Rounding down to zero would produce an invisible font.
        assert_eq!(scale_font_height(-3, 1, 4), -1);
        assert_eq!(scale_font_height(1, 1, 4), 1);
    }
}
