//! wl_pointer / wl_keyboard translation into [`OverlayEvent`]s, with serial
//! and cursor-position tracking for the clipboard and copy fallback.
//!
//! # Routing model (differs from Windows — documented)
//!
//! On Wayland the compositor delivers pointer events to the surface under the
//! POINTER (pointer focus) and key events to the surface with KEYBOARD focus.
//! Both foci are learned from `enter` events:
//!
//! - `wl_pointer.enter/motion/button/axis*` → the entered surface's monitor,
//!   coordinates = surface-local LOGICAL × that output's integer scale →
//!   MONITOR-LOCAL PHYSICAL pixels (the crate contract). Wheel/axis events
//!   need no cursor-based rerouting (unlike `WM_MOUSEWHEEL`, which goes to the
//!   focus window): the compositor already targets the surface under the
//!   pointer.
//! - `wl_keyboard.key` → `xkb_state_key_get_one_sym` → xkb keysym → Win32 VK
//!   via [`crate::hotkeys::keymap`] → `OverlayEvent::KeyDown` to the focused
//!   surface's sink (the controller's `KeyDown` arm is deliberately inert —
//!   see its module docs) AND to the app's key listener, which matches the
//!   frozen-mode plan. Auto-repeat is suppressed: a key already tracked as
//!   pressed whose keymap entry repeats is not re-emitted.
//! - Modifiers come from `wl_keyboard.modifiers` through xkb state
//!   (`XKB_STATE_MODS_EFFECTIVE`; Shift/Ctrl/Mod1=Alt/Mod4=Super→Win) and ride
//!   along with every `KeyDown`/`MouseWheel` event.
//!
//! # Serials and cursor
//!
//! Every input event carrying a `serial` updates a shared cell: the clipboard
//! ([`super::clipboard`]) needs the serial of the key press that triggered the
//! copy to claim the selection. The pointer position is also tracked in
//! VIRTUAL PHYSICAL coordinates while the pointer is over an overlay, giving
//! the services object a cursor answer even though Wayland has no global
//! cursor query.
//!
//! Everything translatable is in small pure helpers (`AxisAccum`,
//! `modifiers_from_bools`, `local_physical`) unit-tested headless; the
//! xkb glue uses `xkbcommon-dl` (dlopened libxkbcommon — no build-time link).

use crate::geometry::{Point, Rect};
use crate::hotkeys::gesture::Modifiers;
use crate::hotkeys::keymap;
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::platform::wayland::shell::ShellState;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::rc::Rc;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use xkbcommon_dl::{
    XkbCommon, xkb_context, xkb_context_flags, xkb_key_direction, xkb_keymap,
    xkb_keymap_compile_flags, xkb_keymap_format, xkb_state, xkb_state_component, xkbcommon_option,
};

/// Linux input-event code for the left mouse button
/// (`<linux/input-event-codes.h>`).
const BTN_LEFT: u32 = 0x110;

/// evdev keycode offset: xkb keycodes are evdev codes + 8.
const EVDEV_TO_XKB: u32 = 8;

/// Raw wl_pointer axis units per wheel notch (libinput's wheel click angle is
/// 15 units; one notch = 120 delta units in the [`OverlayEvent`] contract).
const RAW_UNITS_PER_NOTCH: f64 = 15.0;

/// Input routing for one overlay surface: which monitor it is, where its
/// events go, and how to scale its surface-local logical coordinates.
pub(crate) struct SurfaceRegistration {
    pub monitor_index: usize,
    pub sink: OverlayEventSink,
    /// Integer output scale (surface-local logical × scale = physical).
    pub scale: u32,
    /// Monitor rect in VIRTUAL PHYSICAL pixels (cursor tracking).
    pub rect: Rect,
    /// Set while this surface holds keyboard focus (the focus fallback reads it).
    pub keyboard_focus: Rc<Cell<bool>>,
}

/// wl_surface ObjectId → input routing. Shared with the surfaces themselves
/// so `Drop` can unregister without access to [`ShellState`].
pub(crate) type SurfaceRegistry = Rc<RefCell<HashMap<ObjectId, Rc<SurfaceRegistration>>>>;

