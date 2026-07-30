//! PURE freeze/unfreeze fade: the step schedule (alpha over elapsed time), the
//! easing, the duration/step caps, and the clock the controller's fade driver
//! runs on. No pixel math here (the blend lives in
//! [`crate::overlay::composite::blend_frames`]) and no OS calls — the
//! controller turns these decisions into `set_alpha` calls or blended
//! presents.
//!
//! # Caps (product constraint: snappiness first)
//!
//! - Total duration is [`FADE_DURATION_MS`] (<= 180 ms, hard cap): the fade
//!   must never hold the freeze hostage. The driver ends EVERY fade with the
//!   exact endpoint (fully opaque after freeze, fully transparent / original
//!   pixels before teardown), so a fade can never leave a half-shown overlay.
//! - [`FADE_STEPS`] steps: on Wayland each step is a FULL-FRAME pixel blend
//!   through the normal present path, so the step count doubles as the blend
//!   cap the present path can absorb; constant-alpha platforms (Windows,
//!   macOS) pay only a per-window attribute update per step.
//! - Missed steps are SKIPPED, never queued: alpha is a pure function of
//!   elapsed time, so a slow step (compositor pacing, a busy surface) just
//!   lands on the alpha for the CURRENT time — the fade degrades to fewer,
//!   larger steps instead of stalling past the cap.
//!
//! # Platform paths
//!
//! Constant-alpha surfaces (Windows, macOS) fade via `set_alpha` — a true
//! crossfade against the LIVE desktop underneath. Surfaces without
//! per-surface alpha (Wayland layer-shell shm) blend pixels between the
//! freeze-time capture and the composed frame instead: fade-IN starts on the
//! just-captured frame (the live screen's current pixels, so the overlay
//! appears seamlessly), but fade-OUT ends on the FREEZE-TIME capture —
//! content that changed while frozen reappears with a small pop when the
//! overlay unmaps. That pop is inherent to blending toward a snapshot; a
//! true live crossfade would need the per-surface alpha the protocol lacks.
//!
//! # Interruption state machine
//!
//! States: `Unfrozen` / `Frozen`; transitions `freeze` (fade IN) and
//! `unfreeze` (fade OUT) are ATOMIC — the fade driver runs synchronously on
//! the single UI thread exactly like the mode-change border flash, so
//! mid-fade interruption is impossible by construction. What happens to
//! input pressed DURING a fade is platform-specific (no state corruption
//! anywhere — the controller only ever sees settled requests):
//!
//! - **Freeze toggle during any fade**: QUEUED on every platform (it is the
//!   always-registered global hotkey — Win32 `WM_HOTKEY`, the macOS Carbon
//!   hotkey, the Wayland portal hotkey / IPC intent channel). A toggle
//!   during fade-out therefore starts a fresh freeze right after teardown;
//!   a toggle during fade-in unfreezes right after the fade-in completes.
//! - **Esc during fade-in**: QUEUED only on macOS (overlay key events sit in
//!   the blocked run loop's queue and fire when it resumes — a normal
//!   fade-out follows). On Windows and Wayland it is DROPPED: their Esc
//!   routing arms only after `freeze()` returns (Windows registers the
//!   frozen-mode global hotkeys afterwards; the Wayland `frozen_plan`
//!   installs afterwards), so a first Esc pressed mid-fade does nothing and
//!   a second Esc works as usual.
//! - **Esc during fade-out**: the frozen key paths are still armed (Windows
//!   unregisters after `unfreeze()` returns, the Wayland plan clears
//!   afterwards, the macOS run loop queues) — the Esc lands after teardown
//!   as a documented no-op.
//! - **Esc in capture mode**: not a fade transition at all — it exits
//!   capture instantly (see
//!   [`crate::overlay::controller::OverlayController::unfreeze`]).
//!
//! What IS guaranteed at every boundary, and what the controller tests pin:
//! the last alpha applied by a fade is always the exact endpoint, and a
//! later toggle always finds the controller in the matching settled state.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Total fade duration (hard cap: <= 180 ms per the product constraint).
pub const FADE_DURATION_MS: u64 = 160;
/// Fade steps per transition; also the full-frame-blend cap on Wayland.
pub const FADE_STEPS: u64 = 8;
/// Nominal time between steps (`FADE_DURATION_MS / FADE_STEPS`).
pub const FADE_STEP_MS: u64 = FADE_DURATION_MS / FADE_STEPS;

/// Which way a fade runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FadeDirection {
    /// Freeze: transparent / original pixels -> opaque composed frame.
    In,
    /// Full unfreeze: opaque composed frame -> transparent / original pixels.
    Out,
}

/// One driver iteration: apply `alpha` (0 = transparent/original, 255 =
/// opaque/composed) to every monitor, then wait `wait` before re-sampling the
/// clock. The endpoint itself is NOT a step — the driver applies it exactly
/// after the last step (see module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FadeStep {
    pub alpha: u8,
    pub wait: Duration,
}

