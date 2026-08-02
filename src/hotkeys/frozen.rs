//! Frozen-mode hotkey plan: which gestures the app binds while frozen, and
//! pure matching of a pressed key against that plan.
//!
//! Pure module (no OS imports): the Windows shell registers the plan as global
//! hotkeys; shells where frozen-mode keys arrive through the focused overlay
//! instead use [`match_frozen_key`] on overlay `KeyDown` events.

use crate::hotkeys::gesture::HotkeyGesture;
use crate::overlay::modes::ModeKind;
use crate::settings::model::HotkeySettings;

/// What each frozen-mode binding does when it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrozenAction {
    /// Plain mode key. For the capture binding this ENTERS capture mode; any
    /// other kind is a FULL switch — reset ALL mode state (zoom 1.0, snip
    /// selection cleared, spotlight radius back to default) and activate only
    /// this mode.
    SetMode(ModeKind),
    /// Toggle key (spotlight's `S`): REMOVE the layer when active (the
    /// screen stays frozen, unveiled when nothing is left), add it otherwise.
    ToggleMode(ModeKind),
    /// ADD this mode as a layer WITHOUT touching the existing layers. Not
    /// emitted by the current plan; kept for the platform shells, which
    /// dispatch every variant to the controller.
    AddMode(ModeKind),
    Copy,
    Cancel,
    ResetZoom,
}

/// One planned frozen-mode binding: a gesture plus the action it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrozenRegistration {
    pub gesture: HotkeyGesture,
    pub action: FrozenAction,
}

/// The ordered frozen-mode registration list derived from the CURRENT
/// settings: spotlight toggle (`mode_spotlight`), capture-mode switch
/// (`mode_snip`), then `reset_zoom`, `snip_copy`, `cancel` — five
/// registrations. The bound keys are just data living in the settings model —
/// nothing here hardcodes a key name; only the iteration order is fixed.
///
/// Collisions BETWEEN user-configured bindings are NOT resolved here: they
/// stay in the plan, so the registration layer's duplicate error names the
/// offender (existing behavior for hand-edited settings files), and
/// [`match_frozen_key`]'s first-match-wins mirrors the registration layer,
/// which rejects the later duplicate.
pub fn plan_frozen_registrations(hotkeys: &HotkeySettings) -> Vec<FrozenRegistration> {
    [
        (
            hotkeys.mode_spotlight,
            FrozenAction::ToggleMode(ModeKind::Spotlight),
        ),
        (hotkeys.mode_snip, FrozenAction::SetMode(ModeKind::Snip)),
        (hotkeys.reset_zoom, FrozenAction::ResetZoom),
        (hotkeys.snip_copy, FrozenAction::Copy),
        (hotkeys.cancel, FrozenAction::Cancel),
    ]
    .into_iter()
    .map(|(gesture, action)| FrozenRegistration { gesture, action })
    .collect()
}

