mod commands;
mod core;
mod platform;

use std::sync::Arc;
use std::sync::Mutex;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
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

    // ── Shared HWND for the background minimise monitor ────
    let hwnd_holder: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));

    // ── Dedicated background thread ──────────────────────
    // tauri::async_runtime may throttle when the window is minimized.
    // Run all keyboard/mouse forwarding on an independent thread+tokio
    // runtime so input injection survives window minimize/restore cycles.
    let bg_engine = engine.clone();
    let bg_network = network_hub.clone();
    let auto_host = std::env::var("SHAREMOUSE_AUTO_HOST").ok();
    let auto_client = std::env::var("SHAREMOUSE_AUTO_CLIENT").ok();
    let bg_hwnd = hwnd_holder.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("bg tokio runtime");
        rt.block_on(async move {
            // Persistent network-handler task.
            bg_engine.spawn_network_handler();

            // Auto-start Host
            if auto_host.is_some() {
                log::info!("SHAREMOUSE_AUTO_HOST set — auto-starting host");
                let n = bg_network.clone();
                tokio::spawn(async move {
                    if let Err(e) = n.start_server(24800).await {
                        log::error!("Server error: {}", e);
                    }
                });
                if let Err(e) = bg_engine.clone().start_host().await {
                    log::error!("start_host error: {}", e);
                }
            }

            // Auto-connect as Client
            if let Some(addr) = auto_client {
                log::info!("SHAREMOUSE_AUTO_CLIENT={} — auto-connecting as client", addr);
                match bg_network.connect_to(&addr).await {
                    Ok(peer_id) => {
                        log::info!("Connected to {} (peer: {})", addr, peer_id);
                        if let Err(e) = bg_engine.clone().start_client().await {
                            log::error!("start_client error: {}", e);
                        } else {
                            log::info!("Client mode active");
                        }
                    }
                    Err(e) => log::error!("Failed to connect to {}: {}", addr, e),
                }
            }

            // ── Minimise → hide monitor ─────────────────
            // Windows throttles SendInput/SetCursorPos for minimised
            // windows.  Poll IsIconic() on the main window HWND and
            // convert minimise to hide automatically.
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let hwnd = *bg_hwnd.lock().unwrap();
                    if let Some(h) = hwnd {
                        if h != 0 && is_iconic(h) {
                            restore_and_hide(h);
                            log::info!("minimise → hide (tray) to keep injection alive");
                        }
                    }
                }
            });

            // Hold the runtime open (idle, waiting on spawned tasks).
            std::future::pending::<()>().await;
        });
    });

    tauri::Builder::default()
        .setup(move |app| {
            // System tray — keep process alive when hidden.
            let _tray = TrayIconBuilder::with_id("share-mouse")
                .tooltip("ShareMouse")
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Share the main-window HWND with the background thread so it
            // can detect minimise and turn it into a tray hide.
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(target_os = "windows")]
                if let Ok(hwnd) = window.hwnd() {
                    *hwnd_holder.lock().unwrap() = Some(hwnd.0 as isize);
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // Close → hide to tray (input injection keeps working).
                let _ = window.hide();
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

// ── Windows minimise → tray helpers ──────────────────────

#[cfg(target_os = "windows")]
fn is_iconic(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    use windows::Win32::Foundation::HWND;
    unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

#[cfg(target_os = "windows")]
fn restore_and_hide(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_RESTORE, SW_HIDE};
    use windows::Win32::Foundation::HWND;
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_RESTORE);
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
    }
}

#[cfg(not(target_os = "windows"))]
fn is_iconic(_hwnd: isize) -> bool { false }

#[cfg(not(target_os = "windows"))]
fn restore_and_hide(_hwnd: isize) {}
