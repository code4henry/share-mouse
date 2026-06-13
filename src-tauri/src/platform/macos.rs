/// macOS input capture and injection using CoreGraphics CGEvent API.
///
/// v0.1: Uses a polling-based approach for mouse capture and a proper CGEventTap
/// via the core_graphics crate's built-in tap support.
///
/// Injection: Uses CGEvent::new_mouse_event / CGEvent::new_keyboard_event + CGEventPost.
/// Cursor: Uses CGWarpMouseCursorPosition, CGDisplayHideCursor/ShowCursor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation,
    CGEventType, CGMouseButton, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::core::protocol::{InputEvent, Modifiers, MouseButton};
use super::PlatformInput;

/// Helper: create a default CGEventSource.
fn default_source() -> CGEventSource {
    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .expect("create CGEventSource")
}

pub struct MacOSInput {
    capturing: Arc<AtomicBool>,
}

impl MacOSInput {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl PlatformInput for MacOSInput {
    fn start_capture(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<InputEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        self.capturing.store(true, Ordering::SeqCst);

        let capturing = self.capturing.clone();

        // Spawn a polling thread that reads mouse position at ~250 Hz.
        // Sends normalized absolute coordinates (0.0–1.0) so the engine can do
        // edge detection and the peer can map to its own resolution.
        std::thread::spawn(move || {
            let display = CGDisplay::main();
            let mut last_x: f32 = -1.0;
            let mut last_y: f32 = -1.0;

            while capturing.load(Ordering::SeqCst) {
                let source = match CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
                    Ok(s) => s,
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(16));
                        continue;
                    }
                };

                if let Ok(event) = CGEvent::new_mouse_event(
                    source,
                    CGEventType::MouseMoved,
                    CGPoint::new(0.0, 0.0),
                    CGMouseButton::Left,
                ) {
                    let loc = event.location();
                    let w = display.pixels_wide().max(1) as f32;
                    let h = display.pixels_high().max(1) as f32;
                    let nx = (loc.x as f32 / w).clamp(0.0, 1.0);
                    let ny = (loc.y as f32 / h).clamp(0.0, 1.0);

                    if (nx - last_x).abs() > 0.0005 || (ny - last_y).abs() > 0.0005 {
                        last_x = nx;
                        last_y = ny;
                        let _ = tx.try_send(InputEvent::MouseMoveAbsolute { x: nx, y: ny });
                    }
                }

                // ~250 Hz polling for low-latency cursor tracking
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
        });

