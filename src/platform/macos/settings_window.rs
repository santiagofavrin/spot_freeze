//! Native AppKit settings editor.
//!
//! The validation code in this module deliberately has no AppKit dependency.
//! This keeps malformed fields and conflicting bindings testable on every
//! platform; the modal window below is only an editor for those fields.

use crate::hotkeys::gesture::{HotkeyGesture, Modifiers};
use crate::settings::model::{AppSettings, Rgb};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSButton, NSButtonType, NSFont, NSTextField, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::rc::Rc;

const RADIUS_MIN: u32 = 10;
const RADIUS_MAX: u32 = 2000;
const ZOOM_STEP_MAX: f32 = 4.0;
const ZOOM_MAX_LIMIT: f32 = 64.0;

#[derive(Clone, Debug)]
struct Fields {
    gestures: [String; 6],
    zoom_modifier: String,
    radius: String,
    step_factor: String,
    zoom_min: String,
    zoom_max: String,
    dim_opacity: String,
    snip_dim_opacity: String,
    color: String,
    snip_color: String,
    auto_start: bool,
}

fn fields_from_settings(s: &AppSettings) -> Fields {
    Fields {
        gestures: [
            s.hotkeys.freeze_toggle.to_display(),
            s.hotkeys.mode_spotlight.to_display(),
            s.hotkeys.mode_snip.to_display(),
            s.hotkeys.snip_copy.to_display(),
            s.hotkeys.cancel.to_display(),
            s.hotkeys.reset_zoom.to_display(),
        ],
        zoom_modifier: s.hotkeys.zoom_modifier.to_display(),
        radius: s.spotlight.default_radius.to_string(),
        step_factor: s.zoom.step_factor.to_string(),
        zoom_min: s.zoom.min.to_string(),
        zoom_max: s.zoom.max.to_string(),
        dim_opacity: s.overlay.dim_opacity.to_string(),
        snip_dim_opacity: s.overlay.snip_dim_opacity.to_string(),
        color: s.overlay.color.to_hex(),
        snip_color: s.overlay.snip_color.to_hex(),
        auto_start: s.auto_start,
    }
}

/// Parse and validate all values edited by the window.
///
/// This is intentionally a primitive-string boundary: it is also the
/// contract tested by the headless unit tests below.
fn validate_fields(f: &Fields) -> Result<AppSettings, Vec<String>> {
    let mut errors = Vec::new();
    let mut gestures = Vec::with_capacity(6);
    for (index, text) in f.gestures.iter().enumerate() {
        match HotkeyGesture::parse(text) {
            Ok(g) if g.is_registerable() => gestures.push(g),
            Ok(_) => errors.push(format!("Hotkey {} is not registerable", index + 1)),
            Err(e) => errors.push(format!("Hotkey {}: {e}", index + 1)),
        }
    }
    for i in 0..gestures.len() {
        for j in (i + 1)..gestures.len() {
            if gestures[i] == gestures[j] {
                errors.push(format!(
                    "Hotkey {} conflicts with hotkey {} ({})",
                    i + 1,
                    j + 1,
                    gestures[i].to_display()
                ));
            }
        }
    }
    let zoom_modifier = match Modifiers::parse(&f.zoom_modifier) {
        Ok(m) if !m.is_empty() => Some(m),
        Ok(_) => {
            errors.push("Zoom modifier must contain at least one modifier".into());
            None
        }
        Err(e) => {
            errors.push(format!("Zoom modifier: {e}"));
            None
        }
    };

    let radius = parse_u32(
        "Spotlight radius",
        &f.radius,
        RADIUS_MIN,
        RADIUS_MAX,
        &mut errors,
    );
    let step = parse_f32(
        "Zoom step factor",
        &f.step_factor,
        1.0,
        true,
        ZOOM_STEP_MAX,
        &mut errors,
    );
    let min = parse_f32(
        "Zoom minimum",
        &f.zoom_min,
        1.0,
        false,
        ZOOM_MAX_LIMIT,
        &mut errors,
    );
    let max = parse_f32(
        "Zoom maximum",
        &f.zoom_max,
        1.0,
        false,
        ZOOM_MAX_LIMIT,
        &mut errors,
    );
    if let (Some(min), Some(max)) = (min, max)
        && min >= max
    {
        errors.push("Zoom minimum must be smaller than zoom maximum".into());
    }
    let dim = parse_u8("Overlay opacity", &f.dim_opacity, &mut errors);
    let snip_dim = parse_u8("Capture opacity", &f.snip_dim_opacity, &mut errors);
    let color = parse_color("Overlay color", &f.color, &mut errors);
    let snip_color = parse_color("Capture color", &f.snip_color, &mut errors);

    if !errors.is_empty() || gestures.len() != 6 || zoom_modifier.is_none() {
        return Err(errors);
    }
    let h = &mut AppSettings::default().hotkeys;
    [
        &mut h.freeze_toggle,
        &mut h.mode_spotlight,
        &mut h.mode_snip,
        &mut h.snip_copy,
        &mut h.cancel,
        &mut h.reset_zoom,
    ]
    .into_iter()
    .zip(gestures)
    .for_each(|(slot, value)| *slot = value);
    h.zoom_modifier = zoom_modifier.unwrap();
    Ok(AppSettings {
        hotkeys: h.clone(),
        spotlight: crate::settings::model::SpotlightSettings {
            default_radius: radius.unwrap(),
        },
        zoom: crate::settings::model::ZoomSettings {
            step_factor: step.unwrap(),
            min: min.unwrap(),
            max: max.unwrap(),
        },
        overlay: crate::settings::model::OverlaySettings {
            dim_opacity: dim.unwrap(),
            color: color.unwrap(),
            snip_dim_opacity: snip_dim.unwrap(),
            snip_color: snip_color.unwrap(),
        },
        auto_start: f.auto_start,
    })
}

