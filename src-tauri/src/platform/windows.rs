/// Windows input capture and injection using Win32 API.
///
/// Capture: SetWindowsHookEx(WH_MOUSE_LL / WH_KEYBOARD_LL) in a dedicated thread.
/// Injection: SendInput() for mouse/keyboard events.
/// Cursor: SetCursorPos(), ShowCursor().
/// Screen: GetSystemMetrics(), GetCursorPos().

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_TYPE, KEYBDINPUT, KEYBD_EVENT_FLAGS, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_HWHEEL,
};

// MOUSEEVENTF_MOVE_NOCOALESCE (0x2000) is available in the Windows SDK but
// not yet exported by the windows crate we use.  Define it here so each
// delta is injected independently → smooth 160 Hz cursor on high-refresh
// displays.
const MOUSEEVENTF_MOVE_NOCOALESCE: MOUSE_EVENT_FLAGS =
    MOUSE_EVENT_FLAGS(0x2000u32);
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SetCursorPos, ShowCursor,
    WH_KEYBOARD_LL, WH_MOUSE_LL,
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx,
    GetMessageW, MSG,
    SM_CXSCREEN, SM_CYSCREEN,
};

use crate::core::protocol::{InputEvent, Modifiers, MouseButton};
use super::PlatformInput;

pub struct WindowsInput {
    capturing: Arc<AtomicBool>,
    /// Virtual cursor position (f32 bits) — avoids stale GetCursorPos during drag.
    vx: AtomicU32,
    vy: AtomicU32,
}

impl WindowsInput {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
            vx: AtomicU32::new(0),
            vy: AtomicU32::new(0),
        }
    }
}

// Global pointer for the sender — hooks don't support user data.
static mut HOOK_SENDER: Option<*mut tokio::sync::mpsc::Sender<InputEvent>> = None;

