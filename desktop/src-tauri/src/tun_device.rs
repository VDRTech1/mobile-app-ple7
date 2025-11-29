//! TUN device management for all platforms
//! Creates virtual network interface for VPN traffic

use std::net::Ipv4Addr;
use std::sync::Arc;
use parking_lot::Mutex;

/// MTU for the TUN device
pub const TUN_MTU: usize = 1420; // WireGuard recommended MTU

/// Packet received from TUN device (outbound traffic)
#[derive(Debug)]
pub struct TunPacket {
    pub data: Vec<u8>,
}

/// Platform-independent TUN device handle
pub struct TunDevice {
    name: String,
    address: Ipv4Addr,
    netmask: Ipv4Addr,
    mtu: usize,
    #[cfg(target_os = "linux")]
    inner: LinuxTun,
    #[cfg(target_os = "macos")]
    inner: MacOsTun,
    #[cfg(target_os = "windows")]
    inner: WindowsTun,
}

impl TunDevice {
    /// Create a new TUN device with the given configuration
    pub async fn create(
        name: &str,
        address: Ipv4Addr,
        netmask: Ipv4Addr,
    ) -> Result<Self, String> {
        log::info!("Creating TUN device: {} with address {}/{}", name, address, netmask);

        #[cfg(target_os = "linux")]
        let inner = LinuxTun::create(name, address, netmask).await?;

        #[cfg(target_os = "macos")]
        let inner = MacOsTun::create(name, address, netmask).await?;

        #[cfg(target_os = "windows")]
        let inner = WindowsTun::create(name, address, netmask).await?;

        Ok(Self {
            name: name.to_string(),
            address,
            netmask,
            mtu: TUN_MTU,
            inner,
        })
    }

    /// Get the device name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the device address
    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    /// Read a packet from the TUN device (outbound traffic from apps)
    pub async fn read(&self) -> Result<TunPacket, String> {
        self.inner.read().await
    }

    /// Write a packet to the TUN device (inbound traffic to apps)
    pub async fn write(&self, packet: &[u8]) -> Result<(), String> {
        self.inner.write(packet).await
    }

    /// Add a route through this TUN device
    pub async fn add_route(&self, destination: Ipv4Addr, prefix_len: u8) -> Result<(), String> {
        self.inner.add_route(destination, prefix_len).await
    }

    /// Set the default gateway (for exit node functionality)
    /// exclude_ip: Optional IP to exclude from VPN routing (e.g., relay endpoint to prevent routing loop)
    pub async fn set_default_gateway(&self, exclude_ip: Option<&str>) -> Result<(), String> {
        self.inner.set_default_gateway(exclude_ip).await
    }
}

