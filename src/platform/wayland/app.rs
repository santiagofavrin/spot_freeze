//! Wayland application shell: single instance, settings, the portal freeze
//! hotkey and StatusNotifierItem tray feeding one calloop channel, the
//! overlay controller, and the calloop event loop driving the Wayland queue.
//!
//! Mirrors the Windows `app` module's structure with the documented platform
//! differences:
//!
//! - **Intents, not window messages**: the portal hotkey (compositor-level,
//!   works while frozen) and the tray callbacks fire on their own threads and
//!   post `Intent`s over a `calloop::channel`; the event loop applies them
//!   on the main thread, where everything else runs.
//! - **Frozen-mode keys**: while frozen, keys arrive through the focused
//!   overlay surface (EXCLUSIVE keyboard interactivity), not as global
//!   hotkeys. The input module forwards every `KeyDown` to a key listener,
//!   which matches it against the plan computed at freeze time
//!   ([`plan_frozen_registrations`] + [`match_frozen_key`]) and posts the
//!   action as an `Intent::Frozen`.
//! - **Settings**: there is no settings UI on Linux — the tray's
//!   "Edit settings" opens the JSONC file in the default editor; the file is
//!   re-read on every freeze and via the tray's "Reload settings" item, and a
//!   changed `freeze_toggle` rebinds the portal hotkey then.
//! - **Tray actions**: the tray menu's "Spotlight" freezes into spotlight
//!   mode (or activates the spotlight layer when already frozen) and
//!   "Screenshot" freezes first when unfrozen, then enters capture mode —
//!   the same `OverlayController` entry points the frozen-mode keys use.
//! - **Exit**: the tray Exit item quits immediately — no Yes/No confirmation
//!   dialog (documented Linux difference).
//!
//! All protocol glue (surfaces, capture, clipboard) lives in the sibling
//! modules; this file is wiring.

use crate::hotkeys::frozen::{
    FrozenAction, FrozenRegistration, match_frozen_key, plan_frozen_registrations,
};
use crate::hotkeys::gesture::HotkeyGesture;
use crate::overlay::controller::OverlayController;
use crate::overlay::modes::ModeKind;
use crate::platform::shared::edit;
use crate::platform::wayland::capture::WaylandCapturer;
use crate::platform::wayland::clipboard::WaylandServices;
use crate::platform::wayland::hotkeys_portal::PortalHotkey;
use crate::platform::wayland::ipc;
use crate::platform::wayland::shell::{self, Shell};
use crate::platform::wayland::tray::WaylandTray;
use crate::settings::model::AppSettings;
use crate::settings::store;
use anyhow::{Context, Result, anyhow};
use calloop::channel;
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// Cross-thread intents into the event loop (portal hotkey, tray menu, and
/// the frozen-mode key listener all converge here).
enum Intent {
    /// Freeze/unfreeze (portal hotkey; also the unfreeze path while frozen).
    ToggleFreeze,
    /// Tray "Spotlight": freeze into spotlight mode; when already frozen,
    /// activate the spotlight layer (a no-op when the layer is already on).
    Spotlight,
    /// Tray "Screenshot": freeze first when unfrozen, then enter capture mode.
    Screenshot,
    /// Tray "Edit settings": open the JSONC file.
    EditSettings,
    /// Tray "Open settings folder": reveal the JSONC file in the file
    /// manager.
    OpenSettingsFolder,
    /// Tray "Check for updates": stage the latest release and quit so the
    /// replacement helper can install it.
    Update,
    /// Tray "Reload settings": re-read the JSONC file immediately (a changed
    /// freeze binding is re-registered on the spot).
    ReloadSettings,
    /// Tray "Exit": quit immediately (no confirmation dialog on Linux).
    Exit,
    /// A frozen-mode key matched the freeze-time plan.
    Frozen(FrozenAction),
}

/// Whole application state; owned by [`run`]'s stack frame for the lifetime
/// of the event loop.
struct AppState {
    /// Current settings (re-read from disk on every freeze).
    settings: AppSettings,
    settings_path: PathBuf,
    controller: OverlayController,
    shell: Shell,
    capturer: WaylandCapturer,
    services: WaylandServices,
    /// `None` when the portal is unreachable (the tray still works).
    portal: Option<PortalHotkey>,
    /// `None` when no StatusNotifierWatcher exists (the hotkey still works).
    tray: Option<WaylandTray>,
    /// Frozen-mode plan, computed at every freeze from the current settings;
    /// shared with the key listener, empty while unfrozen.
    frozen_plan: Rc<RefCell<Vec<FrozenRegistration>>>,
    update_available: Option<String>,
    exiting: bool,
}