impl PlatformInput for WindowsInput {
    fn start_capture(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<InputEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        self.capturing.store(true, Ordering::SeqCst);

        let capturing = self.capturing.clone();

        std::thread::spawn(move || {
            let tx_ptr = Box::into_raw(Box::new(tx));

            unsafe {
                HOOK_SENDER = Some(tx_ptr);
            }

            let mouse_hook = unsafe {
                SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_callback), None, 0)
            };
            let keyboard_hook = unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_callback), None, 0)
            };

            if mouse_hook.is_err() || keyboard_hook.is_err() {
                log::error!("Failed to install Windows hooks");
                unsafe {
                    if let Some(ptr) = HOOK_SENDER.take() {
                        drop(Box::from_raw(ptr));
                    }
                }
                return;
            }

            let mouse_hook = mouse_hook.unwrap();
            let keyboard_hook = keyboard_hook.unwrap();

            log::info!("Windows input hooks installed");

            // Message pump — required for low-level hooks
            let mut msg = MSG::default();
            while capturing.load(Ordering::SeqCst) {
                unsafe {
                    let ret = GetMessageW(&mut msg, None, 0, 0);
                    if !ret.as_bool() {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            unsafe {
                let _ = UnhookWindowsHookEx(mouse_hook);
                let _ = UnhookWindowsHookEx(keyboard_hook);
                if let Some(ptr) = HOOK_SENDER.take() {
                    drop(Box::from_raw(ptr));
                }
            }
            log::info!("Windows input hooks removed");
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
                // Relative movement via SendInput → smooth, hardware-level.
                // SetCursorPos was the old path — it jumps the cursor
                // absolutely, causing jitter over LAN.
                send_mouse_event(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE,
                    *dx as i32,
                    *dy as i32,
                    0,
                )?;
            }

            InputEvent::MouseMoveAbsolute { x, y } => {
                let (w, h) = self.get_screen_size()?;
                self.warp_cursor((*x * w as f32) as i32, (*y * h as f32) as i32)?;
            }

            InputEvent::MouseMoveNormalized { dx, dy } => {
                // SendInput MOVEMENT path gives DWM-synced cursor rendering
                // (visibly smoother than SetCursorPos).  Delta is computed
                // from the virtual tracker — NO GetCursorPos, NO drift
                // accumulation even with Windows pointer-speed scaling.
                let (w, h) = self.get_screen_size()?;
                let dx_f = *dx * w as f32;
                let dy_f = *dy * h as f32;
                let old_x = f32::from_bits(self.vx.load(Ordering::SeqCst));
                let old_y = f32::from_bits(self.vy.load(Ordering::SeqCst));
                let new_x = (old_x + dx_f).clamp(0.0, (w - 1) as f32);
                let new_y = (old_y + dy_f).clamp(0.0, (h - 1) as f32);
                self.vx.store(f32::to_bits(new_x), Ordering::SeqCst);
                self.vy.store(f32::to_bits(new_y), Ordering::SeqCst);
                // Inject relative delta for DWM-smooth movement.
                send_mouse_event(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE,
                    (new_x - old_x) as i32,
                    (new_y - old_y) as i32,
                    0,
                )?;
            }

            InputEvent::MouseDown { button } => {
                let flags = match button {
                    MouseButton::Left => MOUSEEVENTF_LEFTDOWN,
                    MouseButton::Right => MOUSEEVENTF_RIGHTDOWN,
                    MouseButton::Middle | MouseButton::Other(_) => MOUSEEVENTF_MIDDLEDOWN,
                };
                send_mouse_event(flags, 0, 0, 0)?;
            }

            InputEvent::MouseUp { button } => {
                let flags = match button {
                    MouseButton::Left => MOUSEEVENTF_LEFTUP,
                    MouseButton::Right => MOUSEEVENTF_RIGHTUP,
                    MouseButton::Middle | MouseButton::Other(_) => MOUSEEVENTF_MIDDLEUP,
                };
                send_mouse_event(flags, 0, 0, 0)?;
            }

            InputEvent::Scroll { dx, dy } => {
                if *dy != 0 {
                    send_mouse_event(MOUSEEVENTF_WHEEL, 0, 0, *dy as i32 * 120)?;
                }
                if *dx != 0 {
                    send_mouse_event(MOUSEEVENTF_HWHEEL, 0, 0, *dx as i32 * 120)?;
                }
            }

            InputEvent::KeyDown { keycode, .. } => {
                send_key_event(mac_keycode_to_win_vk(*keycode), false)?;
            }

            InputEvent::KeyUp { keycode, .. } => {
                send_key_event(mac_keycode_to_win_vk(*keycode), true)?;
            }

            InputEvent::CursorEnter { x, y } => {
                let (w, h) = self.get_screen_size()?;
                let px = (*x * w as f32) as i32;
                let py = (*y * h as f32) as i32;
                self.warp_cursor(px, py)?;
                // Seed virtual tracker from the warped position.
                self.vx.store(f32::to_bits(px as f32), Ordering::SeqCst);
                self.vy.store(f32::to_bits(py as f32), Ordering::SeqCst);
            }

            _ => {}
        }

        Ok(())
    }

    fn hide_cursor(&self) -> anyhow::Result<()> {
        unsafe { ShowCursor(false); }
        Ok(())
    }

    fn show_cursor(&self) -> anyhow::Result<()> {
        unsafe { ShowCursor(true); }
        Ok(())
    }

    fn warp_cursor(&self, x: i32, y: i32) -> anyhow::Result<()> {
        unsafe { SetCursorPos(x, y)?; }
        Ok(())
    }

    fn get_screen_size(&self) -> anyhow::Result<(u32, u32)> {
        unsafe {
            Ok((
                GetSystemMetrics(SM_CXSCREEN) as u32,
                GetSystemMetrics(SM_CYSCREEN) as u32,
            ))
        }
    }

    fn get_cursor_pos(&self) -> anyhow::Result<(i32, i32)> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point)?; }
        Ok((point.x, point.y))
    }
}

// ── Hook callbacks ──────────────────────────────────────

