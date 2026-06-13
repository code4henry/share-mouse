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
    /// Last host cursor position (normalized) — used to compute deltas while
    /// the cursor is logically on a remote screen.
    remote_origin: Option<(f32, f32)>,
}

impl EngineState {
    fn new() -> Self {
        Self {
            role: Role::None,
            cursor_owner: CursorOwner::Local,
            remote_origin: None,
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

/// Pixels from a screen edge that trigger a transition.
const EDGE_ZONE: f32 = 0.004; // normalized
/// When the host cursor (hidden) gets this close to any edge while controlling
/// a remote screen, we warp it back to center ("infinite mouse").
const WARP_MARGIN: f32 = 0.15;

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
        let mut layout = self.layout.lock().await;
        // Don't add twice.
        if layout.screens.iter().any(|s| s.peer_id.as_deref() == Some(peer_id.to_string().as_str())) {
            return;
        }
        // Find the local screen to position relative to it.
        if let Some(local) = layout.screens.iter().find(|s| s.peer_id.is_none()).cloned() {
            let remote = ScreenInfo {
                id: format!("remote-{}", peer_id),
                name: format!("Remote {}", &peer_id.to_string()[..8]),
                rect: ScreenRect {
                    x: local.rect.right(),
                    y: local.rect.y,
                    width: local.rect.width,
                    height: local.rect.height,
                },
                peer_id: Some(peer_id.to_string()),
                width: local.rect.width,
                height: local.rect.height,
                dpi: local.dpi,
            };
            layout.screens.push(remote);
            log::info!("Added remote screen for peer {} (to the right)", peer_id);
        }
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
            s.remote_origin = None;
        }
        let mut capture_rx = {
            let p = self.platform.lock().await;
            p.start_capture()?
        };
        {
            let p = self.platform.lock().await;
            p.show_cursor().ok();
        }
        log::info!("Started as Host — capturing input");

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
            let owner = self.state.lock().await.cursor_owner;
            let is_remote = matches!(owner, CursorOwner::Remote(_));
            if is_remote {
                // Check cursor position against the return edge (left edge).
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
                    if nx <= EDGE_ZONE {
                        // Cursor returned to the host-facing edge — give it back.
                        let peer = if let CursorOwner::Remote(pid) = owner { Some(pid) } else { None };
                        if let Some(pid) = peer {
                            log::info!("Client: cursor at return edge, sending CursorLeave");
                            let _ = self.network.send_to(&pid, InputEvent::CursorLeave).await;
                            self.state.lock().await.cursor_owner = CursorOwner::Local;
                            let p = self.platform.lock().await;
                            p.hide_cursor().ok();
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
            s.remote_origin = None;
        }
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
                        if let Some(peer_id_str) = &neighbor.peer_id {
                            if let Ok(peer_id) = uuid::Uuid::parse_str(peer_id_str) {
                                // Compute entry point on the neighbor (normalized).
                                let (nx, ny) = layout.map_cursor_to_neighbor(
                                    &self.local_screen_id,
                                    edge,
                                    px,
                                    py,
                                    neighbor,
                                );
                                drop(layout);

                                // Tell the peer to take the cursor.
                                let _ = self.network.send_to(&peer_id, InputEvent::CursorEnter { x: nx, y: ny }).await;

                                // Hide our cursor and warp to center (so we can capture deltas freely).
                                {
                                    let p = self.platform.lock().await;
                                    p.hide_cursor().ok();
                                    p.warp_cursor((w / 2) as i32, (h / 2) as i32).ok();
                                }
                                {
                                    let mut s = self.state.lock().await;
                                    s.cursor_owner = CursorOwner::Remote(peer_id);
                                    s.remote_origin = Some((0.5, 0.5)); // we warped to center
                                }
                                log::info!("Cursor handed off to peer {}", peer_id);
                                return;
                            }
                        }
                    }
                }
                // Otherwise: cursor on local screen, nothing to forward.
            }
            CursorOwner::Remote(peer_id) => {
                // Cursor logically on the remote screen. Forward input there.
                match event {
                    InputEvent::MouseMoveAbsolute { x, y } => {
                        // Compute delta from last origin, forward as relative move.
                        let (dx_n, dy_n, new_origin, need_warp) = {
                            let s = self.state.lock().await;
                            let origin = s.remote_origin.unwrap_or((0.5, 0.5));
                            let dx = x - origin.0;
                            let dy = y - origin.1;
                            let near_edge = *x < WARP_MARGIN || *x > 1.0 - WARP_MARGIN
                                || *y < WARP_MARGIN || *y > 1.0 - WARP_MARGIN;
                            (dx, dy, (*x, *y), near_edge)
                        };

                        // Scale normalized delta to pixels.
                        let (w, h) = {
                            let p = self.platform.lock().await;
                            p.get_screen_size().unwrap_or((1920, 1080))
                        };
                        let dx_px = (dx_n * w as f32) as i16;
                        let dy_px = (dy_n * h as f32) as i16;
                        if dx_px != 0 || dy_px != 0 {
                            let _ = self.network.send_to(&peer_id, InputEvent::MouseMove { dx: dx_px, dy: dy_px }).await;
                        }

                        // Infinite-mouse: if near a local edge, warp back to center.
                        if need_warp {
                            let p = self.platform.lock().await;
                            p.warp_cursor((w / 2) as i32, (h / 2) as i32).ok();
                            drop(p);
                            self.state.lock().await.remote_origin = Some((0.5, 0.5));
                        } else {
                            self.state.lock().await.remote_origin = Some(new_origin);
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
                }
                let p = self.platform.lock().await;
                p.show_cursor().ok();
                p.inject_event(&InputEvent::MouseMoveAbsolute { x: *x, y: *y }).ok();
                log::info!("Cursor entered local screen from peer");
            }
            InputEvent::CursorLeave => {
                // Client returned the cursor to us (host). Reclaim it:
                // show our cursor and warp it to the right edge where it left.
                {
                    let mut s = self.state.lock().await;
                    s.cursor_owner = CursorOwner::Local;
                    s.remote_origin = None;
                }
                let p = self.platform.lock().await;
                p.show_cursor().ok();
                if let Ok((w, h)) = p.get_screen_size() {
                    p.warp_cursor((w - 10) as i32, (h / 2) as i32).ok();
                }
                log::info!("Cursor returned to local screen");
            }
            _ => {
                // Regular input event — inject if a remote owns the cursor.
                let owner = self.state.lock().await.cursor_owner;
                if let CursorOwner::Remote(_) = owner {
                    let p = self.platform.lock().await;
                    p.inject_event(event).ok();
                }
            }
        }
    }
}
