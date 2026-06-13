pub mod protocol;
pub mod network;
pub mod screen;

use std::sync::Arc;

use tokio::sync::Mutex;

use protocol::InputEvent;
use network::{NetworkHub, NetworkMessage, PeerId};
use screen::{ScreenLayout, ScreenInfo, ScreenRect};
use crate::platform::PlatformInput;

/// Whether this instance is the host (sending input) or client (receiving input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    None,
    /// This machine has the physical mouse/keyboard — captures input and sends to peers.
    Host,
    /// This machine receives input from a remote host.
    Client,
}

/// State machine for cursor ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorOwner {
    Local,
    Remote(PeerId),
}

/// Mutable engine state.
struct EngineState {
    role: Role,
    cursor_owner: CursorOwner,
    /// Client-side: whether the cursor has moved away from the entry edge.
    /// The return-edge check only fires once armed, preventing immediate
    /// bounce-back on entry.
    cursor_armed: bool,
}

impl EngineState {
    fn new() -> Self {
        Self {
            role: Role::None,
            cursor_owner: CursorOwner::Local,
            cursor_armed: false,
        }
    }
}

/// The main ShareMouse engine.
pub struct Engine {
    platform: Arc<Mutex<Box<dyn PlatformInput>>>,
    network: Arc<NetworkHub>,
    layout: Arc<Mutex<ScreenLayout>>,
    state: Arc<Mutex<EngineState>>,
    local_screen_id: String,
}

/// Normalized distance from the client's host-facing edge that returns the cursor.
const EDGE_ZONE: f32 = 0.01;

impl Engine {
    pub fn new(
        platform: Box<dyn PlatformInput>,
        network: Arc<NetworkHub>,
        local_screen_id: String,
    ) -> Self {
        Self {
            platform: Arc::new(Mutex::new(platform)),
            network,
            layout: Arc::new(Mutex::new(ScreenLayout::new())),
            state: Arc::new(Mutex::new(EngineState::new())),
            local_screen_id,
        }
    }

