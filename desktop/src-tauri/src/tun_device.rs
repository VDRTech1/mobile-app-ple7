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

            // Try to connect to helper and check version
            let mut client = HelperClient::new();
            let helper_responsive = client.ping().is_ok();
            let version_ok = if helper_responsive { client.version_matches() } else { false };

            if !helper_responsive || !version_ok {
                let needs_upgrade = helper_responsive && !version_ok;

                if needs_upgrade {
                    log::info!("Helper version mismatch - upgrading to {}", HelperClient::app_version());
                    // Force full reinstall for version upgrade
                    HelperClient::install_helper().await?;
                } else {
                    log::info!("Helper daemon not responding, checking installation status...");

                    // Clean up stale socket if it exists
                    if HelperClient::is_running() {
                        log::info!("Stale socket found, will reinstall helper");
                    }

                    if HelperClient::is_installed() {
                        // Helper files exist but not responding - try to restart first
                        log::info!("Helper installed but not responding, attempting to restart...");

                        // Unload first (ignore errors)
                        let _ = std::process::Command::new("launchctl")
                            .args(["unload", "/Library/LaunchDaemons/com.ple7.vpn.helper.plist"])
                            .output();

                        // Try to load
                        let _ = std::process::Command::new("launchctl")
                            .args(["load", "/Library/LaunchDaemons/com.ple7.vpn.helper.plist"])
                            .output();

                        // Wait for it to start
                        let mut started = false;
                        for _ in 0..10 {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let mut test_client = HelperClient::new();
                            if test_client.ping().is_ok() {
                                started = true;
                                break;
                            }
                        }

                        if !started {
                            // Restart failed, need full reinstall
                            log::info!("Restart failed, performing full reinstall...");
                            HelperClient::install_helper().await?;
                        }
                    } else {
                        // Helper not installed at all
                        log::info!("Helper daemon not installed, prompting for installation...");
                        HelperClient::install_helper().await?;
                    }
                }

                // Verify helper is now working
                let mut verify_client = HelperClient::new();
                if let Err(e) = verify_client.ping() {
                    return Err(format!("Helper installation failed - please try again or restart your Mac: {}", e));
                }
                client = verify_client;
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

            // Spawn cleanup in a separate thread with timeout to avoid blocking
            let name = self.name.clone();
            std::thread::spawn(move || {
                // Use short timeout (2s) to prevent hanging
                let timeout = std::time::Duration::from_secs(2);
                if let Ok(mut client) = std::panic::catch_unwind(|| HelperClient::new()) {
                    if client.connect_with_timeout(timeout).is_ok() {
                        let _ = client.restore_default_gateway();
                        let _ = client.destroy_tun(&name);
                        log::info!("TUN device {} cleaned up successfully", name);
                    } else {
                        log::warn!("Could not connect to helper for cleanup, TUN may persist");
                    }
                }
            });
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
        /// Original default gateway saved before VPN routes are added
        original_gateway: Option<String>,
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
            // CRITICAL: Capture original default gateway BEFORE any Wintun operations
            // Must be done first because creating adapter can leave stale routes
            let original_gateway = Self::get_original_gateway();
            if let Some(ref gw) = original_gateway {
                log::info!("Captured original default gateway: {}", gw);
            } else {
                log::warn!("Could not determine original default gateway");
            }

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
                original_gateway,
            })
        }

        /// Get the original default gateway before VPN routes are added
        /// Filters out VPN addresses (10.x.x.x) and picks the route with lowest metric
        fn get_original_gateway() -> Option<String> {
            use std::process::Command;
            use std::os::windows::process::CommandExt;

            const CREATE_NO_WINDOW: u32 = 0x08000000;

            let output = Command::new("route")
                .args(["print", "0.0.0.0"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            log::debug!("Route print output for gateway detection:\n{}", stdout);

            // Parse route output to find default gateway with lowest metric
            // Format: "0.0.0.0   0.0.0.0   gateway   interface   metric"
            let mut best_gateway: Option<(String, u32)> = None; // (gateway, metric)

            for line in stdout.lines() {
                let line = line.trim();
                // Look for lines starting with 0.0.0.0 (default route)
                if line.starts_with("0.0.0.0") && !line.contains("On-link") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    // parts[0] = 0.0.0.0, parts[1] = 0.0.0.0 (mask), parts[2] = gateway, parts[3] = interface, parts[4] = metric
                    if parts.len() >= 5 {
                        let gw = parts[2];
                        let metric: u32 = parts[4].parse().unwrap_or(9999);

                        // Validate it's a real IP
                        if let Ok(ip) = gw.parse::<std::net::Ipv4Addr>() {
                            let octets = ip.octets();

                            // Skip VPN addresses (10.x.x.x - common VPN range)
                            if octets[0] == 10 {
                                log::debug!("Skipping VPN address {} (metric {})", gw, metric);
                                continue;
                            }

                            // Skip loopback and link-local
                            if ip.is_loopback() || ip.is_link_local() {
                                log::debug!("Skipping loopback/link-local {} (metric {})", gw, metric);
                                continue;
                            }

                            log::debug!("Found gateway candidate: {} with metric {}", gw, metric);

                            // Keep the one with lowest metric
                            if best_gateway.is_none() || metric < best_gateway.as_ref().unwrap().1 {
                                best_gateway = Some((gw.to_string(), metric));
                            }
                        }
                    }
                }
            }

            if let Some((gw, metric)) = best_gateway {
                log::info!("Selected gateway {} with metric {}", gw, metric);
                Some(gw)
            } else {
                log::warn!("No valid non-VPN gateway found");
                None
            }
        }

        /// Get interface index by name using multiple methods for reliability
        fn get_interface_index(name: &str) -> Result<u32, String> {
            use std::process::Command;
            use std::os::windows::process::CommandExt;

            const CREATE_NO_WINDOW: u32 = 0x08000000;

            // Method 1: Try PowerShell (most reliable)
            log::info!("Getting interface index for '{}' via PowerShell...", name);
            let ps_output = Command::new("powershell")
                .args([
                    "-NoProfile", "-NonInteractive", "-Command",
                    &format!("(Get-NetAdapter -Name '{}' -ErrorAction SilentlyContinue).ifIndex", name)
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            if let Ok(output) = ps_output {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Ok(idx) = stdout.parse::<u32>() {
                    log::info!("PowerShell: interface index = {}", idx);
                    return Ok(idx);
                }
            }

            // Method 2: Try netsh interface show interface
            log::info!("Trying netsh method...");
            let output = Command::new("netsh")
                .args(["interface", "ipv4", "show", "interfaces"])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|e| format!("Failed to get interfaces: {}", e))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            log::debug!("netsh output:\n{}", stdout);

            // Parse output to find interface index by name
            // Format: "Idx     Met         MTU          State                Name"
            for line in stdout.lines() {
                // Case-insensitive match and handle partial matches
                if line.to_lowercase().contains(&name.to_lowercase()) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(idx_str) = parts.first() {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            log::info!("netsh: interface index = {}", idx);
                            return Ok(idx);
                        }
                    }
                }
            }

            // Method 3: Try route print to find interface by IP address
            log::info!("Trying route print method...");
            let route_output = Command::new("route")
                .args(["print"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            if let Ok(output) = route_output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Look for "10.100.0" in the interface list section
                // The format is: "idx  metric  name"
                for line in stdout.lines() {
                    if line.contains("10.100.0") || line.to_lowercase().contains(&name.to_lowercase()) {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        // Try to find a number that looks like an interface index
                        for part in parts.iter().take(3) {
                            if let Ok(idx) = part.parse::<u32>() {
                                if idx > 0 && idx < 1000 {
                                    log::info!("route print: interface index = {}", idx);
                                    return Ok(idx);
                                }
                            }
                        }
                    }
                }
            }

            // Default: return 0 and log warning
            log::warn!("Could not find interface index for '{}', routing may fail", name);
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
                use std::os::windows::process::CommandExt;

                const CREATE_NO_WINDOW: u32 = 0x08000000;
                let mask = Self::prefix_to_mask(prefix_len);

                log::info!("Adding route: {}/{} via {} IF {}", destination, prefix_len, address, if_index);

                // Use IF parameter and metric to specify the interface
                let output = Command::new("route")
                    .args([
                        "add",
                        &destination.to_string(),
                        "mask",
                        &mask.to_string(),
                        &address.to_string(),
                        "metric", "1",
                        "IF",
                        &if_index.to_string(),
                    ])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .map_err(|e| format!("Failed to execute route: {}", e))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    log::warn!("Route add warning: stdout={}, stderr={}", stdout, stderr);
                    // Don't fail on route add errors - the route might already exist
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
            let original_gw = self.original_gateway.clone();

            tokio::task::spawn_blocking(move || {
                use std::process::Command;
                use std::os::windows::process::CommandExt;

                const CREATE_NO_WINDOW: u32 = 0x08000000;

                // Add bypass route for excluded IP via the ORIGINAL default gateway
                // We use the saved gateway from TUN creation time, before any VPN routes were added
                if let (Some(ref ip), Some(ref gw)) = (&exclude, &original_gw) {
                    log::info!("Adding bypass route for {} via original gateway {}", ip, gw);
                    let output = Command::new("route")
                        .args(["add", ip, "mask", "255.255.255.255", gw])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output();

                    match output {
                        Ok(o) if o.status.success() => {
                            log::info!("Bypass route added successfully");
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            log::warn!("Bypass route may already exist: {}", stderr);
                        }
                        Err(e) => {
                            log::error!("Failed to add bypass route: {}", e);
                        }
                    }
                } else if exclude.is_some() {
                    log::warn!("Cannot add bypass route: original gateway not available");
                }

                // Add split routes through VPN interface with low metric to ensure priority
                log::info!("Adding default routes through VPN interface {} (gateway {})", if_index, address);

                // Use metric 1 to ensure VPN routes have highest priority
                // Delete any existing routes first to avoid conflicts
                let _ = Command::new("route")
                    .args(["delete", "0.0.0.0", "mask", "128.0.0.0"])
                    .creation_flags(0x08000000)
                    .output();
                let _ = Command::new("route")
                    .args(["delete", "128.0.0.0", "mask", "128.0.0.0"])
                    .creation_flags(0x08000000)
                    .output();

                let cmd1 = format!("route add 0.0.0.0 mask 128.0.0.0 {} metric 1 IF {}", address, if_index);
                log::info!("Executing: {}", cmd1);
                let output1 = Command::new("route")
                    .args(["add", "0.0.0.0", "mask", "128.0.0.0", &address.to_string(), "metric", "1", "IF", &if_index.to_string()])
                    .creation_flags(0x08000000)
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                if !output1.status.success() {
                    let stderr = String::from_utf8_lossy(&output1.stderr);
                    let stdout = String::from_utf8_lossy(&output1.stdout);
                    log::warn!("Route 0.0.0.0/1 add: stdout={}, stderr={}", stdout, stderr);
                } else {
                    log::info!("Route 0.0.0.0/1 added successfully");
                }

                let cmd2 = format!("route add 128.0.0.0 mask 128.0.0.0 {} metric 1 IF {}", address, if_index);
                log::info!("Executing: {}", cmd2);
                let output2 = Command::new("route")
                    .args(["add", "128.0.0.0", "mask", "128.0.0.0", &address.to_string(), "metric", "1", "IF", &if_index.to_string()])
                    .creation_flags(0x08000000)
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                if !output2.status.success() {
                    let stderr = String::from_utf8_lossy(&output2.stderr);
                    let stdout = String::from_utf8_lossy(&output2.stdout);
                    log::warn!("Route 128.0.0.0/1 add: stdout={}, stderr={}", stdout, stderr);
                } else {
                    log::info!("Route 128.0.0.0/1 added successfully");
                }

                // Print the routing table for debugging
                log::info!("Current VPN routes:");
                if let Ok(route_out) = Command::new("route")
                    .args(["print", "0.0.0.0"])
                    .creation_flags(0x08000000)
                    .output()
                {
                    for line in String::from_utf8_lossy(&route_out.stdout).lines() {
                        if line.contains("0.0.0.0") || line.contains("128.0.0.0") {
                            log::info!("  {}", line);
                        }
                    }
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
