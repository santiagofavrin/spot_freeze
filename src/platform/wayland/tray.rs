//! StatusNotifierItem tray icon (`ksni`) for the Wayland shell.
//!
//! Implementation notes:
//! * The icon is generated at runtime by a pure function (the shared "frost
//!   spotlight" design: a white circle and sky ring with a small 4-point
//!   sparkle on a deep-navy rounded square) in the SNI `IconPixmap` format:
//!   ARGB32, network byte order (A,R,G,B per pixel).
//! * Clicks: BOTH buttons open the menu. Right-click is the host rendering
//!   the SNI dbusmenu (`ContextMenu` stays unimplemented; the spec route is
//!   the `Menu` property). Left-click goes through ksni's `MENU_ON_ACTIVATE`
//!   (`ItemIsMenu = true`), which KDE Plasma and the GNOME appindicator
//!   extension honor by opening the menu on `Activate`. Limitation: a host
//!   that ignores `ItemIsMenu` and still calls `Activate` gets an
//!   `UnknownMethod` error and shows nothing, and the item can no longer
//!   receive left-click itself — `Tray::activate` is dead with this set.
//! * `ksni` runs in `async-io` mode: its D-Bus connection and service loop are
//!   driven by ksni's/zbus's internal executor threads. Our own thread only
//!   owns the ksni handle and pumps intents (tooltip updates, shutdown) so
//!   [`WaylandTray::set_tooltip`] and `Drop` can report/await outcomes.
//! * Registration TOLERATES a missing StatusNotifierWatcher (bare compositor,
//!   no panel): `assume_sni_available` routes the "watcher not found" failure
//!   to `Tray::watcher_offline` instead of failing `spawn`, and ksni keeps
//!   listening for the watcher name to appear on the bus, re-registering when
//!   it does. The tray is simply not displayed until then; menu callbacks and
//!   the tooltip survive the watcher coming and going.

use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use futures_lite::future;
use ksni::TrayMethods;
use ksni::menu::StandardItem;

/// Edge length of the generated icon pixmap in pixels.
const ICON_SIZE: usize = 32;

/// Smallest icon edge that carries the ring sparkle (below it the arm would
/// rasterize as a stray pixel, not a sparkle).
const SPARKLE_MIN_SIZE: usize = 24;

// ---------------------------------------------------------------------------
// Pure: runtime icon pixmap
// ---------------------------------------------------------------------------

