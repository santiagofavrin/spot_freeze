//! Scenario (rework-e): mode-change border flash counts.
//!
//! User feedback: mode changes flash the screen border N times —
//! Spotlight = 1, Zoom = 2, Snip = 3 (and freeze starts in Spotlight with
//! its 1 flash).
//!
//! SHARED API SPEC: the controller gains `flash_count(ModeKind) -> u32`
//! (pinned as an ASSOCIATED function — pure mapping, no instance state).
//!
//! INTEGRATION FLAG (per assignment): if the landed API differs — e.g.
//! `ModeKind::flash_count(self) -> u32` in `overlay::modes`, or a
//! `&self` method on the controller — update the import/call form HERE to
//! match the landed signature (do not weaken the 1/2/3 pins themselves).

use spotfreeze::overlay::controller::OverlayController;
use spotfreeze::overlay::modes::ModeKind;

#[test]
fn flash_counts_are_one_two_three_per_mode() {
    assert_eq!(
        OverlayController::flash_count(ModeKind::Spotlight),
        1,
        "Spotlight flashes once"
    );
    assert_eq!(
        OverlayController::flash_count(ModeKind::Zoom),
        2,
        "Zoom flashes twice"
    );
    assert_eq!(
        OverlayController::flash_count(ModeKind::Snip),
        3,
        "Snip flashes three times"
    );
}

#[test]
fn flash_count_is_pure_mapping_total_over_mode_kinds() {
    // Every ModeKind has a distinct, nonzero flash count (the flash is the
    // mode-change feedback, so it must exist for all three).
    let kinds = [ModeKind::Spotlight, ModeKind::Zoom, ModeKind::Snip];
    let counts: Vec<u32> = kinds.iter().map(|&k| OverlayController::flash_count(k)).collect();
    for (kind, count) in kinds.iter().zip(&counts) {
        assert!(*count >= 1, "{kind:?} must flash at least once");
    }
    let mut sorted = counts.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        counts.len(),
        "flash counts are pairwise distinct: {counts:?}"
    );
}
