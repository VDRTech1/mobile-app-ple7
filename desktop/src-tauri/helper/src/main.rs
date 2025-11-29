//! PLE7 VPN Privileged Helper Daemon
//!
//! This daemon runs as root and manages TUN devices for the PLE7 VPN client.
//! It listens on a Unix socket and accepts commands from the main app.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const SOCKET_PATH: &str = "/var/run/ple7-helper.sock";

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
        /// IP address to exclude from VPN routing (e.g., relay endpoint)
        #[serde(default)]
        exclude_ip: Option<String>,
    },
    #[serde(rename = "restore_default_gateway")]
    RestoreDefaultGateway,
    #[serde(rename = "read_packet")]
    ReadPacket {
        tun_name: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "write_packet")]
    WritePacket {
        tun_name: String,
        #[serde(with = "base64_serde")]
        data: Vec<u8>,
    },
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "ping")]
    Ping,
}

// Helper module for base64 serialization
mod base64_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use base64::{Engine as _, engine::general_purpose};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        general_purpose::STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
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
    /// IP that was excluded from VPN routing (needs to be cleaned up on restore)
    excluded_ip: Option<String>,
}

struct TunInfo {
    address: Ipv4Addr,
    #[allow(dead_code)]
    netmask: Ipv4Addr,
    // File descriptor for the utun device
    fd: i32,
}

impl HelperState {
    fn new() -> Self {
        Self {
            tun_devices: HashMap::new(),
            original_gateway: None,
            excluded_ip: None,
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
            add_route_with_state(state, &destination, prefix_len, &gateway)
        }

        HelperCommand::RemoveRoute { destination, prefix_len } => {
            remove_route(&destination, prefix_len)
        }

        HelperCommand::SetDefaultGateway { gateway, exclude_ip } => {
            set_default_gateway(state, &gateway, exclude_ip.as_deref())
        }

        HelperCommand::RestoreDefaultGateway => {
            restore_default_gateway(state)
        }

        HelperCommand::ReadPacket { tun_name, timeout_ms } => {
            read_packet(state, &tun_name, timeout_ms)
        }

        HelperCommand::WritePacket { tun_name, data } => {
            write_packet(state, &tun_name, &data)
        }
    }
}

// macOS-specific utun creation using system socket
fn create_utun() -> Result<(i32, String), String> {
    // Constants for macOS utun (from sys/kern_control.h and net/if_utun.h)
    const PF_SYSTEM: libc::c_int = 32;
    const SOCK_DGRAM: libc::c_int = 2;
    const SYSPROTO_CONTROL: libc::c_int = 2;
    const AF_SYS_CONTROL: libc::c_uchar = 2;
    const UTUN_CONTROL_NAME: &str = "com.apple.net.utun_control";

    // ctl_info structure (100 bytes: 4 + 96)
    #[repr(C)]
    struct CtlInfo {
        ctl_id: u32,
        ctl_name: [libc::c_char; 96],
    }

    impl Default for CtlInfo {
        fn default() -> Self {
            Self {
                ctl_id: 0,
                ctl_name: [0; 96],
            }
        }
    }

    // sockaddr_ctl structure
    #[repr(C)]
    struct SockaddrCtl {
        sc_len: libc::c_uchar,
        sc_family: libc::c_uchar,
        ss_sysaddr: u16,
        sc_id: u32,
        sc_unit: u32,
        sc_reserved: [u32; 5],
    }

    // CTLIOCGINFO = _IOWR('N', 3, struct ctl_info)
    // Manually compute for macOS: IOC_INOUT | (100 << 16) | ('N' << 8) | 3
    // = 0xC0000000 | (0x64 << 16) | (0x4E << 8) | 3 = 0xC0644E03
    // On macOS, ioctl request parameter is c_ulong (unsigned long)
    #[cfg(target_os = "macos")]
    const CTLIOCGINFO: libc::c_ulong = 0xC0644E03;

    unsafe {
        // Create PF_SYSTEM socket
        let fd = libc::socket(PF_SYSTEM, SOCK_DGRAM, SYSPROTO_CONTROL);
        if fd < 0 {
            return Err(format!("Failed to create socket: {}", std::io::Error::last_os_error()));
        }

        // Prepare ctl_info with utun control name
        let mut info: CtlInfo = Default::default();
        for (i, c) in UTUN_CONTROL_NAME.bytes().enumerate() {
            if i < 96 {
                info.ctl_name[i] = c as libc::c_char;
            }
        }

        // Get the control ID using libc::ioctl
        // On macOS, ioctl signature is: fn(c_int, c_ulong, ...) -> c_int
        let ret = libc::ioctl(fd, CTLIOCGINFO, &mut info as *mut CtlInfo);
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("Failed to get utun control ID: {}", err));
        }

        log::info!("Got utun control ID: {}", info.ctl_id);

        // Try to find an available utun unit
        for unit in 0u32..256 {
            let addr = SockaddrCtl {
                sc_len: std::mem::size_of::<SockaddrCtl>() as libc::c_uchar,
                sc_family: AF_SYS_CONTROL,
                ss_sysaddr: 0,
                sc_id: info.ctl_id,
                sc_unit: unit + 1, // utun0 = unit 1
                sc_reserved: [0; 5],
            };

            let ret = libc::connect(
                fd,
                &addr as *const SockaddrCtl as *const libc::sockaddr,
                std::mem::size_of::<SockaddrCtl>() as libc::socklen_t,
            );

            if ret == 0 {
                let name = format!("utun{}", unit);
                log::info!("Created {}", name);
                return Ok((fd, name));
            }
        }

        libc::close(fd);
        Err("No available utun unit".to_string())
    }
}

