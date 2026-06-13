/// Tauri IPC commands — bridge between the React frontend and the Rust backend.

use std::sync::Arc;
use tauri::State;

use crate::core::{Engine, Role, CursorOwner, screen::ScreenLayout};
use crate::core::network::NetworkHub;
use crate::platform;

/// Shared application state.
pub struct AppState {
    pub engine: Arc<Engine>,
    pub network: Arc<NetworkHub>,
}

/// Role string for the frontend.
#[derive(serde::Serialize)]
pub struct RoleInfo {
    pub role: String,
}

/// Cursor state for the frontend.
#[derive(serde::Serialize)]
pub struct CursorInfo {
    pub owner: String,
    pub peer_id: Option<String>,
}

/// Peer info for the frontend.
#[derive(serde::Serialize)]
pub struct PeerInfo {
    pub id: String,
    pub addr: String,
}

/// Get current role (host or client).
#[tauri::command]
pub async fn get_role(state: State<'_, AppState>) -> Result<RoleInfo, String> {
    let role = state.engine.get_role().await;
    Ok(RoleInfo {
        role: match role {
            Role::Host => "host".to_string(),
            Role::Client => "client".to_string(),
            Role::None => "none".to_string(),
        },
    })
}

/// Switch to host mode: start the TCP server and begin capturing input.
#[tauri::command]
pub async fn set_role_host(state: State<'_, AppState>) -> Result<(), String> {
    // Start the TCP server in the background
    let network = state.network.clone();
    tokio::spawn(async move {
        if let Err(e) = network.start_server(24800).await {
            log::error!("Server error: {}", e);
        }
    });
    // Start host capture + forwarding
    state.engine.clone().start_host().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Switch to client mode: stop capturing, just receive and inject.
#[tauri::command]
pub async fn set_role_client(state: State<'_, AppState>) -> Result<(), String> {
    state.engine.clone().start_client().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Get current screen layout.
#[tauri::command]
pub async fn get_screen_layout(state: State<'_, AppState>) -> Result<ScreenLayout, String> {
    Ok(state.engine.get_layout().await)
}

/// Update the screen layout.
#[tauri::command]
pub async fn set_screen_layout(state: State<'_, AppState>, layout: ScreenLayout) -> Result<(), String> {
    state.engine.set_layout(layout).await;
    Ok(())
}

/// Get cursor ownership state.
#[tauri::command]
pub async fn get_cursor_state(state: State<'_, AppState>) -> Result<CursorInfo, String> {
    let owner = state.engine.get_cursor_owner().await;
    match owner {
        CursorOwner::Local => Ok(CursorInfo {
            owner: "local".to_string(),
            peer_id: None,
        }),
        CursorOwner::Remote(pid) => Ok(CursorInfo {
            owner: "remote".to_string(),
            peer_id: Some(pid.to_string()),
        }),
    }
}

/// Get local screen dimensions.
#[tauri::command]
pub async fn get_local_screen_size() -> Result<(u32, u32), String> {
    let platform = platform::create_platform_input();
    platform.get_screen_size().map_err(|e| e.to_string())
}

/// Connect to a remote host (client mode).
#[tauri::command]
pub async fn connect_to_host(
    state: State<'_, AppState>,
    addr: String,
) -> Result<String, String> {
    let peer_id = state.network.connect_to(&addr)
        .await
        .map_err(|e| e.to_string())?;
    Ok(peer_id.to_string())
}

/// Get list of connected peers.
#[tauri::command]
pub async fn get_peers(state: State<'_, AppState>) -> Result<Vec<PeerInfo>, String> {
    let peers = state.network.get_peers().await;
    Ok(peers
        .into_iter()
        .map(|(id, addr)| PeerInfo {
            id: id.to_string(),
            addr: addr.to_string(),
        })
        .collect())
}

/// Stop the engine and disconnect.
#[tauri::command]
pub async fn stop_engine(state: State<'_, AppState>) -> Result<(), String> {
    state.engine.stop().await.map_err(|e| e.to_string())
}