/// The app icon as raw ARGB32 bytes (SNI `IconPixmap` layout: per pixel A,R,G,B
/// in network byte order), `size`×`size`, top-down. The shared "frost
/// spotlight" design: a filled white circle (the spotlight) inside a sky ring
/// on a deep-navy rounded square, with a small 4-point sparkle on the ring at
/// 45° upper-right; pixels outside the rounded square are transparent.
fn build_icon_argb(size: usize) -> Vec<u8> {
    const NAVY: [u8; 3] = [0x0F, 0x17, 0x2A]; // #0F172A — the tile
    const WHITE: [u8; 3] = [0xF8, 0xFA, 0xFC]; // #F8FAFC — circle and sparkle
    const SKY: [u8; 3] = [0x38, 0xBD, 0xF8]; // #38BDF8 — the ring
    let mut data = vec![0u8; size * size * 4];

    let half = size as f32 / 2.0;
    let corner = size as f32 * 0.22; // rounded-square corner radius
    let circle = size as f32 * 0.30; // spotlight circle radius
    let ring_center = size as f32 * 0.43; // ring stroke-center radius
    let ring_half = size as f32 * 0.03; // ring stroke half-width (6% wide)
    // 4-point sparkle centered ON the ring at 45° upper-right. Its center is
    // snapped to the pixel grid so the arms rasterize symmetric at small
    // sizes (at even sizes the snapped center stays exactly at 45°).
    let sparkle_arm = size as f32 * 0.12;
    let sparkle_d = ring_center * std::f32::consts::FRAC_1_SQRT_2;
    let sparkle_cx = (half + sparkle_d).floor() + 0.5;
    let sparkle_cy = (half - sparkle_d).floor() + 0.5;
    for y in 0..size {
        for x in 0..size {
            // Pixel-center offsets from the icon center (signed, y down).
            let px = x as f32 + 0.5 - half;
            let py = y as f32 + 0.5 - half;
            let (dx, dy) = (px.abs(), py.abs());
            // Rounded-square coverage (SDF of a center-aligned rounded rect).
            let qx = (dx - (half - corner)).max(0.0);
            let qy = (dy - (half - corner)).max(0.0);
            if qx * qx + qy * qy > corner * corner {
                continue; // outside the rounded square: stays transparent
            }
            let dist = (dx * dx + dy * dy).sqrt();
            // The sparkle is an astroid (superellipse with exponent 1/2, so
            // the four arms taper to points); it draws OVER the ring.
            let in_sparkle = size >= SPARKLE_MIN_SIZE && {
                let sx = (x as f32 + 0.5 - sparkle_cx).abs();
                let sy = (y as f32 + 0.5 - sparkle_cy).abs();
                sx.sqrt() + sy.sqrt() <= sparkle_arm.sqrt()
            };
            let [r, g, b] = if dist <= circle {
                WHITE
            } else if in_sparkle {
                WHITE
            } else if (dist - ring_center).abs() <= ring_half {
                SKY
            } else {
                NAVY
            };
            let off = (y * size + x) * 4;
            data[off] = 255; // A — SNI IconPixmap is A,R,G,B per pixel
            data[off + 1] = r;
            data[off + 2] = g;
            data[off + 3] = b;
        }
    }
    data
}

/// The app icon as an SNI [`ksni::Icon`].
fn app_icon() -> ksni::Icon {
    ksni::Icon {
        width: ICON_SIZE as i32,
        height: ICON_SIZE as i32,
        data: build_icon_argb(ICON_SIZE),
    }
}

// ---------------------------------------------------------------------------
// ksni tray + thread
// ---------------------------------------------------------------------------

/// The SNI exposed over D-Bus. Callbacks fire on the tray service's thread.
struct SpotFreezeTray<F, G, H, I, J>
where
    F: Fn() + Send + 'static,
    G: Fn() + Send + 'static,
    H: Fn() + Send + 'static,
    I: Fn() + Send + 'static,
    J: Fn() + Send + 'static,
{
    tooltip: String,
    on_spotlight: F,
    on_screenshot: G,
    on_edit_settings: H,
    on_reload_settings: I,
    on_exit: J,
}

impl<F, G, H, I, J> ksni::Tray for SpotFreezeTray<F, G, H, I, J>
where
    F: Fn() + Send + 'static,
    G: Fn() + Send + 'static,
    H: Fn() + Send + 'static,
    I: Fn() + Send + 'static,
    J: Fn() + Send + 'static,
{
    /// Left-click opens the menu, same as right-click (see the module docs
    /// for which hosts honor this and what the others do).
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "spotfreeze".into()
    }

    fn title(&self) -> String {
        "SpotFreeze".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![app_icon()]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "SpotFreeze".into(),
            description: self.tooltip.clone(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Spotlight".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_spotlight)()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Screenshot".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_screenshot)()),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Edit settings".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_edit_settings)()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Reload settings".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_reload_settings)()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Exit".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_exit)()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Intents the owning thread can send to the tray thread.
enum TrayCommand {
    /// Update the tooltip; the reply carries the ksni update outcome.
    SetTooltip {
        tooltip: String,
        reply: mpsc::Sender<Result<()>>,
    },
    /// Shut the service down and end the thread.
    Shutdown,
}