/// The step schedule at `elapsed` since the fade started; `None` once the
/// duration is exhausted (the driver then applies the exact endpoint). Alpha
/// comes straight from [`fade_alpha`] at the CURRENT time, so any number of
/// missed nominal steps collapses into the next single application.
pub fn fade_step(elapsed: Duration, direction: FadeDirection) -> Option<FadeStep> {
    let ms = elapsed.as_millis() as u64;
    if ms >= FADE_DURATION_MS {
        return None;
    }
    // Wait until the next nominal step boundary (never 0: ms < DURATION and
    // DURATION is a multiple of STEP, so the next boundary is always ahead).
    let next_boundary = (ms / FADE_STEP_MS + 1) * FADE_STEP_MS;
    Some(FadeStep {
        alpha: fade_alpha(ms, direction),
        wait: Duration::from_millis(next_boundary - ms),
    })
}

/// Alpha byte at `elapsed_ms` for `direction`: smoothstep-eased, `In` ramps
/// 0 -> 255, `Out` ramps 255 -> 0 (the mirror — the easing is symmetric, so
/// `fade_alpha(t, In) + fade_alpha(t, Out) == 255` up to rounding). Elapsed
/// times past the duration clamp to the endpoint.
pub fn fade_alpha(elapsed_ms: u64, direction: FadeDirection) -> u8 {
    let t = (elapsed_ms.min(FADE_DURATION_MS)) as f32 / FADE_DURATION_MS as f32;
    let progress = match direction {
        FadeDirection::In => t,
        FadeDirection::Out => 1.0 - t,
    };
    (smoothstep(progress) * 255.0).round() as u8
}

/// Smoothstep (`t*t*(3 - 2t)`): ease in AND out, symmetric around t = 0.5 —
/// a productivity-utility fade, not a bounce. Symmetry keeps the two
/// directions exact mirrors of each other.
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// The fade driver's time source, injected so tests drive the clock instead
/// of waiting on it (test constraint: no real sleeps). Production uses
/// [`FadeClock::system`]; controller tests advance a shared cell from the
/// `sleep` closure, making every fade complete in zero wall-clock time while
/// still walking the full step schedule.
#[derive(Clone)]
pub struct FadeClock {
    now: Rc<dyn Fn() -> Duration>,
    sleep: Rc<dyn Fn(Duration)>,
}

impl FadeClock {
    /// Monotonic clock + `thread::sleep` (the production driver).
    pub fn system() -> Self {
        let epoch = std::time::Instant::now();
        Self {
            now: Rc::new(move || epoch.elapsed()),
            sleep: Rc::new(std::thread::sleep),
        }
    }

    /// Manual clock over a shared cell: `sleep(d)` advances the cell by
    /// exactly `d`, instantly. Nominal-schedule tests step through every
    /// boundary; the cell is shared with the test so it can assert (or jump)
    /// the fade's idea of time.
    pub fn manual(clock: Rc<Cell<Duration>>) -> Self {
        Self {
            now: {
                let clock = clock.clone();
                Rc::new(move || clock.get())
            },
            sleep: Rc::new(move |d| clock.set(clock.get() + d)),
        }
    }

    /// Fully custom clock (e.g. a `sleep` that advances time by MORE than the
    /// request to force the missed-step path).
    pub fn custom(now: Rc<dyn Fn() -> Duration>, sleep: Rc<dyn Fn(Duration)>) -> Self {
        Self { now, sleep }
    }

    pub fn now(&self) -> Duration {
        (self.now)()
    }

