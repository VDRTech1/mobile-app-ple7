//! WireGuard tunnel implementation using boringtun
//! Handles encryption/decryption of VPN traffic

use std::net::{SocketAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;

use boringtun::noise::{Tunn, TunnResult, handshake::parse_handshake_anon};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use base64::Engine as _;

use crate::tun_device::{TunDevice, TUN_MTU};
use crate::stun::AsyncStunClient;

/// WireGuard default port range
const WG_PORT_START: u16 = 51820;
const WG_PORT_END: u16 = 51920;

/// Keepalive interval
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);

/// Handshake timeout
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Peer configuration
#[derive(Debug, Clone)]
pub struct WgPeer {
    pub public_key: [u8; 32],
    pub endpoint: Option<SocketAddr>,
    pub allowed_ips: Vec<(Ipv4Addr, u8)>, // (address, prefix_len)
    pub persistent_keepalive: Option<u16>,
    pub preshared_key: Option<[u8; 32]>,
}

/// WireGuard tunnel configuration
#[derive(Debug, Clone)]
pub struct WgConfig {
    pub private_key: [u8; 32],
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub dns: Option<Ipv4Addr>,
    pub peers: Vec<WgPeer>,
    pub listen_port: Option<u16>,
}

/// Active peer state
struct PeerState {
    tunnel: Tunn,
    endpoint: Option<SocketAddr>,
    last_handshake: Option<Instant>,
    tx_bytes: u64,
    rx_bytes: u64,
}

/// WireGuard tunnel manager
pub struct WgTunnel {
    config: WgConfig,
    private_key: x25519_dalek::StaticSecret,
    public_key: x25519_dalek::PublicKey,
    socket: Arc<UdpSocket>,
    tun_device: Arc<TunDevice>,
    peers: Arc<RwLock<HashMap<[u8; 32], PeerState>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    public_endpoint: Arc<RwLock<Option<SocketAddr>>>,
}