// ============================================================================
// Linux TUN Implementation
// ============================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use tun::{Configuration, AbstractDevice};
    use std::process::Command;
    use std::io::{Read, Write};

    pub struct LinuxTun {
        device: Arc<Mutex<tun::Device>>,
        name: String,
    }

    impl LinuxTun {
        pub async fn create(
            name: &str,
            address: Ipv4Addr,
            netmask: Ipv4Addr,
        ) -> Result<Self, String> {
            let mut config = Configuration::default();
            config
                .tun_name(name)
                .address(address)
                .netmask(netmask)
                .mtu(TUN_MTU as u16)
                .up();

            let device = tun::create(&config)
                .map_err(|e| format!("Failed to create TUN device: {}", e))?;

            let actual_name = device.tun_name()
                .map_err(|e| format!("Failed to get device name: {}", e))?;

            log::info!("Linux TUN device created: {}", actual_name);

            Ok(Self {
                device: Arc::new(Mutex::new(device)),
                name: actual_name,
            })
        }

        pub async fn read(&self) -> Result<TunPacket, String> {
            let device = self.device.clone();

            tokio::task::spawn_blocking(move || {
                let mut device = device.lock();
                let mut buf = vec![0u8; TUN_MTU + 100];
                match device.read(&mut buf) {
                    Ok(n) => Ok(TunPacket {
                        data: buf[..n].to_vec(),
                    }),
                    Err(e) => Err(format!("Failed to read from TUN: {}", e)),
                }
            })
            .await
            .map_err(|e| format!("Read task failed: {}", e))?
        }

        pub async fn write(&self, packet: &[u8]) -> Result<(), String> {
            let device = self.device.clone();
            let packet = packet.to_vec();

            tokio::task::spawn_blocking(move || {
                let mut device = device.lock();
                device.write_all(&packet)
                    .map_err(|e| format!("Failed to write to TUN: {}", e))
            })
            .await
            .map_err(|e| format!("Write task failed: {}", e))?
        }

        pub async fn add_route(&self, destination: Ipv4Addr, prefix_len: u8) -> Result<(), String> {
            let name = self.name.clone();

            tokio::task::spawn_blocking(move || {
                let output = Command::new("ip")
                    .args([
                        "route", "add",
                        &format!("{}/{}", destination, prefix_len),
                        "dev", &name,
                    ])
                    .output()
                    .map_err(|e| format!("Failed to execute ip route: {}", e))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if !stderr.contains("File exists") {
                        return Err(format!("Failed to add route: {}", stderr));
                    }
                }
                Ok(())
            })
            .await
            .map_err(|e| format!("Route task failed: {}", e))?
        }

        pub async fn set_default_gateway(&self, exclude_ip: Option<&str>) -> Result<(), String> {
            let name = self.name.clone();
            let exclude = exclude_ip.map(|s| s.to_string());

            tokio::task::spawn_blocking(move || {
                // Get original default gateway for bypass route
                if let Some(ref ip) = exclude {
                    // Get current default gateway
                    let output = Command::new("ip")
                        .args(["route", "show", "default"])
                        .output()
                        .map_err(|e| format!("Failed to get default route: {}", e))?;

                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // Parse "default via X.X.X.X dev ..."
                    if let Some(gw) = stdout.split_whitespace().skip_while(|&s| s != "via").nth(1) {
                        // Add bypass route for relay endpoint
                        log::info!("Adding bypass route for {} via {}", ip, gw);
                        Command::new("ip")
                            .args(["route", "add", ip, "via", gw])
                            .output()
                            .ok(); // Ignore errors (may already exist)
                    }
                }

                // Add split routes for default gateway
                Command::new("ip")
                    .args(["route", "add", "0.0.0.0/1", "dev", &name])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                Command::new("ip")
                    .args(["route", "add", "128.0.0.0/1", "dev", &name])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                Ok(())
            })
            .await
            .map_err(|e| format!("Default gateway task failed: {}", e))?
        }
    }
}

#[cfg(target_os = "linux")]
use linux::LinuxTun;