    pub fn sleep(&self, d: Duration) {
        (self.sleep)(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- caps (the product constraint, pinned) ----

    #[test]
    fn caps_are_within_the_product_budget() {
        assert!(FADE_DURATION_MS <= 180, "total duration must never exceed 180 ms");
        assert!(FADE_STEPS <= 8, "Wayland full-frame blends are capped at 8");
        assert_eq!(FADE_STEP_MS * FADE_STEPS, FADE_DURATION_MS);
    }

    // ---- fade_alpha ----

    #[test]
    fn alpha_endpoints_are_exact() {
        assert_eq!(fade_alpha(0, FadeDirection::In), 0);
        assert_eq!(fade_alpha(FADE_DURATION_MS, FadeDirection::In), 255);
        assert_eq!(fade_alpha(0, FadeDirection::Out), 255);
        assert_eq!(fade_alpha(FADE_DURATION_MS, FadeDirection::Out), 0);
    }

    #[test]
    fn alpha_clamps_beyond_the_duration() {
        assert_eq!(fade_alpha(FADE_DURATION_MS + 1000, FadeDirection::In), 255);
        assert_eq!(fade_alpha(FADE_DURATION_MS + 1000, FadeDirection::Out), 0);
    }

    #[test]
    fn alpha_is_monotonic_in_both_directions() {
        let mut prev_in = 0;
        let mut prev_out = 255;
        for t in 0..=FADE_DURATION_MS {
            let a_in = fade_alpha(t, FadeDirection::In);
            let a_out = fade_alpha(t, FadeDirection::Out);
            assert!(a_in >= prev_in, "fade-in regressed at {t} ms");
            assert!(a_out <= prev_out, "fade-out regressed at {t} ms");
            prev_in = a_in;
            prev_out = a_out;
        }
    }

    #[test]
    fn alpha_directions_are_mirrors() {
        for t in 0..=FADE_DURATION_MS {
            let sum = fade_alpha(t, FadeDirection::In) as i32
                + fade_alpha(t, FadeDirection::Out) as i32;
            assert!((sum - 255).abs() <= 1, "mirror mismatch at {t} ms: {sum}");
        }
    }

    #[test]
    fn alpha_midpoint_is_about_half() {
        let mid = fade_alpha(FADE_DURATION_MS / 2, FadeDirection::In);
        assert!((mid as i32 - 128).abs() <= 1, "smoothstep midpoint: {mid}");
    }

    // ---- fade_step ----

    #[test]
    fn step_schedule_walks_the_nominal_grid() {
        // Perfectly timed clock: exactly FADE_STEPS steps, on the grid, each
        // waiting one STEP interval; then the schedule is exhausted.
        let mut t = 0;
        let mut steps = 0;
        while let Some(step) = fade_step(ms(t), FadeDirection::In) {
            assert_eq!(step.alpha, fade_alpha(t, FadeDirection::In));
            assert_eq!(step.wait, ms(FADE_STEP_MS));
            t += FADE_STEP_MS;
            steps += 1;
        }
        assert_eq!(steps, FADE_STEPS);
        assert_eq!(t, FADE_DURATION_MS);
    }

    #[test]
    fn step_schedule_waits_to_the_next_boundary_from_any_phase() {
        // Mid-interval elapsed times still produce one step whose wait lands
        // on the next grid boundary — a late step is skipped TO, not queued.
        let step = fade_step(ms(45), FadeDirection::In).expect("inside the duration");
        assert_eq!(step.alpha, fade_alpha(45, FadeDirection::In));
        assert_eq!(step.wait, ms(15)); // next boundary: 60
        let step = fade_step(ms(159), FadeDirection::Out).expect("still inside");
        assert_eq!(step.wait, ms(1));
    }

    #[test]
    fn step_schedule_ends_at_the_duration() {
        assert!(fade_step(ms(FADE_DURATION_MS), FadeDirection::In).is_none());
        assert!(fade_step(ms(FADE_DURATION_MS + 1), FadeDirection::Out).is_none());
    }

    #[test]
    fn missed_steps_collapse_into_the_current_alpha() {
        // A 45 ms stall between samples: the schedule emits the alpha for the
        // CURRENT time (intermediate alphas are gone for good — degrade, not
        // stall) and still terminates on time.
        let mut seen = Vec::new();
        let mut t = 0u64;
        while let Some(step) = fade_step(ms(t), FadeDirection::In) {
            seen.push(step.alpha);
            t += 45; // the surface/compositor stalled: two nominal steps gone
        }
        assert_eq!(
            seen,
            vec![
                fade_alpha(0, FadeDirection::In),
                fade_alpha(45, FadeDirection::In),
                fade_alpha(90, FadeDirection::In),
                fade_alpha(135, FadeDirection::In),
            ]
        );
        assert!(t >= FADE_DURATION_MS);
    }

    // ---- FadeClock ----

    #[test]
    fn system_clock_advances_in_real_time() {
        // The production driver runs on this clock; a real (short) sleep
        // must move it forward.
        let clock = FadeClock::system();
        let t0 = clock.now();
        std::thread::sleep(Duration::from_millis(5));
        let t1 = clock.now();
        assert!(
            t1 >= t0 + Duration::from_millis(4),
            "system clock must advance in real time: {t0:?} -> {t1:?}"
        );
    }

    #[test]
    fn manual_clock_advances_by_the_sleep_request() {
        let cell = Rc::new(Cell::new(Duration::ZERO));
        let clock = FadeClock::manual(cell.clone());
        assert_eq!(clock.now(), Duration::ZERO);
        clock.sleep(ms(20));
        clock.sleep(ms(5));
        assert_eq!(clock.now(), ms(25));
        assert_eq!(cell.get(), ms(25));
    }

    #[test]
    fn custom_clock_can_jump_past_steps() {
        let cell = Rc::new(Cell::new(Duration::ZERO));
        let clock = FadeClock::custom(
            {
                let cell = cell.clone();
                Rc::new(move || cell.get())
            },
            Rc::new(move |_d| cell.set(cell.get() + ms(50))),
        );
        clock.sleep(ms(20)); // asked for 20, stalled for 50
        assert_eq!(clock.now(), ms(50));
    }
}
