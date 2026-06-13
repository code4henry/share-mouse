/// macOS input capture and injection using CoreGraphics CGEvent API.
///
/// v0.6: Uses an active CGEventTap at the HID level to intercept all mouse,
/// keyboard, click, and scroll events at native rate. When the cursor is
/// logically on a remote screen, the tap drops events from the host OS
/// (returning None) and forwards them to the engine → client instead.
///
/// Injection: Uses CGEvent::new_mouse_event / CGEvent::new_keyboard_event + CGEventPost.
/// Cursor: Uses CGWarpMouseCursorPosition, CGDisplayHideCursor/ShowCursor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, CGEventTap,
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

// ── CGEventTap state ────────────────────────────────────

/// Shared state between the tap callback (called on the CFRunLoop thread)
/// and the engine (called on the async runtime).  The tap reads `is_remote`
/// to decide whether to drop events from the host OS.
struct TapState {
    is_remote: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<InputEvent>,
    screen_w: f32,
    screen_h: f32,
}

/// Whether the event type carries cursor position (for edge-detection feed).
fn is_mouse_or_click(t: CGEventType) -> bool {
    use CGEventType::*;
    matches!(t,
        MouseMoved | LeftMouseDown | LeftMouseUp
        | RightMouseDown | RightMouseUp
        | OtherMouseDown | OtherMouseUp
        | LeftMouseDragged | RightMouseDragged | OtherMouseDragged
    )
}

/// Convert a tap CGEventType + event into a MouseButton.
fn button_from_tap(et: CGEventType, _event: &CGEvent) -> MouseButton {
    use CGEventType::*;
    match et {
        LeftMouseDown | LeftMouseUp | LeftMouseDragged => MouseButton::Left,
        RightMouseDown | RightMouseUp | RightMouseDragged => MouseButton::Right,
        _ => MouseButton::Middle, // OtherMouse* — could read MOUSE_EVENT_BUTTON_NUMBER for precision
    }
}