/// Per-frame accumulator for one pointer axis. Wayland sends, per axis per
/// frame: `axis` (raw, f64), optionally `axis_discrete` (v5) or
/// `axis_value120` (v8), then `frame`. Resolution priority:
/// `value120` → `discrete × 120` → `raw × (120 / 15)`.
#[derive(Default, Debug)]
pub(crate) struct AxisAccum {
    value120: i32,
    discrete: i32,
    raw: f64,
}

impl AxisAccum {
    pub fn add_raw(&mut self, value: f64) {
        self.raw += value;
    }

    pub fn add_discrete(&mut self, steps: i32) {
        self.discrete += steps;
    }

    pub fn add_value120(&mut self, value: i32) {
        self.value120 += value;
    }

    /// The frame's delta in the [`OverlayEvent::MouseWheel`] contract
    /// (120 per notch, positive = wheel up/away). Wayland's axis values are
    /// positive when scrolling DOWN, so every branch negates. `None` when no
    /// scroll happened this frame. Sub-notch deltas (high-resolution wheels,
    /// touchpads) pass through unclamped — consumers accumulate them
    /// fractionally (see the `MouseWheel` docs).
    pub fn resolve(&self) -> Option<i32> {
        if self.value120 != 0 {
            Some(-self.value120)
        } else if self.discrete != 0 {
            Some(-self.discrete * 120)
        } else if self.raw != 0.0 {
            Some((-self.raw * (120.0 / RAW_UNITS_PER_NOTCH)).round() as i32)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Modifier bits from the four xkb mod states we track. Pure; unit-tested.
pub(crate) fn modifiers_from_bools(shift: bool, ctrl: bool, alt: bool, logo: bool) -> Modifiers {
    let mut m = Modifiers::NONE;
    if shift {
        m = m | Modifiers::SHIFT;
    }
    if ctrl {
        m = m | Modifiers::CTRL;
    }
    if alt {
        m = m | Modifiers::ALT;
    }
    if logo {
        m = m | Modifiers::WIN;
    }
    m
}

/// Surface-local LOGICAL coordinate → MONITOR-LOCAL PHYSICAL (rounded).
/// Pure; unit-tested.
pub(crate) fn local_physical(surface_local: f64, scale: u32) -> i32 {
    (surface_local * scale.max(1) as f64).round() as i32
}

/// RAII wrapper over the dlopened libxkbcommon context/keymap/state trio.
struct Xkb {
    context: *mut xkb_context,
    keymap: *mut xkb_keymap,
    state: *mut xkb_state,
}

impl Xkb {
    /// Compile a keymap from the compositor's `wl_keyboard.keymap` payload
    /// (mmap'd XKB text) and create the tracking state. `None` when
    /// libxkbcommon or the compile fails (keys are then ignored).
    fn from_mmap(ptr: *const c_void) -> Option<Self> {
        let xkb: &XkbCommon = xkbcommon_option()?;
        // SAFETY: every call targets a valid dlopened symbol; `ptr` is a live
        // NUL-terminated keymap string (compositor-written); null returns are
        // checked before any further use.
        unsafe {
            let context = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            if context.is_null() {
                return None;
            }
            let keymap = (xkb.xkb_keymap_new_from_string)(
                context,
                ptr.cast(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if keymap.is_null() {
                (xkb.xkb_context_unref)(context);
                return None;
            }
            let state = (xkb.xkb_state_new)(keymap);
            if state.is_null() {
                (xkb.xkb_keymap_unref)(keymap);
                (xkb.xkb_context_unref)(context);
                return None;
            }
            Some(Self {
                context,
                keymap,
                state,
            })
        }
    }

    /// `xkb_state_update_key` for an evdev keycode (converted internally).
    fn update_key(&mut self, evdev_code: u32, direction: xkb_key_direction) {
        let Some(xkb) = xkbcommon_option() else { return };
        // SAFETY: `state` is a live xkb_state owned by `self`.
        unsafe {
            (xkb.xkb_state_update_key)(self.state, evdev_code + EVDEV_TO_XKB, direction);
        }
    }

    /// `xkb_state_update_mask` from a wl_keyboard.modifiers event.
    fn update_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(xkb) = xkbcommon_option() else { return };
        // SAFETY: `state` is a live xkb_state owned by `self`.
        unsafe {
            (xkb.xkb_state_update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    /// The single keysym an evdev keycode currently resolves to.
    fn one_sym(&mut self, evdev_code: u32) -> u32 {
        let Some(xkb) = xkbcommon_option() else { return 0 };
        // SAFETY: `state` is a live xkb_state owned by `self`.
        unsafe { (xkb.xkb_state_key_get_one_sym)(self.state, evdev_code + EVDEV_TO_XKB) }
    }

    /// Whether the key repeats under auto-repeat (for repeat suppression).
    fn key_repeats(&mut self, evdev_code: u32) -> bool {
        let Some(xkb) = xkbcommon_option() else { return false };
        // SAFETY: `keymap` is a live xkb_keymap owned by `self`.
        unsafe { (xkb.xkb_keymap_key_repeats)(self.keymap, evdev_code + EVDEV_TO_XKB) != 0 }
    }

    /// Whether a named modifier is effectively active.
    fn mod_active(&mut self, name: &'static std::ffi::CStr) -> bool {
        let Some(xkb) = xkbcommon_option() else { return false };
        // SAFETY: `state` is live; `name` is a static NUL-terminated string.
        unsafe {
            (xkb.xkb_state_mod_name_is_active)(
                self.state,
                name.as_ptr(),
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) != 0
        }
    }

    /// Current modifier set in the crate's [`Modifiers`] bits.
    fn modifiers(&mut self) -> Modifiers {
        modifiers_from_bools(
            self.mod_active(c"Shift"),
            self.mod_active(c"Control"),
            self.mod_active(c"Mod1"),
            self.mod_active(c"Mod4"),
        )
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        let Some(xkb) = xkbcommon_option() else { return };
        // SAFETY: the three handles are live, owned by `self`, released once.
        unsafe {
            (xkb.xkb_state_unref)(self.state);
            (xkb.xkb_keymap_unref)(self.keymap);
            (xkb.xkb_context_unref)(self.context);
        }
    }
}

/// Seat input state: devices, foci, xkb state, serial + cursor tracking.
pub(crate) struct InputState {
    /// Overlay surface routing, keyed by wl_surface ObjectId.
    pub registry: SurfaceRegistry,
    /// Latest serial from ANY input event (the clipboard's set_selection
    /// serial). Shared with the services object.
    pub serial: Rc<Cell<u32>>,
    /// Cursor position in VIRTUAL PHYSICAL pixels while the pointer is over
    /// one of our surfaces; `None` otherwise. Shared with the services object.
    pub cursor_virtual: Rc<Cell<Option<Point>>>,
    /// App hook for `KeyDown` (vk + modifiers): the app matches the
    /// frozen-mode plan. Fires for every non-repeat key press, focus or not.
    pub key_listener: Option<Rc<dyn Fn(u32, Modifiers)>>,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_version: u32,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Pointer-focus surface + last pointer position (monitor-local physical).
    pointer_focus: Option<Rc<SurfaceRegistration>>,
    pointer_local: Point,
    /// Keyboard-focus surface (key event routing).
    keyboard_focus: Option<Rc<SurfaceRegistration>>,
    xkb: Option<Xkb>,
    /// evdev codes currently held (repeat suppression).
    pressed: HashSet<u32>,
    modifiers: Modifiers,
    /// Per-axis frame accumulators: [vertical, horizontal].
    axes: [AxisAccum; 2],
}

impl InputState {
    pub(crate) fn new() -> Self {
        Self {
            registry: Rc::new(RefCell::new(HashMap::new())),
            serial: Rc::new(Cell::new(0)),
            cursor_virtual: Rc::new(Cell::new(None)),
            key_listener: None,
            pointer: None,
            pointer_version: 0,
            keyboard: None,
            pointer_focus: None,
            pointer_local: Point::default(),
            keyboard_focus: None,
            xkb: None,
            pressed: HashSet::new(),
            modifiers: Modifiers::NONE,
            axes: [AxisAccum::default(), AxisAccum::default()],
        }
    }

    fn note_serial(&self, serial: u32) {
        self.serial.set(serial);
    }

    /// Look up the registration for a wl_surface proxy.
    fn registration(&self, surface: &wl_surface::WlSurface) -> Option<Rc<SurfaceRegistration>> {
        self.registry.borrow().get(&surface.id()).cloned()
    }

    /// Pointer position in monitor-local physical pixels for a surface-local
    /// logical coordinate; also updates the virtual cursor tracker.
    fn track_pointer(&mut self, reg: &SurfaceRegistration, sx: f64, sy: f64) -> Point {
        let local = Point::new(local_physical(sx, reg.scale), local_physical(sy, reg.scale));
        self.pointer_local = local;
        self.cursor_virtual
            .set(Some(Point::new(reg.rect.x + local.x, reg.rect.y + local.y)));
        local
    }

    fn emit_pointer(&self, event: OverlayEvent) {
        if let Some(focus) = &self.pointer_focus {
            (focus.sink)(focus.monitor_index, event);
        }
    }

    /// Emit the frame's accumulated vertical wheel delta (horizontal scroll
    /// has no consumer in the overlay contract — dropped by design).
    fn flush_axis_frame(&mut self) {
        if let Some(delta) = self.axes[0].resolve() {
            self.emit_pointer(OverlayEvent::MouseWheel {
                at: self.pointer_local,
                delta,
                modifiers: self.modifiers,
            });
        }
        self.axes[0].clear();
        self.axes[1].clear();
    }

    fn load_keymap(&mut self, format: WEnum<wl_keyboard::KeymapFormat>, fd: std::os::unix::io::OwnedFd, size: u32) {
        self.xkb = None;
        let WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) = format else {
            return; // NoKeymap or unknown: keys stay unmapped
        };
        // mmap the keymap fd read-only, compile, unmap.
        let len = size as usize;
        if len == 0 {
            return;
        }
        // SAFETY: `fd` is a valid compositor-owned keymap fd of `len` bytes;
        // the mapping is unmapped before returning.
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE,
                &fd,
                0,
            )
        };
        let Ok(ptr) = ptr else { return };
        self.xkb = Xkb::from_mmap(ptr);
        // SAFETY: `ptr`/`len` came from the mmap above; unmapped once.
        unsafe {
            let _ = rustix::mm::munmap(ptr, len);
        }
        if self.xkb.is_none() {
            eprintln!("spotfreeze: could not compile the compositor's keymap; frozen-mode keys will not work");
        }
    }

    fn key_pressed(&mut self, key: u32) {
        let Some(xkb) = &mut self.xkb else { return };
        let is_repeat = self.pressed.contains(&key) && xkb.key_repeats(key);
        xkb.update_key(key, xkb_key_direction::XKB_KEY_DOWN);
        self.pressed.insert(key);
        if is_repeat {
            return; // suppress auto-repeat re-emission
        }
        let Some(vk) = keymap::xkb_to_vk(xkb.one_sym(key)) else {
            return; // key outside the gesture vocabulary (e.g. bare modifiers)
        };
        let modifiers = self.modifiers;
        if let Some(focus) = &self.keyboard_focus {
            (focus.sink)(focus.monitor_index, OverlayEvent::KeyDown { vk, modifiers });
        }
        if let Some(listener) = &self.key_listener {
            listener(vk, modifiers);
        }
    }

    fn key_released(&mut self, key: u32) {
        self.pressed.remove(&key);
        if let Some(xkb) = &mut self.xkb {
            xkb.update_key(key, xkb_key_direction::XKB_KEY_UP);
        }
    }

    fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(xkb) = &mut self.xkb else { return };
        xkb.update_mask(depressed, latched, locked, group);
        self.modifiers = xkb.modifiers();
    }
}

// ---------------------------------------------------------------------------
// Dispatch: wl_seat capabilities create/destroy the devices; wl_pointer and
// wl_keyboard events translate into OverlayEvents as documented above.
// ---------------------------------------------------------------------------

impl Dispatch<wl_seat::WlSeat, ()> for ShellState {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(caps) = capabilities else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && state.input.pointer.is_none() {
            let pointer = proxy.get_pointer(qhandle, ());
            state.input.pointer_version = pointer.version();
            state.input.pointer = Some(pointer);
        } else if !caps.contains(wl_seat::Capability::Pointer) {
            state.input.pointer = None;
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.input.keyboard.is_none() {
            state.input.keyboard = Some(proxy.get_keyboard(qhandle, ()));
        } else if !caps.contains(wl_seat::Capability::Keyboard) {
            state.input.keyboard = None;
            state.input.xkb = None;
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for ShellState {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let input = &mut state.input;
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                input.note_serial(serial);
                let Some(reg) = input.registration(&surface) else {
                    return;
                };
                let at = input.track_pointer(&reg, surface_x, surface_y);
                input.pointer_focus = Some(reg.clone());
                (reg.sink)(reg.monitor_index, OverlayEvent::MouseMove { at });
            }
            wl_pointer::Event::Leave { serial, .. } => {
                input.note_serial(serial);
                input.pointer_focus = None;
                input.cursor_virtual.set(None);
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                let Some(focus) = input.pointer_focus.clone() else {
                    return;
                };
                let at = input.track_pointer(&focus, surface_x, surface_y);
                (focus.sink)(focus.monitor_index, OverlayEvent::MouseMove { at });
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                input.note_serial(serial);
                if button != BTN_LEFT {
                    return;
                }
                let at = input.pointer_local;
                match button_state {
                    wl_pointer::ButtonState::Pressed => {
                        input.emit_pointer(OverlayEvent::LeftButtonDown { at })
                    }
                    wl_pointer::ButtonState::Released => {
                        input.emit_pointer(OverlayEvent::LeftButtonUp { at })
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let slot = axis_slot(axis);
                input.axes[slot].add_raw(value);
                if input.pointer_version < 5 {
                    // No frame/discrete events below v5: resolve each raw
                    // event on its own (same 15-units-per-notch rule).
                    let mut single = AxisAccum::default();
                    single.add_raw(value);
                    input.axes[slot].clear();
                    if slot == 0
                        && let Some(delta) = single.resolve()
                    {
                        let at = input.pointer_local;
                        let modifiers = input.modifiers;
                        input.emit_pointer(OverlayEvent::MouseWheel {
                            at,
                            delta,
                            modifiers,
                        });
                    }
                }
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                input.axes[axis_slot(axis)].add_discrete(discrete);
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                input.axes[axis_slot(axis)].add_value120(value120);
            }
            wl_pointer::Event::Frame => {
                input.flush_axis_frame();
            }
            // AxisSource / AxisStop carry no delta information we use.
            _ => {}
        }
    }
}

/// Vertical = slot 0, horizontal = slot 1 (unknown axes fold into 1, which is
/// never emitted).
fn axis_slot(axis: WEnum<wl_pointer::Axis>) -> usize {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => 0,
        _ => 1,
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ShellState {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let input = &mut state.input;
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                input.load_keymap(format, fd, size);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                input.note_serial(serial);
                if let Some(reg) = input.registration(&surface) {
                    reg.keyboard_focus.set(true);
                    input.keyboard_focus = Some(reg);
                }
            }
            wl_keyboard::Event::Leave { serial, .. } => {
                input.note_serial(serial);
                if let Some(focus) = input.keyboard_focus.take() {
                    focus.keyboard_focus.set(false);
                }
                input.pressed.clear();
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                input.note_serial(serial);
                match key_state {
                    wl_keyboard::KeyState::Pressed => input.key_pressed(key),
                    wl_keyboard::KeyState::Released => input.key_released(key),
                    _ => {}
                }
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                input.note_serial(serial);
                input.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            // RepeatInfo: rate/delay are compositor policy; repeats are
            // suppressed via pressed-key tracking instead.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — headless: axis normalization, modifier mapping, coordinate scaling.
// No Wayland connection, no xkb library, no keymap fd.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- AxisAccum ----

    #[test]
    fn axis_value120_wins_over_discrete_and_raw() {
        let mut a = AxisAccum::default();
        a.add_raw(15.0);
        a.add_discrete(1);
        a.add_value120(120);
        assert_eq!(a.resolve(), Some(-120));
    }

    #[test]
    fn axis_discrete_beats_raw_and_scales_by_120() {
        let mut a = AxisAccum::default();
        a.add_raw(15.0);
        a.add_discrete(2);
        assert_eq!(a.resolve(), Some(-240));
        let mut a = AxisAccum::default();
        a.add_discrete(-1);
        assert_eq!(a.resolve(), Some(120));
    }

    #[test]
    fn axis_raw_normalizes_at_15_units_per_notch() {
        let mut a = AxisAccum::default();
        a.add_raw(15.0);
        assert_eq!(a.resolve(), Some(-120), "one notch down = -120");
        let mut a = AxisAccum::default();
        a.add_raw(-15.0);
        assert_eq!(a.resolve(), Some(120), "one notch up = +120");
    }

    #[test]
    fn axis_raw_accumulates_within_a_frame() {
        let mut a = AxisAccum::default();
        a.add_raw(10.0);
        a.add_raw(5.0);
        assert_eq!(a.resolve(), Some(-120));
    }

    #[test]
    fn axis_sub_notch_deltas_pass_through() {
        // Smooth-scroll hardware: small raw values must not truncate to 0.
        let mut a = AxisAccum::default();
        a.add_raw(3.75);
        assert_eq!(a.resolve(), Some(-30));
    }

    #[test]
    fn axis_value120_sub_notch_passes_through() {
        let mut a = AxisAccum::default();
        a.add_value120(30);
        assert_eq!(a.resolve(), Some(-30), "quarter notch down");
        let mut a = AxisAccum::default();
        a.add_value120(-240);
        assert_eq!(a.resolve(), Some(240), "two notches up");
    }

    #[test]
    fn axis_empty_frame_resolves_to_none() {
        assert_eq!(AxisAccum::default().resolve(), None);
    }

    #[test]
    fn axis_clear_resets_everything() {
        let mut a = AxisAccum::default();
        a.add_raw(15.0);
        a.add_discrete(1);
        a.add_value120(120);
        a.clear();
        assert_eq!(a.resolve(), None);
    }

    // ---- modifiers_from_bools ----

    #[test]
    fn modifiers_map_shift_ctrl_alt_logo() {
        assert_eq!(
            modifiers_from_bools(true, true, true, true),
            Modifiers::SHIFT | Modifiers::CTRL | Modifiers::ALT | Modifiers::WIN
        );
        assert_eq!(modifiers_from_bools(false, false, false, false), Modifiers::NONE);
        assert_eq!(modifiers_from_bools(true, false, false, false), Modifiers::SHIFT);
        assert_eq!(modifiers_from_bools(false, true, false, false), Modifiers::CTRL);
        assert_eq!(modifiers_from_bools(false, false, true, false), Modifiers::ALT);
        assert_eq!(modifiers_from_bools(false, false, false, true), Modifiers::WIN);
    }

    // ---- local_physical ----

    #[test]
    fn local_physical_scales_and_rounds() {
        assert_eq!(local_physical(100.0, 1), 100);
        assert_eq!(local_physical(100.0, 2), 200);
        assert_eq!(local_physical(100.49, 2), 201);
        assert_eq!(local_physical(0.0, 2), 0);
    }

    #[test]
    fn local_physical_clamps_zero_scale() {
        assert_eq!(local_physical(100.0, 0), 100);
    }

    // ---- axis_slot ----

    #[test]
    fn axis_slot_routes_vertical_to_zero() {
        assert_eq!(axis_slot(WEnum::Value(wl_pointer::Axis::VerticalScroll)), 0);
        assert_eq!(axis_slot(WEnum::Value(wl_pointer::Axis::HorizontalScroll)), 1);
        assert_eq!(axis_slot(WEnum::Unknown(99)), 1);
    }
}