/// The tray thread's whole life: register the SNI (tolerating a missing
/// watcher), report readiness, then serve tooltip updates until shutdown.
fn tray_thread<F, G, H, I, J>(
    tray: SpotFreezeTray<F, G, H, I, J>,
    ready: mpsc::Sender<Result<()>>,
    commands: mpsc::Receiver<TrayCommand>,
) where
    F: Fn() + Send + 'static,
    G: Fn() + Send + 'static,
    H: Fn() + Send + 'static,
    I: Fn() + Send + 'static,
    J: Fn() + Send + 'static,
{
    let handle = match future::block_on(tray.assume_sni_available(true).spawn())
        .context("failed to start the StatusNotifierItem tray service")
    {
        Ok(handle) => handle,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        // The owner gave up waiting while we were registering.
        future::block_on(handle.shutdown());
        return;
    }
    for command in commands {
        match command {
            TrayCommand::SetTooltip { tooltip, reply } => {
                let updated = future::block_on(handle.update(|tray| tray.tooltip = tooltip));
                let _ = reply.send(match updated {
                    Some(()) => Ok(()),
                    None => Err(anyhow!("the tray service has shut down")),
                });
            }
            TrayCommand::Shutdown => {
                future::block_on(handle.shutdown());
                break;
            }
        }
    }
}

