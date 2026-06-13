/// macOS input capture and injection using CoreGraphics CGEvent API.
///
/// v0.6: Uses an active CGEventTap at the HID level to intercept all mouse,
/// keyboard, click, and scroll events at native rate. When the cursor is
/// logically on a remote screen, the tap drops events from the host OS
/// (returning None) and forwards them to the engine → client instead.
///
/// Injection: Uses CGEvent::new_mouse_event / CGEvent::new_keyboard_event + CGEventPost.
/// Cursor: Uses CGWarpMouseCursorPosition, CGDisplayHideCursor/ShowCursor.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use core_foundation::runloop::kCFRunLoopCommonModes;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
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

// ── CGEventTap state (raw FFI) ──────────────────────────

/// Shared state between the raw tap callback and the engine.
struct TapState {
    is_remote: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<InputEvent>,
    screen_w: f32,  // logical points (CGDisplayBounds)
    screen_h: f32,
    /// Previous modifier flags for FlagsChanged → proper KeyUp/KeyDown.
    prev_modifiers: std::sync::atomic::AtomicU64,
    /// Dropped-event counter (logged periodically when non-zero).
    drops: std::sync::atomic::AtomicU64,
    /// Pending mouse delta (coalesced when channel is full — avoids stutter).
    pending_dx: std::sync::atomic::AtomicI32,
    pending_dy: std::sync::atomic::AtomicI32,
    /// true = engine running (forward events); false = stopped (teardown).
    running: Arc<AtomicBool>,
    /// Whether prev_modifiers has been seeded since entering Remote mode.
    modifiers_seeded: std::sync::atomic::AtomicBool,
}

fn is_pos_event(et: u32) -> bool {
    matches!(et,
        KCG_EVENT_MOUSE_MOVED
        | KCG_EVENT_LEFT_DOWN | KCG_EVENT_LEFT_UP
        | KCG_EVENT_RIGHT_DOWN | KCG_EVENT_RIGHT_UP
        | KCG_EVENT_OTHER_DOWN | KCG_EVENT_OTHER_UP
        | KCG_EVENT_LEFT_DRAGGED | KCG_EVENT_RIGHT_DRAGGED | KCG_EVENT_OTHER_DRAGGED
    )
}

/// Flush any coalesced pending mouse deltas from a previous overload. Must
/// be called before sending a non-mouse event (click/key/scroll) so ordering
/// is preserved.
fn flush_pending(state: &TapState) {
    let dx = state.pending_dx.swap(0, Ordering::Relaxed) as i16;
    let dy = state.pending_dy.swap(0, Ordering::Relaxed) as i16;
    if dx != 0 || dy != 0 {
        // If this send also fails, we lose the pending delta — but the next
        // mouse move will have fresh deltas anyway.
        if state.tx.try_send(InputEvent::MouseMove { dx, dy }).is_err() {
            state.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn btn_from_type(et: u32, _event: *mut std::ffi::c_void) -> MouseButton {
    match et {
        KCG_EVENT_LEFT_DOWN | KCG_EVENT_LEFT_UP | KCG_EVENT_LEFT_DRAGGED => MouseButton::Left,
        KCG_EVENT_RIGHT_DOWN | KCG_EVENT_RIGHT_UP | KCG_EVENT_RIGHT_DRAGGED => MouseButton::Right,
        _ => {
            // Other — read real button number
            let n = unsafe { CGEventGetIntegerValueField(_event, KCG_FIELD_MOUSE_BTN) } as u8;
            MouseButton::Other(n)
        }
    }
}

/// Raw FFI callback — returns NULL to drop the event, or the event to pass it
/// through. Must be `unsafe extern "C"` with no unwinding.
unsafe extern "C" fn raw_tap_callback(
    _proxy: *const std::ffi::c_void,
    event_type: u32,
    event: *mut std::ffi::c_void,
    user_info: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_info.is_null() || event.is_null() {
            return event;
        }
        let state = &*(user_info as *const TapState);
        // running=false → drop events (stop_capture tears down via CGEventTapEnable + CFRunLoopStop)
        if state.running.load(Ordering::Relaxed) {
            tap_callback_impl(event_type, event, state)
        } else {
            std::ptr::null_mut()
        }
    }));
    match result {
        Ok(ptr) => ptr,
        Err(_) => std::ptr::null_mut(), // panic → drop event
    }
}

