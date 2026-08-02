//! PURE transition schedule: the step grid (progress over elapsed time), the
//! easing, the spotlight radius curve, the duration/step caps, and the clock
//! the controller's transition drivers run on. No pixel math here (the blend
//! lives in [`crate::overlay::composite::blend_frames`]) and no OS calls — the
//! controller turns these decisions into `set_alpha` calls, blended presents,
//! or re-composed frames.
//!
//! The schedule drives the freeze/unfreeze fades. A single eased progress byte
//! (`alpha`, 0..=255) parameterizes the veil strength, the whole-window alpha,
//! and the spotlight circle scale, so one step grid drives the choreography.
//!
//! # Motion design (researched: Material motion, Apple HIG)
//!
//! - **Easing**: ease-out cubic (`1 - (1-t)^3`) for entries — Material's
//!   *deceleration curve*: "elements enter the screen at full velocity and
//!   slowly decelerate to a resting point", scaling size up to 100% and
//!   opacity to 100% in the same curve. Exits play the exact time-reverse
//!   (Material's *acceleration curve*: slow start, full-velocity exit) —
//!   `fade_alpha(t, Out) == fade_alpha(DURATION - t, In)` by construction.
//! - **Duration**: [`FADE_DURATION_MS`] = 200 ms. Material places mobile
//!   transitions at 200-300 ms (entering ~225 ms), Apple HIG asks for
//!   animations of "a few tenths of a second" at most, and micro-transition
//!   guidance clusters at 100-250 ms. The veil fade and the circle expansion
//!   share the 200 ms grid.
//! - **Spotlight circle**: enters at 60% of its settled radius and eases out
//!   to 100% ([`spotlight_radius_scale`]); exits shrink it back. Starting at
//!   60% (not 0%) keeps the hole readable from the first visible frame —
//!   a from-zero circle reads as a dot popping, not a spotlight opening.
//!
//! # Caps (product constraint: snappiness first)
//!
//! - Total duration never exceeds 240 ms (hard cap; the 200 ms schedule keeps
//!   margin under it): a transition must never hold the freeze hostage. The
//!   driver ends EVERY transition with the exact endpoint (settled frame /
//!   fully transparent / original pixels), so one can never leave a
//!   half-shown overlay. (The earlier 180 ms cap pre-dated the radius
//!   choreography; 200 ms is the smallest grid that fits an eased veil +
//!   circle ramp without visible stepping, and stays within the revised cap.)
//! - [`FADE_STEPS`] steps: on Wayland each step is a FULL-FRAME present
//!   through the normal present path, so the step count doubles as the frame
//!   cap the present path can absorb; constant-alpha platforms (Windows,
//!   macOS) pay a per-window attribute update plus one present per step.
//! - Missed steps are SKIPPED, never queued: progress is a pure function of
//!   elapsed time, so a slow step (compositor pacing, a busy surface) just
//!   lands on the value for the CURRENT time — the transition degrades to
//!   fewer, larger steps instead of stalling past the cap.
//!
//! # Platform paths
//!
//! Constant-alpha surfaces (Windows, macOS) fade via `set_alpha` — a true
//! crossfade against the LIVE desktop underneath — over frames re-composed
//! with the step's circle scale. Surfaces without per-surface alpha (Wayland
//! layer-shell shm) present re-composed frames whose veil ramps with the step
//! alpha on entry, and blend pixels toward the freeze-time capture on exit
//! (a veil ramp cannot crossfade a zoom base away): fade-IN starts on the
//! just-captured frame (the live screen's current pixels, so the overlay
//! appears seamlessly), but fade-OUT ends on the FREEZE-TIME capture —
//! content that changed while frozen reappears with a small pop when the
//! overlay unmaps. That pop is inherent to blending toward a snapshot; a
//! true live crossfade would need the per-surface alpha the protocol lacks.
//! In-session spotlight toggles do not use this schedule. They repaint the
//! settled on/off state once, immediately.
//!
//! # Interruption state machine
//!
//! States: `Unfrozen` / `Frozen`; transitions `freeze` (fade IN) and
//! `unfreeze` (fade OUT) are ATOMIC — the drivers run synchronously on the
//! single UI thread, so mid-transition interruption is impossible by
//! construction. What happens
//! to input pressed DURING a transition is platform-specific (no state
//! corruption anywhere — the controller only ever sees settled requests):
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
//! the last progress applied by a transition is always the exact endpoint,
//! and a later toggle always finds the controller in the matching settled
//! state.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Total transition duration (hard cap: <= 240 ms per the product constraint).
pub const FADE_DURATION_MS: u64 = 200;
/// Steps per transition; also the full-frame-present cap on Wayland.
pub const FADE_STEPS: u64 = 8;
/// Nominal time between steps (`FADE_DURATION_MS / FADE_STEPS`).
pub const FADE_STEP_MS: u64 = FADE_DURATION_MS / FADE_STEPS;

/// Spotlight circle scale at the START of an enter transition, in permille of
/// the settled radius (exit transitions end on it): 60%.
pub const SPOTLIGHT_SCALE_MIN_PM: u32 = 600;

/// Which way a transition runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FadeDirection {
    /// Enter (freeze, spotlight toggle-on): progress ramps 0 -> 255.
    In,
    /// Exit (full unfreeze, spotlight toggle-off): progress ramps 255 -> 0,
    /// the exact time-reverse of `In`.
    Out,
}

/// One driver iteration: apply `alpha` (the eased progress byte — 0 =
/// transparent/original/60% circle, 255 = opaque/composed/settled) to every
/// monitor, then wait `wait` before re-sampling the clock. The endpoint itself
/// is NOT a step — the driver applies it exactly after the last step (see
/// module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FadeStep {
    pub alpha: u8,
    pub wait: Duration,
}