fn parse_u32(label: &str, text: &str, min: u32, max: u32, errors: &mut Vec<String>) -> Option<u32> {
    match text.trim().parse::<u32>() {
        Ok(v) if (min..=max).contains(&v) => Some(v),
        Ok(_) => {
            errors.push(format!("{label} must be between {min} and {max}"));
            None
        }
        Err(_) => {
            errors.push(format!("{label} must be a whole number"));
            None
        }
    }
}

fn parse_u8(label: &str, text: &str, errors: &mut Vec<String>) -> Option<u8> {
    match text.trim().parse::<u8>() {
        Ok(v) => Some(v),
        Err(_) => {
            errors.push(format!("{label} must be between 0 and 255"));
            None
        }
    }
}

fn parse_f32(
    label: &str,
    text: &str,
    min: f32,
    exclusive: bool,
    max: f32,
    errors: &mut Vec<String>,
) -> Option<f32> {
    match text.trim().parse::<f32>() {
        Ok(v) if v.is_finite() && (if exclusive { v > min } else { v >= min }) && v <= max => {
            Some(v)
        }
        _ => {
            errors.push(format!("{label} must be finite and in the allowed range"));
            None
        }
    }
}

fn parse_color(label: &str, text: &str, errors: &mut Vec<String>) -> Option<Rgb> {
    match Rgb::parse_hex(text.trim()) {
        Ok(c) => Some(c),
        Err(e) => {
            errors.push(format!("{label}: {e}"));
            None
        }
    }
}

struct WindowIvars {
    fields: Vec<Retained<NSTextField>>,
    auto_start: Retained<NSButton>,
    result: Rc<RefCell<Option<AppSettings>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotFreezeSettingsTarget"]
    #[ivars = WindowIvars]
    struct SettingsTarget;

    impl SettingsTarget {
        #[unsafe(method(save:))]
        fn save(&self, _sender: &AnyObject) {
            let values: Vec<String> = self.ivars().fields.iter().map(|f| f.stringValue().to_string()).collect();
            let f = Fields {
                gestures: values[..6].to_vec().try_into().unwrap(),
                zoom_modifier: values[6].clone(),
                radius: values[7].clone(),
                step_factor: values[8].clone(),
                zoom_min: values[9].clone(),
                zoom_max: values[10].clone(),
                dim_opacity: values[11].clone(),
                snip_dim_opacity: values[12].clone(),
                color: values[13].clone(),
                snip_color: values[14].clone(),
                auto_start: {
                    let state: isize = unsafe { msg_send![&*self.ivars().auto_start, state] };
                    state == 1
                },
            };
            match validate_fields(&f) {
                Ok(settings) => {
                    *self.ivars().result.borrow_mut() = Some(settings);
                    NSApplication::sharedApplication(self.mtm()).stopModal();
                }
                Err(errors) => {
                    let alert = NSAlert::new(self.mtm());
                    alert.setMessageText(&NSString::from_str("Check your settings"));
                    alert.setInformativeText(&NSString::from_str(&errors[0]));
                    let _ = alert.addButtonWithTitle(&NSString::from_str("OK"));
                    alert.window().setLevel(1001);
                    alert.runModal();
                }
            }
        }

        #[unsafe(method(cancel:))]
        fn cancel(&self, _sender: &AnyObject) {
            NSApplication::sharedApplication(self.mtm()).stopModal();
        }

    }

    unsafe impl NSObjectProtocol for SettingsTarget {}

    unsafe impl NSWindowDelegate for SettingsTarget {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            NSApplication::sharedApplication(self.mtm()).stopModal();
        }
    }
);

