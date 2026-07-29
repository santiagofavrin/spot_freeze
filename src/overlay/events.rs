//! Overlay input events: the contract every platform's overlay surface
//! implementation delivers to the overlay controller. Pure data — no OS types.

use crate::geometry::Point;
use crate::hotkeys::gesture::Modifiers;
use std::rc::Rc;

/// Events an overlay surface reports to the controller.
///
/// Coordinates are MONITOR-LOCAL physical pixels of the monitor the event
/// occurred on, and the event is tagged with THAT monitor's index. For the
/// wheel this is NOT necessarily the surface that received the OS event
/// (wheel messages typically go to the focus surface), so backends reroute
/// by cursor position before emitting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverlayEvent {
    MouseMove {
        at: Point,
    },
    /// `delta` is the RAW wheel delta in 120-per-notch units — one notch =
    /// ±120; positive = wheel up/away. Smooth-scroll hardware (precision
    /// touchpads, high-resolution wheels) sends sub-notch deltas
    /// (|delta| < 120); consumers must accumulate or apply them fractionally
    /// (see [`crate::overlay::modes::ModeStack::on_wheel`]), never truncate.
    MouseWheel {
        at: Point,
        delta: i32,
        modifiers: Modifiers,
    },
    LeftButtonDown {
        at: Point,
    },
    LeftButtonUp {
        at: Point,
    },
    /// `vk` is the Win32 virtual-key code — the crate-wide key lingua franca;
    /// backends translate their native key codes via
    /// [`crate::hotkeys::keymap`]. `modifiers` is the modifier state at event
    /// time.
    KeyDown {
        vk: u32,
        modifiers: Modifiers,
    },
}

/// Callback invoked on the UI thread with `(monitor_index, event)`.
pub type OverlayEventSink = Rc<dyn Fn(usize, OverlayEvent)>;