    /// Persistent network handler: consumes incoming events and injects them,
    /// and updates the layout when peers connect/disconnect.
    pub fn spawn_network_handler(self: &Arc<Self>) {
        let mut rx = self.network.subscribe();
        let engine = self.clone();
        tauri::async_runtime::spawn(async move {
            log::info!("Network handler task started");
            loop {
                match rx.recv().await {
                    Ok(NetworkMessage::Event { from, event }) => {
                        engine.handle_network_event(from, &event).await;
                    }
                    Ok(NetworkMessage::PeerConnected { id, addr }) => {
                        log::info!("Peer connected: {} ({})", id, addr);
                        // If we're the host, add the peer's screen to our layout.
                        let role = engine.state.lock().await.role;
                        if role == Role::Host {
                            engine.add_remote_screen(id).await;
                        }
                    }
                    Ok(NetworkMessage::PeerDisconnected { id }) => {
                        log::info!("Peer disconnected: {}", id);
                        engine.remove_remote_screen(id).await;
                    }
                    Ok(NetworkMessage::Listening { addr }) => {
                        log::info!("Listening on {}", addr);
                    }
                    Ok(NetworkMessage::Error { message }) => {
                        log::error!("Network error: {}", message);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("Network channel lagged by {} messages", n);
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// Add a remote screen to the right of the local screen for a given peer.
    async fn add_remote_screen(&self, peer_id: PeerId) {
        self.ensure_local_screen().await;
        let pid_str = peer_id.to_string();
        let mut layout = self.layout.lock().await;
        // Don't add twice.
        if layout.screens.iter().any(|s| s.peer_id.as_deref() == Some(pid_str.as_str())) {
            return;
        }
        // Find the local screen to position relative to it.
        if let Some(local) = layout.screens.iter().find(|s| s.peer_id.is_none()).cloned() {
            let remote = ScreenInfo {
                id: format!("remote-{}", peer_id),
                name: format!("Remote {}", &pid_str[..8]),
                rect: ScreenRect {
                    x: local.rect.right(),
                    y: local.rect.y,
                    width: local.rect.width,
                    height: local.rect.height,
                },
                peer_id: Some(pid_str),
                width: local.rect.width,
                height: local.rect.height,
                dpi: local.dpi,
            };
            layout.screens.push(remote);
            log::info!("Added remote screen for peer {} (to the right)", peer_id);
        }
    }

    /// Ensure the local screen exists in the layout (race-free, idempotent).
    async fn ensure_local_screen(&self) {
        {
            let layout = self.layout.lock().await;
            if layout.screens.iter().any(|s| s.id == self.local_screen_id) {
                return;
            }
        }
        let size = {
            let p = self.platform.lock().await;
            p.get_screen_size()
        };
        if let Ok((w, h)) = size {
            let mut layout = self.layout.lock().await;
            if !layout.screens.iter().any(|s| s.id == self.local_screen_id) {
                layout.screens.push(ScreenInfo {
                    id: self.local_screen_id.clone(),
                    name: "Local".to_string(),
                    rect: ScreenRect { x: 0, y: 0, width: w, height: h },
                    peer_id: None,
                    width: w,
                    height: h,
                    dpi: 72,
                });
                log::info!("Lazy-seeded local screen {}x{} (id={})", w, h, self.local_screen_id);
            }
        }
    }

    /// Whether the OS permission needed for capture/injection is granted.
    pub async fn check_permission_simple(&self) -> bool {
        let p = self.platform.lock().await;
        p.check_permission()
    }

    /// Remove a peer's screen from the layout.
    async fn remove_remote_screen(&self, peer_id: PeerId) {
        let mut layout = self.layout.lock().await;
        let pid = peer_id.to_string();
        layout.screens.retain(|s| s.peer_id.as_deref() != Some(pid.as_str()));
    }

    /// Switch to Host mode.
    pub async fn start_host(self: Arc<Self>) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::Host;
            s.cursor_owner = CursorOwner::Local;
            s.cursor_armed = false;
        }
        let mut capture_rx = {
            let p = self.platform.lock().await;
            p.set_is_remote(false);
            p.start_capture()?
        };
        {
            let p = self.platform.lock().await;
            p.show_cursor().ok();
        }
        log::info!("Started as Host — CGEventTap capturing input");

        let engine = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = capture_rx.recv().await {
                engine.handle_captured_event(&event).await;
            }
            log::info!("Capture task ended");
        });
        Ok(())
    }

    /// Switch to Client mode.
    pub async fn start_client(self: Arc<Self>) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::Client;
            s.cursor_owner = CursorOwner::Local;
        }
        let p = self.platform.lock().await;
        p.stop_capture().ok();
        p.show_cursor().ok();

        // Start the client cursor monitor (detects when to return the cursor).
        let engine = self.clone();
        tauri::async_runtime::spawn(async move {
            engine.client_monitor_loop().await;
        });
        log::info!("Started as Client — waiting for remote input");
        Ok(())
    }