impl SettingsTarget {
    fn new(mtm: MainThreadMarker, ivars: WindowIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

fn field(mtm: MainThreadMarker, frame: NSRect, value: &str) -> Retained<NSTextField> {
    let f = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    f.setStringValue(&NSString::from_str(value));
    f.setEditable(true);
    f
}

fn label(mtm: MainThreadMarker, frame: NSRect, text: &str) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    l
}

/// Show the modal settings editor. Save returns the validated copy; closing
/// the panel or pressing Cancel returns `None`.
pub fn run_modal(mtm: MainThreadMarker, current: &AppSettings) -> Option<AppSettings> {
    let values = fields_from_settings(current);
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 520.0,
            height: 640.0,
        },
    };
    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let window: Retained<NSWindow> = unsafe {
        msg_send![
            NSWindow::alloc(mtm),
            initWithContentRect: rect,
            styleMask: style,
            backing: objc2_app_kit::NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    window.setTitle(&NSString::from_str("SpotFreeze Settings"));
    unsafe { window.setReleasedWhenClosed(false) };

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
    effect.setMaterial(NSVisualEffectMaterial::Sidebar);
    effect.setState(NSVisualEffectState::Active);
    window.setContentView(Some(&effect));

    let strings = [
        values.gestures[0].clone(),
        values.gestures[1].clone(),
        values.gestures[2].clone(),
        values.gestures[3].clone(),
        values.gestures[4].clone(),
        values.gestures[5].clone(),
        values.zoom_modifier.clone(),
        values.radius.clone(),
        values.step_factor.clone(),
        values.zoom_min.clone(),
        values.zoom_max.clone(),
        values.dim_opacity.clone(),
        values.snip_dim_opacity.clone(),
        values.color.clone(),
        values.snip_color.clone(),
    ];
    let mut controls = Vec::new();
    let mut y = 585.0;
    let sections = ["Hotkeys", "Spotlight", "Zoom", "Overlay", "General"];
    for (i, text) in strings.iter().enumerate() {
        if i == 0 || i == 7 || i == 8 || i == 11 || i == 14 {
            let title = sections[match i {
                0 => 0,
                7 => 1,
                8 => 2,
                11 => 3,
                _ => 4,
            }];
            let heading = label(
                mtm,
                NSRect {
                    origin: NSPoint { x: 28.0, y },
                    size: NSSize {
                        width: 460.0,
                        height: 20.0,
                    },
                },
                title,
            );
            heading.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            effect.addSubview(&heading);
            y -= 20.0;
        }
        let row = field(
            mtm,
            NSRect {
                origin: NSPoint { x: 235.0, y },
                size: NSSize {
                    width: 245.0,
                    height: 20.0,
                },
            },
            text,
        );
        let row_label = label(
            mtm,
            NSRect {
                origin: NSPoint { x: 28.0, y },
                size: NSSize {
                    width: 195.0,
                    height: 20.0,
                },
            },
            match i {
                0 => "Freeze toggle",
                1 => "Spotlight",
                2 => "Capture",
                3 => "Copy selection",
                4 => "Cancel / unfreeze",
                5 => "Reset zoom",
                6 => "Zoom modifier",
                7 => "Default radius",
                8 => "Step factor",
                9 => "Minimum zoom",
                10 => "Maximum zoom",
                11 => "Dim opacity",
                12 => "Capture opacity",
                13 => "Overlay color",
                _ => "Capture color",
            },
        );
        effect.addSubview(&row_label);
        effect.addSubview(&row);
        controls.push(row);
        y -= 23.0;
    }
    let check = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect {
            origin: NSPoint { x: 28.0, y: 42.0 },
            size: NSSize {
                width: 300.0,
                height: 24.0,
            },
        },
    );
    check.setTitle(&NSString::from_str("Launch at login"));
    check.setButtonType(NSButtonType::Switch);
    check.setState(if values.auto_start { 1 } else { 0 });
    effect.addSubview(&check);

    let save = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect {
            origin: NSPoint { x: 365.0, y: 14.0 },
            size: NSSize {
                width: 115.0,
                height: 28.0,
            },
        },
    );
    save.setTitle(&NSString::from_str("Save"));
    save.setKeyEquivalent(&NSString::from_str("\r"));
    let cancel = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect {
            origin: NSPoint { x: 240.0, y: 14.0 },
            size: NSSize {
                width: 115.0,
                height: 28.0,
            },
        },
    );
    cancel.setTitle(&NSString::from_str("Cancel"));
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    let result = Rc::new(RefCell::new(None));
    let target = SettingsTarget::new(
        mtm,
        WindowIvars {
            fields: controls,
            auto_start: check,
            result: result.clone(),
        },
    );
    unsafe {
        save.setTarget(Some(&target));
        save.setAction(Some(sel!(save:)));
        cancel.setTarget(Some(&target));
        cancel.setAction(Some(sel!(cancel:)));
    }
    effect.addSubview(&save);
    effect.addSubview(&cancel);
    window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    window.center();
    NSApplication::sharedApplication(mtm).activate();
    window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).runModalForWindow(&window);
    result.borrow_mut().take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_display_round_trips() {
        let original = AppSettings::default();
        assert_eq!(
            validate_fields(&fields_from_settings(&original)).unwrap(),
            original
        );
    }

    #[test]
    fn rejects_bad_hotkey() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.gestures[0] = "not-a-hotkey".into();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_conflicting_hotkeys() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.gestures[1] = f.gestures[0].clone();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_invalid_zoom_and_opacity() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.zoom_min = "4".into();
        f.zoom_max = "2".into();
        f.step_factor = "1".into();
        f.dim_opacity = "256".into();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_bad_colors() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.color = "blue".into();
        f.snip_color = "#12345".into();
        assert!(validate_fields(&f).is_err());
    }
}
