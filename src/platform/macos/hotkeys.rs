//! Global freeze hotkey via Carbon's `RegisterEventHotKey`.
//!
//! Why Carbon in 2026: it is the only macOS global-hotkey API that works
//! WITHOUT Accessibility (AX) permission or an event-tap prompt — the
//! permissions story of this app is "Screen Recording only". The API is
//! deprecated but fully functional on macOS 14 and receives events through
//! the app's own event loop (no run-loop source, no thread).
//!
//! Hand-rolled FFI (no crate): the whole surface is five functions and two
//! structs from HIToolbox's CarbonEvents + Menus headers, linked from the
//! Carbon umbrella framework. All hotkeys share the app's `EventHotKeyID`
//! signature `'SPFZ'`; a single event handler on the application event
//! target dispatches every hot-key-pressed event to one Rust closure.
//!
//! Rebind semantics are register-first (mirroring the Windows shell): the
//! new [`CarbonHotkey`] is created while the old one is still active, and
//! the old registration drops only after the new one succeeded — a failed
//! rebind can never leave the app with NO freeze hotkey.
//!
//! Layout caveat: like every CGKeyCode consumer (see `surface.rs`),
//! keyCodes are ANSI-QWERTY positional, so bindings follow physical keys on
//! non-QWERTY layouts.

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::hotkeys::keymap;
use anyhow::{Result, anyhow, bail};
use std::ffi::c_void;
use std::ptr;

// ---------------------------------------------------------------------------
// Carbon FFI (HIToolbox), linked from the Carbon umbrella framework.
// ---------------------------------------------------------------------------

/// `OSStatus`.
type OSStatus = i32;
/// `EventTargetRef` — opaque.
type EventTargetRef = *mut c_void;
/// `EventRef` — opaque.
type EventRef = *mut c_void;
/// `EventHotKeyRef` — opaque.
type EventHotKeyRef = *mut c_void;
/// `EventHandlerRef` — opaque.
type EventHandlerRef = *mut c_void;
/// `EventHandlerCallRef` — opaque.
type EventHandlerCallRef = *mut c_void;
/// `EventHandlerUPP` — the event-handler callback pointer.
type EventHandlerUPP = unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

/// `EventHotKeyID` (CarbonEvents.h): an application signature plus an id.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

/// `EventTypeSpec` (CarbonEvents.h): an event class + kind pair.
#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_hot_key_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(in_hot_key: EventHotKeyRef) -> OSStatus;
    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: EventHandlerUPP,
        in_num_types: usize,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RemoveEventHandler(in_handler_ref: EventHandlerRef) -> OSStatus;
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn GetEventClass(in_event: EventRef) -> u32;
    fn GetEventKind(in_event: EventRef) -> u32;
    fn GetEventParameter(
        in_event: EventRef,
        in_param_name: u32,
        in_desired_type: u32,
        out_actual_type: *mut u32,
        in_buffer_size: usize,
        out_actual_size: *mut usize,
        out_data: *mut c_void,
    ) -> OSStatus;
}

// ---------------------------------------------------------------------------
// Carbon constants (Events.h / CarbonEvents.h / Menus.h).
// ---------------------------------------------------------------------------

/// `kEventClassKeyboard` (`'keyb'`).
const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
/// `kEventHotKeyPressed`.
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
/// `kEventParamDirectObject` (`'----'`) — the parameter carrying the
/// `EventHotKeyID` of the fired hotkey.
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
/// `typeEventHotKeyID` (`'hkid'`).
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");
/// `eventNotHandledErr`.
const EVENT_NOT_HANDLED: OSStatus = -9874;
/// This app's `EventHotKeyID` signature (`'SPFZ'`).
const HOTKEY_SIGNATURE: u32 = u32::from_be_bytes(*b"SPFZ");
/// `cmdKey` — the Command key, mapped from [`Modifiers::WIN`].
const CMD_KEY: u32 = 0x0100;
/// `shiftKey`.
const SHIFT_KEY: u32 = 0x0200;
/// `optionKey` — mapped from [`Modifiers::ALT`].
const OPTION_KEY: u32 = 0x0800;
/// `controlKey` — mapped from [`Modifiers::CTRL`].
const CONTROL_KEY: u32 = 0x1000;