        Ok(rx)
    }

    fn stop_capture(&self) -> anyhow::Result<()> {
        self.capturing.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn inject_event(&self, event: &InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                let (cx, cy) = self.get_cursor_pos()?;
                self.warp_cursor(cx + *dx as i32, cy + *dy as i32)?;
            }

            InputEvent::MouseMoveAbsolute { x, y } => {
                let (w, h) = self.get_screen_size()?;
                self.warp_cursor((*x * w as f32) as i32, (*y * h as f32) as i32)?;
            }

            InputEvent::MouseDown { button } => {
                let source = default_source();
                let (x, y) = self.get_cursor_pos()?;
                let event_type = match button {
                    MouseButton::Left => CGEventType::LeftMouseDown,
                    MouseButton::Right => CGEventType::RightMouseDown,
                    MouseButton::Middle | MouseButton::Other(_) => CGEventType::OtherMouseDown,
                };
                let cg_event = CGEvent::new_mouse_event(
                    source,
                    event_type,
                    CGPoint::new(x as f64, y as f64),
                    mouse_button_to_cg(*button),
                )
                .map_err(|_| anyhow::anyhow!("mouse down event"))?;
                cg_event.post(CGEventTapLocation::HID);
            }

            InputEvent::MouseUp { button } => {
                let source = default_source();
                let (x, y) = self.get_cursor_pos()?;
                let event_type = match button {
                    MouseButton::Left => CGEventType::LeftMouseUp,
                    MouseButton::Right => CGEventType::RightMouseUp,
                    MouseButton::Middle | MouseButton::Other(_) => CGEventType::OtherMouseUp,
                };
                let cg_event = CGEvent::new_mouse_event(
                    source,
                    event_type,
                    CGPoint::new(x as f64, y as f64),
                    mouse_button_to_cg(*button),
                )
                .map_err(|_| anyhow::anyhow!("mouse up event"))?;
                cg_event.post(CGEventTapLocation::HID);
            }

            InputEvent::Scroll { dx, dy } => {
                let source = default_source();
                let cg_event = CGEvent::new(source)
                    .map_err(|_| anyhow::anyhow!("scroll event"))?;
                cg_event.set_type(CGEventType::ScrollWheel);
                cg_event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 0);
                cg_event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, *dy as i64);
                cg_event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, *dx as i64);
                cg_event.post(CGEventTapLocation::HID);
            }

            InputEvent::KeyDown { keycode, modifiers } => {
                let source = default_source();
                let cg_event = CGEvent::new_keyboard_event(source, *keycode, true)
                    .map_err(|_| anyhow::anyhow!("key down event"))?;
                apply_modifiers(&cg_event, *modifiers);
                cg_event.post(CGEventTapLocation::HID);
            }

            InputEvent::KeyUp { keycode, modifiers } => {
                let source = default_source();
                let cg_event = CGEvent::new_keyboard_event(source, *keycode, false)
                    .map_err(|_| anyhow::anyhow!("key up event"))?;
                apply_modifiers(&cg_event, *modifiers);
                cg_event.post(CGEventTapLocation::HID);
            }

            InputEvent::CursorEnter { x, y } => {
                let (w, h) = self.get_screen_size()?;
                self.warp_cursor((*x * w as f32) as i32, (*y * h as f32) as i32)?;
            }

            _ => {}
        }

        Ok(())
    }

    fn hide_cursor(&self) -> anyhow::Result<()> {
        let display = CGDisplay::main();
        display.hide_cursor().map_err(|c| anyhow::anyhow!("hide_cursor: {c}"))
    }

    fn show_cursor(&self) -> anyhow::Result<()> {
        let display = CGDisplay::main();
        display.show_cursor().map_err(|c| anyhow::anyhow!("show_cursor: {c}"))
    }

    fn warp_cursor(&self, x: i32, y: i32) -> anyhow::Result<()> {
        unsafe {
            CGWarpMouseCursorPosition(CGPoint::new(x as f64, y as f64));
        }
        Ok(())
    }

    fn get_screen_size(&self) -> anyhow::Result<(u32, u32)> {
        let display = CGDisplay::main();
        Ok((display.pixels_wide() as u32, display.pixels_high() as u32))
    }

    fn get_cursor_pos(&self) -> anyhow::Result<(i32, i32)> {
        let source = default_source();
        let event = CGEvent::new_mouse_event(
            source,
            CGEventType::MouseMoved,
            CGPoint::new(0.0, 0.0),
            CGMouseButton::Left,
        )
        .map_err(|_| anyhow::anyhow!("get cursor pos"))?;
        let loc = event.location();
        Ok((loc.x as i32, loc.y as i32))
    }
}

/// Extract modifier flags from a CGEvent.
fn extract_modifiers(event: &CGEvent) -> Modifiers {
    let flags = event.get_flags();
    let mut mods = Modifiers::NONE;
    if flags.contains(CGEventFlags::CGEventFlagShift)     { mods |= Modifiers::SHIFT; }
    if flags.contains(CGEventFlags::CGEventFlagControl)   { mods |= Modifiers::CTRL; }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) { mods |= Modifiers::ALT; }
    if flags.contains(CGEventFlags::CGEventFlagCommand)   { mods |= Modifiers::META; }
    if flags.contains(CGEventFlags::CGEventFlagAlphaShift){ mods |= Modifiers::CAPS; }
    mods
}

/// Apply modifier flags to a CGEvent.
fn apply_modifiers(event: &CGEvent, modifiers: Modifiers) {
    let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
    if modifiers.contains(Modifiers::SHIFT) { flags |= CGEventFlags::CGEventFlagShift; }
    if modifiers.contains(Modifiers::CTRL)  { flags |= CGEventFlags::CGEventFlagControl; }
    if modifiers.contains(Modifiers::ALT)   { flags |= CGEventFlags::CGEventFlagAlternate; }
    if modifiers.contains(Modifiers::META)  { flags |= CGEventFlags::CGEventFlagCommand; }
    if modifiers.contains(Modifiers::CAPS)  { flags |= CGEventFlags::CGEventFlagAlphaShift; }
    event.set_flags(flags);
}

/// Convert our MouseButton to CGMouseButton.
fn mouse_button_to_cg(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
        MouseButton::Other(n) => match n {
            0 => CGMouseButton::Left,
            1 => CGMouseButton::Right,
            2 => CGMouseButton::Center,
            _ => CGMouseButton::Left,
        },
    }
}

extern "C" {
    fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint);
}
