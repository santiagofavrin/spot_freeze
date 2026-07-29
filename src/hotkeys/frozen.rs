//! Frozen-mode hotkey plan: which gestures the app binds while frozen, and
//! pure matching of a pressed key against that plan.
//!
//! Pure module (no OS imports): the Windows shell registers the plan as global
//! hotkeys; shells where frozen-mode keys arrive through the focused overlay
//! instead use [`match_frozen_key`] on overlay `KeyDown` events.

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::overlay::modes::ModeKind;
use crate::settings::model::HotkeySettings;

/// What each frozen-mode binding does when it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrozenAction {
    /// Plain mode key: FULL switch — reset ALL mode state (zoom 1.0, snip
    /// selection cleared, spotlight radius back to default) and activate only
    /// this mode.
    SetMode(ModeKind),
    /// Shift+mode key: ADD this mode as a composable layer WITHOUT touching
    /// the existing layers.
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

/// The two mode bindings as `(gesture, kind)` pairs, read from `hotkeys`.
/// The bound keys (default `S` / `C`) are just data living in the settings
/// model — nothing here hardcodes a key name; only the iteration order is
/// fixed. Zoom has no binding: it is wheel-driven (`zoom_modifier` + wheel).
fn mode_bindings(hotkeys: &HotkeySettings) -> [(HotkeyGesture, ModeKind); 2] {
    [
        (hotkeys.mode_spotlight, ModeKind::Spotlight),
        (hotkeys.mode_snip, ModeKind::Snip),
    ]
}

/// The ordered frozen-mode registration list derived from the CURRENT
/// settings. For each of the two mode bindings, the PLAIN gesture (full
/// switch) followed by its DERIVED Shift+variant (additive layer); then
/// `reset_zoom`, `snip_copy`, `cancel`. Seven registrations in the common case.
///
/// Conflict guard: a derived Shift+variant whose gesture is already claimed —
/// by the always-active freeze toggle, by ANY user-configured frozen binding
/// (including one that itself contains Shift), or by an earlier mode's
/// Shift+variant — is SKIPPED silently: an explicit binding always beats a
/// derived one. There is no logging infrastructure to report the skip through;
/// this comment is the record. Collisions BETWEEN user-configured bindings are
/// NOT resolved here: they stay in the plan, so the registration layer's
/// duplicate error names the offender (existing behavior for hand-edited
/// settings files).
pub fn plan_frozen_registrations(hotkeys: &HotkeySettings) -> Vec<FrozenRegistration> {
    let bindings = mode_bindings(hotkeys);

    // Every user-configured gesture claims its slot UP FRONT, regardless of
    // plan position, so a derived Shift+variant can never steal an explicitly
    // configured binding (e.g. mode_snip = "Shift+S" beats the Shift+S
    // derived from mode_spotlight = "S", even though spotlight plans first).
    let mut claimed: Vec<HotkeyGesture> = Vec::with_capacity(4 + 2 * bindings.len());
    claimed.push(hotkeys.freeze_toggle);
    claimed.push(hotkeys.reset_zoom);
    claimed.push(hotkeys.snip_copy);
    claimed.push(hotkeys.cancel);
    claimed.extend(bindings.iter().map(|(gesture, _)| *gesture));

    let mut plan = Vec::with_capacity(7);
    for (gesture, kind) in bindings {
        plan.push(FrozenRegistration {
            gesture,
            action: FrozenAction::SetMode(kind),
        });
        // Additive-layer gesture: same key with Shift added. When the binding
        // already contains Shift, this equals the plain gesture and is
        // skipped by the guard like any other collision.
        let shifted = HotkeyGesture::new(gesture.modifiers | Modifiers::SHIFT, gesture.vk);
        if !claimed.contains(&shifted) {
            claimed.push(shifted);
            plan.push(FrozenRegistration {
                gesture: shifted,
                action: FrozenAction::AddMode(kind),
            });
        }
    }
    for (gesture, action) in [
        (hotkeys.reset_zoom, FrozenAction::ResetZoom),
        (hotkeys.snip_copy, FrozenAction::Copy),
        (hotkeys.cancel, FrozenAction::Cancel),
    ] {
        plan.push(FrozenRegistration { gesture, action });
    }
    plan
}