impl AppState {
    fn handle_intent(&mut self, intent: Intent) {
        match intent {
            Intent::ToggleFreeze => self.toggle_freeze(),
            Intent::Spotlight => self.spotlight(),
            Intent::Screenshot => self.screenshot(),
            Intent::EditSettings => {
                if let Err(e) = edit::open_in_editor(&self.settings_path) {
                    eprintln!("spotfreeze: could not open the settings editor: {e:#}");
                }
            }
            Intent::OpenSettingsFolder => {
                if let Err(e) = edit::open_settings_folder(&self.settings_path) {
                    eprintln!("spotfreeze: could not open the settings folder: {e:#}");
                }
            }
            Intent::Update => self.update(),
            Intent::ReloadSettings => self.reload_settings(),
            Intent::Frozen(action) => self.apply_frozen_action(action),
            Intent::Exit => self.exiting = true,
        }
    }

    fn update(&mut self) {
        if self.update_available.is_none() {
            if let Some(tray) = self.tray.as_mut() {
                let _ = tray.set_update_state("Checking…", false);
            }
            match crate::update::check_latest() {
                Ok(crate::update::CheckResult::UpToDate) => {
                    if let Some(tray) = self.tray.as_mut() {
                        let _ = tray.set_update_state("SpotFreeze is up to date", false);
                    }
                }
                Ok(crate::update::CheckResult::Available { version }) => {
                    if let Some(tray) = self.tray.as_mut() {
                        let _ = tray
                            .set_update_state(&format!("Download and install v{version}"), true);
                    }
                    self.update_available = Some(version);
                }
                Err(e) => {
                    if let Some(tray) = self.tray.as_mut() {
                        let _ = tray.set_update_state("Check for updates…", true);
                    }
                    eprintln!("spotfreeze: could not check for updates: {e:#}");
                }
            }
            return;
        }
        if let Some(tray) = self.tray.as_mut() {
            let _ = tray.set_update_state("Downloading and installing…", false);
        }
        match crate::update::stage_latest(|_, _| ()) {
            Ok(()) => self.exiting = true,
            Err(e) => {
                self.update_available = None;
                if let Some(tray) = self.tray.as_mut() {
                    let _ = tray.set_update_state("Check for updates…", true);
                }
                eprintln!("spotfreeze: could not update: {e:#}");
            }
        }
    }

    /// Freeze/unfreeze toggle (the portal hotkey's only job).
    fn toggle_freeze(&mut self) {
        if self.controller.is_frozen() {
            unfreeze_syncing_plan(&mut self.controller, &self.frozen_plan);
            return;
        }
        self.freeze_with_plan();
    }

    /// The freeze half of the toggle: reload settings, freeze into Spotlight
    /// mode (the controller default), and arm the frozen-mode key plan.
    fn freeze_with_plan(&mut self) {
        self.reload_settings();
        let plan = plan_frozen_registrations(&self.settings.hotkeys);
        let factory = self.shell.create_surface_factory();
        match self
            .controller
            .freeze(&self.capturer, &self.settings, &factory, &self.services)
        {
            Ok(()) => {
                *self.frozen_plan.borrow_mut() = plan;
                // If the compositor denied exclusive keyboard focus, demote
                // the surfaces to on-demand (click-to-focus) when possible.
                self.shell.ensure_keyboard_focus();
            }
            Err(e) => eprintln!("spotfreeze: could not freeze the screen: {e:#}"),
        }
    }

    /// Tray "Spotlight": freeze into spotlight mode when unfrozen; when
    /// frozen, activate the spotlight layer (a no-op when it is already on).
    fn spotlight(&mut self) {
        if self.controller.is_frozen() {
            self.controller
                .add_mode(ModeKind::Spotlight, &self.services);
        } else {
            self.freeze_with_plan();
        }
    }

    /// Tray "Screenshot": freeze first when unfrozen, then enter capture
    /// mode (a failed freeze leaves the session unfrozen and `set_mode`
    /// no-ops; the freeze error is already reported).
    fn screenshot(&mut self) {
        if !self.controller.is_frozen() {
            self.freeze_with_plan();
        }
        self.controller.set_mode(ModeKind::Snip, &self.services);
    }

