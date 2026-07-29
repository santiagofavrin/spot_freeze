//! Thin Win32 wrapper over `RegisterHotKey` / `UnregisterHotKey` / `WM_HOTKEY`.
//!
//! One manager per hidden message window; all calls must happen on the thread
//! that owns `hwnd`. The Win32 calls themselves are not testable headless, so
//! all bookkeeping (id allocation, duplicate detection, id → gesture lookup)
//! lives in the private pure [`Book`] struct, which is unit-tested without
//! ever touching `RegisterHotKey`. Gesture parse/format/validation logic lives
//! in [`crate::hotkeys::gesture`].

use crate::hotkeys::gesture::HotkeyGesture;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use windows::Win32::Foundation::{HWND, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};

/// Opaque registration id (matches the id passed to `RegisterHotKey` and
/// delivered in `WM_HOTKEY`'s `wParam`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HotkeyId(pub i32);

/// Highest id `RegisterHotKey` accepts for a window hotkey (`0x0000`–`0xBFFF`;
/// `0xC000`–`0xFFFF` is reserved for `GlobalAddAtom`-based DLL hotkeys).
/// Ids start at 1 so 0 stays available as a "no hotkey" sentinel if needed.
const MAX_HOTKEY_ID: i32 = 0xBFFF;

/// Pure bookkeeping behind [`HotkeyManager`]: which registration ids map to
/// which gestures, and how the next id is allocated. No Win32 imports or
/// calls — this is the unit-testable core of the manager.
struct Book {
    map: HashMap<i32, HotkeyGesture>,
    next_id: i32,
}

impl Book {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_id: 1,
        }
    }

    /// Validate + allocate + record in one pure step. Mirrors the bookkeeping
    /// half of [`HotkeyManager::register`]; the manager rolls back with
    /// [`Book::remove`] when the subsequent `RegisterHotKey` call fails.
    fn register(&mut self, gesture: HotkeyGesture) -> Result<i32> {
        if !gesture.is_registerable() {
            bail!("hotkey gesture is not registerable");
        }
        if self.map.values().any(|&g| g == gesture) {
            bail!("hotkey gesture is already registered in this manager");
        }
        let id = self.alloc_id()?;
        self.map.insert(id, gesture);
        Ok(id)
    }

    /// First free id in `1..=MAX_HOTKEY_ID`, scanning forward from `next_id`
    /// and wrapping once. Errors only when the whole id space is taken.
    fn alloc_id(&mut self) -> Result<i32> {
        let start = self.next_id;
        loop {
            let id = self.next_id;
            self.next_id = if id >= MAX_HOTKEY_ID { 1 } else { id + 1 };
            if !self.map.contains_key(&id) {
                return Ok(id);
            }
            if self.next_id == start {
                bail!("hotkey id space exhausted");
            }
        }
    }

    /// Remove a recorded id, returning its gesture. Unknown ids are an error.
    fn remove(&mut self, id: i32) -> Result<HotkeyGesture> {
        self.map
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown hotkey id {id}"))
    }

    fn get(&self, id: i32) -> Option<HotkeyGesture> {
        self.map.get(&id).copied()
    }

    fn ids(&self) -> impl Iterator<Item = i32> + '_ {
        self.map.keys().copied()
    }
}

/// Registers global hotkeys on a hidden message window and maps ids back to
/// the gestures that produced them.
pub struct HotkeyManager {
    hwnd: HWND,
    book: Book,
}

impl HotkeyManager {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            book: Book::new(),
        }
    }

    /// Register `gesture` globally. Errors when the gesture fails
    /// [`HotkeyGesture::is_registerable`], when the same gesture is already
    /// registered in THIS manager, or when `RegisterHotKey` fails (e.g. another
    /// application owns the combination).
    pub fn register(&mut self, gesture: HotkeyGesture) -> Result<HotkeyId> {
        let id = self.book.register(gesture)?;
        // SAFETY: `hwnd` is a valid window handle owned by the calling thread
        // (struct contract). `id` is freshly allocated and unused. Modifier
        // bits are Win32-compatible by construction (see `Modifiers`). On
        // failure the bookkeeping is rolled back so the id stays reusable.
        let result = unsafe {
            RegisterHotKey(
                Some(self.hwnd),
                id,
                HOT_KEY_MODIFIERS(gesture.modifiers.bits()),
                gesture.vk,
            )
        };
        if let Err(e) = result {
            let _ = self.book.remove(id);
            return Err(e).context("RegisterHotKey failed (combination may be owned by another application)");
        }
        Ok(HotkeyId(id))
    }

    /// Unregister a previously returned id. Unknown ids are an error.
    pub fn unregister(&mut self, id: HotkeyId) -> Result<()> {
        if self.book.get(id.0).is_none() {
            bail!("unknown hotkey id {}", id.0);
        }
        // SAFETY: `hwnd` is valid per the struct contract and `id` is
        // currently registered on it (checked above against our own map).
        // The map entry is dropped only after Win32 confirms success, so a
        // failure leaves manager state consistent.
        unsafe { UnregisterHotKey(Some(self.hwnd), id.0) }
            .with_context(|| format!("UnregisterHotKey failed for id {}", id.0))?;
        let _ = self.book.remove(id.0);
        Ok(())
    }

    /// Unregister everything this manager registered (used on settings rebind
    /// and on shutdown).
    ///
    /// Best-effort: every id is attempted even if some fail; the first
    /// failure is returned after the sweep. Successfully unregistered ids are
    /// forgotten, failed ones stay in the map so a later retry can see them.
    pub fn unregister_all(&mut self) -> Result<()> {
        let ids: Vec<i32> = self.book.ids().collect();
        let mut first_err: Option<anyhow::Error> = None;
        for id in ids {
            // SAFETY: `hwnd` is valid per the struct contract and every id in
            // `ids` was registered on it by this manager.
            match unsafe { UnregisterHotKey(Some(self.hwnd), id) } {
                Ok(()) => {
                    let _ = self.book.remove(id);
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.into());
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e.context("one or more hotkeys failed to unregister")),
            None => Ok(()),
        }
    }

    pub fn gesture(&self, id: HotkeyId) -> Option<HotkeyGesture> {
        self.book.get(id.0)
    }

    /// Call from the owner window's proc on `WM_HOTKEY`; resolves `wparam` to
    /// the registered `(id, gesture)`. Returns `None` for foreign ids.
    pub fn handle_wm_hotkey(&self, wparam: WPARAM) -> Option<(HotkeyId, HotkeyGesture)> {
        // `WM_HOTKEY` carries the registration id in `wParam`; truncating to
        // i32 matches the id range passed to `RegisterHotKey`.
        let id = HotkeyId(wparam.0 as i32);
        self.book.get(id.0).map(|gesture| (id, gesture))
    }
}

