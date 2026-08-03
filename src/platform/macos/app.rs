//! Application wiring (macOS): accessory `NSApplication`, single instance,
//! Carbon freeze hotkey, status-item tray, settings, overlay controller, and
//! the run loop. Mirrors the Windows `app` module's shape.
//!
//! Differences from the Windows shell, by design:
//!
//! - **Frozen-mode keys arrive through the overlay windows**, not as global
//!   hotkeys. The surface factory below wraps the controller's event sink and
//!   routes `KeyDown` events through [`plan_frozen_registrations`] /
//!   [`match_frozen_key`] → `set_mode` / `add_mode` / `reset_view` /
//!   `snip_copy_and_close` / `unfreeze` (mouse events pass through). The plan
//!   is rebuilt from the current settings at every freeze.
//! - **Settings are edited in a native settings window** (tray "Settings…";
//!   the raw JSONC is reachable via "Open Settings Folder"). Saving persists
//!   to disk and applies immediately (hotkey rebind, auto-start). Settings
//!   are additionally RE-READ from disk at every freeze and the Carbon
//!   hotkey is re-registered (register-first) if `freeze_toggle` changed, so
//!   hand edits of the JSONC still take effect. A malformed file keeps the
//!   previous settings and shows an alert instead of silently resetting to
//!   defaults.
//! - **Exit keeps a confirmation** — an `NSAlert` Yes/No, then cleanup and
//!   `exit(0)`.
//!
//! Reentrancy rule (the AppKit analog of the Windows shell's nested-loop
//! commentary): `NSAlert.runModal` pumps the run loop, which can fire the
//! overlay event monitors and the Carbon hotkey. Therefore no `RefCell`
//! borrow of [`AppState`] is EVER held across an alert: every error is pushed
//! onto a thread-local queue ([`queue_alert`]) and shown by [`drain_alerts`]
//! AFTER the borrow that produced it is released. Action handlers (Carbon
//! callback, tray sink, overlay sink) call `drain_alerts` when done.

use crate::geometry::Rect;
use crate::hotkeys::frozen::{
    FrozenAction, FrozenRegistration, match_frozen_key, plan_frozen_registrations,
};
use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::overlay::controller::OverlayController;
use crate::overlay::events::{OverlayEvent, OverlayEventSink};
use crate::overlay::modes::ModeKind;
use crate::platform::macos::autostart;
use crate::platform::macos::capture::MacCapturer;
use crate::platform::macos::clipboard::MacServices;
use crate::platform::macos::hotkeys::{CarbonHotkey, CarbonHotkeyManager};
use crate::platform::macos::settings_window;
use crate::platform::macos::surface;
use crate::platform::macos::tray::{MacTray, TrayEvent};
use crate::settings::model::AppSettings;
use crate::settings::store;
use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSRunningApplication,
};
use objc2_core_graphics::CGShieldingWindowLevel;
use objc2_foundation::NSString;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// The bundle identifier used for the single-instance check (matches the
/// packaged `SpotFreeze.app`); also the LaunchAgent label ([`crate::autostart`]).
pub(crate) const BUNDLE_ID: &str = "com.spotfreeze.app";

/// Whole application state, shared with the Carbon/tray/overlay callbacks
/// behind a `RefCell`. See the module docs for the borrow discipline.
struct AppState {
    /// Current settings; refreshed from disk at every freeze.
    settings: AppSettings,
    settings_path: PathBuf,
    controller: OverlayController,
    capturer: MacCapturer,
    services: MacServices,
    /// `Some` once the Carbon event handler is installed.
    hotkeys: Option<CarbonHotkeyManager>,
    /// The live freeze-hotkey registration, if it succeeded.
    freeze_hotkey: Option<CarbonHotkey>,
    /// The gesture actually registered right now (`None` when registration
    /// failed — the next freeze then retries it).
    bound_gesture: Option<HotkeyGesture>,
    tray: Option<MacTray>,
    /// Frozen-mode key plan, rebuilt at every freeze.
    frozen_plan: Vec<FrozenRegistration>,
    update_available: Option<String>,
}