// ============================================================================
// macOS TUN Implementation (via privileged helper daemon)
// ============================================================================

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::helper_client::HelperClient;

    pub struct MacOsTun {
        name: String,
        address: Ipv4Addr,
    }

    impl MacOsTun {
        pub async fn create(
            name: &str,
            address: Ipv4Addr,
            netmask: Ipv4Addr,
        ) -> Result<Self, String> {
            log::info!("macOS: Creating TUN device via helper daemon");
            log::info!("macOS: Address: {}, Netmask: {}", address, netmask);

            // Check if helper is running
            if !HelperClient::is_running() {
                if !HelperClient::is_installed() {
                    log::info!("Helper daemon not installed, prompting for installation...");
                    HelperClient::install_helper().await?;
                } else {
                    // Helper is installed but not running - try to start it
                    log::info!("Helper installed but not running, attempting to start...");
                    let output = std::process::Command::new("launchctl")
                        .args(["load", "/Library/LaunchDaemons/com.ple7.vpn.helper.plist"])
                        .output()
                        .map_err(|e| format!("Failed to start helper: {}", e))?;

                    if !output.status.success() {
                        return Err("Failed to start helper daemon. Please reinstall the app.".to_string());
                    }

                    // Wait for it to start
                    for _ in 0..10 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if HelperClient::is_running() {
                            break;
                        }
                    }

                    if !HelperClient::is_running() {
                        return Err("Helper daemon failed to start".to_string());
                    }
                }
            }

            // Connect to helper and create TUN
            let mut client = HelperClient::new();

            // Ping to verify connection
            if let Err(e) = client.ping() {
                return Err(format!("Helper daemon not responding: {}", e));
            }

            log::info!("Connected to helper daemon");

            // Create TUN device via helper
            let response = client.create_tun(
                name,
                &address.to_string(),
                &netmask.to_string(),
            )?;

            if !response.success {
                return Err(format!("Helper failed to create TUN: {}", response.message));
            }

            let actual_name = response.data
                .as_ref()
                .and_then(|d| d.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or(name)
                .to_string();

            log::info!("macOS TUN device created via helper: {}", actual_name);

            // Note: Reading/writing to the TUN is done via the utun interface
            // The helper keeps the device alive, we use the interface name to
            // interact with it via BPF or by opening the utun directly

            Ok(Self {
                name: actual_name,
                address,
            })
        }

        pub async fn read(&self) -> Result<TunPacket, String> {
            let name = self.name.clone();

            tokio::task::spawn_blocking(move || {
                let mut client = HelperClient::new();

                // Use 100ms timeout to prevent blocking forever
                match client.read_packet(&name, Some(100)) {
                    Ok(Some(data)) => Ok(TunPacket { data }),
                    Ok(None) => Err("timeout".to_string()), // Timeout, caller should retry
                    Err(e) => Err(format!("Failed to read from TUN: {}", e)),
                }
            })
            .await
            .map_err(|e| format!("Read task failed: {}", e))?
        }

        pub async fn write(&self, packet: &[u8]) -> Result<(), String> {
            let name = self.name.clone();
            let packet = packet.to_vec();

            tokio::task::spawn_blocking(move || {
                let mut client = HelperClient::new();
                client.write_packet(&name, &packet)
            })
            .await
            .map_err(|e| format!("Write task failed: {}", e))?
        }

        pub async fn add_route(&self, destination: Ipv4Addr, prefix_len: u8) -> Result<(), String> {
            let address = self.address.to_string();
            let dest = destination.to_string();

            log::info!("Adding route {}/{} via helper", dest, prefix_len);

            let mut client = HelperClient::new();
            let response = client.add_route(&dest, prefix_len, &address)?;

            if response.success {
                Ok(())
            } else {
                Err(format!("Failed to add route: {}", response.message))
            }
        }

        pub async fn set_default_gateway(&self, exclude_ip: Option<&str>) -> Result<(), String> {
            let address = self.address.to_string();

            log::info!("Setting default gateway to {} via helper", address);
            if let Some(ip) = exclude_ip {
                log::info!("Excluding {} from VPN routing (bypass route)", ip);
            }

            let mut client = HelperClient::new();
            let response = client.set_default_gateway(&address, exclude_ip)?;

            if response.success {
                Ok(())
            } else {
                Err(format!("Failed to set default gateway: {}", response.message))
            }
        }
    }

    impl Drop for MacOsTun {
        fn drop(&mut self) {
            log::info!("Cleaning up TUN device: {}", self.name);

            // Tell helper to destroy the TUN and restore routing
            if let Ok(mut client) = std::panic::catch_unwind(|| HelperClient::new()) {
                let _ = client.restore_default_gateway();
                let _ = client.destroy_tun(&self.name);
            }
        }
    }
}

#[cfg(target_os = "macos")]
use macos::MacOsTun;

