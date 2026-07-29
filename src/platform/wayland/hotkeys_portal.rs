//! Global freeze hotkey via the XDG GlobalShortcuts portal (`ashpd`/zbus).
//!
//! Implementation notes:
//! * The portal trigger string (`"<Super>f"` style) is derived by
//!   [`portal_trigger`], a pure function unit-tested headless; everything
//!   below it touches D-Bus and runs on a dedicated background thread.
//! * `ashpd` is built on zbus in `async-io` mode, so the connection drives
//!   itself on zbus's internal executor thread and the portal thread only
//!   needs a plain `futures_lite::future::block_on` — no Tokio runtime.
//! * One portal session holds the single `toggle` binding. [`PortalHotkey::rebind`]
//!   is REGISTER-FIRST (same contract as the Windows `RegisterHotKey` flow):
//!   the new session is created and bound while the old one is still active,
//!   and the old session is closed only after the new binding is accepted, so
//!   a rejected rebind can never leave NO freeze hotkey registered.
//! * `Activated` signals are filtered by our shortcut id; the callback fires
//!   on the portal thread (the app shell marshals it onto its own loop).
//! * Dropping the handle closes the portal session (unbinding the shortcut)
//!   and joins the thread.

use std::fmt;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::{CreateSessionOptions, Session};
use futures_lite::future;
use futures_lite::stream::StreamExt;

use crate::hotkeys::gesture::HotkeyGesture;
use crate::hotkeys::keymap;

/// The one and only shortcut this app binds through the portal.
const SHORTCUT_ID: &str = "toggle";
/// User-readable description shown by portal UIs for [`SHORTCUT_ID`].
const SHORTCUT_DESCRIPTION: &str = "Toggle screen freeze";

// ---------------------------------------------------------------------------
// Pure: gesture → portal trigger string
// ---------------------------------------------------------------------------

/// X keysym names for the named non-alphanumeric keys, keyed by xkb keysym
/// value (mirrors the vocabulary of [`crate::hotkeys::keymap`]). Letters,
/// digits and F-keys are derived from the keysym value directly instead.
const KEYSYM_NAMES: &[(u32, &str)] = &[
    (0xFF1B, "Escape"),
    (0xFF0D, "Return"),
    (0x0020, "space"),
    (0xFF09, "Tab"),
    (0xFF08, "BackSpace"),
    (0xFFFF, "Delete"),
    (0xFF63, "Insert"),
    (0xFF50, "Home"),
    (0xFF57, "End"),
    (0xFF55, "Page_Up"),   // XK_Prior; Page_Up is the portal-conventional alias
    (0xFF56, "Page_Down"), // XK_Next
    (0xFF52, "Up"),
    (0xFF54, "Down"),
    (0xFF51, "Left"),
    (0xFF53, "Right"),
    (0xFF61, "Print"),
    (0x003D, "equal"),
    (0x002D, "minus"),
    (0x002C, "comma"),
    (0x002E, "period"),
];

/// X keysym name of a Win32 virtual-key code (via [`keymap::vk_to_xkb`]);
/// `None` outside the keymap vocabulary.
fn key_token(vk: u32) -> Option<String> {
    let keysym = keymap::vk_to_xkb(vk)?;
    // Letters and digits: the (lowercase) ASCII keysym IS the trigger name.
    if let Some(c) = char::from_u32(keysym)
        && c.is_ascii_alphanumeric()
    {
        return Some(c.to_string());
    }
    // XK_F1 == 0xFFBE, contiguous through F24 (mirrors keymap).
    if (0xFFBE..=0xFFD5).contains(&keysym) {
        return Some(format!("F{}", keysym - 0xFFBE + 1));
    }
    KEYSYM_NAMES
        .iter()
        .find(|(k, _)| *k == keysym)
        .map(|(_, name)| (*name).to_string())
}

/// Map a gesture to the XDG portal trigger grammar (e.g. `"<Super>f"`,
/// `"<Control><Alt>q"`, `"Escape"`). Modifiers emit `<Control>`, `<Alt>`,
/// `<Shift>`, `<Super>` in that canonical order; the key is the X keysym
/// name. Pure.
pub fn portal_trigger(gesture: HotkeyGesture) -> Result<String, UnmappableTriggerError> {
    let key = key_token(gesture.vk).ok_or(UnmappableTriggerError(gesture))?;
    let mut out = String::new();
    for (flag, token) in [
        (crate::hotkeys::gesture::Modifiers::CTRL, "<Control>"),
        (crate::hotkeys::gesture::Modifiers::ALT, "<Alt>"),
        (crate::hotkeys::gesture::Modifiers::SHIFT, "<Shift>"),
        (crate::hotkeys::gesture::Modifiers::WIN, "<Super>"),
    ] {
        if gesture.modifiers.contains(flag) {
            out.push_str(token);
        }
    }
    out.push_str(&key);
    Ok(out)
}