/// The tap callback — runs on the CFRunLoop thread, must not block.
/// Uses `blocking_send` so events are never silently dropped; backpressure
/// naturally limits the forwarding rate.
fn tap_callback(
    _proxy: *const std::ffi::c_void,
    event_type: CGEventType,
    event: &CGEvent,
    state: &TapState,
) -> Option<CGEvent> {
    let remote = state.is_remote.load(Ordering::Relaxed);

    if remote {
        // ── Remote: drop EVERYTHING from the host OS, forward to client ──
        match event_type {
            CGEventType::MouseMoved => {
                let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i16;
                let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i16;
                if dx != 0 || dy != 0 {
                    let _ = state.tx.blocking_send(InputEvent::MouseMove { dx, dy });
                }
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
                let btn = button_from_tap(event_type, event);
                let _ = state.tx.blocking_send(InputEvent::MouseDown { button: btn });
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp | CGEventType::OtherMouseUp => {
                let btn = button_from_tap(event_type, event);
                let _ = state.tx.blocking_send(InputEvent::MouseUp { button: btn });
            }
            CGEventType::ScrollWheel => {
                let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                let _ = state.tx.blocking_send(InputEvent::Scroll { dx: dx as i16, dy: dy as i16 });
            }
            CGEventType::KeyDown => {
                let kc = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                let mods = extract_modifiers(event);
                let _ = state.tx.blocking_send(InputEvent::KeyDown { keycode: kc as u16, modifiers: mods });
            }
            CGEventType::KeyUp => {
                let kc = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                let mods = extract_modifiers(event);
                let _ = state.tx.blocking_send(InputEvent::KeyUp { keycode: kc as u16, modifiers: mods });
            }
            CGEventType::FlagsChanged => {
                // Modifier-only change — forward modifiers via a fake key event.
                let mods = extract_modifiers(event);
                let _ = state.tx.blocking_send(InputEvent::KeyDown { keycode: 0, modifiers: mods });
            }
            _ => {}
        }
        return None; // DROP — host OS never sees this event
    }

    // ── Local: feed cursor position for edge detection, let OS process ──
    if is_mouse_or_click(event_type) {
        let loc = event.location();
        let nx = (loc.x as f32 / state.screen_w).clamp(0.0, 1.0);
        let ny = (loc.y as f32 / state.screen_h).clamp(0.0, 1.0);
        let _ = state.tx.blocking_send(InputEvent::MouseMoveAbsolute { x: nx, y: ny });
    }
    Some(event.clone()) // pass-through to OS
}

pub struct MacOSInput {
    /// Whether the engine has told the tap the cursor is on a remote screen.
    pub tap_is_remote: Arc<AtomicBool>,
}

impl MacOSInput {
    pub fn new() -> Self {
        Self {
            tap_is_remote: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl PlatformInput for MacOSInput {
    fn start_capture(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<InputEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel(1024);

        let is_remote = self.tap_is_remote.clone();
        let display = CGDisplay::main();
        let screen_w = display.pixels_wide().max(1) as f32;
        let screen_h = display.pixels_high().max(1) as f32;
        let event_types = vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDown,  CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown, CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown, CGEventType::OtherMouseUp,
            CGEventType::LeftMouseDragged, CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown, CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ];

        // CGEventTap is not Send — create it inside the dedicated thread.
        std::thread::spawn(move || {
            let state = TapState {
                is_remote,
                tx: tx.clone(),
                screen_w,
                screen_h,
            };
            let state_ref: &'static TapState = Box::leak(Box::new(state)); // lives forever

            let tap = match CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                event_types,
                move |proxy, etype, event| {
                    // catch_unwind prevents a panic from crossing the FFI boundary
                    // (UB). If the callback panics, drop the event.
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tap_callback(proxy, etype, event, state_ref)
                    }))
                    .unwrap_or_else(|_| {
                        log::error!("tap callback panicked — dropping event");
                        None
                    })
                },
            ) {
                Ok(t) => t,
                Err(()) => {
                    log::error!(
                        "CGEventTap failed — grant Accessibility + Input Monitoring in System Settings"
                    );
                    return;
                }
            };

            log::info!(
                "CGEventTap created (HID, active); screen {}x{}",
                screen_w as u32, screen_h as u32
            );

            unsafe {
                let source = tap.mach_port.create_runloop_source(0)
                    .expect("CFMachPortCreateRunLoopSource");
                let run_loop = CFRunLoop::get_current();
                run_loop.add_source(&source, kCFRunLoopCommonModes);
                tap.enable();
                log::info!("CGEventTap thread running");
                CFRunLoop::run_current();
            }
        });

        Ok(rx)
    }

    fn stop_capture(&self) -> anyhow::Result<()> {
        // The CFRunLoop thread can't be stopped cleanly from outside —
        // it lives until the process exits. This is acceptable for a
        // system-tray-style app.
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

            InputEvent::MouseMoveNormalized { dx, dy } => {
                let (w, h) = self.get_screen_size()?;
                let (cx, cy) = self.get_cursor_pos()?;
                self.warp_cursor(cx + (*dx * w as f32) as i32, cy + (*dy * h as f32) as i32)?;
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
        log::debug!("warp_cursor ({},{})", x, y);
        unsafe {
            CGWarpMouseCursorPosition(CGPoint::new(x as f64, y as f64));
        }
        Ok(())
    }

    fn check_permission(&self) -> bool {
        check_accessibility_trusted()
    }

    fn set_is_remote(&self, remote: bool) {
        self.tap_is_remote.store(remote, Ordering::Relaxed);
    }

    fn get_screen_size(&self) -> anyhow::Result<(u32, u32)> {
        let display = CGDisplay::main();
        Ok((display.pixels_wide() as u32, display.pixels_high() as u32))
    }

    fn get_cursor_pos(&self) -> anyhow::Result<(i32, i32)> {
        let source = default_source();
        let event = CGEvent::new(source)
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
    fn AXIsProcessTrusted() -> u8;
}

/// Check whether this process has Accessibility permission.
pub fn check_accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}