#[cfg(test)]
mod tests {
    //! Headless-safe: only the pure `Book` bookkeeping and the Win32-free
    //! `handle_wm_hotkey` / `gesture` paths are exercised. No test calls
    //! `register`/`unregister`/`unregister_all` on a real manager — those
    //! would hit the global OS hotkey table.
    use super::*;
    use crate::hotkeys::gesture::Modifiers;

    /// Ctrl+<vk> — a normal, registerable chord.
    fn chord(vk: u32) -> HotkeyGesture {
        HotkeyGesture {
            modifiers: Modifiers::CTRL,
            vk,
        }
    }

    #[test]
    fn register_assigns_sequential_ids_and_records_gestures() {
        let mut book = Book::new();
        let f = chord(0x46); // Ctrl+F
        let q = chord(0x51); // Ctrl+Q
        let id_f = book.register(f).unwrap();
        let id_q = book.register(q).unwrap();
        assert_eq!(id_f, 1);
        assert_eq!(id_q, 2);
        assert_eq!(book.get(id_f), Some(f));
        assert_eq!(book.get(id_q), Some(q));
        assert_eq!(book.get(999), None);
    }

    #[test]
    fn duplicate_gesture_is_rejected() {
        let mut book = Book::new();
        let f = chord(0x46);
        book.register(f).unwrap();
        assert!(book.register(f).is_err());
        // Rejection must not consume an id.
        let next = book.register(chord(0x51)).unwrap();
        assert_eq!(next, 2);
    }

    #[test]
    fn modifier_only_chord_is_rejected() {
        let mut book = Book::new();
        // VK_SHIFT (0x10) is a modifier key, not a usable gesture key.
        let bad = HotkeyGesture {
            modifiers: Modifiers::NONE,
            vk: 0x10,
        };
        assert!(book.register(bad).is_err());
    }

    #[test]
    fn remove_returns_gesture_and_frees_lookup() {
        let mut book = Book::new();
        let f = chord(0x46);
        let id = book.register(f).unwrap();
        assert_eq!(book.remove(id).unwrap(), f);
        assert_eq!(book.get(id), None);
        // After removal the gesture may be registered again.
        let id2 = book.register(f).unwrap();
        assert_eq!(book.get(id2), Some(f));
    }

    #[test]
    fn remove_unknown_id_is_an_error() {
        let mut book = Book::new();
        assert!(book.remove(42).is_err());
    }

    #[test]
    fn alloc_id_wraps_past_max_to_one() {
        let mut book = Book::new();
        book.next_id = MAX_HOTKEY_ID;
        let first = book.register(chord(0x46)).unwrap();
        let second = book.register(chord(0x51)).unwrap();
        assert_eq!(first, MAX_HOTKEY_ID);
        assert_eq!(second, 1);
    }

    #[test]
    fn alloc_id_errors_when_space_is_exhausted() {
        let mut book = Book::new();
        // Fill the entire id space directly (bypassing validation — this is
        // bookkeeping-only and stays headless).
        let g = chord(0x46);
        for id in 1..=MAX_HOTKEY_ID {
            book.map.insert(id, g);
        }
        assert!(book.alloc_id().is_err());
    }

    #[test]
    fn handle_wm_hotkey_resolves_registered_and_ignores_foreign_ids() {
        // A null HWND is fine here: handle_wm_hotkey performs no Win32 call.
        let mut mgr = HotkeyManager::new(HWND::default());
        let f = chord(0x46);
        let id = mgr.book.register(f).unwrap();

        let resolved = mgr.handle_wm_hotkey(WPARAM(id as usize));
        assert_eq!(resolved, Some((HotkeyId(id), f)));

        assert_eq!(mgr.handle_wm_hotkey(WPARAM(0x5AFEusize)), None);
        assert_eq!(mgr.gesture(HotkeyId(id)), Some(f));
        assert_eq!(mgr.gesture(HotkeyId(12345)), None);
    }
}