/// Resolve a frozen-mode key press against the plan: the FIRST registration
/// whose gesture equals `gesture` wins (plan order is priority order).
/// Duplicate gestures only ever fire their first entry, matching the
/// registration layer, which rejects later duplicates.
pub fn match_frozen_key(
    plan: &[FrozenRegistration],
    gesture: HotkeyGesture,
) -> Option<FrozenAction> {
    plan.iter().find(|r| r.gesture == gesture).map(|r| r.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gesture(s: &str) -> HotkeyGesture {
        HotkeyGesture::parse(s).unwrap()
    }

    /// Settings with distinct, deliberately NON-default bindings, so no
    /// assertion can pass by accident through a default value (the defaults
    /// are the settings model's business, not this module's).
    fn custom_hotkeys() -> HotkeySettings {
        HotkeySettings {
            freeze_toggle: gesture("Ctrl+Alt+Q"),
            mode_spotlight: gesture("F5"),
            mode_snip: gesture("F7"),
            snip_copy: gesture("Ctrl+Enter"),
            cancel: gesture("Ctrl+Backspace"),
            reset_zoom: gesture("Ctrl+F8"),
            ..Default::default()
        }
    }

    /// All actions planned for one gesture, in plan order.
    fn planned(plan: &[FrozenRegistration], g: HotkeyGesture) -> Vec<FrozenAction> {
        plan.iter()
            .filter(|r| r.gesture == g)
            .map(|r| r.action)
            .collect()
    }

    #[test]
    fn plan_is_spotlight_capture_then_reset_copy_cancel() {
        let plan = plan_frozen_registrations(&custom_hotkeys());
        let actual: Vec<(HotkeyGesture, FrozenAction)> =
            plan.iter().map(|r| (r.gesture, r.action)).collect();
        let expected = vec![
            (gesture("F5"), FrozenAction::ToggleMode(ModeKind::Spotlight)),
            (gesture("F7"), FrozenAction::SetMode(ModeKind::Snip)),
            (gesture("Ctrl+F8"), FrozenAction::ResetZoom),
            (gesture("Ctrl+Enter"), FrozenAction::Copy),
            (gesture("Ctrl+Backspace"), FrozenAction::Cancel),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn plan_reads_settings_not_hardcoded_keys() {
        let h = custom_hotkeys();
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, h.mode_spotlight),
            vec![FrozenAction::ToggleMode(ModeKind::Spotlight)]
        );
        assert_eq!(
            planned(&plan, h.mode_snip),
            vec![FrozenAction::SetMode(ModeKind::Snip)]
        );
    }

    #[test]
    fn default_settings_plan_has_all_five_registrations() {
        // Structural smoke test over the shipped defaults, whatever keys they
        // bind: spotlight toggle, capture switch, and the reset/copy/cancel
        // bindings. Zoom is NOT a hotkey — it is the implicit zoom-modifier
        // wheel chord, so no `ToggleMode(ModeKind::Zoom)` is registered.
        let plan = plan_frozen_registrations(&HotkeySettings::default());
        assert_eq!(plan.len(), 5);
        for action in [
            FrozenAction::ToggleMode(ModeKind::Spotlight),
            FrozenAction::SetMode(ModeKind::Snip),
            FrozenAction::ResetZoom,
            FrozenAction::Copy,
            FrozenAction::Cancel,
        ] {
            assert!(plan.iter().any(|r| r.action == action), "{action:?}");
        }
        assert!(
            !plan
                .iter()
                .any(|r| r.action == FrozenAction::ToggleMode(ModeKind::Zoom)),
            "no zoom toggle registration"
        );
    }

    #[test]
    fn duplicate_bindings_stay_in_plan_for_the_manager_to_report() {
        // Two bindings on the SAME gesture (hand-edited config): both stay in
        // the plan so the registration layer's duplicate error names the
        // offender; matching fires the FIRST entry, mirroring the
        // registration layer, which rejects the later duplicate.
        let h = HotkeySettings {
            reset_zoom: gesture("F5"), // duplicates mode_spotlight
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("F5")),
            vec![
                FrozenAction::ToggleMode(ModeKind::Spotlight),
                FrozenAction::ResetZoom,
            ]
        );
        assert_eq!(
            match_frozen_key(&plan, gesture("F5")),
            Some(FrozenAction::ToggleMode(ModeKind::Spotlight))
        );
    }

    #[test]
    fn binding_duplicating_the_freeze_toggle_also_stays_in_plan() {
        // The freeze toggle lives in the same registration layer; the plan
        // does not guard against duplicating it either — the duplicate error
        // names the offender there too.
        let h = HotkeySettings {
            reset_zoom: gesture("Ctrl+Alt+Q"), // duplicates freeze_toggle
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("Ctrl+Alt+Q")),
            vec![FrozenAction::ResetZoom]
        );
    }

    // ---- match_frozen_key ----

    #[test]
    fn match_resolves_each_planned_gesture_to_its_action() {
        let plan = plan_frozen_registrations(&custom_hotkeys());
        for registration in &plan {
            assert_eq!(
                match_frozen_key(&plan, registration.gesture),
                Some(registration.action),
                "{:?}",
                registration.gesture
            );
        }
    }

    #[test]
    fn match_unknown_gesture_is_none() {
        let plan = plan_frozen_registrations(&custom_hotkeys());
        assert_eq!(match_frozen_key(&plan, gesture("F1")), None);
        assert_eq!(match_frozen_key(&plan, gesture("Shift+F5")), None);
        assert_eq!(match_frozen_key(&[], gesture("F5")), None);
    }
}