/// [`portal_trigger`] failure: the gesture's key has no xkb keysym (outside
/// the [`keymap`] vocabulary, e.g. a `0x..` hex-fallback vk).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnmappableTriggerError(pub HotkeyGesture);

impl fmt::Display for UnmappableTriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the hotkey {} has no XDG portal trigger equivalent (its key is not in the keymap vocabulary)",
            self.0.to_display()
        )
    }
}

impl std::error::Error for UnmappableTriggerError {}

// ---------------------------------------------------------------------------
// Portal thread
// ---------------------------------------------------------------------------

/// Intents the owning thread can send to the portal thread.
enum PortalCommand {
    /// Register-first rebind to a new trigger string; the reply carries the
    /// outcome so `rebind` can report success while the old binding is active.
    Rebind {
        trigger: String,
        reply: mpsc::Sender<Result<()>>,
    },
    /// Close the session and end the thread.
    Shutdown,
}

/// Either arm of the portal thread's select loop.
enum LoopEvent {
    /// `None` when the signal stream ended (the bus connection dropped).
    Signal(Option<ashpd::desktop::global_shortcuts::Activated>),
    /// `None` when every command sender hung up.
    Command(Option<PortalCommand>),
}

/// Wake the portal thread after queuing a command: it parks inside
/// [`next_command`] with its waker stored in `wake`.
fn send_command(
    commands: &mpsc::Sender<PortalCommand>,
    wake: &Arc<Mutex<Option<Waker>>>,
    command: PortalCommand,
) -> Result<()> {
    commands
        .send(command)
        .map_err(|_| anyhow!("the portal hotkey thread is not running"))?;
    if let Some(waker) = wake.lock().unwrap().take() {
        waker.wake();
    }
    Ok(())
}

/// Await one command, bridging the blocking `std::sync::mpsc` receiver into
/// the async loop. `None` once all senders are gone.
async fn next_command(
    commands: &mpsc::Receiver<PortalCommand>,
    wake: &Arc<Mutex<Option<Waker>>>,
) -> Option<PortalCommand> {
    future::poll_fn(|cx| match commands.try_recv() {
        Ok(command) => Poll::Ready(Some(command)),
        Err(TryRecvError::Disconnected) => Poll::Ready(None),
        Err(TryRecvError::Empty) => {
            *wake.lock().unwrap() = Some(cx.waker().clone());
            // Re-check AFTER storing the waker: a command queued between the
            // first try_recv and the waker store must not sleep forever (the
            // sender saw an empty slot and skipped the wake).
            match commands.try_recv() {
                Ok(command) => Poll::Ready(Some(command)),
                Err(TryRecvError::Disconnected) => Poll::Ready(None),
                Err(TryRecvError::Empty) => Poll::Pending,
            }
        }
    })
    .await
}

/// Create a fresh portal session and bind `trigger` on it. On any failure
/// after the session exists, the half-bound session is closed before the
/// error propagates.
async fn bind_session(portal: &GlobalShortcuts, trigger: &str) -> Result<Session<GlobalShortcuts>> {
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .context(
            "the XDG GlobalShortcuts portal refused to create a session \
             (is xdg-desktop-portal-hyprland installed and running?)",
        )?;
    let shortcut =
        NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION).preferred_trigger(Some(trigger));
    let bound = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await
        .and_then(|request| request.response());
    match bound {
        Ok(_) => Ok(session),
        Err(e) => {
            let _ = session.close().await;
            Err(e).with_context(|| {
                format!(
                    "the GlobalShortcuts portal rejected the trigger \"{trigger}\" \
                     (is xdg-desktop-portal-hyprland installed and running?)"
                )
            })
        }
    }
}

