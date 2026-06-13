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

    // Seed a default single-screen layout so edge detection has a local screen entry.
    {
        let engine = engine.clone();
        tauri::async_runtime::spawn(async move {
            let size = engine.with_platform(|p| p.get_screen_size()).await;
            if let Ok((w, h)) = size {
                use core::screen::{ScreenInfo, ScreenRect};
                let layout = core::screen::ScreenLayout {
                    screens: vec![ScreenInfo {
                        id: engine.local_id_for_setup(),
                        name: "Local".to_string(),
                        rect: ScreenRect { x: 0, y: 0, width: w, height: h },
                        peer_id: None,
                        width: w,
                        height: h,
                        dpi: 72,
                    }],
                };
                engine.set_layout(layout).await;
                log::info!("Default layout: local screen {}x{}", w, h);
            }
        });
    }

    // Spawn the persistent network handler (consumes incoming events, injects them)
    engine.spawn_network_handler();

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
            commands::connect_to_host,
            commands::get_peers,
            commands::stop_engine,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
