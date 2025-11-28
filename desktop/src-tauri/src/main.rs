// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod tunnel;
mod config;
mod stun;
mod tun_device;
mod wireguard;
mod websocket;

use std::sync::Arc;
use std::io::Write;
use std::fs::OpenOptions;
use tauri::{Manager, Emitter};
use tokio::sync::Mutex;
use tunnel::{TunnelManager, AppState};

fn get_log_path() -> std::path::PathBuf {
    // Use ~/Library/Logs on macOS, temp dir on other platforms
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let log_dir = std::path::PathBuf::from(home).join("Library/Logs");
            if log_dir.exists() {
                return log_dir.join("ple7-vpn.log");
            }
        }
    }

    // Fallback to temp directory
    std::env::temp_dir().join("ple7-vpn.log")
}

fn log_to_file(msg: &str) {
    let log_path = get_log_path();
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{}] {}", timestamp, msg);
    }
}

fn main() {
    // Clear previous log
    let log_path = get_log_path();
    let _ = std::fs::write(&log_path, "");

    // Set up panic hook to log panics to file
    std::panic::set_hook(Box::new(|panic_info| {
        let msg = format!("PANIC: {}", panic_info);
        log_to_file(&msg);
        eprintln!("{}", msg);
    }));

    log_to_file("=== PLE7 VPN Starting ===");
    log_to_file(&format!("Log file: {:?}", log_path));
    log_to_file(&format!("OS: {}", std::env::consts::OS));
    log_to_file(&format!("Arch: {}", std::env::consts::ARCH));

    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log_to_file("env_logger initialized");
    log::info!("Starting PLE7 VPN...");

    log_to_file("Building Tauri app...");
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Handle deep link on Windows/Linux when app is already running
            log_to_file(&format!("Single instance args: {:?}", args));
            if let Some(url) = args.get(1) {
                if url.starts_with("ple7://") {
                    log_to_file(&format!("Deep link received: {}", url));
                    let _ = app.emit("deep-link", url.clone());
                }
            }
        }))
        .setup(|app| {
            log_to_file("Setup callback started");

            // Register deep link URL scheme at runtime (Windows/Linux)
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                log_to_file("Registering deep link URL scheme...");
                match tauri_plugin_deep_link::register_all(app) {
                    Ok(_) => log_to_file("Deep link URL scheme registered successfully"),
                    Err(e) => log_to_file(&format!("Failed to register deep link: {}", e)),
                }
            }

            // Initialize app state
            log_to_file("Creating TunnelManager...");
            let tunnel_manager = Arc::new(Mutex::new(TunnelManager::new()));

            log_to_file("Creating ApiClient...");
            let api_client = api::ApiClient::new("https://ple7.com".to_string());

            log_to_file("Managing AppState...");
            app.manage(AppState {
                tunnel_manager,
                api_client,
            });

            // Check for deep link URL in command line args (Windows startup case)
            let args: Vec<String> = std::env::args().collect();
            log_to_file(&format!("Startup args: {:?}", args));
            for arg in args.iter().skip(1) {
                if arg.starts_with("ple7://") {
                    log_to_file(&format!("Deep link on startup: {}", arg));
                    let url = arg.clone();
                    let handle = app.handle().clone();
                    // Emit after a short delay to ensure frontend is ready
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let _ = handle.emit("deep-link", url);
                    });
                    break;
                }
            }

            log_to_file("App setup complete");
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

    match result {
        Ok(()) => {
            log_to_file("Application exited normally");
        }
        Err(e) => {
            let error_msg = format!("ERROR: Application failed: {}", e);
            log_to_file(&error_msg);
            log::error!("{}", error_msg);
            eprintln!("{}", error_msg);
        }
    }
}
