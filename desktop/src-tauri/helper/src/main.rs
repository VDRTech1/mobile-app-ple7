//! PLE7 VPN Privileged Helper Daemon
//!
//! This daemon runs as root and manages TUN devices for the PLE7 VPN client.
//! It listens on a Unix socket and accepts commands from the main app.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const SOCKET_PATH: &str = "/var/run/ple7-helper.sock";
const TUN_MTU: u16 = 1420;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command")]
enum HelperCommand {
    #[serde(rename = "create_tun")]
    CreateTun {
        name: String,
        address: String,
        netmask: String,
    },
    #[serde(rename = "destroy_tun")]
    DestroyTun {
        name: String,
    },
    #[serde(rename = "add_route")]
    AddRoute {
        destination: String,
        prefix_len: u8,
        gateway: String,
    },
    #[serde(rename = "remove_route")]
    RemoveRoute {
        destination: String,
        prefix_len: u8,
    },
    #[serde(rename = "set_default_gateway")]
    SetDefaultGateway {
        gateway: String,
    },
    #[serde(rename = "restore_default_gateway")]
    RestoreDefaultGateway,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
struct HelperResponse {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

struct HelperState {
    tun_devices: HashMap<String, TunInfo>,
    original_gateway: Option<String>,
}

struct TunInfo {
    address: Ipv4Addr,
    #[allow(dead_code)]
    netmask: Ipv4Addr,
    fd: i32,
}

impl HelperState {
    fn new() -> Self {
        Self {
            tun_devices: HashMap::new(),
            original_gateway: None,
        }
    }
}

fn main() {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    log::info!("PLE7 Helper Daemon starting...");

    // Check if running as root
    if unsafe { libc::geteuid() } != 0 {
        log::error!("Helper daemon must run as root!");
        std::process::exit(1);
    }

    // Remove old socket if it exists
    if Path::new(SOCKET_PATH).exists() {
        fs::remove_file(SOCKET_PATH).ok();
    }

    // Create Unix socket listener
    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Failed to bind socket: {}", e);
            std::process::exit(1);
        }
    };

    // Set socket permissions (allow all users to connect)
    if let Err(e) = fs::set_permissions(SOCKET_PATH, fs::Permissions::from_mode(0o666)) {
        log::warn!("Failed to set socket permissions: {}", e);
    }

    log::info!("Listening on {}", SOCKET_PATH);

    let state = Arc::new(Mutex::new(HelperState::new()));

    // Handle connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    handle_connection(stream, state);
                });
            }
            Err(e) => {
                log::error!("Connection error: {}", e);
            }
        }
    }
}

fn handle_connection(mut stream: UnixStream, state: Arc<Mutex<HelperState>>) {
    log::debug!("New connection");

    let mut buffer = vec![0u8; 4096];

    loop {
        // Read command
        let n = match stream.read(&mut buffer) {
            Ok(0) => {
                log::debug!("Connection closed");
                return;
            }
            Ok(n) => n,
            Err(e) => {
                log::error!("Read error: {}", e);
                return;
            }
        };

        let request = String::from_utf8_lossy(&buffer[..n]);
        log::debug!("Received: {}", request);

        // Parse and handle command
        let response = match serde_json::from_str::<HelperCommand>(&request) {
            Ok(cmd) => handle_command(cmd, &state),
            Err(e) => HelperResponse {
                success: false,
                message: format!("Invalid command: {}", e),
                data: None,
            },
        };

        // Send response
        let response_json = serde_json::to_string(&response).unwrap();
        if let Err(e) = stream.write_all(response_json.as_bytes()) {
            log::error!("Write error: {}", e);
            return;
        }
        if let Err(e) = stream.write_all(b"\n") {
            log::error!("Write error: {}", e);
            return;
        }
    }
}

fn handle_command(cmd: HelperCommand, state: &Arc<Mutex<HelperState>>) -> HelperResponse {
    match cmd {
        HelperCommand::Ping => {
            HelperResponse {
                success: true,
                message: "pong".to_string(),
                data: None,
            }
        }

        HelperCommand::Status => {
            let state = state.lock().unwrap();
            let tun_names: Vec<&String> = state.tun_devices.keys().collect();
            HelperResponse {
                success: true,
                message: "ok".to_string(),
                data: Some(serde_json::json!({
                    "active_tuns": tun_names,
                    "has_original_gateway": state.original_gateway.is_some(),
                })),
            }
        }

        HelperCommand::CreateTun { name, address, netmask } => {
            create_tun(state, &name, &address, &netmask)
        }

        HelperCommand::DestroyTun { name } => {
            destroy_tun(state, &name)
        }

        HelperCommand::AddRoute { destination, prefix_len, gateway } => {
            add_route(&destination, prefix_len, &gateway)
        }

        HelperCommand::RemoveRoute { destination, prefix_len } => {
            remove_route(&destination, prefix_len)
        }

        HelperCommand::SetDefaultGateway { gateway } => {
            set_default_gateway(state, &gateway)
        }

        HelperCommand::RestoreDefaultGateway => {
            restore_default_gateway(state)
        }
    }
}