impl AppState {
    /// Re-register the Carbon hotkey when the binding changed, REGISTER-FIRST
    /// (the old registration drops only after the new one succeeds, so a
    /// failed rebind can never leave no freeze hotkey registered).
    fn rebind_freeze_hotkey_if_changed(&mut self) {
        let new_gesture = self.settings.hotkeys.freeze_toggle;
        if self.bound_gesture == Some(new_gesture) {
            return;
        }
        let Some(result) = self.hotkeys.as_ref().map(|m| m.register(new_gesture)) else {
            return;
        };
        match result {
            Ok(hotkey) => {
                self.freeze_hotkey = Some(hotkey);
                self.bound_gesture = Some(new_gesture);
                // The tooltip follows the binding that is actually live.
                if let (Some(tray), Some(mtm)) = (&self.tray, MainThreadMarker::new()) {
                    tray.set_tooltip(mtm, &tooltip_text(&self.settings));
                }
            }
            Err(e) => {
                let still = self
                    .bound_gesture
                    .map(|g| format!("\n\nThe previous binding {} still works.", g.to_display()))
                    .unwrap_or_default();
                queue_alert(format!(
                    "Could not register the freeze hotkey {}:\n{e:#}{still}",
                    new_gesture.to_display()
                ));
            }
        }
    }

    /// Route an overlay `KeyDown` through the frozen-mode plan. The modes
    /// never see key events (controller contract) — this is the only
    /// consumer. Errors are deferred, never shown mid-borrow.
    fn on_frozen_key(&mut self, vk: u32, modifiers: Modifiers) {
        if !self.controller.is_frozen() {
            return;
        }
        let gesture = HotkeyGesture::new(modifiers, vk);
        match match_frozen_key(&self.frozen_plan, gesture) {
            Some(FrozenAction::SetMode(kind)) => self.controller.set_mode(kind, &self.services),
            Some(FrozenAction::ToggleMode(kind)) => {
                self.controller.toggle_mode(kind, &self.services)
            }
            Some(FrozenAction::AddMode(kind)) => self.controller.add_mode(kind, &self.services),
            Some(FrozenAction::Copy) => {
                if let Err(e) = self.controller.snip_copy_and_close(&self.services) {
                    queue_alert(format!("Could not copy the snip:\n{e:#}"));
                }
            }
            Some(FrozenAction::Cancel) => self.controller.unfreeze(),
            Some(FrozenAction::ResetZoom) => self.controller.reset_view(),
            None => {}
        }
    }
}