    /// A frozen-mode key matched the plan: apply it exactly like the Windows
    /// shell applies its global-hotkey actions.
    fn apply_frozen_action(&mut self, action: FrozenAction) {
        match action {
            FrozenAction::SetMode(kind) => self.controller.set_mode(kind, &self.services),
            FrozenAction::ToggleMode(kind) => self.controller.toggle_mode(kind, &self.services),
            FrozenAction::AddMode(kind) => self.controller.add_mode(kind, &self.services),
            FrozenAction::Copy => {
                if let Err(e) = self.controller.snip_copy_and_close(&self.services) {
                    eprintln!("spotfreeze: could not copy the snip: {e:#}");
                }
            }
            FrozenAction::Cancel => self.controller.unfreeze(),
            FrozenAction::ResetZoom => self.controller.reset_view(),
        }
        // The controller may have unfrozen itself (copy, or a mode asking to
        // exit): the plan goes stale with the session.
        if !self.controller.is_frozen() {
            self.frozen_plan.borrow_mut().clear();
        }
    }

    /// Re-read the settings file (external edits apply on the NEXT freeze);
    /// keep the in-memory copy on a malformed file. A changed `freeze_toggle`
    /// rebinds the portal hotkey immediately, and the tooltip follows the
    /// binding that is actually live.
    fn reload_settings(&mut self) {
        let reloaded = match store::load(&self.settings_path) {
            Ok(settings) => settings,
            Err(e) => {
                eprintln!(
                    "spotfreeze: could not load {} ({e:#}); keeping the previous settings",
                    self.settings_path.display()
                );
                return;
            }
        };
        if reloaded.hotkeys.freeze_toggle != self.settings.hotkeys.freeze_toggle
            && let Some(portal) = self.portal.as_mut()
        {
            match portal.rebind(reloaded.hotkeys.freeze_toggle) {
                Ok(()) => {
                    if let Some(tray) = self.tray.as_mut() {
                        let _ = tray.set_tooltip(&tooltip_text(&reloaded));
                    }
                }
                Err(e) => eprintln!(
                    "spotfreeze: could not rebind the freeze hotkey {}: {e:#}\n\
                     The previous binding still works.",
                    reloaded.hotkeys.freeze_toggle.to_display()
                ),
            }
        }
        self.settings = reloaded;
    }
}

/// The unfreeze half of the freeze toggle: `unfreeze()` in capture mode only
/// EXITS capture (the session stays frozen), so the frozen plan is cleared
/// only when the session actually ended. Clearing it earlier would strand the
/// still-frozen session: the key listener matches against this plan, so every
/// frozen-mode key would go dead.
fn unfreeze_syncing_plan(
    controller: &mut OverlayController,
    frozen_plan: &RefCell<Vec<FrozenRegistration>>,
) {
    controller.unfreeze();
    if !controller.is_frozen() {
        frozen_plan.borrow_mut().clear();
    }
}

/// Tray tooltip: app name, version, and the current freeze binding.
fn tooltip_text(settings: &AppSettings) -> String {
    format!(
        "SpotFreeze v{} — freeze: {}",
        env!("CARGO_PKG_VERSION"),
        settings.hotkeys.freeze_toggle.to_display()
    )
}