fn create_tun(state: &Arc<Mutex<HelperState>>, name: &str, address: &str, netmask: &str) -> HelperResponse {
    log::info!("Creating TUN device: {} with address {}/{}", name, address, netmask);

    let addr: Ipv4Addr = match address.parse() {
        Ok(a) => a,
        Err(e) => return HelperResponse {
            success: false,
            message: format!("Invalid address: {}", e),
            data: None,
        },
    };

    let mask: Ipv4Addr = match netmask.parse() {
        Ok(m) => m,
        Err(e) => return HelperResponse {
            success: false,
            message: format!("Invalid netmask: {}", e),
            data: None,
        },
    };

    // Create TUN device using tun crate
    let mut config = tun::Configuration::default();
    config
        .address(addr)
        .netmask(mask)
        .mtu(TUN_MTU)
        .up();

    match tun::create(&config) {
        Ok(device) => {
            use tun::AbstractDevice;
            let actual_name = device.tun_name().unwrap_or_else(|_| name.to_string());
            let fd = device.as_raw_fd();

            log::info!("TUN device created: {} (fd: {})", actual_name, fd);

            // Store device info (we keep the device alive by storing fd reference)
            // Note: We're leaking the device here intentionally to keep it alive
            // The fd will be closed when we destroy the TUN
            std::mem::forget(device);

            let mut state = state.lock().unwrap();
            state.tun_devices.insert(actual_name.clone(), TunInfo {
                address: addr,
                netmask: mask,
                fd,
            });

            HelperResponse {
                success: true,
                message: format!("TUN device {} created", actual_name),
                data: Some(serde_json::json!({
                    "name": actual_name,
                    "address": address,
                    "fd": fd,
                })),
            }
        }
        Err(e) => {
            log::error!("Failed to create TUN: {}", e);
            HelperResponse {
                success: false,
                message: format!("Failed to create TUN device: {}", e),
                data: None,
            }
        }
    }
}

fn destroy_tun(state: &Arc<Mutex<HelperState>>, name: &str) -> HelperResponse {
    log::info!("Destroying TUN device: {}", name);

    let mut state = state.lock().unwrap();
    if let Some(info) = state.tun_devices.remove(name) {
        // Close the file descriptor
        unsafe {
            libc::close(info.fd);
        }
        HelperResponse {
            success: true,
            message: format!("TUN device {} destroyed", name),
            data: None,
        }
    } else {
        HelperResponse {
            success: false,
            message: format!("TUN device {} not found", name),
            data: None,
        }
    }
}

fn add_route(destination: &str, prefix_len: u8, gateway: &str) -> HelperResponse {
    log::info!("Adding route: {}/{} via {}", destination, prefix_len, gateway);

    let output = Command::new("route")
        .args(["-n", "add", "-net", &format!("{}/{}", destination, prefix_len), gateway])
        .output();

    match output {
        Ok(output) => {
            if output.status.success() {
                HelperResponse {
                    success: true,
                    message: "Route added".to_string(),
                    data: None,
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("File exists") {
                    HelperResponse {
                        success: true,
                        message: "Route already exists".to_string(),
                        data: None,
                    }
                } else {
                    HelperResponse {
                        success: false,
                        message: format!("Failed to add route: {}", stderr),
                        data: None,
                    }
                }
            }
        }
        Err(e) => HelperResponse {
            success: false,
            message: format!("Failed to execute route command: {}", e),
            data: None,
        },
    }
}

fn remove_route(destination: &str, prefix_len: u8) -> HelperResponse {
    log::info!("Removing route: {}/{}", destination, prefix_len);

    let output = Command::new("route")
        .args(["-n", "delete", "-net", &format!("{}/{}", destination, prefix_len)])
        .output();

    match output {
        Ok(output) => {
            if output.status.success() {
                HelperResponse {
                    success: true,
                    message: "Route removed".to_string(),
                    data: None,
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                HelperResponse {
                    success: false,
                    message: format!("Failed to remove route: {}", stderr),
                    data: None,
                }
            }
        }
        Err(e) => HelperResponse {
            success: false,
            message: format!("Failed to execute route command: {}", e),
            data: None,
        },
    }
}

fn set_default_gateway(state: &Arc<Mutex<HelperState>>, gateway: &str) -> HelperResponse {
    log::info!("Setting default gateway to: {}", gateway);

    // Save current default gateway
    let output = Command::new("route")
        .args(["-n", "get", "default"])
        .output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("gateway:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mut state = state.lock().unwrap();
                    state.original_gateway = Some(parts[1].to_string());
                    log::info!("Saved original gateway: {}", parts[1]);
                }
            }
        }
    }

    // Add split routes for VPN (0.0.0.0/1 and 128.0.0.0/1)
    let result1 = Command::new("route")
        .args(["-n", "add", "-net", "0.0.0.0/1", gateway])
        .output();

    let result2 = Command::new("route")
        .args(["-n", "add", "-net", "128.0.0.0/1", gateway])
        .output();

    match (result1, result2) {
        (Ok(o1), Ok(o2)) if o1.status.success() && o2.status.success() => {
            HelperResponse {
                success: true,
                message: "Default gateway set".to_string(),
                data: None,
            }
        }
        _ => HelperResponse {
            success: false,
            message: "Failed to set default gateway".to_string(),
            data: None,
        },
    }
}

fn restore_default_gateway(state: &Arc<Mutex<HelperState>>) -> HelperResponse {
    log::info!("Restoring default gateway");

    // Remove VPN routes
    Command::new("route")
        .args(["-n", "delete", "-net", "0.0.0.0/1"])
        .output()
        .ok();

    Command::new("route")
        .args(["-n", "delete", "-net", "128.0.0.0/1"])
        .output()
        .ok();

    let state = state.lock().unwrap();
    if let Some(ref original) = state.original_gateway {
        log::info!("Restored original gateway: {}", original);
    }

    HelperResponse {
        success: true,
        message: "Default gateway restored".to_string(),
        data: None,
    }
}