// Deferred error alerts (module docs: never shown while an `AppState`
// borrow is held).
thread_local! {
    static PENDING_ALERTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn queue_alert(message: String) {
    PENDING_ALERTS.with(|q| q.borrow_mut().push(message));
}

/// Show every queued alert; called at the end of each top-level action, with
/// no `AppState` borrow outstanding.
fn drain_alerts() {
    PENDING_ALERTS.with(|q| {
        for message in q.borrow_mut().drain(..) {
            show_alert(&message);
        }
    });
}

/// Run SpotFreeze until the user exits. Responsibilities, in order:
///
/// 1. **Single instance**: another running app with our bundle identifier ⇒
///    exit silently (unpackaged dev runs have no bundle id and skip this).
/// 2. **Activation policy `.accessory`**: no Dock icon, no menu bar (the app
///    lives in the status bar; the bundle also sets `LSUIElement`).
/// 3. **Settings**: load via [`store::load`] (creates `spotfreeze.jsonc` with
///    defaults on first run; malformed file → defaults at startup), then
///    reconcile the LaunchAgent login item with the loaded setting so a
///    hand-edited JSONC takes effect on this launch.
/// 4. **Carbon hotkey** for `freeze_toggle` and the **status item**; failures
///    are reported but never fatal — the other path must always work.
/// 5. **Run loop**: `NSApp run` on the main thread; all callbacks dispatch
///    back into the shared [`AppState`].
pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().context("SpotFreeze must run on the main thread")?;
    if already_running() {
        return Ok(()); // second instance: exit silently, desktop untouched
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let settings_path = store::default_settings_path().context("locating spotfreeze.jsonc")?;
    store::migrate_legacy_settings(&settings_path);
    let settings = store::load(&settings_path).unwrap_or_default();
    // Auto-start: reconcile the LaunchAgent plist with the setting (covers
    // hand-edited JSONC and an exe that moved since the plist was written).
    // Reported but never fatal — the tray and hotkey must always come up.
    if let Err(e) = autostart::apply_auto_start(settings.auto_start) {
        queue_alert(format!("Could not update the login item:\n{e:#}"));
    }
    let state = Rc::new(RefCell::new(AppState {
        settings,
        settings_path,
        controller: OverlayController::new(),
        capturer: MacCapturer::new(),
        services: MacServices,
        hotkeys: None,
        freeze_hotkey: None,
        bound_gesture: None,
        tray: None,
        frozen_plan: Vec::new(),
        update_available: None,
    }));

    let on_hotkey = {
        let state = Rc::clone(&state);
        move || {
            toggle_freeze(&state);
            drain_alerts();
        }
    };
    match CarbonHotkeyManager::install(Box::new(on_hotkey)) {
        Ok(manager) => {
            state.borrow_mut().hotkeys = Some(manager);
            register_freeze_hotkey(&state);
        }
        Err(e) => queue_alert(format!(
            "Could not install the hotkey handler:\n{e:#}\n\nThe tray menu still works."
        )),
    }

    let tray_sink: Rc<dyn Fn(TrayEvent)> = {
        let state = Rc::clone(&state);
        Rc::new(move |event| {
            match event {
                TrayEvent::MenuSpotlight => tray_spotlight(&state),
                TrayEvent::MenuScreenshot => tray_screenshot(&state),
                TrayEvent::MenuSettings => open_settings(&state),
                TrayEvent::MenuOpenSettingsFolder => open_settings_folder(&state),
                TrayEvent::MenuUpdate => update_app(&state),
                TrayEvent::MenuReloadSettings => reload_settings(&state),
                TrayEvent::MenuExit => confirm_exit(&state),
            }
            drain_alerts();
        })
    };
    let tooltip = tooltip_text(&state.borrow().settings);
    match MacTray::create(mtm, &tooltip, tray_sink) {
        Ok(tray) => state.borrow_mut().tray = Some(tray),
        Err(e) => queue_alert(format!(
            "Could not create the status item:\n{e:#}\n\nThe freeze hotkey still works."
        )),
    }

    drain_alerts();
    app.run();
    Ok(())
}

/// `true` when another process with our bundle identifier is already running.
/// Dev builds (no bundle) skip the check: `bundleIdentifier` is nil there.
fn already_running() -> bool {
    if NSRunningApplication::currentApplication()
        .bundleIdentifier()
        .is_none()
    {
        return false;
    }
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(BUNDLE_ID))
        .count()
        > 1
}

/// Register the always-active freeze hotkey; failures defer an alert and
/// leave `bound_gesture` unset so the next freeze retries.
fn register_freeze_hotkey(state: &Rc<RefCell<AppState>>) {
    let mut s = state.borrow_mut();
    let gesture = s.settings.hotkeys.freeze_toggle;
    let Some(result) = s.hotkeys.as_ref().map(|m| m.register(gesture)) else {
        return;
    };
    match result {
        Ok(hotkey) => {
            s.freeze_hotkey = Some(hotkey);
            s.bound_gesture = Some(gesture);
        }
        Err(e) => queue_alert(format!(
            "Could not register the freeze hotkey {}:\n{e:#}\n\nThe tray menu still works.",
            gesture.to_display()
        )),
    }
}

/// Re-read the settings file (edited externally) and follow a changed freeze
/// binding immediately. A malformed file keeps the previous settings (startup
/// uses defaults instead, per the store contract).
fn reload_settings(state: &Rc<RefCell<AppState>>) {
    let mut s = state.borrow_mut();
    match store::load(&s.settings_path) {
        Ok(loaded) => s.settings = loaded,
        Err(e) => queue_alert(format!(
            "Could not read {}:\n{e:#}\n\nKeeping the previous settings.",
            s.settings_path.display()
        )),
    }
    s.rebind_freeze_hotkey_if_changed();
}

/// Freeze/unfreeze toggle on the global hotkey.
fn toggle_freeze(state: &Rc<RefCell<AppState>>) {
    if state.borrow().controller.is_frozen() {
        state.borrow_mut().controller.unfreeze();
        return;
    }
    freeze_if_unfrozen(state);
}

