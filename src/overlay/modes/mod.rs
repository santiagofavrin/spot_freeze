//! Overlay modes (Spotlight / Zoom / Snip): per-mode state machines plus
//! rendering. The trait is implementable and testable WITHOUT Win32 — modes
//! render into plain [`DibBuffer`]s via [`crate::overlay::composite`].

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect};
use crate::hotkeys::gesture::Modifiers;

pub mod snip;
pub mod spotlight;
pub mod zoom;

/// Which overlay mode is active.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ModeKind {
    Spotlight,
    Zoom,
    Snip,
}

/// What the controller must do after a mode handled an event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeEffect {
    /// `(monitor_index, dirty_region)` pairs to repaint. `dirty_region` is in
    /// monitor-local physical pixels; `None` = repaint the whole monitor.
    pub repaint: Vec<(usize, Option<Rect>)>,
    /// The mode asks the controller to unfreeze (reserved; Esc and the copy
    /// hotkey are normally handled globally by the app/controller).
    pub exit: bool,
}

impl ModeEffect {
    /// No repaint, no exit.
    pub fn none() -> Self {
        Self::default()
    }

    /// Repaint one monitor (`None` dirty = full monitor), no exit.
    pub fn repaint(monitor: usize, dirty: Option<Rect>) -> Self {
        Self {
            repaint: vec![(monitor, dirty)],
            exit: false,
        }
    }
}

/// A snip drag: two endpoints in MONITOR-LOCAL physical pixels on `monitor`,
/// in ANY drag direction — normalization happens in
/// [`crate::overlay::composite::crop_normalized`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SnipSelection {
    pub monitor: usize,
    pub a: Point,
    pub b: Point,
}

/// Common contract for the three overlay modes.
///
/// The CONTROLLER owns the frozen captures and the windows; the MODE owns mode
/// state (cursor position, radius, zoom factor, selection) and knows how to
/// render each monitor's frame from the original capture.
pub trait OverlayMode {
    fn kind(&self) -> ModeKind;

    /// Cursor moved on `monitor` at `at` (monitor-local px).
    fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect;

    /// Mouse wheel on `monitor` at `at`; `delta` in RAW Win32 wheel units
    /// (one notch = `WHEEL_DELTA` = 120, positive = up/away); `modifiers` =
    /// currently held modifier keys. Smooth-scroll hardware sends sub-notch
    /// deltas (|delta| < 120): modes must accumulate or apply them
    /// fractionally, never truncate them to whole notches.
    fn on_wheel(&mut self, monitor: usize, at: Point, delta: i32, modifiers: Modifiers)
    -> ModeEffect;

    fn on_left_button_down(&mut self, monitor: usize, at: Point) -> ModeEffect;

    fn on_left_button_up(&mut self, monitor: usize, at: Point) -> ModeEffect;

    /// A key press that is not a mode-switch/global hotkey. `vk` is a Win32
    /// virtual-key code (plain number), `modifiers` = currently held.
    fn on_key(&mut self, vk: u32, modifiers: Modifiers) -> ModeEffect;

    /// Write the COMPLETE frame for `monitor` into `out`: every pixel must be
    /// overwritten (typically: darken, then mode-specific compositing via
    /// [`crate::overlay::composite`]). `out` has the same dimensions as
    /// `original`; `dim_alpha` comes from settings.
    fn render(&self, monitor: usize, original: &DibBuffer, out: &mut DibBuffer, dim_alpha: u8);

    /// Snip only: the current selection, if any. Default: `None`.
    fn snip_selection(&self) -> Option<SnipSelection> {
        None
    }

    /// Reset-view hotkey (default binding `0`) — only Zoom overrides.
    /// Default: no-op.
    fn reset_view(&mut self) -> ModeEffect {
        ModeEffect::none()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_effect_none_is_empty() {
        let e = ModeEffect::none();
        assert!(e.repaint.is_empty());
        assert!(!e.exit);
        assert_eq!(e, ModeEffect::default());
    }

    #[test]
    fn mode_effect_repaint_single_monitor() {
        let dirty = Rect::new(3, -4, 10, 20);
        let e = ModeEffect::repaint(2, Some(dirty));
        assert_eq!(e.repaint, vec![(2, Some(dirty))]);
        assert!(!e.exit);
    }

    #[test]
    fn mode_effect_repaint_full_monitor_when_dirty_none() {
        let e = ModeEffect::repaint(0, None);
        assert_eq!(e.repaint, vec![(0, None)]);
    }

    #[test]
    fn trait_defaults_are_inert() {
        // A minimal implementer exercises the default trait methods.
        struct Dummy;
        impl OverlayMode for Dummy {
            fn kind(&self) -> ModeKind {
                ModeKind::Spotlight
            }
            fn on_mouse_move(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
                ModeEffect::none()
            }
            fn on_wheel(
                &mut self,
                _monitor: usize,
                _at: Point,
                _delta: i32,
                _modifiers: Modifiers,
            ) -> ModeEffect {
                ModeEffect::none()
            }
            fn on_left_button_down(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
                ModeEffect::none()
            }
            fn on_left_button_up(&mut self, _monitor: usize, _at: Point) -> ModeEffect {
                ModeEffect::none()
            }
            fn on_key(&mut self, _vk: u32, _modifiers: Modifiers) -> ModeEffect {
                ModeEffect::none()
            }
            fn render(&self, _monitor: usize, _original: &DibBuffer, _out: &mut DibBuffer, _dim_alpha: u8) {}
        }
        let mut d = Dummy;
        assert_eq!(d.snip_selection(), None);
        assert_eq!(d.reset_view(), ModeEffect::none());
    }
}