/// The step schedule at `elapsed` since the transition started; `None` once
/// the duration is exhausted (the driver then applies the exact endpoint).
/// Alpha comes straight from [`fade_alpha`] at the CURRENT time, so any
/// number of missed nominal steps collapses into the next single application.
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

/// Eased progress byte at `elapsed_ms` for `direction`: `In` follows the
/// ease-out cubic 0 -> 255, `Out` evaluates the SAME curve at the mirrored
/// time (`(DURATION - elapsed) / DURATION`), so `Out` at time t equals `In`
/// at time `DURATION - t` bit-for-bit — the exit is the exact time-reverse of
/// the entry. Elapsed times past the duration clamp to the endpoint.
pub fn fade_alpha(elapsed_ms: u64, direction: FadeDirection) -> u8 {
    let ms = elapsed_ms.min(FADE_DURATION_MS);
    let t = match direction {
        FadeDirection::In => ms,
        FadeDirection::Out => FADE_DURATION_MS - ms,
    } as f32
        / FADE_DURATION_MS as f32;
    (ease_out_cubic(t) * 255.0).round() as u8
}

/// Spotlight radius scale in permille of the settled radius for the eased
/// progress byte `alpha`: 60% ([`SPOTLIGHT_SCALE_MIN_PM`]) at alpha 0 up to
/// 100% (1000) at alpha 255, linear in the (already eased) progress.
pub fn spotlight_radius_scale(alpha: u8) -> u32 {
    SPOTLIGHT_SCALE_MIN_PM + (u32::from(alpha) * (1000 - SPOTLIGHT_SCALE_MIN_PM) + 127) / 255
}

/// Ease-out cubic (`1 - (1-t)^3`): full initial velocity decelerating to a
/// rest — Material's deceleration curve for entering elements, a close
/// polynomial approximation of `cubic-bezier(0.0, 0.0, 0.2, 1.0)`.
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

/// The transition driver's time source, injected so tests drive the clock
/// instead of waiting on it (test constraint: no real sleeps). Production
/// uses [`FadeClock::system`]; controller tests advance a shared cell from
/// the `sleep` closure, making every transition complete in zero wall-clock
/// time while still walking the full step schedule.
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
    /// the transition's idea of time.
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
        assert!(
            FADE_DURATION_MS <= 240,
            "total duration must never exceed 240 ms"
        );
        assert!(
            FADE_STEPS <= 8,
            "Wayland full-frame presents are capped at 8"
        );
        assert_eq!(FADE_STEP_MS * FADE_STEPS, FADE_DURATION_MS);
    }

    // ---- ease_out_cubic ----

    #[test]
    fn ease_out_cubic_endpoints_and_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Deceleration: most of the distance is covered early.
        assert!(
            ease_out_cubic(0.5) > 0.8,
            "half time covers >80%: {}",
            ease_out_cubic(0.5)
        );
        // Monotonically increasing.
        let mut prev = 0.0;
        for i in 1..=100 {
            let t = i as f32 / 100.0;
            let v = ease_out_cubic(t);
            assert!(v >= prev, "regressed at {t}");
            prev = v;
        }
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
    fn alpha_exit_is_the_exact_time_reverse_of_the_entry() {
        for t in 0..=FADE_DURATION_MS {
            assert_eq!(
                fade_alpha(t, FadeDirection::Out),
                fade_alpha(FADE_DURATION_MS - t, FadeDirection::In),
                "mirror mismatch at {t} ms"
            );
        }
    }

    #[test]
    fn alpha_entry_decelerates() {
        // Ease-out: the first quarter covers more progress than the last.
        let first = fade_alpha(FADE_DURATION_MS / 4, FadeDirection::In);
        let last = 255 - fade_alpha(3 * FADE_DURATION_MS / 4, FadeDirection::In);
        assert!(
            first > last * 2,
            "entry must be front-loaded: first quarter {first} vs last {last}"
        );
    }

    // ---- spotlight_radius_scale ----

    #[test]
    fn radius_scale_endpoints_are_exact() {
        assert_eq!(spotlight_radius_scale(0), SPOTLIGHT_SCALE_MIN_PM);
        assert_eq!(spotlight_radius_scale(255), 1000);
    }

    #[test]
    fn radius_scale_is_monotonic_within_the_60_100_band() {
        let mut prev = SPOTLIGHT_SCALE_MIN_PM;
        for a in 1..=255u16 {
            let s = spotlight_radius_scale(a as u8);
            assert!(s >= prev, "scale regressed at alpha {a}");
            assert!((SPOTLIGHT_SCALE_MIN_PM..=1000).contains(&s));
            prev = s;
        }
    }

    #[test]
    fn radius_scale_midpoint_is_about_80_percent() {
        // Eased midpoint alpha (~223 at half time) puts the circle near 95%;
        // the linear-in-alpha midpoint (128) sits near 80%.
        let s = spotlight_radius_scale(128);
        assert!((795..=810).contains(&s), "alpha 128 scale: {s}");
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
        assert_eq!(step.wait, ms(5)); // next boundary: 50
        let step = fade_step(ms(FADE_DURATION_MS - 1), FadeDirection::Out).expect("still inside");
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
            t += 45; // the surface/compositor stalled: nominal steps missed
        }
        assert_eq!(
            seen,
            vec![
                fade_alpha(0, FadeDirection::In),
                fade_alpha(45, FadeDirection::In),
                fade_alpha(90, FadeDirection::In),
                fade_alpha(135, FadeDirection::In),
                fade_alpha(180, FadeDirection::In),
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
