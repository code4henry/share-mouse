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

    // Create the engine. The local screen layout is seeded lazily
    // (Engine::ensure_local_screen) on first capture / peer connect.
    let local_screen_id = format!("local-{}", uuid::Uuid::new_v4());
    let engine = Arc::new(Engine::new(platform_input, network_hub.clone(), local_screen_id));

    // Spawn the persistent network handler (consumes incoming events, injects them)
    engine.spawn_network_handler();

    tauri::Builder::default()
        .setup({
            let engine = engine.clone();
            let network = network_hub.clone();
            move |_app| {
                // Test/dev affordance: auto-start Host mode without UI.
                if std::env::var("SHAREMOUSE_AUTO_HOST").is_ok() {
                    log::info!("SHAREMOUSE_AUTO_HOST set — auto-starting host");
                    let engine = engine.clone();
                    let network = network.clone();
                    tauri::async_runtime::spawn(async move {
                        let n = network.clone();
                        tokio::spawn(async move {
                            if let Err(e) = n.start_server(24800).await {
                                log::error!("Server error: {}", e);
                            }
                        });
                        if let Err(e) = engine.clone().start_host().await {
                            log::error!("start_host error: {}", e);
                        }
                        let granted = engine.check_permission_simple().await;
                        log::info!("Accessibility permission: {}", if granted { "GRANTED" } else { "MISSING" });
                    });
                }
                // Auto-connect as Client to a given IP:port.
                if let Ok(addr) = std::env::var("SHAREMOUSE_AUTO_CLIENT") {
                    log::info!("SHAREMOUSE_AUTO_CLIENT={} — auto-connecting as client", addr);
                    let engine = engine.clone();
                    let network = network.clone();
                    tauri::async_runtime::spawn(async move {
                        match network.connect_to(&addr).await {
                            Ok(peer_id) => {
                                log::info!("Connected to {} (peer: {})", addr, peer_id);
                                if let Err(e) = engine.clone().start_client().await {
                                    log::error!("start_client error: {}", e);
                                } else {
                                    log::info!("Client mode active");
                                }
                            }
                            Err(e) => log::error!("Failed to connect to {}: {}", addr, e),
                        }
                    });
                }
                Ok(())
            }
        })
        .plugin(
            tauri_plugin_log::Builder::default()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("share-mouse".into()),
                    }),
                ])
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .build(),
        )
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
            commands::check_permission,
            commands::open_accessibility_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