/// The implementation — called from raw_tap_callback. Returns a raw CGEventRef.
fn tap_callback_impl(
    event_type: u32,
    event: *mut std::ffi::c_void,
    state: &TapState,
) -> *mut std::ffi::c_void {
    let remote = state.is_remote.load(Ordering::Relaxed);

    if remote {
        // ── Remote: drop from host, forward to client ──
        // On the first event after entering Remote mode, seed the modifier
        // tracker so FlagsChanged doesn't emit spurious key events.
        if !state.modifiers_seeded.swap(true, Ordering::Relaxed) {
            let flags = unsafe { CGEventGetFlags(event) };
            state.prev_modifiers.store(flags, Ordering::Relaxed);
        }
        match event_type {
            KCG_EVENT_MOUSE_MOVED | KCG_EVENT_LEFT_DRAGGED
            | KCG_EVENT_RIGHT_DRAGGED | KCG_EVENT_OTHER_DRAGGED => {
                let dx = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_DELTA_X) } as i16;
                let dy = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_DELTA_Y) } as i16;
                if dx != 0 || dy != 0 {
                    // Accumulate pending deltas (from prior failed sends), clamp to i16.
                    let pdx = state.pending_dx.swap(0, Ordering::Relaxed);
                    let pdy = state.pending_dy.swap(0, Ordering::Relaxed);
                    let tot: (i16, i16) = {
                        let x = (dx as i32).saturating_add(pdx);
                        let y = (dy as i32).saturating_add(pdy);
                        (x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                         y.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                    };
                    if state.tx.try_send(InputEvent::MouseMove { dx: tot.0, dy: tot.1 }).is_err() {
                        // Channel full — park the delta for next frame.
                        state.pending_dx.fetch_add(tot.0 as i32, Ordering::Relaxed);
                        state.pending_dy.fetch_add(tot.1 as i32, Ordering::Relaxed);
                        let drops = state.drops.fetch_add(1, Ordering::Relaxed);
                        if drops % 1000 == 999 {
                            log::warn!("tap: {} events dropped (channel full)", drops + 1);
                        }
                    }
                }
                // Cursor is hidden in Remote mode — this instantaneous jump is
                // invisible. Recentering keeps the physical Mac cursor off every
                // edge, so macOS keeps emitting deltas in the pushed direction
                // and the remote cursor never stalls. CGWarpMouseCursorPosition
                // does NOT post a mouse event → no feedback loop.
                unsafe {
                    CGWarpMouseCursorPosition(CGPoint {
                        x: state.screen_w as f64 / 2.0,
                        y: state.screen_h as f64 / 2.0,
                    });
                }
            }
            KCG_EVENT_LEFT_DOWN | KCG_EVENT_RIGHT_DOWN | KCG_EVENT_OTHER_DOWN => {
                flush_pending(state);
                let btn = btn_from_type(event_type, event);
                if state.tx.try_send(InputEvent::MouseDown { button: btn }).is_err() {
                    state.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            KCG_EVENT_LEFT_UP | KCG_EVENT_RIGHT_UP | KCG_EVENT_OTHER_UP => {
                flush_pending(state);
                let btn = btn_from_type(event_type, event);
                if state.tx.try_send(InputEvent::MouseUp { button: btn }).is_err() {
                    state.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            KCG_EVENT_SCROLL_WHEEL => {
                flush_pending(state);
                let dy = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_SCROLL_AXIS_1) } as i16;
                let dx = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_SCROLL_AXIS_2) } as i16;
                if state.tx.try_send(InputEvent::Scroll { dx, dy }).is_err() {
                    state.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            KCG_EVENT_KEY_DOWN => {
                flush_pending(state);
                let kc = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_KEYCODE) } as u16;
                let mods = raw_modifiers(unsafe { CGEventGetFlags(event) });
                if state.tx.try_send(InputEvent::KeyDown { keycode: kc as u16, modifiers: mods }).is_err() {
                    state.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            KCG_EVENT_KEY_UP => {
                flush_pending(state);
                let kc = unsafe { CGEventGetIntegerValueField(event, KCG_FIELD_KEYCODE) } as u16;
                let mods = raw_modifiers(unsafe { CGEventGetFlags(event) });
                if state.tx.try_send(InputEvent::KeyUp { keycode: kc as u16, modifiers: mods }).is_err() {
                    state.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            KCG_EVENT_FLAGS_CHANGED => {
                flush_pending(state);
                let new_flags = unsafe { CGEventGetFlags(event) };
                let prev = state.prev_modifiers.swap(new_flags, Ordering::Relaxed);
                let delta = new_flags ^ prev;
                if delta != 0 {
                    // Determine which modifier keys changed from the NSEvent flags.
                    // macos modifier flag bits (same as CGEventFlags):
                    const NX_SHIFTMASK:   u64 = 1 << 17; // 0x20000
                    const NX_CONTROLMASK: u64 = 1 << 18; // 0x40000
                    const NX_ALTERNATEMASK: u64 = 1 << 19; // 0x80000
                    const NX_COMMANDMASK: u64 = 1 << 20; // 0x100000
                    const ALL: u64 = NX_SHIFTMASK | NX_CONTROLMASK | NX_ALTERNATEMASK | NX_COMMANDMASK;
                    let ks = [(ALL & delta & NX_SHIFTMASK, 56u16),   // kVK_Shift
                              (ALL & delta & NX_CONTROLMASK, 59u16), // kVK_Control
                              (ALL & delta & NX_ALTERNATEMASK, 58u16), // kVK_Option
                              (ALL & delta & NX_COMMANDMASK, 55u16)]; // kVK_Command
                    for (m, kc) in ks {
                        if m != 0 {
                            let down = (new_flags & m) != 0;
                            let mods = raw_modifiers(new_flags);
                            let evt = if down {
                                InputEvent::KeyDown { keycode: kc, modifiers: mods }
                            } else {
                                InputEvent::KeyUp { keycode: kc, modifiers: mods }
                            };
                            if state.tx.try_send(evt).is_err() {
                                state.drops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // DROP from host OS — return NULL
        return std::ptr::null_mut();
    }

    // ── Local: feed cursor position for edge detection, pass through ──
    // Reset modifier seeding so next Remote entry gets fresh state.
    state.modifiers_seeded.store(false, Ordering::Relaxed);
    if is_pos_event(event_type) {
        let loc = unsafe { CGEventGetLocation(event) };
        let nx = (loc.x as f32 / state.screen_w).clamp(0.0, 1.0);
        let ny = (loc.y as f32 / state.screen_h).clamp(0.0, 1.0);
        if state.tx.try_send(InputEvent::MouseMoveAbsolute { x: nx, y: ny }).is_err() {
            state.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    // Pass through — return the original event (standard CGEventTap pattern).
    event
}

/// Read modifier flags from raw CGEventFlags into our Modifiers bitmask.
fn raw_modifiers(raw: u64) -> Modifiers {
    let mut m = Modifiers::NONE;
    if raw & 0x20000 != 0 { m |= Modifiers::SHIFT; }   // NX_SHIFTMASK
    if raw & 0x40000 != 0 { m |= Modifiers::CTRL; }    // NX_CONTROLMASK
    if raw & 0x80000 != 0 { m |= Modifiers::ALT; }     // NX_ALTERNATEMASK
    if raw & 0x100000 != 0 { m |= Modifiers::META; }   // NX_COMMANDMASK
    if raw & 0x10000 != 0 { m |= Modifiers::CAPS; }    // NX_ALPHASHIFTMASK
    m
}

pub struct MacOSInput {
    /// Whether the engine has told the tap the cursor is on a remote screen.
    pub tap_is_remote: Arc<AtomicBool>,
    /// Set when a tap is already running — prevents double-tap on host restart.
    tap_running: Arc<AtomicBool>,
    /// Raw CFMachPortRef for CGEventTapEnable(false) on stop.
    tap_handle: std::sync::atomic::AtomicUsize,
    /// Raw CFRunLoopRef — stored by the spawned thread, used by stop_capture
    /// to wake the runloop even when idle.
    runloop_handle: std::sync::atomic::AtomicUsize,
}

impl MacOSInput {
    pub fn new() -> Self {
        Self {
            tap_is_remote: Arc::new(AtomicBool::new(false)),
            tap_running: Arc::new(AtomicBool::new(false)),
            tap_handle: std::sync::atomic::AtomicUsize::new(0),
            runloop_handle: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl PlatformInput for MacOSInput {
    fn start_capture(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<InputEvent>> {
        // Prevent double-tap if the user restarts Host without stopping engine.
        if self.tap_running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("CGEventTap is already running — stop the engine first");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(4096);

        let is_remote = self.tap_is_remote.clone();
        let stop_flag = self.tap_running.clone();
        let rl_handle_ptr = &self.runloop_handle as *const std::sync::atomic::AtomicUsize as usize;

        // Use CGDisplayBounds (logical points, Retina-safe) not pixels_wide()
        let bounds = unsafe { CGDisplayBounds(CGDisplay::main().id) };
        let screen_w = bounds.size.width.max(1.0) as f32;
        let screen_h = bounds.size.height.max(1.0) as f32;

        // Build TapState on the heap; the raw callback receives its pointer.
        let state = Box::new(TapState {
            is_remote,
            tx: tx.clone(),
            screen_w,
            screen_h,
            prev_modifiers: std::sync::atomic::AtomicU64::new(0),
            drops: std::sync::atomic::AtomicU64::new(0),
            pending_dx: std::sync::atomic::AtomicI32::new(0),
            pending_dy: std::sync::atomic::AtomicI32::new(0),
            running: stop_flag.clone(),
            modifiers_seeded: std::sync::atomic::AtomicBool::new(false),
        });
        let state_ptr = Box::into_raw(state) as *mut std::ffi::c_void;
        let state_ptr_addr = state_ptr as usize; // raw ptr → Send-safe integer

        let tap: *mut std::ffi::c_void = unsafe {
            CGEventTapCreate(
                KCG_SESSION_EVENT_TAP,
                KCG_HEAD_INSERT_EVENT_TAP,
                KCG_EVENT_TAP_OPTION_DEFAULT,
                KCG_EVENT_MASK,
                raw_tap_callback,
                state_ptr,
            )
        };

        if tap.is_null() {
            self.tap_running.store(false, Ordering::SeqCst);
            unsafe { drop(Box::from_raw(state_ptr as *mut TapState)); }
            anyhow::bail!(
                "CGEventTap failed — grant Accessibility + Input Monitoring in System Settings"
            );
        }

        self.tap_handle.store(tap as usize, Ordering::SeqCst);
        let tap_addr = tap as usize; // Send-safe handle

        log::info!(
            "Raw CGEventTap created (Session, active); logical screen {:.0}x{:.0}",
            screen_w, screen_h
        );

        std::thread::spawn(move || {
            let tap: *mut std::ffi::c_void = tap_addr as *mut std::ffi::c_void;
            let state_ptr = state_ptr_addr as *mut std::ffi::c_void;
            let rl_ptr = rl_handle_ptr as *const std::sync::atomic::AtomicUsize;
            unsafe {
                let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                if source.is_null() {
                    log::error!("CFMachPortCreateRunLoopSource failed");
                    return;
                }
                let rl = CFRunLoopGetCurrent();
                CFRunLoopAddSource(
                    rl,
                    source,
                    kCFRunLoopCommonModes as *const std::ffi::c_void,
                );
                CGEventTapEnable(tap, true);
                // Store runloop ref so stop_capture can wake us.
                (*rl_ptr).store(CFRunLoopGetCurrent() as usize, Ordering::SeqCst);
                log::info!("Raw CGEventTap thread running");
                CFRunLoopRun();

                // After runloop stops, clean up.
                CGEventTapEnable(tap, false);
                let state = &*(state_ptr as *const TapState);
                let d = state.drops.load(Ordering::Relaxed);
                if d > 0 {
                    log::warn!("Tap exiting; {} events dropped due to channel full", d);
                }
                drop(Box::from_raw(state_ptr as *mut TapState));
            }
        });

        Ok(rx)
    }

    fn stop_capture(&self) -> anyhow::Result<()> {
        // Disable the tap immediately so it stops firing new callbacks.
        let handle = self.tap_handle.swap(0, Ordering::SeqCst) as *mut std::ffi::c_void;
        if !handle.is_null() {
            unsafe { CGEventTapEnable(handle, false); }
        }
        // Wake the runloop so the thread exits (even if idle, no callbacks).
        let rl = self.runloop_handle.swap(0, Ordering::SeqCst) as *mut std::ffi::c_void;
        if !rl.is_null() {
            unsafe { CFRunLoopStop(rl); }
        }
        // Tell the callback to drop events (stops forwarding).
        self.tap_running.store(false, Ordering::SeqCst);
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
        let err = unsafe {
            CGWarpMouseCursorPosition(CGPoint::new(x as f64, y as f64))
        };
        if err != 0 {
            log::warn!("warp_cursor({x},{y}) failed: CGError {err}");
        }
        Ok(())
    }

    fn check_permission(&self) -> bool {
        check_accessibility_trusted()
    }

    fn set_is_remote(&self, remote: bool) {
        // When entering Remote mode, seed prev_modifiers from the current flags
        // so the first FlagsChanged doesn't compare against a stale/zero value.
        // We can't access TapState from here (no handle), so use a one-shot
        // via the AtomicBool side-channel: the tap callback reads this flag on
        // the next event and snapshots the modifier state.
        self.tap_is_remote.store(remote, Ordering::Relaxed);
        if remote {
            // Warp-on-every-move needs zero suppression, else macOS lags
            // mouse events ~250 ms after each CGWarpMouseCursorPosition.
            unsafe { CGSetLocalEventsSuppressionInterval(0.0); }
        }
    }

    fn get_screen_size(&self) -> anyhow::Result<(u32, u32)> {
        // Use logical points (CGDisplayBounds), not physical pixels.  The tap
        // normalizes deltas against logical size; the engine must match.
        let bounds = unsafe { CGDisplayBounds(CGDisplay::main().id) };
        Ok((bounds.size.width as u32, bounds.size.height as u32))
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
    fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint) -> i32;
    fn AXIsProcessTrusted() -> u8;
    // Raw CGEventTap FFI — bypasses core-graphics 0.24.0's broken None→event bug
    fn CGEventTapCreate(
        tap: u32, place: u32, options: u32,
        eventsOfInterest: u64,
        callback: RawTapCallback,
        userInfo: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const std::ffi::c_void,
        port: *mut std::ffi::c_void,
        order: isize,
    ) -> *mut std::ffi::c_void;
    fn CFRunLoopAddSource(
        rl: *mut std::ffi::c_void,
        source: *mut std::ffi::c_void,
        mode: *const std::ffi::c_void,
    );
    fn CFRunLoopGetCurrent() -> *mut std::ffi::c_void;
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: *mut std::ffi::c_void);
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    // Event accessors on a raw CGEventRef
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGEventGetIntegerValueField(event: *mut std::ffi::c_void, field: u32) -> i64;
    fn CGEventGetFlags(event: *mut std::ffi::c_void) -> u64;
    fn CGDisplayBounds(display: u32) -> core_graphics::geometry::CGRect;
    fn CGSetLocalEventsSuppressionInterval(seconds: f64);
}

type RawTapCallback = unsafe extern "C" fn(
    proxy: *const std::ffi::c_void,
    event_type: u32,
    event: *mut std::ffi::c_void,
    user_info: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void;

// CGEventTap constants (not exposed by the crate)
const KCG_SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;

// CGEventType values we match on
const KCG_EVENT_MOUSE_MOVED: u32 = 5;
const KCG_EVENT_LEFT_DOWN: u32 = 1;
const KCG_EVENT_LEFT_UP: u32 = 2;
const KCG_EVENT_RIGHT_DOWN: u32 = 3;
const KCG_EVENT_RIGHT_UP: u32 = 4;
const KCG_EVENT_OTHER_DOWN: u32 = 25;
const KCG_EVENT_OTHER_UP: u32 = 26;
const KCG_EVENT_LEFT_DRAGGED: u32 = 6;
const KCG_EVENT_RIGHT_DRAGGED: u32 = 7;
const KCG_EVENT_OTHER_DRAGGED: u32 = 27;
const KCG_EVENT_SCROLL_WHEEL: u32 = 22;
const KCG_EVENT_KEY_DOWN: u32 = 10;
const KCG_EVENT_KEY_UP: u32 = 11;
const KCG_EVENT_FLAGS_CHANGED: u32 = 12;

// EventField raw values (same as crate's EventField)
const KCG_FIELD_DELTA_X: u32 = 4;
const KCG_FIELD_DELTA_Y: u32 = 5;
const KCG_FIELD_SCROLL_AXIS_1: u32 = 11;
const KCG_FIELD_SCROLL_AXIS_2: u32 = 12;
const KCG_FIELD_KEYCODE: u32 = 9;
const KCG_FIELD_MOUSE_BTN: u32 = 3;

// Mask of all event types the tap should intercept (bitfield for CGEventTapCreate)
const KCG_EVENT_MASK: u64 =
    (1 << KCG_EVENT_MOUSE_MOVED)
    | (1 << KCG_EVENT_LEFT_DOWN)   | (1 << KCG_EVENT_LEFT_UP)
    | (1 << KCG_EVENT_RIGHT_DOWN)  | (1 << KCG_EVENT_RIGHT_UP)
    | (1 << KCG_EVENT_OTHER_DOWN)  | (1 << KCG_EVENT_OTHER_UP)
    | (1 << KCG_EVENT_LEFT_DRAGGED)| (1 << KCG_EVENT_RIGHT_DRAGGED)
    | (1 << KCG_EVENT_OTHER_DRAGGED)
    | (1 << KCG_EVENT_SCROLL_WHEEL)
    | (1 << KCG_EVENT_KEY_DOWN)    | (1 << KCG_EVENT_KEY_UP)
    | (1 << KCG_EVENT_FLAGS_CHANGED);

/// Check whether this process has Accessibility permission.
pub fn check_accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}