/// Resolve a frozen-mode key press against the plan: the FIRST registration
/// whose gesture equals `gesture` wins (plan order is priority order).
/// Duplicate gestures only ever fire their first entry, matching the
/// registration layer, which rejects later duplicates.
pub fn match_frozen_key(plan: &[FrozenRegistration], gesture: HotkeyGesture) -> Option<FrozenAction> {
    plan.iter()
        .find(|r| r.gesture == gesture)
        .map(|r| r.action)
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

    fn has_action(plan: &[FrozenRegistration], action: FrozenAction) -> bool {
        plan.iter().any(|r| r.action == action)
    }

    #[test]
    fn mode_bindings_reads_settings_not_hardcoded_keys() {
        let h = custom_hotkeys();
        assert_eq!(
            mode_bindings(&h),
            [
                (gesture("F5"), ModeKind::Spotlight),
                (gesture("F7"), ModeKind::Snip),
            ]
        );
    }

    #[test]
    fn plan_registers_each_mode_twice_then_reset_copy_cancel() {
        let plan = plan_frozen_registrations(&custom_hotkeys());
        let actual: Vec<(HotkeyGesture, FrozenAction)> =
            plan.iter().map(|r| (r.gesture, r.action)).collect();
        // Per mode: plain = full switch, derived Shift+variant = additive layer.
        let expected = vec![
            (gesture("F5"), FrozenAction::SetMode(ModeKind::Spotlight)),
            (gesture("Shift+F5"), FrozenAction::AddMode(ModeKind::Spotlight)),
            (gesture("F7"), FrozenAction::SetMode(ModeKind::Snip)),
            (gesture("Shift+F7"), FrozenAction::AddMode(ModeKind::Snip)),
            (gesture("Ctrl+F8"), FrozenAction::ResetZoom),
            (gesture("Ctrl+Enter"), FrozenAction::Copy),
            (gesture("Ctrl+Backspace"), FrozenAction::Cancel),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn default_settings_plan_has_all_seven_registrations() {
        // Structural smoke test over the shipped defaults, whatever keys they
        // bind: every mode gets a full-switch AND an additive-layer gesture.
        let plan = plan_frozen_registrations(&HotkeySettings::default());
        assert_eq!(plan.len(), 7);
        for kind in [ModeKind::Spotlight, ModeKind::Snip] {
            assert!(has_action(&plan, FrozenAction::SetMode(kind)), "{kind:?}");
            assert!(has_action(&plan, FrozenAction::AddMode(kind)), "{kind:?}");
        }
        for action in [
            FrozenAction::ResetZoom,
            FrozenAction::Copy,
            FrozenAction::Cancel,
        ] {
            assert!(has_action(&plan, action), "{action:?}");
        }
    }

    #[test]
    fn shift_variant_adds_shift_to_existing_modifiers() {
        let h = HotkeySettings {
            mode_spotlight: gesture("Ctrl+Alt+S"),
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("Ctrl+Alt+Shift+S")),
            vec![FrozenAction::AddMode(ModeKind::Spotlight)]
        );
    }

    #[test]
    fn binding_already_containing_shift_gets_no_variant() {
        // Plain and Shift+variant would be identical: only the plain switch
        // is planned, and no duplicate registration is attempted.
        let h = HotkeySettings {
            mode_snip: gesture("Shift+F7"),
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("Shift+F7")),
            vec![FrozenAction::SetMode(ModeKind::Snip)]
        );
        assert_eq!(plan.len(), 6);
    }

    #[test]
    fn explicit_mode_binding_wins_over_earlier_derived_variant() {
        // mode_snip explicitly owns "Shift+F5": the Shift+variant derived from
        // mode_spotlight ("F5") must yield EVEN THOUGH spotlight plans first —
        // user-configured bindings claim their slots up front.
        let h = HotkeySettings {
            mode_spotlight: gesture("F5"),
            mode_snip: gesture("Shift+F5"),
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("F5")),
            vec![FrozenAction::SetMode(ModeKind::Spotlight)]
        );
        assert_eq!(
            planned(&plan, gesture("Shift+F5")),
            vec![FrozenAction::SetMode(ModeKind::Snip)]
        );
        assert!(!has_action(&plan, FrozenAction::AddMode(ModeKind::Spotlight)));
        // Snip's own variant is skipped too: it already contains Shift.
        assert!(!has_action(&plan, FrozenAction::AddMode(ModeKind::Snip)));
        assert_eq!(plan.len(), 5); // 2 plains + reset/copy/cancel
    }

    #[test]
    fn derived_variant_yields_to_non_mode_binding() {
        // snip_copy explicitly owns "Shift+F7": snip's derived variant skips.
        let h = HotkeySettings {
            snip_copy: gesture("Shift+F7"),
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("Shift+F7")),
            vec![FrozenAction::Copy]
        );
        assert!(!has_action(&plan, FrozenAction::AddMode(ModeKind::Snip)));
        assert_eq!(plan.len(), 6);
    }

    #[test]
    fn derived_variant_yields_to_freeze_toggle() {
        // The freeze toggle is bound in the same registration layer, so a
        // derived variant equal to it would fail as a duplicate and land in
        // the failure report; the guard skips it instead. The plain mode
        // binding itself is unaffected.
        let h = HotkeySettings {
            freeze_toggle: gesture("Shift+F5"),
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("F5")),
            vec![FrozenAction::SetMode(ModeKind::Spotlight)]
        );
        assert!(!has_action(&plan, FrozenAction::AddMode(ModeKind::Spotlight)));
        assert_eq!(plan.len(), 6);
    }

    #[test]
    fn duplicate_plain_bindings_stay_in_plan_for_the_manager_to_report() {
        // Two modes bound to the SAME plain gesture (hand-edited config): both
        // stay in the plan so the registration layer's duplicate error names
        // the offender in the failure report. Their identical Shift+variants
        // collide, so only the first mode's additive layer is planned.
        let h = HotkeySettings {
            mode_snip: gesture("F5"), // duplicates mode_spotlight
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            planned(&plan, gesture("F5")),
            vec![
                FrozenAction::SetMode(ModeKind::Spotlight),
                FrozenAction::SetMode(ModeKind::Snip),
            ]
        );
        assert_eq!(
            planned(&plan, gesture("Shift+F5")),
            vec![FrozenAction::AddMode(ModeKind::Spotlight)]
        );
        assert_eq!(plan.len(), 6); // 2 plains (one duplicated) + 1 surviving variant + reset/copy/cancel
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
    fn match_first_of_duplicate_gestures_wins() {
        // mode_snip duplicates mode_spotlight's plain gesture: the FIRST plan
        // entry fires, matching the registration layer, which rejects the
        // later duplicate.
        let h = HotkeySettings {
            mode_snip: gesture("F5"), // duplicates mode_spotlight
            ..custom_hotkeys()
        };
        let plan = plan_frozen_registrations(&h);
        assert_eq!(
            match_frozen_key(&plan, gesture("F5")),
            Some(FrozenAction::SetMode(ModeKind::Spotlight))
        );
    }

    #[test]
    fn match_unknown_gesture_is_none() {
        let plan = plan_frozen_registrations(&custom_hotkeys());
        assert_eq!(match_frozen_key(&plan, gesture("F9")), None);
        assert_eq!(match_frozen_key(&plan, gesture("Shift+F9")), None);
        assert_eq!(match_frozen_key(&[], gesture("F5")), None);
    }
}