/// The crate's [`Modifiers`] bits → Carbon `EventModifiers` flags.
/// Command stands in for Win/Super (the crate's cross-platform `WIN` slot).
fn carbon_modifiers(modifiers: Modifiers) -> u32 {
    let mut out = 0;
    if modifiers.contains(Modifiers::WIN) {
        out |= CMD_KEY;
    }
    if modifiers.contains(Modifiers::SHIFT) {
        out |= SHIFT_KEY;
    }
    if modifiers.contains(Modifiers::ALT) {
        out |= OPTION_KEY;
    }
    if modifiers.contains(Modifiers::CTRL) {
        out |= CONTROL_KEY;
    }
    out
}

// ---------------------------------------------------------------------------
// Manager + registration RAII.
// ---------------------------------------------------------------------------

/// Owns the single Carbon event handler and the boxed dispatch closure.
/// Install once for the app's lifetime; register/unregister hotkeys against
/// it. Dropping removes the handler and releases the closure.
pub struct CarbonHotkeyManager {
    handler: EventHandlerRef,
    /// `Box<Box<dyn Fn()>>` leaked as the handler's `user_data`; reclaimed in
    /// `Drop` (after `RemoveEventHandler`, so no callback can still run).
    callback: *mut c_void,
}

impl CarbonHotkeyManager {
    /// Install the event handler for hot-key-pressed events on the
    /// application target. `on_hotkey` runs on the main thread for every
    /// registered hotkey (the app has exactly one: the freeze toggle).
    pub fn install(on_hotkey: Box<dyn Fn()>) -> Result<Self> {
        // Double-box so `user_data` is a THIN pointer (a trait-object box is
        // a fat pointer and cannot round-trip through void*).
        let callback = Box::into_raw(Box::new(on_hotkey)) as *mut c_void;
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };
        let mut handler: EventHandlerRef = ptr::null_mut();
        // SAFETY: `spec` points to one EventTypeSpec (num_types = 1);
        // `callback` is a live Box<dyn Fn()> pointer kept until
        // RemoveEventHandler runs in Drop.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                carbon_event_handler,
                1,
                &spec,
                callback,
                &mut handler,
            )
        };
        if status != 0 || handler.is_null() {
            // SAFETY: the handler was not installed, so nothing can call the
            // callback; reclaim the box.
            drop(unsafe { Box::from_raw(callback as *mut Box<dyn Fn()>) });
            bail!("InstallEventHandler failed with OSStatus {status}");
        }
        Ok(Self { handler, callback })
    }

    /// Register `gesture` as a global hotkey. Errors when the key has no
    /// macOS keycode (F21–F24, PrintScreen) or when the OS refuses the
    /// combination (`eventHotKeyExistsErr` for a combo owned by another app).
    pub fn register(&self, gesture: HotkeyGesture) -> Result<CarbonHotkey> {
        let key_code = keymap::vk_to_cg_keycode(gesture.vk)
            .ok_or_else(|| anyhow!("the key {} has no macOS keycode", gesture.to_display()))?;
        let id = EventHotKeyID {
            signature: HOTKEY_SIGNATURE,
            id: 1,
        };
        let mut hotkey: EventHotKeyRef = ptr::null_mut();
        // SAFETY: `hotkey` is valid out-storage; all other parameters are
        // plain values.
        let status = unsafe {
            RegisterEventHotKey(
                key_code as u32,
                carbon_modifiers(gesture.modifiers),
                id,
                GetApplicationEventTarget(),
                0,
                &mut hotkey,
            )
        };
        if status != 0 || hotkey.is_null() {
            bail!(
                "RegisterEventHotKey for {} failed with OSStatus {status}",
                gesture.to_display()
            );
        }
        Ok(CarbonHotkey { hotkey })
    }
}

