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
use std::io::Write;
use std::fs::OpenOptions;
use std::collections::VecDeque;
use tauri::{Manager, Emitter};
use tokio::sync::Mutex;
use parking_lot::RwLock;
use tunnel::{TunnelManager, AppState};

// Global log buffer for frontend access
static LOG_BUFFER: std::sync::OnceLock<Arc<RwLock<VecDeque<String>>>> = std::sync::OnceLock::new();
static LOG_BUFFER_SIZE: usize = 500;

fn get_log_buffer() -> Arc<RwLock<VecDeque<String>>> {
    LOG_BUFFER.get_or_init(|| Arc::new(RwLock::new(VecDeque::with_capacity(LOG_BUFFER_SIZE)))).clone()
}

fn add_to_log_buffer(msg: &str) {
    let buffer = get_log_buffer();
    let mut logs = buffer.write();
    if logs.len() >= LOG_BUFFER_SIZE {
        logs.pop_front(); // O(1) instead of O(n)
    }
    logs.push_back(msg.to_string());
}

/// Custom logger that captures logs for the UI
struct UiLogger;

impl log::Log for UiLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // Use std time - works on all platforms
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = now.as_secs() % 86400; // Seconds since midnight
            let hours = secs / 3600;
            let mins = (secs % 3600) / 60;
            let sec = secs % 60;
            let millis = now.subsec_millis();

            let level = match record.level() {
                log::Level::Error => "ERROR",
                log::Level::Warn => "WARN",
                log::Level::Info => "INFO",
                log::Level::Debug => "DEBUG",
                log::Level::Trace => "TRACE",
            };
            let msg = format!("[{:02}:{:02}:{:02}.{:03}] {} {}", hours, mins, sec, millis, level, record.args());

            // Print to stdout/stderr
            if record.level() <= log::Level::Warn {
                eprintln!("{}", msg);
            } else {
                println!("{}", msg);
            }

            // Add to UI buffer
            add_to_log_buffer(&msg);

            // Also write to file
            log_to_file(&msg);
        }
    }

    fn flush(&self) {}
}

static LOGGER: UiLogger = UiLogger;

#[tauri::command]
fn get_logs(since_index: Option<usize>) -> (Vec<String>, usize) {
    let buffer = get_log_buffer();
    let logs = buffer.read();
    let start = since_index.unwrap_or(0);
    let new_logs: Vec<String> = logs.iter().skip(start).cloned().collect();
    let current_index = logs.len();
    (new_logs, current_index)
}

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
    // Skip file logging in release builds for performance
    #[cfg(debug_assertions)]
    {
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
    #[cfg(not(debug_assertions))]
    {
        let _ = msg; // Suppress unused warning
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

    // Initialize custom logging that captures to UI buffer
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(log::LevelFilter::Info))
        .expect("Failed to set logger");

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
                use tauri_plugin_deep_link::DeepLinkExt;
                log_to_file("Registering deep link URL scheme...");
                match app.deep_link().register("ple7") {
                    Ok(_) => log_to_file("Deep link URL scheme 'ple7' registered successfully"),
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
            get_logs,
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
