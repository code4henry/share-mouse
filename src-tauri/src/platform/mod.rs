/// Platform abstraction trait for input capture and injection.
///
/// Each OS implements this trait to provide:
/// - Capturing mouse/keyboard events (on the host)
/// - Injecting mouse/keyboard events (on the client)
/// - Hiding/showing the cursor
/// - Getting screen dimensions

use crate::core::protocol::InputEvent;

/// Platform-specific input operations.
pub trait PlatformInput: Send + Sync {
    /// Start capturing input events. Returns a receiver that yields captured events.
    /// While capturing, the local cursor is hidden and all input is intercepted.
    fn start_capture(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<InputEvent>>;

    /// Stop capturing input events and restore normal input.
    fn stop_capture(&self) -> anyhow::Result<()>;

    /// Inject a single input event into the OS.
    fn inject_event(&self, event: &InputEvent) -> anyhow::Result<()>;

    /// Hide the local cursor.
    fn hide_cursor(&self) -> anyhow::Result<()>;

    /// Show the local cursor.
    fn show_cursor(&self) -> anyhow::Result<()>;

    /// Move the cursor to an absolute position.
    fn warp_cursor(&self, x: i32, y: i32) -> anyhow::Result<()>;

    /// Get the primary screen dimensions (width, height).
    fn get_screen_size(&self) -> anyhow::Result<(u32, u32)>;

    /// Get the current cursor position.
    fn get_cursor_pos(&self) -> anyhow::Result<(i32, i32)>;
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

/// Create the platform-specific input implementation.
#[cfg(target_os = "macos")]
pub fn create_platform_input() -> Box<dyn PlatformInput> {
    Box::new(macos::MacOSInput::new())
}

#[cfg(target_os = "windows")]
pub fn create_platform_input() -> Box<dyn PlatformInput> {
    Box::new(windows::WindowsInput::new())
}