impl Drop for CarbonHotkeyManager {
    fn drop(&mut self) {
        // SAFETY: removing the handler we installed in `install`, exactly
        // once.
        unsafe {
            RemoveEventHandler(self.handler);
        }
        // SAFETY: with the handler removed no callback can still run; reclaim
        // the leaked box.
        drop(unsafe { Box::from_raw(self.callback as *mut Box<dyn Fn()>) });
    }
}

/// One registered global hotkey; unregisters on drop.
pub struct CarbonHotkey {
    hotkey: EventHotKeyRef,
}

impl Drop for CarbonHotkey {
    fn drop(&mut self) {
        // SAFETY: unregistering the ref handed out by RegisterEventHotKey,
        // exactly once.
        unsafe {
            UnregisterEventHotKey(self.hotkey);
        }
    }
}

/// The Carbon event handler: verify this is OUR hotkey (signature `'SPFZ'`)
/// and invoke the stored closure. Runs on the main thread inside the app's
/// event dispatch.
unsafe extern "C" fn carbon_event_handler(
    _call: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    // SAFETY: Carbon guarantees a valid event for the handler's duration;
    // user_data is the box installed by `install` (alive until after
    // RemoveEventHandler).
    unsafe {
        if event.is_null()
            || user_data.is_null()
            || GetEventClass(event) != K_EVENT_CLASS_KEYBOARD
            || GetEventKind(event) != K_EVENT_HOT_KEY_PRESSED
        {
            return EVENT_NOT_HANDLED;
        }
        let mut id = EventHotKeyID {
            signature: 0,
            id: 0,
        };
        let status = GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            size_of::<EventHotKeyID>(),
            ptr::null_mut(),
            &mut id as *mut EventHotKeyID as *mut c_void,
        );
        if status != 0 || id.signature != HOTKEY_SIGNATURE {
            return EVENT_NOT_HANDLED;
        }
        (*(user_data as *mut Box<dyn Fn()>))();
        0 // noErr: handled
    }
}

#[cfg(test)]
mod tests {
    //! Headless-safe: the mapping tables and FFI constants only — no
    //! registration, no event loop.
    use super::*;

    #[test]
    fn carbon_flags_match_the_crate_modifiers() {
        assert_eq!(carbon_modifiers(Modifiers::NONE), 0);
        assert_eq!(carbon_modifiers(Modifiers::WIN), CMD_KEY);
        assert_eq!(carbon_modifiers(Modifiers::SHIFT), SHIFT_KEY);
        assert_eq!(carbon_modifiers(Modifiers::ALT), OPTION_KEY);
        assert_eq!(carbon_modifiers(Modifiers::CTRL), CONTROL_KEY);
        assert_eq!(
            carbon_modifiers(Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::WIN),
            CMD_KEY | SHIFT_KEY | OPTION_KEY | CONTROL_KEY
        );
    }

    #[test]
    fn carbon_flag_values_match_events_h() {
        // Events.h modifier bit values.
        assert_eq!(CMD_KEY, 0x0100);
        assert_eq!(SHIFT_KEY, 0x0200);
        assert_eq!(OPTION_KEY, 0x0800);
        assert_eq!(CONTROL_KEY, 0x1000);
    }

    #[test]
    fn fourcc_constants_match_their_ascii_sources() {
        assert_eq!(K_EVENT_CLASS_KEYBOARD, 0x6B65_7962); // 'keyb'
        assert_eq!(K_EVENT_PARAM_DIRECT_OBJECT, 0x2D2D_2D2D); // '----'
        assert_eq!(TYPE_EVENT_HOT_KEY_ID, 0x686B_6964); // 'hkid'
        assert_eq!(HOTKEY_SIGNATURE, 0x5350_465A); // 'SPFZ'
    }

    #[test]
    fn hotkey_id_is_two_u32s_for_the_by_value_abi() {
        assert_eq!(size_of::<EventHotKeyID>(), 8);
        assert_eq!(align_of::<EventHotKeyID>(), 4);
    }
}