unsafe extern "system" fn mouse_hook_callback(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        if let Some(ptr) = HOOK_SENDER {
            let tx = &*ptr;
            let mi = &*(l_param.0 as *const MSLLHOOKSTRUCT);
            let event_type = w_param.0 as u32;

            let input_event = match event_type {
                0x0200 => { // WM_MOUSEMOVE
                    let (w, h) = screen_size_cached();
                    Some(InputEvent::MouseMoveAbsolute {
                        x: mi.pt.x as f32 / w as f32,
                        y: mi.pt.y as f32 / h as f32,
                    })
                }
                0x0201 => Some(InputEvent::MouseDown { button: MouseButton::Left }),
                0x0202 => Some(InputEvent::MouseUp { button: MouseButton::Left }),
                0x0204 => Some(InputEvent::MouseDown { button: MouseButton::Right }),
                0x0205 => Some(InputEvent::MouseUp { button: MouseButton::Right }),
                0x0207 => Some(InputEvent::MouseDown { button: MouseButton::Middle }),
                0x0208 => Some(InputEvent::MouseUp { button: MouseButton::Middle }),
                0x020A => { // WM_MOUSEWHEEL
                    let delta = (mi.mouse_data as i32) >> 16;
                    Some(InputEvent::Scroll { dx: 0, dy: (delta / 120) as i16 })
                }
                0x020E => { // WM_MOUSEHWHEEL
                    let delta = (mi.mouse_data as i32) >> 16;
                    Some(InputEvent::Scroll { dx: (delta / 120) as i16, dy: 0 })
                }
                _ => None,
            };

            if let Some(evt) = input_event {
                let _ = tx.try_send(evt);
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

unsafe extern "system" fn keyboard_hook_callback(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        if let Some(ptr) = HOOK_SENDER {
            let tx = &*ptr;
            let kb = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vk_code as u16;
            let event_type = w_param.0 as u32;

            let mut modifiers = Modifiers::NONE;
            if (kb.flags & 0x10) != 0 { modifiers |= Modifiers::ALT; }
            if (kb.flags & 0x20) != 0 { modifiers |= Modifiers::META; }

            let input_event = match event_type {
                0x0100 | 0x0104 => Some(InputEvent::KeyDown { keycode: vk, modifiers }),
                0x0101 | 0x0105 => Some(InputEvent::KeyUp { keycode: vk, modifiers }),
                _ => None,
            };

            if let Some(evt) = input_event {
                let _ = tx.try_send(evt);
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

// ── Win32 helper types (our own definitions for hook structs) ──

#[repr(C)]
struct MSLLHOOKSTRUCT {
    pt: POINT,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
struct KBDLLHOOKSTRUCT {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

// ── Injection helpers ───────────────────────────────────

fn send_mouse_event(
    flags: MOUSE_EVENT_FLAGS,
    dx: i32,
    dy: i32,
    mouse_data: i32,
) -> anyhow::Result<()> {
    let input = INPUT {
        r#type: INPUT_TYPE(0), // INPUT_MOUSE
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: Default::default(),
            },
        },
    };

    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
    Ok(())
}

fn send_key_event(vk: u16, key_up: bool) -> anyhow::Result<()> {
    let flags = if key_up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS::default() };

    let input = INPUT {
        r#type: INPUT_TYPE(1), // INPUT_KEYBOARD
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: Default::default(),
            },
        },
    };

    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
    Ok(())
}

fn screen_size_cached() -> (u32, u32) {
    static CACHED: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) as u32,
            GetSystemMetrics(SM_CYSCREEN) as u32,
        )
    })
}