/// Capture + overlay entry; no-op when already frozen ([`TrayEvent::MenuSpotlight`]
/// and [`TrayEvent::MenuScreenshot`] rely on that for their frozen branch).
fn freeze_if_unfrozen(state: &Rc<RefCell<AppState>>) {
    if state.borrow().controller.is_frozen() {
        return;
    }

    reload_settings(state);
    {
        let mut s = state.borrow_mut();
        s.frozen_plan = plan_frozen_registrations(&s.settings.hotkeys);
    }

    // The factory wraps the controller's sink: overlay keys route through
    // the frozen plan (they never reach the modes), everything else passes
    // through. The weak upgrade fails after teardown — a no-op.
    let weak = Rc::downgrade(state);
    let surfaces = move |index: usize,
                         rect: Rect,
                         rects: Rc<Vec<Rect>>,
                         sink: OverlayEventSink|
          -> Result<Box<dyn crate::platform::OverlaySurface>> {
        let weak = weak.clone();
        let wrapped: OverlayEventSink = Rc::new(move |monitor, event| {
            if let OverlayEvent::KeyDown { vk, modifiers } = event {
                if let Some(state) = weak.upgrade() {
                    state.borrow_mut().on_frozen_key(vk, modifiers);
                }
                return;
            }
            sink(monitor, event);
        });
        surface::create_overlay_surface(index, rect, rects, wrapped)
    };

    let result = {
        let mut s = state.borrow_mut();
        // Split the RefMut into disjoint field borrows for the freeze call.
        let AppState {
            controller,
            capturer,
            settings,
            services,
            ..
        } = &mut *s;
        controller.freeze(capturer, settings, &surfaces, services)
    };
    if let Err(e) = result {
        queue_alert(format!("Could not freeze the screen:\n{e:#}"));
    }
}

/// Tray "Spotlight": freeze into spotlight mode when unfrozen (the default
/// freeze mode); when already frozen, activate the spotlight layer — a no-op
/// when the layer is already on.
fn tray_spotlight(state: &Rc<RefCell<AppState>>) {
    if state.borrow().controller.is_frozen() {
        let mut s = state.borrow_mut();
        // Split the RefMut into disjoint field borrows for the mode call.
        let AppState {
            controller,
            services,
            ..
        } = &mut *s;
        controller.add_mode(ModeKind::Spotlight, services);
    } else {
        freeze_if_unfrozen(state);
    }
}

/// Tray "Screenshot": freeze first when unfrozen, then enter snip/capture
/// mode. A failed freeze leaves the overlay closed and `set_mode` no-ops.
fn tray_screenshot(state: &Rc<RefCell<AppState>>) {
    freeze_if_unfrozen(state);
    let mut s = state.borrow_mut();
    // Split the RefMut into disjoint field borrows for the mode call.
    let AppState {
        controller,
        services,
        ..
    } = &mut *s;
    controller.set_mode(ModeKind::Snip, services);
}

/// "Settings…": open the native settings window. `run_modal` pumps its own
/// modal loop, so NO `AppState` borrow may be held across the call (module
/// docs): the current settings and path are cloned out first, the borrow is
/// dropped, and the window runs. On Save, persist and apply exactly like the
/// Windows shell's `apply_saved_settings`/`apply_new_settings` flow.
fn open_settings(state: &Rc<RefCell<AppState>>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let (current, path) = {
        let s = state.borrow();
        (s.settings.clone(), s.settings_path.clone())
    };
    let Some(new_settings) = settings_window::run_modal(mtm, &current) else {
        return; // cancelled / closed: nothing to do
    };
    if let Err(e) = store::save(&path, &new_settings) {
        queue_alert(format!("Could not save {}:\n{e:#}", path.display()));
    }
    // Swap in the new settings and re-register the freeze hotkey if its
    // binding changed (register-first; also refreshes the tray tooltip).
    let auto_start_changed = new_settings.auto_start != current.auto_start;
    let auto_start = new_settings.auto_start;
    {
        let mut s = state.borrow_mut();
        s.settings = new_settings;
        s.rebind_freeze_hotkey_if_changed();
    }
    // Auto-start toggled via the settings window: reconcile the LaunchAgent
    // now (hand-edited JSONC is covered by startup reconciliation).
    if auto_start_changed && let Err(e) = autostart::apply_auto_start(auto_start) {
        queue_alert(format!("Could not update the login item:\n{e:#}"));
    }
}

