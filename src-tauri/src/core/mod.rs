pub mod protocol;
pub mod network;
pub mod screen;

use std::sync::Arc;

use tokio::sync::Mutex;

use protocol::InputEvent;
use network::{NetworkHub, NetworkMessage, PeerId};
use screen::ScreenLayout;
use crate::platform::PlatformInput;

/// Whether this instance is the host (sending input) or client (receiving input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// No active role — idle.
    None,
    /// This machine has the physical mouse/keyboard — captures input and sends to peers.
    Host,
    /// This machine receives input from a remote host.
    Client,
}

/// State machine for cursor ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorOwner {
    /// Cursor is on this machine's screen.
    Local,
    /// Cursor is on a remote machine's screen (identified by peer ID).
    Remote(PeerId),
}

/// Mutable engine state.
struct EngineState {
    role: Role,
    cursor_owner: CursorOwner,
}

/// The main ShareMouse engine that ties everything together.
pub struct Engine {
    platform: Arc<Mutex<Box<dyn PlatformInput>>>,
    network: Arc<NetworkHub>,
    layout: Arc<Mutex<ScreenLayout>>,
    state: Arc<Mutex<EngineState>>,
    local_screen_id: String,
}

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
            state: Arc::new(Mutex::new(EngineState {
                role: Role::None,
                cursor_owner: CursorOwner::Local,
            })),
            local_screen_id,
        }
    }

    /// Spawn the persistent network handler task. Call once at app launch.
    /// It consumes incoming network messages and injects received input events.
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
                    }
                    Ok(NetworkMessage::PeerDisconnected { id }) => {
                        log::info!("Peer disconnected: {}", id);
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
                    Err(_) => {
                        log::info!("Network handler channel closed");
                        break;
                    }
                }
            }
        });
    }

    /// Switch to Host mode: set role and start capturing input.
    pub async fn start_host(self: Arc<Self>) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::Host;
            s.cursor_owner = CursorOwner::Local;
        }

        // Start capturing input from the OS
        let mut capture_rx = {
            let p = self.platform.lock().await;
            p.start_capture()?
        };

        // Show our cursor (we're the host, cursor starts here)
        {
            let p = self.platform.lock().await;
            p.show_cursor().ok();
        }

        log::info!("Started as Host — capturing input");

        // Spawn the capture-forwarding task
        let engine = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = capture_rx.recv().await {
                engine.handle_captured_event(&event).await;
            }
            log::info!("Capture task ended");
        });

        Ok(())
    }

    /// Switch to Client mode: stop capturing, just receive and inject.
    pub async fn start_client(self: Arc<Self>) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::Client;
        }
        let p = self.platform.lock().await;
        p.stop_capture().ok();
        p.show_cursor().ok();
        log::info!("Started as Client — waiting for remote input");
        Ok(())
    }

    /// Stop the engine and disconnect.
    pub async fn stop(&self) -> anyhow::Result<()> {
        {
            let mut s = self.state.lock().await;
            s.role = Role::None;
            s.cursor_owner = CursorOwner::Local;
        }
        let p = self.platform.lock().await;
        p.stop_capture().ok();
        p.show_cursor().ok();
        log::info!("Engine stopped");
        Ok(())
    }

    /// Set the screen layout.
    pub async fn set_layout(&self, layout: ScreenLayout) {
        *self.layout.lock().await = layout;
    }

    /// Get current layout.
    pub async fn get_layout(&self) -> ScreenLayout {
        self.layout.lock().await.clone()
    }

    /// Get current role.
    pub async fn get_role(&self) -> Role {
        self.state.lock().await.role
    }

    /// Get current cursor owner.
    pub async fn get_cursor_owner(&self) -> CursorOwner {
        self.state.lock().await.cursor_owner
    }

    /// Access the platform input (for one-shot setup queries). Locks the mutex.
    pub async fn with_platform<R>(&self, f: impl FnOnce(&dyn PlatformInput) -> R) -> R {
        let p = self.platform.lock().await;
        f(p.as_ref())
    }

    /// Local screen id (for setup).
    pub fn local_id_for_setup(&self) -> String {
        self.local_screen_id.clone()
    }

    /// Platform accessor for one-shot setup (locks mutex).
    pub async fn platform_for_setup(&self) -> std::sync::Arc<tokio::sync::Mutex<Box<dyn PlatformInput>>> {
        self.platform.clone()
    }

    /// Process a captured local input event (host mode).
    /// Checks for screen edge transitions and either forwards to a peer
    /// or lets the event pass through normally.
    async fn handle_captured_event(&self, event: &InputEvent) {
        let role = self.state.lock().await.role;
        if role != Role::Host {
            return;
        }

        let cursor_owner = self.state.lock().await.cursor_owner;

        match cursor_owner {
            CursorOwner::Local => {
                // Cursor is on our screen — check for edge transitions
                if let InputEvent::MouseMoveAbsolute { x, y } = event {
                    // Convert normalized coords to pixels for edge detection
                    let (w, h) = {
                        let p = self.platform.lock().await;
                        match p.get_screen_size() {
                            Ok(size) => size,
                            Err(_) => return,
                        }
                    };
                    let px = (*x * w as f32) as i32;
                    let py = (*y * h as f32) as i32;

                    let layout = self.layout.lock().await;
                    if let Some((edge, neighbor)) = layout.detect_edge(&self.local_screen_id, px, py) {
                        if let Some(peer_id_str) = &neighbor.peer_id {
                            // Cursor is leaving our screen!
                            let (nx, ny) = layout.map_cursor_to_neighbor(
                                &self.local_screen_id,
                                edge,
                                px,
                                py,
                                neighbor,
                            );

                            if let Ok(peer_id) = uuid::Uuid::parse_str(peer_id_str) {
                                // Tell the peer to show cursor at the entry point
                                let _ = self.network.send_to(&peer_id, InputEvent::CursorEnter { x: nx, y: ny }).await;
                            }

                            // Hide our cursor
                            let p = self.platform.lock().await;
                            p.hide_cursor().ok();
                            drop(p);

                            // Update ownership
                            {
                                let mut s = self.state.lock().await;
                                if let Ok(pid) = uuid::Uuid::parse_str(peer_id_str) {
                                    s.cursor_owner = CursorOwner::Remote(pid);
                                }
                            }
                            log::info!("Cursor left local screen → forwarding to peer");
                            return;
                        }
                    }
                }
                // Event stays local — nothing to do (listen-only capture)
            }
            CursorOwner::Remote(peer_id) => {
                // Cursor is on a remote screen — forward all events there
                let _ = self.network.send_to(&peer_id, event.clone()).await;
            }
        }
    }

    /// Process a received network event (client mode).
    async fn handle_network_event(&self, from: PeerId, event: &InputEvent) {
        match event {
            InputEvent::CursorEnter { x, y } => {
                // Remote host says cursor is entering our screen
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
                {
                    let mut s = self.state.lock().await;
                    s.cursor_owner = CursorOwner::Local;
                }
                let p = self.platform.lock().await;
                p.hide_cursor().ok();
            }
            _ => {
                // Regular input event — inject into the OS if a remote owns the cursor
                let owner = self.state.lock().await.cursor_owner;
                if let CursorOwner::Remote(_) = owner {
                    let p = self.platform.lock().await;
                    p.inject_event(event).ok();
                }
            }
        }
    }
}