fn configure_utun(name: &str, address: &str, netmask: &str) -> Result<(), String> {
    // Use ifconfig to configure the interface
    let output = Command::new("ifconfig")
        .args([name, address, address, "netmask", netmask, "up"])
        .output()
        .map_err(|e| format!("Failed to execute ifconfig: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to configure interface: {}", stderr));
    }

    Ok(())
}

fn create_tun(state: &Arc<Mutex<HelperState>>, _name: &str, address: &str, netmask: &str) -> HelperResponse {
    log::info!("Creating TUN device with address {}/{}", address, netmask);

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

    // Create utun device
    let (fd, actual_name) = match create_utun() {
        Ok((fd, name)) => (fd, name),
        Err(e) => {
            log::error!("Failed to create utun: {}", e);
            return HelperResponse {
                success: false,
                message: format!("Failed to create TUN device: {}", e),
                data: None,
            };
        }
    };

    // Configure the interface
    if let Err(e) = configure_utun(&actual_name, address, netmask) {
        log::error!("Failed to configure utun: {}", e);
        unsafe { libc::close(fd); }
        return HelperResponse {
            success: false,
            message: format!("Failed to configure TUN device: {}", e),
            data: None,
        };
    }

    // Store device info
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
        })),
    }
}

fn destroy_tun(state: &Arc<Mutex<HelperState>>, name: &str) -> HelperResponse {
    log::info!("Destroying TUN device: {}", name);

    let mut state = state.lock().unwrap();
    if let Some(info) = state.tun_devices.remove(name) {
        // Close the file descriptor to destroy the utun
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

fn add_route_via_gateway(destination: &str, prefix_len: u8, gateway: &str) -> HelperResponse {
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

fn add_route_with_state(state: &Arc<Mutex<HelperState>>, destination: &str, prefix_len: u8, gateway: &str) -> HelperResponse {
    log::info!("Adding route: {}/{} via {}", destination, prefix_len, gateway);

    // Find the interface name by looking up the gateway IP in our TUN devices
    let interface_name = {
        let state = state.lock().unwrap();
        let gateway_ip: std::net::Ipv4Addr = match gateway.parse() {
            Ok(ip) => ip,
            Err(_) => {
                log::warn!("Invalid gateway IP: {}, using gateway-based route", gateway);
                return add_route_via_gateway(destination, prefix_len, gateway);
            }
        };

        state.tun_devices.iter()
            .find(|(_, info)| info.address == gateway_ip)
            .map(|(name, _)| name.clone())
    };

    // If we found the interface, use -interface; otherwise fall back to gateway
    let output = if let Some(ref iface) = interface_name {
        log::info!("Using interface-based route: {}/{} via interface {}", destination, prefix_len, iface);
        Command::new("route")
            .args(["-n", "add", "-net", &format!("{}/{}", destination, prefix_len), "-interface", iface])
            .output()
    } else {
        log::info!("Using gateway-based route: {}/{} via gateway {}", destination, prefix_len, gateway);
        Command::new("route")
            .args(["-n", "add", "-net", &format!("{}/{}", destination, prefix_len), gateway])
            .output()
    };

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

fn set_default_gateway(state: &Arc<Mutex<HelperState>>, gateway: &str, exclude_ip: Option<&str>) -> HelperResponse {
    log::info!("Setting default gateway to: {}", gateway);
    if let Some(ip) = exclude_ip {
        log::info!("Excluding IP from VPN routing: {}", ip);
    }

    // Save current default gateway
    let mut original_gw: Option<String> = None;
    let output = Command::new("route")
        .args(["-n", "get", "default"])
        .output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("gateway:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    original_gw = Some(parts[1].to_string());
                    let mut state = state.lock().unwrap();
                    state.original_gateway = Some(parts[1].to_string());
                    log::info!("Saved original gateway: {}", parts[1]);
                }
            }
        }
    }

    // Add bypass route for excluded IP (e.g., relay endpoint) via original gateway
    // This MUST be done BEFORE setting VPN routes to prevent routing loop
    if let (Some(ip), Some(ref orig_gw)) = (exclude_ip, &original_gw) {
        log::info!("Adding bypass route for {} via {}", ip, orig_gw);
        let result = Command::new("route")
            .args(["-n", "add", "-host", ip, orig_gw])
            .output();

        match result {
            Ok(o) if o.status.success() => {
                log::info!("Bypass route added successfully");
                // Store excluded IP so we can remove it on restore
                let mut state = state.lock().unwrap();
                state.excluded_ip = Some(ip.to_string());
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                log::warn!("Bypass route may already exist: {}", stderr);
                // Still store it so we can try to clean it up
                let mut state = state.lock().unwrap();
                state.excluded_ip = Some(ip.to_string());
            }
            Err(e) => {
                log::error!("Failed to add bypass route: {}", e);
                return HelperResponse {
                    success: false,
                    message: format!("Failed to add bypass route for {}: {}", ip, e),
                    data: None,
                };
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

    let mut state = state.lock().unwrap();

    // Remove bypass route for excluded IP
    if let Some(ref excluded) = state.excluded_ip {
        log::info!("Removing bypass route for {}", excluded);
        Command::new("route")
            .args(["-n", "delete", "-host", excluded])
            .output()
            .ok();
    }
    state.excluded_ip = None;

    if let Some(ref original) = state.original_gateway {
        log::info!("Restored original gateway: {}", original);
    }

    HelperResponse {
        success: true,
        message: "Default gateway restored".to_string(),
        data: None,
    }
}

fn read_packet(state: &Arc<Mutex<HelperState>>, tun_name: &str, timeout_ms: Option<u64>) -> HelperResponse {
    // Get fd without holding lock during blocking read
    let fd = {
        let state = state.lock().unwrap();
        match state.tun_devices.get(tun_name) {
            Some(info) => info.fd,
            None => {
                return HelperResponse {
                    success: false,
                    message: format!("TUN device {} not found", tun_name),
                    data: None,
                };
            }
        }
    }; // Lock released here

    // Set read timeout if specified
    if let Some(timeout) = timeout_ms {
        let tv = libc::timeval {
            tv_sec: (timeout / 1000) as i64,
            tv_usec: ((timeout % 1000) * 1000) as i32,
        };
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
    }

    // Read from utun - utun packets have a 4-byte header (AF family)
    let mut buf = vec![0u8; 65535];
    let n = unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };

    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut {
            // Don't log timeouts - they're expected and frequent
            return HelperResponse {
                success: true,
                message: "timeout".to_string(),
                data: None,
            };
        }
        log::error!("[HELPER] Read failed on {}: {}", tun_name, err);
        return HelperResponse {
            success: false,
            message: format!("Read failed: {}", err),
            data: None,
        };
    }

    if n < 4 {
        log::warn!("[HELPER] Packet too short: {} bytes", n);
        return HelperResponse {
            success: false,
            message: "Packet too short".to_string(),
            data: None,
        };
    }

    // Log successful read with packet details
    let packet = &buf[4..n as usize];
    if packet.len() >= 20 {
        let src_ip = format!("{}.{}.{}.{}", packet[12], packet[13], packet[14], packet[15]);
        let dst_ip = format!("{}.{}.{}.{}", packet[16], packet[17], packet[18], packet[19]);
        let proto = match packet[9] {
            1 => "ICMP",
            6 => "TCP",
            17 => "UDP",
            _ => "OTHER",
        };
        log::info!("[HELPER] TUN READ: {} bytes {} -> {} ({})", packet.len(), src_ip, dst_ip, proto);
    } else {
        log::info!("[HELPER] TUN READ: {} bytes (too short for IP header)", packet.len());
    }

    use base64::{Engine as _, engine::general_purpose};

    HelperResponse {
        success: true,
        message: "ok".to_string(),
        data: Some(serde_json::json!({
            "packet": general_purpose::STANDARD.encode(packet),
            "length": packet.len(),
        })),
    }
}

fn write_packet(state: &Arc<Mutex<HelperState>>, tun_name: &str, data: &[u8]) -> HelperResponse {
    let state = state.lock().unwrap();

    let tun_info = match state.tun_devices.get(tun_name) {
        Some(info) => info,
        None => {
            return HelperResponse {
                success: false,
                message: format!("TUN device {} not found", tun_name),
                data: None,
            };
        }
    };

    let fd = tun_info.fd;

    // Prepare packet with utun header
    // utun header: 4 bytes indicating address family in NETWORK BYTE ORDER (big-endian)
    // AF_INET = 2, AF_INET6 = 30 on macOS
    let mut packet = Vec::with_capacity(4 + data.len());

    // Detect IP version from first nibble
    let af = if !data.is_empty() && (data[0] >> 4) == 6 {
        libc::AF_INET6 as u32  // IPv6
    } else {
        libc::AF_INET as u32   // IPv4
    };

    // CRITICAL: macOS utun expects address family in network byte order (big-endian)
    packet.extend_from_slice(&af.to_be_bytes());
    packet.extend_from_slice(data);

    let n = unsafe {
        libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len())
    };

    if n < 0 {
        let err = std::io::Error::last_os_error();
        return HelperResponse {
            success: false,
            message: format!("Write failed: {}", err),
            data: None,
        };
    }

    HelperResponse {
        success: true,
        message: "ok".to_string(),
        data: Some(serde_json::json!({
            "written": n - 4,  // Subtract header bytes
        })),
    }
}