/// Run SpotFreeze until the user exits. Responsibilities, in order:
///
/// 1. **Single instance**: flock on `$XDG_RUNTIME_DIR/spotfreeze.lock`; a
///    second instance exits `Ok(())` immediately WITHOUT touching the desktop.
/// 2. **Wayland**: connect, bind globals, snapshot outputs (see
///    [`shell::Shell::connect`]).
/// 3. **Settings**: load via [`store::load`] (creates `spotfreeze.jsonc` with
///    defaults on first run; malformed file → defaults).
/// 4. **Portal hotkey + tray**: both feed the intent channel. Failures are
///    reported on stderr but never fatal: the other path must keep working.
/// 5. **Event loop**: a calloop loop over the intent channel and the Wayland
///    connection fd; [`shell::Shell::flush`] + [`shell::Shell::dispatch_pending`]
///    run before every poll so events buffered by the capture pump are never
///    stranded.
pub fn run() -> Result<()> {
    // 1. Single instance. `_lock` carries the flock until the process exits.
    let Some(_lock) = shell::acquire_instance_lock()? else {
        return Ok(()); // already running: exit silently, desktop untouched
    };

    // 2. Wayland connection + globals + output snapshot.
    let shell = Shell::connect()?;

    // 3. Settings: malformed JSONC → defaults and keep running (per contract).
    let settings_path = store::default_settings_path().context("locating spotfreeze.jsonc")?;
    store::migrate_legacy_settings(&settings_path);
    let settings = store::load(&settings_path).unwrap_or_default();

    // 4. Event loop + intent channel.
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().context("creating the calloop event loop")?;
    let (intent_tx, intent_rx) = channel::channel::<Intent>();

    let mut state = AppState {
        settings,
        settings_path,
        controller: OverlayController::new(),
        capturer: shell.make_capturer(),
        services: shell.make_services(),
        shell,
        portal: None,
        tray: None,
        frozen_plan: Rc::new(RefCell::new(Vec::new())),
        update_available: None,
        exiting: false,
    };

    // Portal freeze hotkey (compositor-level: keeps working while frozen).
    match PortalHotkey::spawn(state.settings.hotkeys.freeze_toggle, {
        let tx = intent_tx.clone();
        move || {
            let _ = tx.send(Intent::ToggleFreeze);
        }
    }) {
        Ok(portal) => state.portal = Some(portal),
        Err(e) => eprintln!(
            "spotfreeze: could not bind the global freeze hotkey: {e:#}\n\
             The tray menu still works. On Hyprland this needs xdg-desktop-portal-hyprland."
        ),
    }

    // Tray icon (silently absent without a StatusNotifierWatcher).
    match WaylandTray::spawn(
        &tooltip_text(&state.settings),
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::Spotlight);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::Screenshot);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::EditSettings);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::OpenSettingsFolder);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::Update);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::ReloadSettings);
            }
        },
        {
            let tx = intent_tx.clone();
            move || {
                let _ = tx.send(Intent::Exit);
            }
        },
    ) {
        Ok(tray) => state.tray = Some(tray),
        Err(e) => eprintln!(
            "spotfreeze: could not create the tray icon: {e:#}\nThe freeze hotkey still works."
        ),
    }

    // IPC listener for `spotfreeze --spotlight` and `spotfreeze --capture`
    // (compositor keybinds; works regardless of the portal).
    let ipc_listener = match ipc::bind_listener() {
        Ok(listener) => Some(listener),
        Err(e) => {
            eprintln!(
                "spotfreeze: could not bind the IPC socket (CLI mode flags will not work): {e:#}"
            );
            None
        }
    };

    // Frozen-mode key routing: the input module reports every KeyDown here;
    // match the freeze-time plan and post the action.
    state.shell.set_key_listener({
        let plan = state.frozen_plan.clone();
        let tx = intent_tx.clone();
        Rc::new(move |vk, modifiers| {
            let gesture = HotkeyGesture::new(modifiers, vk);
            if let Some(action) = match_frozen_key(&plan.borrow(), gesture) {
                let _ = tx.send(Intent::Frozen(action));
            }
        })
    });

    // Sources: the intent channel and the Wayland connection fd.
    let handle = event_loop.handle();
    handle
        .insert_source(intent_rx, |event, (), state| {
            if let channel::Event::Msg(intent) = event {
                state.handle_intent(intent);
            }
        })
        .map_err(|e| anyhow!("registering the intent channel: {}", e.error))?;
    if let Some(listener) = ipc_listener {
        match listener.try_clone() {
            Ok(poll_listener) => {
                handle
                    .insert_source(
                        Generic::new(poll_listener, Interest::READ, Mode::Level),
                        move |_, _, state| {
                            if let Some(command) = ipc::drain_mode_command(&listener) {
                                match command {
                                    ipc::ModeCommand::Spotlight => state.spotlight(),
                                    ipc::ModeCommand::Capture => state.screenshot(),
                                }
                            }
                            Ok(PostAction::Continue)
                        },
                    )
                    .map_err(|e| anyhow!("registering the IPC source: {}", e.error))?;
            }
            Err(e) => eprintln!(
                "spotfreeze: could not poll the IPC socket (CLI mode flags will not work): {e:#}"
            ),
        }
    }
    let wayland_fd = state
        .shell
        .poll_fd()
        .context("duplicating the Wayland connection fd")?;
    handle
        .insert_source(
            Generic::new(wayland_fd, Interest::READ, Mode::Level),
            |_, _, state| match state.shell.read_and_dispatch() {
                Ok(()) => Ok(PostAction::Continue),
                Err(e) => {
                    eprintln!("spotfreeze: Wayland connection error: {e:#}");
                    state.exiting = true;
                    Ok(PostAction::Remove)
                }
            },
        )
        .map_err(|e| anyhow!("registering the Wayland event source: {}", e.error))?;

    // 5. Main loop.
    while !state.exiting {
        state.shell.flush()?;
        state.shell.dispatch_pending()?;
        event_loop
            .dispatch(None, &mut state)
            .context("dispatching the event loop")?;
        // Present frames deferred while their surface's buffer slots were
        // busy (releases read off the connection during dispatch free them).
        state.controller.process_pending_repaints();
    }

    // Teardown: drop order takes care of the portal, tray, clipboard source,
    // and the connection; the lock releases when `_lock` closes.
    state.controller.unfreeze();
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Headless-safe: in-memory capturer/surfaces/services — no Wayland
    //! connection, no windows, no hotkeys, no clipboard.
    use super::*;
    use crate::capture::{Capturer, DibBuffer, MonitorInfo};
    use crate::geometry::{Point, Rect};
    use crate::overlay::events::OverlayEventSink;
    use crate::platform::{OverlaySurface, PlatformServices, SurfaceFactory};

    struct FakeCapturer {
        captured: Vec<(MonitorInfo, DibBuffer)>,
    }

    impl Capturer for FakeCapturer {
        fn capture_all(&self) -> Result<Vec<(MonitorInfo, DibBuffer)>> {
            Ok(self.captured.clone())
        }
    }

    struct FakeSurface;

    impl OverlaySurface for FakeSurface {
        fn present(&mut self, _frame: &DibBuffer, _dirty: Option<Rect>) -> Result<()> {
            Ok(())
        }
    }

    struct FakeServices;

    impl PlatformServices for FakeServices {
        fn cursor_position_virtual(&self) -> Option<Point> {
            Some(Point::new(4, 4))
        }

        fn copy_image_to_clipboard(&self, _frame: &DibBuffer) -> Result<()> {
            Ok(())
        }
    }

    fn one_monitor() -> Vec<(MonitorInfo, DibBuffer)> {
        let monitor = MonitorInfo {
            rect: Rect::new(0, 0, 8, 8),
            dpi_x: 96,
            dpi_y: 96,
            is_primary: true,
            device_name: String::new(),
        };
        let frame = DibBuffer {
            width: 8,
            height: 8,
            stride: 32,
            pixels: vec![0xAB; 8 * 8 * 4],
        };
        vec![(monitor, frame)]
    }

    fn frozen_controller() -> OverlayController {
        let factory = |_index: usize,
                       _rect: Rect,
                       _rects: Rc<Vec<Rect>>,
                       _sink: OverlayEventSink|
         -> Result<Box<dyn OverlaySurface>> { Ok(Box::new(FakeSurface)) };
        let factory: &SurfaceFactory = &factory;
        let mut controller = OverlayController::new();
        controller
            .freeze(
                &FakeCapturer {
                    captured: one_monitor(),
                },
                &AppSettings::default(),
                factory,
                &FakeServices,
            )
            .expect("freeze with fakes");
        controller
    }

    #[test]
    fn toggle_unfreeze_in_capture_keeps_the_plan_until_the_session_ends() {
        let mut controller = frozen_controller();
        let plan = RefCell::new(plan_frozen_registrations(&AppSettings::default().hotkeys));
        assert!(!plan.borrow().is_empty());

        controller.set_mode(ModeKind::Snip, &FakeServices);
        assert!(controller.is_frozen(), "capture entry keeps the session");

        // First toggle while in capture: only exits capture — the session
        // stays frozen, so the plan must stay live for its keys.
        unfreeze_syncing_plan(&mut controller, &plan);
        assert!(
            controller.is_frozen(),
            "unfreeze in capture only exits capture"
        );
        assert!(
            !plan.borrow().is_empty(),
            "the plan must survive while the session is frozen"
        );

        // Second toggle: the session really ends and the plan dies with it.
        unfreeze_syncing_plan(&mut controller, &plan);
        assert!(!controller.is_frozen());
        assert!(plan.borrow().is_empty(), "the plan dies with the session");
    }
}