// ============================================================================
// Windows TUN Implementation (using Wintun)
// ============================================================================

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use wintun::{Adapter, Session};
    use std::sync::Arc;

    const WINTUN_POOL: &str = "PLE7";
    const RING_CAPACITY: u32 = 0x400000; // 4MB ring buffer

    pub struct WindowsTun {
        session: Arc<Session>,
        #[allow(dead_code)]
        adapter: Arc<Adapter>,
        name: String,
        address: Ipv4Addr,
        #[allow(dead_code)]
        netmask: Ipv4Addr,
        interface_index: u32,
    }

    impl WindowsTun {
        /// Load wintun.dll from multiple possible locations
        fn load_wintun() -> Result<wintun::Wintun, String> {
            // Try to get the executable directory
            if let Ok(exe_path) = std::env::current_exe() {
                log::info!("Executable path: {:?}", exe_path);

                if let Some(exe_dir) = exe_path.parent() {
                    // Locations to try in order of preference
                    let locations = vec![
                        exe_dir.join("wintun.dll"),
                        exe_dir.join("resources").join("wintun.dll"),
                        exe_dir.join("_up_").join("wintun.dll"),
                        exe_dir.parent().map(|p| p.join("wintun.dll")).unwrap_or_default(),
                        exe_dir.parent().map(|p| p.join("resources").join("wintun.dll")).unwrap_or_default(),
                    ];

                    for dll_path in &locations {
                        if dll_path.as_os_str().is_empty() {
                            continue;
                        }
                        log::info!("Looking for wintun.dll at: {:?}", dll_path);
                        if dll_path.exists() {
                            log::info!("Found wintun.dll at: {:?}", dll_path);
                            return unsafe { wintun::load_from_path(dll_path) }
                                .map_err(|e| format!("Failed to load wintun.dll from {:?}: {}", dll_path, e));
                        }
                    }

                    log::warn!("wintun.dll not found in any expected location");
                }
            }

            // Fall back to default loading (current directory, system directories)
            log::info!("Trying default wintun.dll load locations (system PATH)");
            unsafe { wintun::load() }
                .map_err(|e| format!("Failed to load wintun.dll: {}. Please ensure wintun.dll is in the app directory or download from https://www.wintun.net", e))
        }

        pub async fn create(
            name: &str,
            address: Ipv4Addr,
            netmask: Ipv4Addr,
        ) -> Result<Self, String> {
            // Find wintun.dll - check multiple locations
            let wintun = Self::load_wintun()?;

            // First, try to delete any stale adapter from previous session
            log::info!("Checking for stale adapter '{}'...", name);
            match Adapter::open(&wintun, name) {
                Ok(old_adapter) => {
                    log::info!("Found existing adapter, dropping it first...");
                    drop(old_adapter);
                    // Give Windows time to clean up
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(_) => {
                    log::info!("No existing adapter found");
                }
            }

            // Create or open adapter (returns Arc<Adapter>)
            log::info!("Creating new Wintun adapter '{}' in pool '{}'...", name, WINTUN_POOL);
            let adapter = match Adapter::create(&wintun, WINTUN_POOL, name, None) {
                Ok(adapter) => {
                    log::info!("Wintun adapter created successfully");
                    adapter
                }
                Err(e) => {
                    log::warn!("Failed to create adapter: {}. Trying to open existing...", e);
                    // If create fails, try to open existing (might be from a previous session)
                    match Adapter::open(&wintun, name) {
                        Ok(adapter) => {
                            log::info!("Opened existing Wintun adapter");
                            adapter
                        }
                        Err(e2) => {
                            return Err(format!(
                                "Failed to create or open Wintun adapter. \
                                Create error: {}. Open error: {}. \
                                Please ensure you're running as Administrator and no other VPN is using Wintun.",
                                e, e2
                            ));
                        }
                    }
                }
            };

            // Configure IP address using netsh
            Self::configure_address(&adapter, name, address, netmask)?;

            // Get interface index for routing
            let interface_index = Self::get_interface_index(name)?;
            log::info!("Wintun adapter interface index: {}", interface_index);

            // Start session
            let session = adapter.start_session(RING_CAPACITY)
                .map_err(|e| format!("Failed to start Wintun session: {}", e))?;

            log::info!("Windows TUN device created: {} (IF {})", name, interface_index);

            Ok(Self {
                session: Arc::new(session),
                adapter, // Already Arc<Adapter>
                name: name.to_string(),
                address,
                netmask,
                interface_index,
            })
        }

        /// Get interface index by name using netsh
        fn get_interface_index(name: &str) -> Result<u32, String> {
            use std::process::Command;

            let output = Command::new("netsh")
                .args(["interface", "ipv4", "show", "interfaces"])
                .output()
                .map_err(|e| format!("Failed to get interfaces: {}", e))?;

            let stdout = String::from_utf8_lossy(&output.stdout);

            // Parse output to find interface index by name
            // Format: "Idx     Met         MTU          State                Name"
            for line in stdout.lines() {
                if line.contains(name) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(idx_str) = parts.first() {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            return Ok(idx);
                        }
                    }
                }
            }

            // Default to a reasonable value if not found
            log::warn!("Could not find interface index for {}, using default", name);
            Ok(0)
        }

        fn configure_address(_adapter: &Adapter, name: &str, address: Ipv4Addr, netmask: Ipv4Addr) -> Result<(), String> {
            use std::process::Command;

            // Use netsh to set IP address
            let output = Command::new("netsh")
                .args([
                    "interface", "ip", "set", "address",
                    &format!("name={}", name),
                    "static",
                    &address.to_string(),
                    &netmask.to_string(),
                ])
                .output()
                .map_err(|e| format!("Failed to execute netsh: {}", e))?;

            if !output.status.success() {
                log::warn!("netsh set address failed, trying alternative method");
            }

            Ok(())
        }

        pub async fn read(&self) -> Result<TunPacket, String> {
            let session = self.session.clone();

            tokio::task::spawn_blocking(move || {
                match session.receive_blocking() {
                    Ok(packet) => Ok(TunPacket {
                        data: packet.bytes().to_vec(),
                    }),
                    Err(e) => Err(format!("Failed to read from Wintun: {}", e)),
                }
            })
            .await
            .map_err(|e| format!("Read task failed: {}", e))?
        }

        pub async fn write(&self, packet: &[u8]) -> Result<(), String> {
            let session = self.session.clone();
            let packet_data = packet.to_vec();

            tokio::task::spawn_blocking(move || {
                let mut write_packet = session.allocate_send_packet(packet_data.len() as u16)
                    .map_err(|e| format!("Failed to allocate packet: {}", e))?;

                write_packet.bytes_mut().copy_from_slice(&packet_data);
                session.send_packet(write_packet);
                Ok(())
            })
            .await
            .map_err(|e| format!("Write task failed: {}", e))?
        }

        pub async fn add_route(&self, destination: Ipv4Addr, prefix_len: u8) -> Result<(), String> {
            let address = self.address;
            let if_index = self.interface_index;

            tokio::task::spawn_blocking(move || {
                use std::process::Command;

                // Use IF parameter to specify the interface
                let output = Command::new("route")
                    .args([
                        "add",
                        &destination.to_string(),
                        "mask",
                        &Self::prefix_to_mask(prefix_len).to_string(),
                        &address.to_string(),
                        "IF",
                        &if_index.to_string(),
                    ])
                    .output()
                    .map_err(|e| format!("Failed to execute route: {}", e))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("Failed to add route: {}", stderr));
                }

                Ok(())
            })
            .await
            .map_err(|e| format!("Route task failed: {}", e))?
        }

        pub async fn set_default_gateway(&self, exclude_ip: Option<&str>) -> Result<(), String> {
            let address = self.address;
            let exclude = exclude_ip.map(|s| s.to_string());
            let if_index = self.interface_index;

            tokio::task::spawn_blocking(move || {
                use std::process::Command;

                // Add bypass route for excluded IP via default gateway (NOT through VPN interface)
                if let Some(ref ip) = exclude {
                    // Get current default gateway using route print
                    let output = Command::new("route")
                        .args(["print", "0.0.0.0"])
                        .output()
                        .map_err(|e| format!("Failed to get routes: {}", e))?;

                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // Parse route output to find default gateway (look for 0.0.0.0 ... gateway)
                    for line in stdout.lines() {
                        if line.contains("0.0.0.0") && !line.contains("On-link") {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 3 {
                                let gw = parts[2];
                                if gw.parse::<std::net::Ipv4Addr>().is_ok() {
                                    log::info!("Adding bypass route for {} via {}", ip, gw);
                                    Command::new("route")
                                        .args(["add", ip, "mask", "255.255.255.255", gw])
                                        .output()
                                        .ok(); // Ignore errors (may already exist)
                                    break;
                                }
                            }
                        }
                    }
                }

                // Add split routes through VPN interface (use IF to specify interface)
                log::info!("Adding default routes through VPN interface {}", if_index);

                let output1 = Command::new("route")
                    .args(["add", "0.0.0.0", "mask", "128.0.0.0", &address.to_string(), "IF", &if_index.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                if !output1.status.success() {
                    log::warn!("Route 0.0.0.0/1 add warning: {}", String::from_utf8_lossy(&output1.stderr));
                }

                let output2 = Command::new("route")
                    .args(["add", "128.0.0.0", "mask", "128.0.0.0", &address.to_string(), "IF", &if_index.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                if !output2.status.success() {
                    log::warn!("Route 128.0.0.0/1 add warning: {}", String::from_utf8_lossy(&output2.stderr));
                }

                Ok(())
            })
            .await
            .map_err(|e| format!("Default gateway task failed: {}", e))?
        }

        fn prefix_to_mask(prefix_len: u8) -> Ipv4Addr {
            let mask: u32 = if prefix_len == 0 {
                0
            } else {
                !0u32 << (32 - prefix_len)
            };
            Ipv4Addr::from(mask.to_be_bytes())
        }
    }
}

#[cfg(target_os = "windows")]
use windows::WindowsTun;