/// "Open Settings Folder": reveal `spotfreeze.jsonc` in Finder, selected in
/// its folder (detached — the app never blocks on Finder).
fn open_settings_folder(state: &Rc<RefCell<AppState>>) {
    let path = state.borrow().settings_path.clone();
    if let Err(e) = crate::platform::shared::edit::open_settings_folder(&path) {
        queue_alert(format!("Could not open the settings folder:\n{e:#}"));
    }
}

/// Stage the latest release, then exit cleanly so the helper can replace and
/// relaunch the app bundle.
fn update_app(state: &Rc<RefCell<AppState>>) {
    if state.borrow().update_available.is_none() {
        state
            .borrow_mut()
            .tray
            .as_ref()
            .map(|tray| tray.set_update_state("Checking…", false));
        match crate::update::check_latest() {
            Ok(crate::update::CheckResult::UpToDate) => {
                state
                    .borrow()
                    .tray
                    .as_ref()
                    .map(|tray| tray.set_update_state("SpotFreeze is up to date", false));
            }
            Ok(crate::update::CheckResult::Available { version }) => {
                state.borrow_mut().update_available = Some(version.clone());
                state.borrow().tray.as_ref().map(|tray| {
                    tray.set_update_state(&format!("Download and install v{version}"), true)
                });
            }
            Err(e) => {
                state
                    .borrow()
                    .tray
                    .as_ref()
                    .map(|tray| tray.set_update_state("Check for updates…", true));
                queue_alert(format!("Could not check for updates:\n{e:#}"));
            }
        }
        return;
    }
    state
        .borrow()
        .tray
        .as_ref()
        .map(|tray| tray.set_update_state("Downloading and installing…", false));
    match crate::update::stage_latest(|_, _| ()) {
        Ok(()) => {
            let mut s = state.borrow_mut();
            s.controller.unfreeze();
            s.freeze_hotkey = None;
            s.hotkeys = None;
            if let Some(tray) = &s.tray {
                tray.remove();
            }
            s.tray = None;
            drop(s);
            std::process::exit(0);
        }
        Err(e) => {
            state.borrow_mut().update_available = None;
            state
                .borrow()
                .tray
                .as_ref()
                .map(|tray| tray.set_update_state("Check for updates…", true));
            queue_alert(format!("Could not update SpotFreeze:\n{e:#}"));
        }
    }
}

/// Tray "Exit": confirm, clean up, quit.
fn confirm_exit(state: &Rc<RefCell<AppState>>) {
    if !ask_exit_confirmation() {
        return;
    }
    // Idempotent teardown: drop the overlay, unregister the Carbon hotkey and
    // handler, remove the status item. `process::exit` runs no destructors,
    // so this must be explicit.
    let mut s = state.borrow_mut();
    s.controller.unfreeze();
    s.freeze_hotkey = None;
    s.hotkeys = None;
    if let Some(tray) = &s.tray {
        tray.remove();
    }
    s.tray = None;
    drop(s);
    std::process::exit(0);
}

/// Tray tooltip: app name, version, and the current freeze binding (same text as Windows).
fn tooltip_text(settings: &AppSettings) -> String {
    format!(
        "SpotFreeze v{} — freeze: {}",
        env!("CARGO_PKG_VERSION"),
        settings.hotkeys.freeze_toggle.to_display()
    )
}

/// Non-fatal error dialog. The window level is raised above the shielding
/// overlay windows as a safety margin (in practice alerts only appear while
/// unfrozen — every frozen-path failure closes the overlay first).
fn show_alert(message: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("SpotFreeze"));
    alert.setInformativeText(&NSString::from_str(message));
    let _ = alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert
        .window()
        .setLevel(CGShieldingWindowLevel() as isize + 1);
    NSApplication::sharedApplication(mtm).activate();
    alert.runModal();
}

/// The single Yes/No exit confirmation (macOS keeps it, unlike Linux).
fn ask_exit_confirmation() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Exit SpotFreeze?"));
    let _ = alert.addButtonWithTitle(&NSString::from_str("Exit"));
    let _ = alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert
        .window()
        .setLevel(CGShieldingWindowLevel() as isize + 1);
    NSApplication::sharedApplication(mtm).activate();
    alert.runModal() == NSAlertFirstButtonReturn
}