// ── macOS → Windows keycode translation ──────────────────
//
/// Translates a macOS virtual keycode (hardware-position-based) into a
/// Windows virtual-key code.  Without this, macOS keycodes leak onto
/// SendInput as if they were Windows VK values — producing garbled input.
///
/// Coverage: ANSI letters, numbers, symbols, modifiers, arrows, F-keys,
/// numpad, and common special keys.  Unmapped keys fall through to the
/// raw keycode (correct for a few coincidental overlaps).
fn mac_keycode_to_win_vk(kc: u16) -> u16 {
    match kc {
        // ── ANSI letters (QWERTY positions, independent of macOS layout) ──
        0x00 => 0x41, // A
        0x01 => 0x53, // S
        0x02 => 0x44, // D
        0x03 => 0x46, // F
        0x04 => 0x48, // H
        0x05 => 0x47, // G
        0x06 => 0x5A, // Z
        0x07 => 0x58, // X
        0x08 => 0x43, // C
        0x09 => 0x56, // V
        0x0B => 0x42, // B
        0x0C => 0x51, // Q
        0x0D => 0x57, // W
        0x0E => 0x45, // E
        0x0F => 0x52, // R
        0x10 => 0x59, // Y
        0x11 => 0x54, // T
        // ── Numbers ──
        0x12 => 0x31, // 1
        0x13 => 0x32, // 2
        0x14 => 0x33, // 3
        0x15 => 0x34, // 4
        0x16 => 0x36, // 6
        0x17 => 0x35, // 5
        0x18 => 0xBB, // =
        0x19 => 0x39, // 9
        0x1A => 0x37, // 7
        0x1B => 0xBD, // -
        0x1C => 0x38, // 8
        0x1D => 0x30, // 0
        0x1E => 0xDD, // ]
        0x1F => 0x4F, // O
        0x20 => 0x55, // U
        0x21 => 0xDB, // [
        0x22 => 0x49, // I
        0x23 => 0x50, // P
        0x24 => 0x0D, // Return (was: VK_HOME 0x24)
        0x25 => 0x4C, // L
        0x26 => 0x4A, // J
        0x27 => 0xDE, // '
        0x28 => 0x4B, // K
        0x29 => 0xBA, // ;
        0x2A => 0xDC, // \
        0x2B => 0xBC, // ,
        0x2C => 0xBF, // /
        0x2D => 0x4E, // N
        0x2E => 0x4D, // M
        0x2F => 0xBE, // .
        0x30 => 0x09, // Tab
        0x31 => 0x20, // Space (was: VK_1 0x31)
        0x32 => 0xC0, // ` (backtick)
        0x33 => 0x08, // Backspace
        0x34 => 0x0D, // Enter (powerbook)
        0x35 => 0x1B, // Escape
        // ── Modifiers ──   (Mac Cmd → Win Ctrl, Mac Ctrl → Win Win-key)
        0x36 => 0xA3, // Right Cmd  → VK_RCONTROL
        0x37 => 0xA2, // Left Cmd   → VK_LCONTROL
        0x38 => 0xA0, // Left Shift
        0x39 => 0x14, // Caps Lock
        0x3A => 0xA4, // Left Option  → VK_LMENU
        0x3B => 0x5C, // Left Control → VK_LWIN
        0x3C => 0xA1, // Right Shift
        0x3D => 0xA5, // Right Option → VK_RMENU
        0x3E => 0x5D, // Right Control → VK_RWIN
        0x3F => 0xAD, // fn (global) → VK_VOLUME_MUTE (soft fallback)
        // ── Arrows ──
        0x7B => 0x25, // Left
        0x7C => 0x27, // Right
        0x7D => 0x28, // Down
        0x7E => 0x26, // Up
        // ── F1–F12 ──
        0x7A => 0x70, // F1
        0x78 => 0x71, // F2
        0x63 => 0x72, // F3
        0x76 => 0x73, // F4
        0x60 => 0x74, // F5
        0x61 => 0x75, // F6
        0x62 => 0x76, // F7
        0x64 => 0x77, // F8
        0x65 => 0x78, // F9
        0x6D => 0x79, // F10
        0x67 => 0x7A, // F11
        0x6F => 0x7B, // F12
        // ── Numpad ──
        0x41 => 0x6E, // NumPad .      → VK_DECIMAL
        0x43 => 0x6A, // NumPad *      → VK_MULTIPLY
        0x45 => 0x6D, // NumPad +      → VK_ADD
        0x4B => 0x6F, // NumPad /      → VK_DIVIDE
        0x4C => 0x0D, // NumPad Enter  → VK_RETURN
        0x4E => 0x6C, // NumPad -      → VK_SUBTRACT
        0x51 => 0x6B, // NumPad = → VK_ADD fallback
        0x52 => 0x60, // NumPad 0
        0x53 => 0x61, // NumPad 1
        0x54 => 0x62, // NumPad 2
        0x55 => 0x63, // NumPad 3
        0x56 => 0x64, // NumPad 4
        0x57 => 0x65, // NumPad 5
        0x58 => 0x66, // NumPad 6
        0x59 => 0x67, // NumPad 7
        0x5B => 0x68, // NumPad 8
        0x5C => 0x69, // NumPad 9
        // ── Misc ──
        0x48 => 0x2D, // Vol-Up    → VK_VOLUME_UP
        0x49 => 0x2E, // Vol-Down  → VK_VOLUME_DOWN
        0x4A => 0xAD, // Mute      → VK_VOLUME_MUTE
        0x47 => 0x91, // NumLock   → VK_NUMLOCK
        0x71 => 0x24, // Home (help key on some kb)
        0x72 => 0x2D, // Insert → VK_INSERT (also help)
        0x73 => 0x2E, // Delete → VK_DELETE
        0x74 => 0x21, // Page Up
        0x75 => 0x22, // Page Down
        0x77 => 0x23, // End
        0x79 => 0x03, // Break → VK_CANCEL (fn + esc on some mac)
        // Everything else passes through unchanged.
        _ => kc,
    }
}
