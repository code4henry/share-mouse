mod commands;
mod core;
mod platform;

use std::sync::Arc;
use commands::AppState;
use core::{Engine, network::NetworkHub};
use platform::create_platform_input;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Create platform input handler
    let platform_input = create_platform_input();

    // Create network hub
    let (network_hub, _net_rx) = NetworkHub::new();
    let network_hub = Arc::new(network_hub);

    // Create the engine
    let local_screen_id = format!("local-{}", uuid::Uuid::new_v4());
    let engine = Arc::new(Engine::new(platform_input, network_hub.clone(), local_screen_id));

    // Build the Tauri app
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::default().build())
        .manage(AppState {
            engine,
            network: network_hub,
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_role,
            commands::set_role_host,
            commands::set_role_client,
            commands::get_screen_layout,
            commands::set_screen_layout,
            commands::get_cursor_state,
            commands::get_local_screen_size,
            commands::start_server,
            commands::connect_to_host,
            commands::get_peers,
            commands::stop_engine,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