impl WgTunnel {
    /// Create a new WireGuard tunnel
    pub async fn new(config: WgConfig) -> Result<Self, String> {
        // Parse private key
        let private_key = x25519_dalek::StaticSecret::from(config.private_key);
        let public_key = x25519_dalek::PublicKey::from(&private_key);

        log::info!("Creating WireGuard tunnel with public key: {}",
            base64::engine::general_purpose::STANDARD.encode(public_key.as_bytes()));

        // Find available port
        let listen_port = config.listen_port.unwrap_or_else(|| Self::find_available_port());
        let bind_addr = format!("0.0.0.0:{}", listen_port);

        let socket = UdpSocket::bind(&bind_addr)
            .map_err(|e| format!("Failed to bind UDP socket on {}: {}", bind_addr, e))?;

        socket.set_nonblocking(true)
            .map_err(|e| format!("Failed to set socket non-blocking: {}", e))?;

        log::info!("WireGuard listening on port {}", listen_port);

        // Discover public endpoint via STUN
        let stun_client = AsyncStunClient::new();
        let public_endpoint = match stun_client.discover_for_port(listen_port).await {
            Ok(result) => {
                log::info!("Public endpoint discovered: {}", result.public_addr);
                Some(result.public_addr)
            }
            Err(e) => {
                log::warn!("STUN discovery failed: {}. Direct P2P may not work.", e);
                None
            }
        };

        // Create TUN device
        let tun_device = TunDevice::create("ple7", config.address, config.netmask).await?;

        // Initialize peers
        let mut peers_map = HashMap::new();
        for peer in &config.peers {
            let peer_public_key = x25519_dalek::PublicKey::from(peer.public_key);

            let tunnel = Tunn::new(
                private_key.clone(),
                peer_public_key,
                peer.preshared_key,
                peer.persistent_keepalive,
                0,
                None,
            ).map_err(|e| format!("Failed to create tunnel for peer: {}", e))?;

            peers_map.insert(peer.public_key, PeerState {
                tunnel,
                endpoint: peer.endpoint,
                last_handshake: None,
                tx_bytes: 0,
                rx_bytes: 0,
            });
        }

        Ok(Self {
            config,
            private_key,
            public_key,
            socket: Arc::new(socket),
            tun_device: Arc::new(tun_device),
            peers: Arc::new(RwLock::new(peers_map)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            public_endpoint: Arc::new(RwLock::new(public_endpoint)),
        })
    }

    fn find_available_port() -> u16 {
        for port in WG_PORT_START..=WG_PORT_END {
            if UdpSocket::bind(format!("0.0.0.0:{}", port)).is_ok() {
                return port;
            }
        }
        // Fallback to random port
        0
    }

    /// Start the tunnel
    pub async fn start(&self) -> Result<(), String> {
        use std::sync::atomic::Ordering;

        if self.running.load(Ordering::SeqCst) {
            return Err("Tunnel already running".to_string());
        }

        self.running.store(true, Ordering::SeqCst);

        // Add routes for allowed IPs
        for peer in &self.config.peers {
            for (addr, prefix) in &peer.allowed_ips {
                if let Err(e) = self.tun_device.add_route(*addr, *prefix).await {
                    log::warn!("Failed to add route {}/{}: {}", addr, prefix, e);
                }
            }
        }

        // Spawn packet handling tasks
        let socket_read = self.socket.clone();
        let socket_write = self.socket.clone();
        let tun = self.tun_device.clone();
        let peers = self.peers.clone();
        let running = self.running.clone();
        let private_key = self.private_key.clone();

        // Task 1: Read from UDP socket (incoming WireGuard packets)
        let peers_udp = peers.clone();
        let tun_udp = tun.clone();
        let running_udp = running.clone();
        tokio::spawn(async move {
            Self::udp_read_loop(socket_read, peers_udp, tun_udp, running_udp).await;
        });

        // Task 2: Read from TUN device (outgoing packets from apps)
        let peers_tun = peers.clone();
        let running_tun = running.clone();
        tokio::spawn(async move {
            Self::tun_read_loop(tun, socket_write, peers_tun, running_tun).await;
        });

        // Task 3: Periodic keepalive and handshake
        let peers_keepalive = peers.clone();
        let socket_keepalive = self.socket.clone();
        let running_keepalive = running.clone();
        tokio::spawn(async move {
            Self::keepalive_loop(socket_keepalive, peers_keepalive, running_keepalive).await;
        });

        // Initiate handshakes with all peers
        self.initiate_handshakes().await?;

        log::info!("WireGuard tunnel started");
        Ok(())
    }

    /// Initiate handshakes with all peers
    async fn initiate_handshakes(&self) -> Result<(), String> {
        let mut peers = self.peers.write();

        for (pub_key, peer_state) in peers.iter_mut() {
            if let Some(endpoint) = peer_state.endpoint {
                let mut dst = [0u8; 2048];
                match peer_state.tunnel.format_handshake_initiation(&mut dst, false) {
                    TunnResult::WriteToNetwork(data) => {
                        if let Err(e) = self.socket.send_to(data, endpoint) {
                            log::warn!("Failed to send handshake to {:?}: {}", endpoint, e);
                        } else {
                            log::info!("Sent handshake initiation to {}", endpoint);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    /// Stop the tunnel
    pub async fn stop(&self) -> Result<(), String> {
        use std::sync::atomic::Ordering;

        self.running.store(false, Ordering::SeqCst);
        log::info!("WireGuard tunnel stopped");
        Ok(())
    }

    /// UDP read loop - handles incoming WireGuard packets
    async fn udp_read_loop(
        socket: Arc<UdpSocket>,
        peers: Arc<RwLock<HashMap<[u8; 32], PeerState>>>,
        tun: Arc<TunDevice>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }

            // Read from UDP socket
            let socket_clone = socket.clone();
            let result = tokio::task::spawn_blocking(move || {
                socket_clone.set_read_timeout(Some(Duration::from_millis(100))).ok();
                let mut buf = [0u8; 65535];
                socket_clone.recv_from(&mut buf).map(|(n, addr)| (buf, n, addr))
            }).await;

            let (buf, len, src_addr) = match result {
                Ok(Ok(data)) => data,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Ok(Err(e)) => {
                    log::error!("UDP recv error: {}", e);
                    continue;
                }
                Err(e) => {
                    log::error!("UDP recv task error: {}", e);
                    continue;
                }
            };

            // Process packet - collect data to write, then drop lock before async I/O
            let write_data: Option<Vec<u8>> = {
                let mut peers = peers.write();

                let mut result_data = None;
                for (_pub_key, peer_state) in peers.iter_mut() {
                    let mut dst = [0u8; 65535];

                    match peer_state.tunnel.decapsulate(None, &buf[..len], &mut dst) {
                        TunnResult::WriteToTunnelV4(data, _) => {
                            peer_state.rx_bytes += data.len() as u64;
                            peer_state.endpoint = Some(src_addr);
                            result_data = Some(data.to_vec());
                            break;
                        }
                        TunnResult::WriteToTunnelV6(data, _) => {
                            peer_state.rx_bytes += data.len() as u64;
                            peer_state.endpoint = Some(src_addr);
                            result_data = Some(data.to_vec());
                            break;
                        }
                        TunnResult::WriteToNetwork(data) => {
                            if let Err(e) = socket.send_to(data, src_addr) {
                                log::error!("Failed to send response: {}", e);
                            }
                        }
                        TunnResult::Done => {
                            peer_state.last_handshake = Some(Instant::now());
                        }
                        TunnResult::Err(_e) => {
                            continue;
                        }
                    }
                }
                result_data
            }; // Lock dropped here

            // Now do async I/O outside the lock
            if let Some(data) = write_data {
                if let Err(e) = tun.write(&data).await {
                    log::error!("Failed to write to TUN: {}", e);
                }
            }
        }
    }

    /// TUN read loop - handles outgoing packets from applications
    async fn tun_read_loop(
        tun: Arc<TunDevice>,
        socket: Arc<UdpSocket>,
        peers: Arc<RwLock<HashMap<[u8; 32], PeerState>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }

            // Read packet from TUN device
            let packet = match tun.read().await {
                Ok(p) => p,
                Err(e) => {
                    if running.load(Ordering::SeqCst) {
                        log::error!("TUN read error: {}", e);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };

            // Determine destination from IP header
            if packet.data.len() < 20 {
                continue; // Invalid IP packet
            }

            let dst_ip = Ipv4Addr::new(
                packet.data[16],
                packet.data[17],
                packet.data[18],
                packet.data[19],
            );

            // Find the peer that handles this destination
            let mut peers = peers.write();

            for (pub_key, peer_state) in peers.iter_mut() {
                // Check if destination matches any allowed IP
                let matches = peer_state.endpoint.is_some(); // Simplified - send to first peer with endpoint

                if matches {
                    if let Some(endpoint) = peer_state.endpoint {
                        let mut dst = [0u8; 65535];

                        match peer_state.tunnel.encapsulate(&packet.data, &mut dst) {
                            TunnResult::WriteToNetwork(data) => {
                                peer_state.tx_bytes += data.len() as u64;
                                if let Err(e) = socket.send_to(data, endpoint) {
                                    log::error!("Failed to send encrypted packet: {}", e);
                                }
                            }
                            TunnResult::Err(e) => {
                                log::warn!("Encapsulation error: {:?}", e);
                            }
                            _ => {}
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Keepalive loop - sends periodic keepalives and maintains handshakes
    async fn keepalive_loop(
        socket: Arc<UdpSocket>,
        peers: Arc<RwLock<HashMap<[u8; 32], PeerState>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);

        loop {
            interval.tick().await;

            if !running.load(Ordering::SeqCst) {
                break;
            }

            let mut peers = peers.write();

            for (pub_key, peer_state) in peers.iter_mut() {
                if let Some(endpoint) = peer_state.endpoint {
                    let mut dst = [0u8; 2048];

                    // Check if we need to send keepalive or re-handshake
                    match peer_state.tunnel.update_timers(&mut dst) {
                        TunnResult::WriteToNetwork(data) => {
                            if let Err(e) = socket.send_to(data, endpoint) {
                                log::warn!("Failed to send keepalive: {}", e);
                            }
                        }
                        TunnResult::Err(e) => {
                            log::debug!("Timer update: {:?}", e);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Get public endpoint (for reporting to control plane)
    pub fn public_endpoint(&self) -> Option<SocketAddr> {
        *self.public_endpoint.read()
    }

    /// Get tunnel statistics
    pub fn get_stats(&self) -> Vec<(String, u64, u64)> {
        let peers = self.peers.read();
        peers.iter().map(|(key, state)| {
            let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);
            (key_b64, state.tx_bytes, state.rx_bytes)
        }).collect()
    }

    /// Update peer endpoint (for NAT traversal)
    pub fn update_peer_endpoint(&self, public_key: &[u8; 32], endpoint: SocketAddr) {
        let mut peers = self.peers.write();
        if let Some(peer) = peers.get_mut(public_key) {
            log::info!("Updating peer endpoint: {:?} -> {}", public_key, endpoint);
            peer.endpoint = Some(endpoint);
        }
    }

    /// Set default gateway to route all traffic through VPN
    pub async fn set_default_gateway(&self) -> Result<(), String> {
        log::info!("Setting default gateway through VPN tunnel");
        self.tun_device.set_default_gateway().await
    }
}

/// Parse WireGuard config string into WgConfig
pub fn parse_wg_config(config_str: &str) -> Result<WgConfig, String> {
    let mut private_key = None;
    let mut address = None;
    let mut netmask = Ipv4Addr::new(255, 255, 255, 0);
    let mut dns = None;
    let mut listen_port = None;
    let mut peers = Vec::new();
    let mut current_peer: Option<WgPeer> = None;

    for line in config_str.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line == "[Interface]" {
            continue;
        }

        if line == "[Peer]" {
            if let Some(peer) = current_peer.take() {
                peers.push(peer);
            }
            current_peer = Some(WgPeer {
                public_key: [0u8; 32],
                endpoint: None,
                allowed_ips: Vec::new(),
                persistent_keepalive: None,
                preshared_key: None,
            });
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "PrivateKey" => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(value)
                        .map_err(|e| format!("Invalid private key: {}", e))?;
                    let arr: [u8; 32] = bytes.try_into()
                        .map_err(|_| "Private key must be 32 bytes")?;
                    private_key = Some(arr);
                }
                "Address" => {
                    // Parse address with optional CIDR
                    let (addr_str, prefix) = if value.contains('/') {
                        let parts: Vec<&str> = value.split('/').collect();
                        (parts[0], parts.get(1).and_then(|p| p.parse::<u8>().ok()))
                    } else {
                        (value, None)
                    };
                    address = Some(addr_str.parse::<Ipv4Addr>()
                        .map_err(|e| format!("Invalid address: {}", e))?);
                    if let Some(prefix) = prefix {
                        netmask = prefix_to_netmask(prefix);
                    }
                }
                "DNS" => {
                    dns = Some(value.parse::<Ipv4Addr>()
                        .map_err(|e| format!("Invalid DNS: {}", e))?);
                }
                "ListenPort" => {
                    listen_port = Some(value.parse::<u16>()
                        .map_err(|e| format!("Invalid listen port: {}", e))?);
                }
                "PublicKey" => {
                    if let Some(ref mut peer) = current_peer {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(value)
                            .map_err(|e| format!("Invalid public key: {}", e))?;
                        peer.public_key = bytes.try_into()
                            .map_err(|_| "Public key must be 32 bytes")?;
                    }
                }
                "Endpoint" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.endpoint = Some(value.parse::<SocketAddr>()
                            .map_err(|e| format!("Invalid endpoint: {}", e))?);
                    }
                }
                "AllowedIPs" => {
                    if let Some(ref mut peer) = current_peer {
                        for ip_range in value.split(',') {
                            let ip_range = ip_range.trim();
                            // Skip IPv6 addresses (contain colons)
                            if ip_range.contains(':') {
                                continue;
                            }
                            let (addr, prefix) = if ip_range.contains('/') {
                                let parts: Vec<&str> = ip_range.split('/').collect();
                                let addr = match parts[0].parse::<Ipv4Addr>() {
                                    Ok(a) => a,
                                    Err(_) => continue, // Skip invalid addresses
                                };
                                let prefix = parts[1].parse::<u8>().unwrap_or(32);
                                (addr, prefix)
                            } else {
                                match ip_range.parse::<Ipv4Addr>() {
                                    Ok(addr) => (addr, 32),
                                    Err(_) => continue, // Skip invalid addresses
                                }
                            };
                            peer.allowed_ips.push((addr, prefix));
                        }
                    }
                }
                "PersistentKeepalive" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.persistent_keepalive = Some(value.parse::<u16>()
                            .map_err(|e| format!("Invalid keepalive: {}", e))?);
                    }
                }
                "PresharedKey" => {
                    if let Some(ref mut peer) = current_peer {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(value)
                            .map_err(|e| format!("Invalid preshared key: {}", e))?;
                        peer.preshared_key = Some(bytes.try_into()
                            .map_err(|_| "Preshared key must be 32 bytes")?);
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(peer) = current_peer {
        peers.push(peer);
    }

    Ok(WgConfig {
        private_key: private_key.ok_or("Missing PrivateKey")?,
        address: address.ok_or("Missing Address")?,
        netmask,
        dns,
        peers,
        listen_port,
    })
}

fn prefix_to_netmask(prefix: u8) -> Ipv4Addr {
    let mask: u32 = if prefix == 0 {
        0
    } else if prefix >= 32 {
        !0u32
    } else {
        !0u32 << (32 - prefix)
    };
    Ipv4Addr::from(mask.to_be_bytes())
}
