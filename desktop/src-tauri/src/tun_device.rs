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
    pub async fn set_default_gateway(&self) -> Result<(), String> {
        self.inner.set_default_gateway().await
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

        pub async fn set_default_gateway(&self) -> Result<(), String> {
            let name = self.name.clone();

            tokio::task::spawn_blocking(move || {
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
// macOS TUN Implementation
// ============================================================================

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use tun::{Configuration, AbstractDevice};
    use std::process::Command;
    use std::io::{Read, Write};

    pub struct MacOsTun {
        device: Arc<Mutex<tun::Device>>,
        name: String,
        address: Ipv4Addr,
    }

    impl MacOsTun {
        /// Check if running with root privileges
        fn is_root() -> bool {
            unsafe { libc::geteuid() == 0 }
        }

        pub async fn create(
            _name: &str,
            address: Ipv4Addr,
            netmask: Ipv4Addr,
        ) -> Result<Self, String> {
            log::info!("macOS: Creating TUN device with address {}/{}", address, netmask);
            log::info!("macOS: Running as root: {}", Self::is_root());

            let mut config = Configuration::default();
            config
                .address(address)
                .netmask(netmask)
                .mtu(TUN_MTU as u16)
                .up();

            let device = tun::create(&config)
                .map_err(|e| {
                    let error_str = e.to_string();
                    if error_str.contains("Operation not permitted") || error_str.contains("os error 1") {
                        log::error!("macOS TUN device creation failed due to insufficient permissions");
                        log::error!("To use PLE7 VPN on macOS, the app needs elevated privileges.");
                        log::error!("Please run the app with: sudo /Applications/PLE7\\ VPN.app/Contents/MacOS/PLE7\\ VPN");
                        format!(
                            "Permission denied: macOS requires administrator privileges to create VPN tunnels. \
                            Please run the app with sudo or grant it administrator access in System Preferences > Security & Privacy."
                        )
                    } else {
                        format!("Failed to create TUN device: {}", e)
                    }
                })?;

            let actual_name = device.tun_name()
                .map_err(|e| format!("Failed to get device name: {}", e))?;

            log::info!("macOS TUN device created successfully: {}", actual_name);

            Ok(Self {
                device: Arc::new(Mutex::new(device)),
                name: actual_name,
                address,
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
            let address = self.address;

            tokio::task::spawn_blocking(move || {
                let output = Command::new("route")
                    .args([
                        "-n", "add", "-net",
                        &format!("{}/{}", destination, prefix_len),
                        &address.to_string(),
                    ])
                    .output()
                    .map_err(|e| format!("Failed to execute route: {}", e))?;

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

        pub async fn set_default_gateway(&self) -> Result<(), String> {
            let address = self.address;

            tokio::task::spawn_blocking(move || {
                Command::new("route")
                    .args(["-n", "add", "-net", "0.0.0.0/1", &address.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                Command::new("route")
                    .args(["-n", "add", "-net", "128.0.0.0/1", &address.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                Ok(())
            })
            .await
            .map_err(|e| format!("Default gateway task failed: {}", e))?
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
    }

    impl WindowsTun {
        pub async fn create(
            name: &str,
            address: Ipv4Addr,
            netmask: Ipv4Addr,
        ) -> Result<Self, String> {
            // Load wintun.dll
            let wintun = unsafe { wintun::load() }
                .map_err(|e| format!("Failed to load wintun.dll: {}. Make sure wintun.dll is in the same directory as the executable.", e))?;

            // Create or open adapter (returns Arc<Adapter>)
            let adapter = match Adapter::create(&wintun, WINTUN_POOL, name, None) {
                Ok(adapter) => adapter,
                Err(e) => {
                    log::warn!("Failed to create adapter, trying to open existing: {}", e);
                    Adapter::open(&wintun, name)
                        .map_err(|e| format!("Failed to open adapter: {}", e))?
                }
            };

            // Configure IP address using netsh
            Self::configure_address(&adapter, name, address, netmask)?;

            // Start session
            let session = adapter.start_session(RING_CAPACITY)
                .map_err(|e| format!("Failed to start Wintun session: {}", e))?;

            log::info!("Windows TUN device created: {}", name);

            Ok(Self {
                session: Arc::new(session),
                adapter, // Already Arc<Adapter>
                name: name.to_string(),
                address,
                netmask,
            })
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

            tokio::task::spawn_blocking(move || {
                use std::process::Command;

                let output = Command::new("route")
                    .args([
                        "add",
                        &destination.to_string(),
                        "mask",
                        &Self::prefix_to_mask(prefix_len).to_string(),
                        &address.to_string(),
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

        pub async fn set_default_gateway(&self) -> Result<(), String> {
            let address = self.address;

            tokio::task::spawn_blocking(move || {
                use std::process::Command;

                Command::new("route")
                    .args(["add", "0.0.0.0", "mask", "128.0.0.0", &address.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

                Command::new("route")
                    .args(["add", "128.0.0.0", "mask", "128.0.0.0", &address.to_string()])
                    .output()
                    .map_err(|e| format!("Failed to add route: {}", e))?;

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
