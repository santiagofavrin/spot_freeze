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
//!   re-read on every freeze and a changed `freeze_toggle` rebinds the portal
//!   hotkey then (external edits apply on next freeze, no watcher).
//! - **Exit**: the tray Exit item quits immediately — no Yes/No confirmation
//!   dialog (documented Linux difference).
//!
//! All protocol glue (surfaces, capture, clipboard) lives in the sibling
//! modules; this file is wiring.

use crate::hotkeys::frozen::{FrozenAction, FrozenRegistration, match_frozen_key, plan_frozen_registrations};
use crate::hotkeys::gesture::HotkeyGesture;
use crate::overlay::controller::OverlayController;
use crate::platform::shared::edit;
use crate::platform::wayland::capture::WaylandCapturer;
use crate::platform::wayland::clipboard::WaylandServices;
use crate::platform::wayland::hotkeys_portal::PortalHotkey;
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
    /// Tray "Edit settings" (and tray activation): open the JSONC file.
    EditSettings,
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
    exiting: bool,
}

impl AppState {
    fn handle_intent(&mut self, intent: Intent) {
        match intent {
            Intent::ToggleFreeze => self.toggle_freeze(),
            Intent::EditSettings => {
                if let Err(e) = edit::open_in_editor(&self.settings_path) {
                    eprintln!("spotfreeze: could not open the settings editor: {e:#}");
                }
            }
            Intent::Frozen(action) => self.apply_frozen_action(action),
            Intent::Exit => self.exiting = true,
        }
    }

    /// Freeze/unfreeze toggle (the portal hotkey's only job).
    fn toggle_freeze(&mut self) {
        if self.controller.is_frozen() {
            self.controller.unfreeze();
            self.frozen_plan.borrow_mut().clear();
            return;
        }
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

    /// A frozen-mode key matched the plan: apply it exactly like the Windows
    /// shell applies its global-hotkey actions.
    fn apply_frozen_action(&mut self, action: FrozenAction) {
        match action {
            FrozenAction::SetMode(kind) => self.controller.set_mode(kind, &self.services),
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

/// Tray tooltip: app name + the current freeze binding.
fn tooltip_text(settings: &AppSettings) -> String {
    format!(
        "SpotFreeze — freeze: {}",
        settings.hotkeys.freeze_toggle.to_display()
    )
}

/// Run SpotFreeze until the user exits. Responsibilities, in order:
///
/// 1. **Single instance**: flock on `$XDG_RUNTIME_DIR/spotfreeze.lock`; a
///    second instance exits `Ok(())` immediately WITHOUT touching the desktop.
/// 2. **Wayland**: connect, bind globals, snapshot outputs (see
///    [`shell::Shell::connect`]).
/// 3. **Settings**: load via [`store::load`] (creates `spotfreeze.json` with
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
    let settings_path = store::default_settings_path().context("locating spotfreeze.json")?;
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
                let _ = tx.send(Intent::EditSettings);
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
    }

    // Teardown: drop order takes care of the portal, tray, clipboard source,
    // and the connection; the lock releases when `_lock` closes.
    state.controller.unfreeze();
    Ok(())
}