/// The portal thread's whole life: connect, bind, report readiness, then
/// multiplex `Activated` signals against commands until shutdown.
async fn portal_loop<F>(
    trigger: String,
    on_activated: F,
    ready: mpsc::Sender<Result<()>>,
    commands: mpsc::Receiver<PortalCommand>,
    wake: Arc<Mutex<Option<Waker>>>,
) where
    F: Fn() + Send + 'static,
{
    let portal = match GlobalShortcuts::new().await.with_context(|| {
        "cannot reach the XDG GlobalShortcuts portal \
         (is xdg-desktop-portal-hyprland installed and running?)"
    }) {
        Ok(portal) => portal,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut session = match bind_session(&portal, &trigger).await {
        Ok(session) => session,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut activated = match portal
        .receive_activated()
        .await
        .context("cannot subscribe to GlobalShortcuts Activated signals")
    {
        Ok(stream) => stream,
        Err(e) => {
            let _ = session.close().await;
            let _ = ready.send(Err(e));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        // The owner gave up waiting while we were setting up.
        let _ = session.close().await;
        return;
    }

    loop {
        let signal = async { LoopEvent::Signal(activated.next().await) };
        let command = async { LoopEvent::Command(next_command(&commands, &wake).await) };
        futures_lite::pin!(signal, command);
        match future::race(signal, command).await {
            LoopEvent::Signal(Some(activated)) => {
                if activated.shortcut_id() == SHORTCUT_ID {
                    on_activated();
                }
            }
            // The bus connection dropped; nothing more will arrive.
            LoopEvent::Signal(None) => break,
            LoopEvent::Command(None) => break,
            LoopEvent::Command(Some(PortalCommand::Shutdown)) => {
                let _ = session.close().await;
                break;
            }
            LoopEvent::Command(Some(PortalCommand::Rebind { trigger, reply })) => {
                // REGISTER-FIRST: the old session stays active (and firing)
                // until the new binding is accepted.
                match bind_session(&portal, &trigger).await {
                    Ok(new_session) => {
                        // Best-effort: a failed close only leaks one portal
                        // session until process exit.
                        let _ = session.close().await;
                        session = new_session;
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
        }
    }
}

/// A bound global shortcut session. `on_activated` fires on a dedicated
/// background thread every time the compositor reports the shortcut pressed.
pub struct PortalHotkey {
    commands: mpsc::Sender<PortalCommand>,
    wake: Arc<Mutex<Option<Waker>>>,
    thread: Option<JoinHandle<()>>,
}

impl PortalHotkey {
    /// Create a portal session and bind `trigger` as the global shortcut.
    /// Errors when the portal is unreachable or the compositor rejects the
    /// binding (e.g. `xdg-desktop-portal-hyprland` not installed).
    pub fn spawn<F>(trigger: HotkeyGesture, on_activated: F) -> Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        let trigger = portal_trigger(trigger)?;
        let (commands, command_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::channel();
        let wake = Arc::new(Mutex::new(None));
        let thread = std::thread::Builder::new()
            .name("spotfreeze-portal-hotkey".into())
            .spawn({
                let wake = wake.clone();
                move || {
                    future::block_on(portal_loop(trigger, on_activated, ready, command_rx, wake));
                }
            })
            .context("failed to spawn the portal hotkey thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                wake,
                thread: Some(thread),
            }),
            // The thread already unwound its setup; joining is immediate.
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!(
                    "the portal hotkey thread exited during initialization"
                ))
            }
        }
    }

    /// Rebind to a new trigger (settings save). Errors leave the previous
    /// binding active.
    pub fn rebind(&mut self, trigger: HotkeyGesture) -> Result<()> {
        let trigger = portal_trigger(trigger)?;
        let (reply, reply_rx) = mpsc::channel();
        send_command(
            &self.commands,
            &self.wake,
            PortalCommand::Rebind { trigger, reply },
        )?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("the portal hotkey thread is not running"))?
    }
}

impl Drop for PortalHotkey {
    fn drop(&mut self) {
        let _ = send_command(&self.commands, &self.wake, PortalCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (headless-safe: pure mapping + the channel bridge; never D-Bus)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkeys::gesture::Modifiers;

    fn trigger(gesture: &str) -> String {
        portal_trigger(HotkeyGesture::parse(gesture).unwrap()).unwrap()
    }

    #[test]
    fn default_bindings_map_to_portal_triggers() {
        assert_eq!(trigger("Win+F"), "<Super>f");
        assert_eq!(trigger("Ctrl+Alt+Q"), "<Control><Alt>q");
        assert_eq!(trigger("S"), "s");
        assert_eq!(trigger("0"), "0");
        assert_eq!(trigger("Esc"), "Escape");
        assert_eq!(trigger("Ctrl+C"), "<Control>c");
        assert_eq!(trigger("Shift+F5"), "<Shift>F5");
    }

    #[test]
    fn modifier_tokens_are_canonical_order() {
        assert_eq!(
            trigger("Ctrl+Alt+Shift+Win+A"),
            "<Control><Alt><Shift><Super>a"
        );
        assert_eq!(trigger("Alt+Shift+F"), "<Alt><Shift>f");
        assert_eq!(trigger("Win+Q"), "<Super>q");
        assert_eq!(trigger("Ctrl+Win+Left"), "<Control><Super>Left");
    }

    #[test]
    fn named_keys_map_to_x_keysym_names() {
        let cases: &[(&str, &str)] = &[
            ("Esc", "Escape"),
            ("Enter", "Return"),
            ("Space", "space"),
            ("Tab", "Tab"),
            ("Backspace", "BackSpace"),
            ("Delete", "Delete"),
            ("Insert", "Insert"),
            ("Home", "Home"),
            ("End", "End"),
            ("PageUp", "Page_Up"),
            ("PageDown", "Page_Down"),
            ("Up", "Up"),
            ("Down", "Down"),
            ("Left", "Left"),
            ("Right", "Right"),
            ("PrintScreen", "Print"),
            ("OemPlus", "equal"),
            ("OemMinus", "minus"),
            ("OemComma", "comma"),
            ("OemPeriod", "period"),
        ];
        for (name, want) in cases {
            assert_eq!(&trigger(name), want, "trigger of {name}");
        }
    }

    #[test]
    fn letters_digits_and_fkeys_use_their_names() {
        for c in 'a'..='z' {
            assert_eq!(trigger(&c.to_uppercase().to_string()), c.to_string());
        }
        for c in '0'..='9' {
            assert_eq!(trigger(&c.to_string()), c.to_string());
        }
        for n in 1..=24 {
            assert_eq!(trigger(&format!("F{n}")), format!("F{n}"));
        }
    }

    #[test]
    fn every_parseable_key_maps_to_a_trigger() {
        // The whole gesture-parser vocabulary must stay mappable; mirrors the
        // coverage guard in `hotkeys::keymap`'s tests so drift fails loudly.
        for c in 'A'..='Z' {
            assert!(portal_trigger(HotkeyGesture::new(Modifiers::NONE, c as u32)).is_ok());
        }
        for c in '0'..='9' {
            assert!(portal_trigger(HotkeyGesture::new(Modifiers::NONE, c as u32)).is_ok());
        }
        for n in 1..=24u32 {
            assert!(portal_trigger(HotkeyGesture::new(Modifiers::NONE, 0x70 + (n - 1))).is_ok());
        }
        for &(_, vk) in crate::hotkeys::gesture::NAMED_KEYS {
            assert!(portal_trigger(HotkeyGesture::new(Modifiers::NONE, vk)).is_ok());
        }
    }

    #[test]
    fn unmappable_keys_produce_a_typed_error_naming_the_key() {
        // VK outside the keymap vocabulary (only reachable via the parser's
        // hex fallback or `HotkeyGesture::new`).
        let err = portal_trigger(HotkeyGesture::new(Modifiers::NONE, 0x2A)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("0x2A"), "error names the key: {msg}");
        assert!(msg.contains("XDG portal trigger"), "{msg}");
        let _: &dyn std::error::Error = &err;
        assert_eq!(
            err,
            UnmappableTriggerError(HotkeyGesture::new(Modifiers::NONE, 0x2A))
        );
        // Modifier VKs are not bindable keys either.
        assert!(portal_trigger(HotkeyGesture::new(Modifiers::NONE, 0x10)).is_err());
    }

    #[test]
    fn command_queued_before_polling_is_delivered() {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(Mutex::new(None));
        send_command(&tx, &wake, PortalCommand::Shutdown).unwrap();
        let got = future::block_on(next_command(&rx, &wake));
        assert!(matches!(got, Some(PortalCommand::Shutdown)));
    }

    #[test]
    fn command_wakes_a_parked_waiter() {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(Mutex::new(None));
        let waiter = std::thread::spawn({
            let wake = wake.clone();
            move || future::block_on(next_command(&rx, &wake))
        });
        // Let the waiter park inside poll_fn before sending.
        std::thread::sleep(std::time::Duration::from_millis(50));
        send_command(&tx, &wake, PortalCommand::Shutdown).unwrap();
        let got = waiter.join().unwrap();
        assert!(matches!(got, Some(PortalCommand::Shutdown)));
    }

    #[test]
    fn dropping_all_senders_ends_the_wait() {
        let (tx, rx) = mpsc::channel::<PortalCommand>();
        let wake = Arc::new(Mutex::new(None));
        drop(tx);
        assert!(future::block_on(next_command(&rx, &wake)).is_none());
    }
}
