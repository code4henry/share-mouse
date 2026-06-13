pub mod protocol;
pub mod network;
pub mod screen;

use std::sync::Arc;

use tokio::sync::Mutex;

use protocol::InputEvent;
use network::{NetworkHub, PeerId};
use screen::ScreenLayout;
use crate::platform::PlatformInput;

/// Whether this instance is the host (sending input) or client (receiving input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
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

/// The main ShareMouse engine that ties everything together.
pub struct Engine {
    platform: Arc<Mutex<Box<dyn PlatformInput>>>,
    network: Arc<NetworkHub>,
    layout: Arc<Mutex<ScreenLayout>>,
    role: Arc<Mutex<Role>>,
    cursor_owner: Arc<Mutex<CursorOwner>>,
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
            role: Arc::new(Mutex::new(Role::Client)),
            cursor_owner: Arc::new(Mutex::new(CursorOwner::Local)),
            local_screen_id,
        }
    }

    /// Set the screen layout.
    pub async fn set_layout(&self, layout: ScreenLayout) {
        let mut l = self.layout.lock().await;
        *l = layout;
    }

    /// Get current layout.
    pub async fn get_layout(&self) -> ScreenLayout {
        self.layout.lock().await.clone()
    }

    /// Set the role.
    pub async fn set_role(&self, role: Role) {
        let mut r = self.role.lock().await;
        *r = role;
    }

    /// Get current role.
    pub async fn get_role(&self) -> Role {
        *self.role.lock().await
    }

    /// Get current cursor owner.
    pub async fn get_cursor_owner(&self) -> CursorOwner {
        *self.cursor_owner.lock().await
    }

    /// Start the engine — begin capturing input (if host) and processing network events.
    pub async fn start(&self) -> anyhow::Result<()> {
        // Note: the full event loop will be driven by the network hub's broadcast channel

        let role = *self.role.lock().await;
        if role == Role::Host {
            let platform = self.platform.lock().await;
            let _capture_rx = platform.start_capture()?;
            log::info!("Started as Host — capturing input");
        } else {
            log::info!("Started as Client — waiting for remote input");
        }

        Ok(())
    }

    /// Stop the engine.
    pub async fn stop(&self) -> anyhow::Result<()> {
        let platform = self.platform.lock().await;
        platform.stop_capture()?;
        platform.show_cursor()?;
        log::info!("Engine stopped");
        Ok(())
    }

    /// Process a captured local input event (host mode).
    /// Checks for screen edge transitions and either forwards to a peer
    /// or lets the event pass through normally.
    pub async fn handle_captured_event(&self, event: &InputEvent) {
        let role = *self.role.lock().await;
        if role != Role::Host {
            return;
        }

        let cursor_owner = *self.cursor_owner.lock().await;

        match cursor_owner {
            CursorOwner::Local => {
                // Cursor is on our screen — check for edge transitions
                if let InputEvent::MouseMoveAbsolute { x, y } = event {
                    let layout = self.layout.lock().await;
                    if let Some((edge, neighbor)) = layout.detect_edge(
                        &self.local_screen_id,
                        *x as i32,
                        *y as i32,
                    ) {
                        if let Some(peer_id_str) = &neighbor.peer_id {
                            // Cursor is leaving our screen!
                            let (nx, ny) = layout.map_cursor_to_neighbor(
                                &self.local_screen_id,
                                edge,
                                *x as i32,
                                *y as i32,
                                neighbor,
                            );

                            // Tell the peer to show cursor at the entry point
                            if let Ok(peer_id) = uuid::Uuid::parse_str(peer_id_str) {
                                let _ = self.network.send_to(&peer_id, InputEvent::CursorEnter { x: nx, y: ny }).await;
                                // Also forward subsequent mouse moves
                                let _ = self.network.send_to(&peer_id, InputEvent::MouseMoveAbsolute { x: *x, y: *y }).await;
                            }

                            // Hide our cursor and warp it away from the edge
                            let platform = self.platform.lock().await;
                            platform.hide_cursor().ok();

                            // Update ownership
                            drop(platform);
                            {
                                let mut owner = self.cursor_owner.lock().await;
                                if let Ok(pid) = uuid::Uuid::parse_str(peer_id_str) {
                                    *owner = CursorOwner::Remote(pid);
                                }
                            }
                            return;
                        }
                    }
                }
                // Event stays local — no action needed (we're listen-only on the tap)
            }
            CursorOwner::Remote(peer_id) => {
                // Cursor is on a remote screen — forward all events there
                let _ = self.network.send_to(&peer_id, event.clone()).await;
            }
        }
    }

    /// Process a received network event (client mode).
    pub async fn handle_network_event(&self, from: PeerId, event: &InputEvent) {
        match event {
            InputEvent::CursorEnter { x, y } => {
                // Remote host says cursor is entering our screen
                {
                    let mut owner = self.cursor_owner.lock().await;
                    *owner = CursorOwner::Remote(from);
                }
                let platform = self.platform.lock().await;
                platform.show_cursor().ok();
                platform.inject_event(&InputEvent::MouseMoveAbsolute { x: *x, y: *y }).ok();
            }
            InputEvent::CursorLeave => {
                // Remote says cursor is leaving
                {
                    let mut owner = self.cursor_owner.lock().await;
                    *owner = CursorOwner::Local;
                }
                let platform = self.platform.lock().await;
                platform.hide_cursor().ok();
            }
            _ => {
                // Regular input event — inject into the OS
                let owner = self.cursor_owner.lock().await;
                if let CursorOwner::Remote(_) = *owner {
                    drop(owner);
                    let platform = self.platform.lock().await;
                    platform.inject_event(event).ok();
                }
            }
        }
    }
}