    /// Client-side monitor: when the cursor is being controlled by the host
    /// (owner == Remote), poll the local cursor and detect when it returns to
    /// the edge facing the host — then give the cursor back.
    async fn client_monitor_loop(self: Arc<Self>) {
        loop {
            let (is_remote, armed) = {
                let s = self.state.lock().await;
                (matches!(s.cursor_owner, CursorOwner::Remote(_)), s.cursor_armed)
            };
            if is_remote {
                let pos = {
                    let p = self.platform.lock().await;
                    p.get_cursor_pos().ok()
                };
                if let Some((x, _y)) = pos {
                    let (w, _h) = {
                        let p = self.platform.lock().await;
                        p.get_screen_size().unwrap_or((1920, 1080))
                    };
                    let nx = if w > 0 { x as f32 / w as f32 } else { 0.0 };

                    if !armed {
                        // Cursor just entered at the edge; wait for it to move into
                        // the screen before arming the return check.
                        let (_, new_armed) = hysteresis_decision(armed, nx);
                        if new_armed {
                            self.state.lock().await.cursor_armed = true;
                            log::debug!("client_monitor: armed (nx={:.3})", nx);
                        }
                    } else {
                        let (should_return, _) = hysteresis_decision(armed, nx);
                        if should_return {
                            // Armed and cursor returned to the host-facing edge.
                            let peer = if let CursorOwner::Remote(pid) = self.state.lock().await.cursor_owner {
                                Some(pid)
                            } else {
                                None
                            };
                            if let Some(pid) = peer {
                                log::info!("client_monitor: nx={:.4} armed, sending CursorLeave to {}", nx, pid);
                                let _ = self.network.send_to(&pid, InputEvent::CursorLeave).await;
                                let mut s = self.state.lock().await;
                                s.cursor_owner = CursorOwner::Local;
                                s.cursor_armed = false;
                                let p = self.platform.lock().await;
                                p.hide_cursor().ok();
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(8)).await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }

    /// Stop the engine.
    pub async fn stop(&self) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::None;
            s.cursor_owner = CursorOwner::Local;
            s.cursor_armed = false;
        }
        let p = self.platform.lock().await;
        p.set_is_remote(false);
        let p = self.platform.lock().await;
        p.stop_capture().ok();
        p.show_cursor().ok();
        log::info!("Engine stopped");
        Ok(())
    }

    pub async fn set_layout(&self, layout: ScreenLayout) {
        *self.layout.lock().await = layout;
    }

    pub async fn get_layout(&self) -> ScreenLayout {
        self.layout.lock().await.clone()
    }

    pub async fn get_role(&self) -> Role {
        self.state.lock().await.role
    }

    pub async fn get_cursor_owner(&self) -> CursorOwner {
        self.state.lock().await.cursor_owner
    }

    pub async fn with_platform<R>(&self, f: impl FnOnce(&dyn PlatformInput) -> R) -> R {
        let p = self.platform.lock().await;
        f(p.as_ref())
    }

    pub fn local_id_for_setup(&self) -> String {
        self.local_screen_id.clone()
    }

    pub async fn platform_for_setup(&self) -> Arc<Mutex<Box<dyn PlatformInput>>> {
        self.platform.clone()
    }

    /// Process a captured local input event (host mode).
    async fn handle_captured_event(&self, event: &InputEvent) {
        let role = self.state.lock().await.role;
        if role != Role::Host {
            return;
        }
        self.ensure_local_screen().await;

        let cursor_owner = self.state.lock().await.cursor_owner;

        match cursor_owner {
            CursorOwner::Local => {
                // Cursor on our screen — check for edge transitions.
                if let InputEvent::MouseMoveAbsolute { x, y } = event {
                    // Convert normalized → actual pixels to match the layout's space.
                    let (w, h) = {
                        let p = self.platform.lock().await;
                        p.get_screen_size().unwrap_or((1920, 1080))
                    };
                    let px = (*x * w as f32) as i32;
                    let py = (*y * h as f32) as i32;

                    let layout = self.layout.lock().await;
                    if let Some((edge, neighbor)) =
                        layout.detect_edge(&self.local_screen_id, px, py)
                    {
                        let neighbor_peer = neighbor.peer_id.clone();
                        let (nx, ny) = layout.map_cursor_to_neighbor(
                            &self.local_screen_id,
                            edge,
                            px,
                            py,
                            neighbor,
                        );
                        drop(layout);

                        log::info!(
                            "EDGE HIT: edge={:?} neighbor_peer={:?} entry=({:.3},{:.3})",
                            edge, neighbor_peer, nx, ny
                        );

                        if let Some(peer_id_str) = neighbor_peer {
                            if let Ok(peer_id) = uuid::Uuid::parse_str(&peer_id_str) {
                                // Tell the peer to take the cursor.
                                let _ = self.network.send_to(&peer_id, InputEvent::CursorEnter { x: nx, y: ny }).await;
                                log::info!("-> CursorEnter to peer {} at ({:.3},{:.3})", peer_id, nx, ny);

                                // Tell the tap to drop events from the host OS.
                                {
                                    let p = self.platform.lock().await;
                                    p.set_is_remote(true);
                                    p.hide_cursor().ok();
                                }
                                {
                                    let mut s = self.state.lock().await;
                                    s.cursor_owner = CursorOwner::Remote(peer_id);
                                }
                                log::info!("host: is_remote=true, owner=Remote({})", peer_id);
                                return;
                            }
                        }
                    }
                }
                // Otherwise: cursor on local screen, nothing to forward.
            }
            CursorOwner::Remote(peer_id) => {
                // Cursor logically on the remote screen. The tap supplies raw
                // mouse deltas, clicks, keys, and scroll. Forward all of them.
                match event {
                    InputEvent::MouseMove { dx, dy } => {
                        // Raw HID pixel deltas from the tap — normalize to host
                        // screen size so the client can scale by its own.
                        let (w, h) = {
                            let p = self.platform.lock().await;
                            p.get_screen_size().unwrap_or((1920, 1080))
                        };
                        let dx_n = *dx as f32 / (w as f32).max(1.0);
                        let dy_n = *dy as f32 / (h as f32).max(1.0);
                        if dx_n.abs() > 0.0001 || dy_n.abs() > 0.0001 {
                            let _ = self.network.send_to(
                                &peer_id,
                                InputEvent::MouseMoveNormalized { dx: dx_n, dy: dy_n },
                            ).await;
                        }
                    }
                    // Clicks, keys, scroll — forward directly.
                    other => {
                        let _ = self.network.send_to(&peer_id, other.clone()).await;
                    }
                }
            }
        }
    }

    /// Process a received network event.
    async fn handle_network_event(&self, from: PeerId, event: &InputEvent) {
        match event {
            InputEvent::CursorEnter { x, y } => {
                // Remote host hands the cursor to us.
                {
                    let mut s = self.state.lock().await;
                    s.cursor_owner = CursorOwner::Remote(from);
                    s.cursor_armed = false; // disarmed on entry (avoid bounce-back)
                }
                let p = self.platform.lock().await;
                p.show_cursor().ok();
                p.inject_event(&InputEvent::MouseMoveAbsolute { x: *x, y: *y }).ok();
                log::info!("<- CursorEnter from {} at ({:.3},{:.3})", from, x, y);
            }
            InputEvent::CursorLeave => {
                // Client returned the cursor to us (host). Reclaim it — tell the
                // tap to let events pass through again, show our cursor.
                {
                    let mut s = self.state.lock().await;
                    s.cursor_owner = CursorOwner::Local;
                    s.cursor_armed = false;
                }
                let p = self.platform.lock().await;
                p.set_is_remote(false);
                p.show_cursor().ok();
                if let Ok((w, h)) = p.get_screen_size() {
                    p.warp_cursor((w as f32 * 0.75) as i32, (h / 2) as i32).ok();
                }
                log::info!("<- CursorLeave from {}, owner=Local, is_remote=false", from);
            }
            other => {
                // Regular input event — inject if a remote owns the cursor.
                let owner = self.state.lock().await.cursor_owner;
                if let CursorOwner::Remote(_) = owner {
                    log::debug!("<- injected {:?} from {}", other, from);
                    let p = self.platform.lock().await;
                    p.inject_event(event).ok();
                }
            }
        }
    }
}

/// Pure decision for the client return-edge hysteresis.
/// Returns (should_return_cursor, new_armed_state).
/// - Not armed: arm once the cursor moves past ARM_THRESHOLD into the screen.
/// - Armed: return the cursor when it comes back to the edge (<= EDGE_ZONE).
fn hysteresis_decision(armed: bool, nx: f32) -> (bool, bool) {
    const ARM_THRESHOLD: f32 = 0.06;
    if !armed {
        if nx > ARM_THRESHOLD {
            (false, true) // arm, don't return
        } else {
            (false, false) // stay disarmed
        }
    } else if nx <= EDGE_ZONE {
        (true, false) // return + disarm
    } else {
        (false, true) // stay armed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_does_not_return_on_entry() {
        // Cursor enters at the left edge (nx≈0); must NOT bounce back.
        let (ret, armed) = hysteresis_decision(false, 0.0);
        assert!(!ret, "must not return immediately on entry");
        assert!(!armed);
        let (ret, _) = hysteresis_decision(false, 0.005);
        assert!(!ret, "must not return while still at the edge");
    }

    #[test]
    fn hysteresis_arms_after_moving_in() {
        let (_, armed) = hysteresis_decision(false, 0.1); // moved into screen
        assert!(armed);
    }

    #[test]
    fn hysteresis_returns_only_when_armed_and_back_at_edge() {
        // Armed, cursor moves back to the edge → return.
        let (ret, armed) = hysteresis_decision(true, 0.0);
        assert!(ret, "should return when armed and back at edge");
        assert!(!armed);
        // Armed but mid-screen → no return.
        let (ret, _) = hysteresis_decision(true, 0.5);
        assert!(!ret);
    }
}
