// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod tunnel;
mod config;
mod stun;
mod tun_device;
mod wireguard;
mod websocket;

#[cfg(target_os = "macos")]
mod helper_client;

use std::sync::Arc;
use tauri::{Manager, Emitter};
use tokio::sync::Mutex;
use tunnel::{TunnelManager, AppState};

/// Minimal logger - only prints errors to stderr in release builds
struct MinimalLogger;

impl log::Log for MinimalLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // In release: only errors. In debug: info and above
        #[cfg(debug_assertions)]
        { metadata.level() <= log::Level::Info }
        #[cfg(not(debug_assertions))]
        { metadata.level() <= log::Level::Error }
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: MinimalLogger = MinimalLogger;

fn main() {
    // Set up panic hook
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("PANIC: {}", panic_info);
    }));

    // Initialize minimal logging
    log::set_logger(&LOGGER)
        .map(|()| {
            #[cfg(debug_assertions)]
            { log::set_max_level(log::LevelFilter::Info) }
            #[cfg(not(debug_assertions))]
            { log::set_max_level(log::LevelFilter::Error) }
        })
        .expect("Failed to set logger");

    log::info!("Starting PLE7 VPN...");

    let result = tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Handle deep link on Windows/Linux when app is already running
            if let Some(url) = args.get(1) {
                if url.starts_with("ple7://") {
                    let _ = app.emit("deep-link", url.clone());
                }
            }
        }))
        .setup(|app| {
            // Register deep link URL scheme at runtime (Windows/Linux)
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let _ = app.deep_link().register("ple7");
            }

            // Initialize app state
            let tunnel_manager = Arc::new(Mutex::new(TunnelManager::new()));
            let api_client = api::ApiClient::new("https://ple7.com".to_string());

            app.manage(AppState {
                tunnel_manager,
                api_client,
            });

            // Check for deep link URL in command line args (Windows startup case)
            let args: Vec<String> = std::env::args().collect();
            for arg in args.iter().skip(1) {
                if arg.starts_with("ple7://") {
                    let url = arg.clone();
                    let handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let _ = handle.emit("deep-link", url);
                    });
                    break;
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api::login,
            api::verify_token,
            api::get_networks,
            api::get_devices,
            api::get_device_config,
            api::get_relays,
            api::auto_register_device,
            api::set_exit_node,
            config::store_token,
            config::get_stored_token,
            config::clear_stored_token,
            tunnel::connect_vpn,
            tunnel::disconnect_vpn,
            tunnel::get_connection_status,
            tunnel::get_connection_stats,
        ])
        .run(tauri::generate_context!());

    if let Err(e) = result {
        log::error!("Application failed: {}", e);
    }
}
