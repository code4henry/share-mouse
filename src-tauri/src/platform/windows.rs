/// Windows input capture and injection using Win32 API.
///
/// Capture: SetWindowsHookEx(WH_MOUSE_LL / WH_KEYBOARD_LL) in a dedicated thread.
/// Injection: SendInput() for mouse/keyboard events.
/// Cursor: SetCursorPos(), ShowCursor().
/// Screen: GetSystemMetrics(), GetCursorPos().

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_TYPE, KEYBDINPUT, KEYBD_EVENT_FLAGS, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_HWHEEL,
};
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
}

impl WindowsInput {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
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
                let (cx, cy) = self.get_cursor_pos()?;
                self.warp_cursor(cx + *dx as i32, cy + *dy as i32)?;
            }

            InputEvent::MouseMoveAbsolute { x, y } => {
                let (w, h) = self.get_screen_size()?;
                self.warp_cursor((*x * w as f32) as i32, (*y * h as f32) as i32)?;
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
                send_key_event(*keycode as u16, false)?;
            }

            InputEvent::KeyUp { keycode, .. } => {
                send_key_event(*keycode as u16, true)?;
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