/// Tray icon with a "Spotlight" / "Screenshot" / "Edit settings" /
/// "Reload settings" / "Exit" menu; both mouse buttons open the menu (see
/// the module docs). Callbacks fire on the tray's own thread.
pub struct WaylandTray {
    commands: mpsc::Sender<TrayCommand>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandTray {
    /// Register the SNI and show the icon. The callbacks fire in menu order:
    /// "Spotlight", "Screenshot", "Edit settings", "Reload settings", "Exit".
    pub fn spawn<F, G, H, I, J>(
        tooltip: &str,
        on_spotlight: F,
        on_screenshot: G,
        on_edit_settings: H,
        on_reload_settings: I,
        on_exit: J,
    ) -> Result<Self>
    where
        F: Fn() + Send + 'static,
        G: Fn() + Send + 'static,
        H: Fn() + Send + 'static,
        I: Fn() + Send + 'static,
        J: Fn() + Send + 'static,
    {
        let (commands, command_rx) = mpsc::channel();
        let (ready, ready_rx) = mpsc::channel();
        let tray = SpotFreezeTray {
            tooltip: tooltip.to_string(),
            on_spotlight,
            on_screenshot,
            on_edit_settings,
            on_reload_settings,
            on_exit,
        };
        let thread = std::thread::Builder::new()
            .name("spotfreeze-tray".into())
            .spawn(move || tray_thread(tray, ready, command_rx))
            .context("failed to spawn the tray thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                thread: Some(thread),
            }),
            // The thread already unwound its setup; joining is immediate.
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the tray thread exited during initialization"))
            }
        }
    }

    /// Update the hover tooltip (follows the live freeze binding).
    pub fn set_tooltip(&mut self, tooltip: &str) -> Result<()> {
        let (reply, reply_rx) = mpsc::channel();
        self.commands
            .send(TrayCommand::SetTooltip {
                tooltip: tooltip.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("the tray thread is not running"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("the tray thread is not running"))?
    }
}

impl Drop for WaylandTray {
    fn drop(&mut self) {
        let _ = self.commands.send(TrayCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (headless-safe: pure icon pixmap + tray wiring; never D-Bus)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `[A, R, G, B]` of pixel `(x, y)`.
    fn pixel(data: &[u8], size: usize, x: usize, y: usize) -> [u8; 4] {
        let off = (y * size + x) * 4;
        [data[off], data[off + 1], data[off + 2], data[off + 3]]
    }

    #[test]
    fn icon_has_sni_argb32_layout() {
        let icon = app_icon();
        assert_eq!(icon.width, ICON_SIZE as i32);
        assert_eq!(icon.height, ICON_SIZE as i32);
        assert_eq!(icon.data.len(), ICON_SIZE * ICON_SIZE * 4);
    }

    #[test]
    fn icon_alpha_is_binary_with_real_coverage() {
        let data = build_icon_argb(ICON_SIZE);
        // Every pixel is fully transparent or fully opaque — no half-alpha.
        let transparent = data.chunks_exact(4).filter(|p| p[0] == 0).count();
        let opaque = data.chunks_exact(4).filter(|p| p[0] == 255).count();
        assert_eq!(transparent + opaque, ICON_SIZE * ICON_SIZE);
        // The rounded corners actually cut (both kinds exist).
        assert!(transparent > 0, "rounded corners must be transparent");
        assert!(opaque > 0, "the square body must be opaque");
    }

    #[test]
    fn icon_corners_transparent_center_opaque_across_sizes() {
        for size in [16usize, 22, 32, 48] {
            let data = build_icon_argb(size);
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert_eq!(
                    pixel(&data, size, x, y)[0],
                    0,
                    "corner ({x},{y}) at size {size}"
                );
            }
            let mid = size / 2;
            assert_eq!(
                pixel(&data, size, mid, mid)[0],
                255,
                "center at size {size}"
            );
        }
    }

    #[test]
    fn icon_is_the_frost_spotlight_palette_in_network_byte_order() {
        let size = ICON_SIZE;
        let data = build_icon_argb(size);
        // Center: inside the spotlight circle → opaque white, A,R,G,B order.
        assert_eq!(pixel(&data, size, 16, 16), [255, 0xF8, 0xFA, 0xFC]);
        // On the ring (stroke center 43% of the edge): opaque sky.
        assert_eq!(pixel(&data, size, 16, 2), [255, 0x38, 0xBD, 0xF8]);
        assert_eq!(pixel(&data, size, 2, 16), [255, 0x38, 0xBD, 0xF8]);
        // Between the circle and the ring, and at the edge midpoint: navy.
        assert_eq!(pixel(&data, size, 16, 5), [255, 0x0F, 0x17, 0x2A]);
        assert_eq!(pixel(&data, size, 16, 0), [255, 0x0F, 0x17, 0x2A]);
        // Transparent pixels are zeroed (no color bleed into a hidden pixel).
        assert_eq!(pixel(&data, size, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn icon_is_center_symmetric_below_the_sparkle_size() {
        // The sparkle is the only asymmetric element; the tile, circle, and
        // ring stay center-symmetric at every size.
        for size in [16usize, 22] {
            let data = build_icon_argb(size);
            for y in 0..size {
                for x in 0..size {
                    assert_eq!(
                        pixel(&data, size, x, y),
                        pixel(&data, size, size - 1 - x, y),
                        "horizontal asymmetry at ({x}, {y}) size {size}"
                    );
                    assert_eq!(
                        pixel(&data, size, x, y),
                        pixel(&data, size, x, size - 1 - y),
                        "vertical asymmetry at ({x}, {y}) size {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn sparkle_is_upper_right_only_from_24px() {
        let is_white = |p: [u8; 4]| p[0] == 255 && p[1..] == [0xF8, 0xFA, 0xFC];
        for size in [16usize, 22, 24, 32, 48] {
            let data = build_icon_argb(size);
            let half = size as f32 / 2.0;
            let circle = size as f32 * 0.30;
            let mut sparkle_pixels = 0;
            for y in 0..size {
                for x in 0..size {
                    let px = x as f32 + 0.5 - half;
                    let py = y as f32 + 0.5 - half;
                    if is_white(pixel(&data, size, x, y)) && px * px + py * py > circle * circle
                    {
                        sparkle_pixels += 1;
                        assert!(
                            px > 0.0 && py < 0.0,
                            "sparkle pixel outside the upper-right quadrant at ({x}, {y}) size {size}"
                        );
                    }
                }
            }
            if size >= SPARKLE_MIN_SIZE {
                assert!(sparkle_pixels > 0, "no sparkle at size {size}");
            } else {
                assert_eq!(sparkle_pixels, 0, "sparkle below {SPARKLE_MIN_SIZE}px");
            }
        }
    }

    type CounterTray = SpotFreezeTray<
        Box<dyn Fn() + Send>,
        Box<dyn Fn() + Send>,
        Box<dyn Fn() + Send>,
        Box<dyn Fn() + Send>,
        Box<dyn Fn() + Send>,
    >;

    /// Callback invocation counters, one per tray menu action.
    struct Counters {
        spotlights: Arc<AtomicUsize>,
        screenshots: Arc<AtomicUsize>,
        edits: Arc<AtomicUsize>,
        reloads: Arc<AtomicUsize>,
        exits: Arc<AtomicUsize>,
    }

    fn counter_tray() -> (CounterTray, Counters) {
        let counters = Counters {
            spotlights: Arc::new(AtomicUsize::new(0)),
            screenshots: Arc::new(AtomicUsize::new(0)),
            edits: Arc::new(AtomicUsize::new(0)),
            reloads: Arc::new(AtomicUsize::new(0)),
            exits: Arc::new(AtomicUsize::new(0)),
        };
        let bump = |counter: &Arc<AtomicUsize>| {
            let counter = counter.clone();
            Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }) as Box<dyn Fn() + Send>
        };
        let tray = SpotFreezeTray {
            tooltip: "Freeze: Win+F".to_string(),
            on_spotlight: bump(&counters.spotlights),
            on_screenshot: bump(&counters.screenshots),
            on_edit_settings: bump(&counters.edits),
            on_reload_settings: bump(&counters.reloads),
            on_exit: bump(&counters.exits),
        };
        (tray, counters)
    }

    #[test]
    fn sni_properties_follow_the_contract() {
        let tray = counter_tray().0;
        assert_eq!(ksni::Tray::id(&tray), "spotfreeze");
        assert_eq!(ksni::Tray::title(&tray), "SpotFreeze");
        let tip = ksni::Tray::tool_tip(&tray);
        assert_eq!(tip.title, "SpotFreeze");
        assert_eq!(tip.description, "Freeze: Win+F");
        let icons = ksni::Tray::icon_pixmap(&tray);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].width, ICON_SIZE as i32);
    }

    #[test]
    fn menu_items_are_wired_to_the_callbacks() {
        let (mut tray, counters) = counter_tray();
        let menu = ksni::Tray::menu(&tray);
        let mut labels = Vec::new();
        let mut separators = Vec::new();
        for (index, item) in menu.into_iter().enumerate() {
            match item {
                ksni::MenuItem::Standard(item) => {
                    labels.push(item.label.clone());
                    (item.activate)(&mut tray);
                }
                ksni::MenuItem::Separator => separators.push(index),
                _ => panic!("expected standard menu items and separators only"),
            }
        }
        assert_eq!(
            labels,
            [
                "Spotlight",
                "Screenshot",
                "Edit settings",
                "Reload settings",
                "Exit"
            ]
        );
        assert_eq!(separators, [2], "one separator after the freeze actions");
        assert_eq!(counters.spotlights.load(Ordering::Relaxed), 1);
        assert_eq!(counters.screenshots.load(Ordering::Relaxed), 1);
        assert_eq!(counters.edits.load(Ordering::Relaxed), 1);
        assert_eq!(counters.reloads.load(Ordering::Relaxed), 1);
        assert_eq!(counters.exits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn left_click_opens_the_menu_via_item_is_menu() {
        // SNI gives the item no way to pop its own menu on Activate; ksni's
        // MENU_ON_ACTIVATE (ItemIsMenu = true) is the protocol route that
        // makes the host open the menu on left-click.
        assert!(<CounterTray as ksni::Tray>::MENU_ON_ACTIVATE);
    }
}
