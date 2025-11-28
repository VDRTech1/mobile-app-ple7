//! Tunnel manager - coordinates VPN connection lifecycle
//! Integrates WireGuard, STUN, WebSocket, and TUN device

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::sync::Mutex;
use base64::Engine as _;
use parking_lot::RwLock;

use crate::api::ApiClient;
use crate::stun::AsyncStunClient;
use crate::wireguard::{WgTunnel, WgConfig, parse_wg_config};
use crate::websocket::{ManagedWsClient, WsConfig, WsEvent};

/// App state type for Tauri commands
pub struct AppState {
    pub tunnel_manager: Arc<Mutex<TunnelManager>>,
    pub api_client: ApiClient,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    DiscoveringEndpoint,
    Handshaking,
    Connected,
    Disconnecting,
    Error(String),
}

/// Connection statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStats {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub connected_peers: usize,
    pub public_endpoint: Option<String>,
    pub connection_type: String, // "direct" or "relay"
}

/// Tunnel manager - handles the VPN connection lifecycle
pub struct TunnelManager {
    status: Arc<RwLock<ConnectionStatus>>,
    stats: Arc<RwLock<ConnectionStats>>,
    wg_tunnel: Arc<Mutex<Option<WgTunnel>>>,
    ws_client: Arc<Mutex<Option<ManagedWsClient>>>,
    is_running: Arc<AtomicBool>,
    current_device_id: Arc<RwLock<Option<String>>>,
    current_network_id: Arc<RwLock<Option<String>>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self {
            status: Arc::new(RwLock::new(ConnectionStatus::Disconnected)),
            stats: Arc::new(RwLock::new(ConnectionStats {
                tx_bytes: 0,
                rx_bytes: 0,
                connected_peers: 0,
                public_endpoint: None,
                connection_type: "unknown".to_string(),
            })),
            wg_tunnel: Arc::new(Mutex::new(None)),
            ws_client: Arc::new(Mutex::new(None)),
            is_running: Arc::new(AtomicBool::new(false)),
            current_device_id: Arc::new(RwLock::new(None)),
            current_network_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Connect to VPN using the device configuration
    pub async fn connect(
        &self,
        config_str: &str,
        device_id: &str,
        network_id: &str,
        api_base_url: &str,
        token: &str,
        use_exit_node: bool,
    ) -> Result<(), String> {
        if self.is_running.load(Ordering::SeqCst) {
            log::warn!("[TUNNEL] Already connected, rejecting new connection");
            return Err("Already connected".to_string());
        }

        log::info!("[TUNNEL] ========== TUNNEL CONNECT START ==========");
        log::info!("[TUNNEL] Device: {}, Network: {}", device_id, network_id);
        log::info!("[TUNNEL] API URL: {}", api_base_url);
        *self.status.write() = ConnectionStatus::Connecting;

        // Parse WireGuard configuration
        log::info!("[TUNNEL] Phase 0: Parsing WireGuard config...");
        let wg_config = match parse_wg_config(config_str) {
            Ok(c) => {
                log::info!("[TUNNEL] ✓ WireGuard config parsed successfully");
                c
            }
            Err(e) => {
                log::error!("[TUNNEL] ✗ Failed to parse WireGuard config: {}", e);
                return Err(e);
            }
        };
        log::info!("[TUNNEL] Parsed WireGuard config with {} peers", wg_config.peers.len());
        for (i, peer) in wg_config.peers.iter().enumerate() {
            log::info!("[TUNNEL]   Peer {}: endpoint={:?}, allowed_ips={:?}",
                i, peer.endpoint, peer.allowed_ips);
        }

        // Store current session info
        *self.current_device_id.write() = Some(device_id.to_string());
        *self.current_network_id.write() = Some(network_id.to_string());

        // Phase 1: Discover our public endpoint via STUN
        log::info!("[TUNNEL] Phase 1: STUN endpoint discovery...");
        *self.status.write() = ConnectionStatus::DiscoveringEndpoint;
        let stun_client = AsyncStunClient::new();
        log::info!("[TUNNEL]   Contacting STUN servers...");
        let public_endpoint = match stun_client.discover_public_endpoint().await {
            Ok(result) => {
                log::info!("[TUNNEL] ✓ STUN discovery successful");
                log::info!("[TUNNEL]   Public endpoint: {}", result.public_addr);
                log::info!("[TUNNEL]   Local endpoint: {}", result.local_addr);
                self.stats.write().public_endpoint = Some(result.public_addr.to_string());
                Some(result.public_addr)
            }
            Err(e) => {
                log::warn!("[TUNNEL] ⚠ STUN discovery failed: {}", e);
                log::warn!("[TUNNEL]   Will use relay instead of direct P2P");
                None
            }
        };

        // Phase 2: Connect WebSocket for real-time peer updates (optional - VPN works via relay without it)
        log::info!("[TUNNEL] Phase 2: WebSocket connection (optional)...");
        let ws_url = format!("{}/ws/mesh", api_base_url.replace("http://", "ws://").replace("https://", "wss://"));
        log::info!("[TUNNEL]   WebSocket URL: {}", ws_url);

        let ws_config = WsConfig {
            base_url: api_base_url.to_string(),
            token: token.to_string(),
            device_id: device_id.to_string(),
            reconnect_interval: Duration::from_secs(5),
        };

        let ws_client = ManagedWsClient::new(ws_config);
        let _status_clone = self.status.clone();

        // Try to start WebSocket - but don't fail if it doesn't work
        // The VPN will still function via relay, just without real-time P2P updates
        log::info!("[TUNNEL]   Attempting WebSocket connection...");
        let ws_connected = match ws_client.start(Box::new(move |event| {
            match event {
                WsEvent::PeerEndpointUpdate { device_id, public_key, endpoint } => {
                    log::info!("Peer endpoint update: {} -> {}", public_key, endpoint);
                }
                WsEvent::PeerOnline { device_id, .. } => {
                    log::info!("Peer came online: {}", device_id);
                }
                WsEvent::PeerOffline { device_id } => {
                    log::info!("Peer went offline: {}", device_id);
                }
                _ => {}
            }
        })).await {
            Ok(_) => {
                log::info!("WebSocket connected for real-time peer updates");
                true
            }
            Err(e) => {
                log::warn!("WebSocket connection failed: {}. Continuing without real-time updates.", e);
                false
            }
        };

        // Only try to register/subscribe if WebSocket connected
        if ws_connected {
            if let Some(endpoint) = public_endpoint {
                if let Err(e) = ws_client.register_endpoint(endpoint).await {
                    log::warn!("Failed to register endpoint: {}", e);
                }
            }
            if let Err(e) = ws_client.subscribe(network_id).await {
                log::warn!("Failed to subscribe to network: {}", e);
            }
            *self.ws_client.lock().await = Some(ws_client);
        }

        // Phase 3: Create and start WireGuard tunnel
        *self.status.write() = ConnectionStatus::Handshaking;

        let tunnel = WgTunnel::new(wg_config).await?;

        // Update stats with public endpoint from tunnel
        if let Some(endpoint) = tunnel.public_endpoint() {
            self.stats.write().public_endpoint = Some(endpoint.to_string());
        }

        tunnel.start().await?;

        // If exit node is selected, route all traffic through VPN
        if use_exit_node {
            log::info!("[TUNNEL] Exit node enabled, setting default gateway through VPN");
            if let Err(e) = tunnel.set_default_gateway().await {
                log::warn!("[TUNNEL] Failed to set default gateway: {}", e);
                // Don't fail the connection, just warn
            }
        }

        *self.wg_tunnel.lock().await = Some(tunnel);
        self.is_running.store(true, Ordering::SeqCst);

        // Determine connection type
        let connection_type = if public_endpoint.is_some() {
            "direct".to_string()
        } else {
            "relay".to_string()
        };
        self.stats.write().connection_type = connection_type;

        *self.status.write() = ConnectionStatus::Connected;
        log::info!("VPN connection established");

        // Start stats update task
        self.start_stats_updater();

        Ok(())
    }

    /// Start background task to update connection statistics
    fn start_stats_updater(&self) {
        let stats = self.stats.clone();
        let tunnel = self.wg_tunnel.clone();
        let running = self.is_running.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));

            while running.load(Ordering::SeqCst) {
                interval.tick().await;

                if let Some(tun) = tunnel.lock().await.as_ref() {
                    let peer_stats = tun.get_stats();
                    let mut s = stats.write();
                    s.tx_bytes = peer_stats.iter().map(|(_, tx, _)| tx).sum();
                    s.rx_bytes = peer_stats.iter().map(|(_, _, rx)| rx).sum();
                    s.connected_peers = peer_stats.len();
                }
            }
        });
    }

    /// Disconnect from VPN
    pub async fn disconnect(&self) -> Result<(), String> {
        if !self.is_running.load(Ordering::SeqCst) {
            return Err("Not connected".to_string());
        }

        log::info!("Disconnecting VPN");
        *self.status.write() = ConnectionStatus::Disconnecting;

        // Stop WireGuard tunnel
        if let Some(tunnel) = self.wg_tunnel.lock().await.as_ref() {
            tunnel.stop().await?;
        }
        *self.wg_tunnel.lock().await = None;

        // Stop WebSocket
        if let Some(ws) = self.ws_client.lock().await.as_ref() {
            ws.stop();
        }
        *self.ws_client.lock().await = None;

        // Clear session info
        *self.current_device_id.write() = None;
        *self.current_network_id.write() = None;

        self.is_running.store(false, Ordering::SeqCst);
        *self.status.write() = ConnectionStatus::Disconnected;

        // Reset stats
        *self.stats.write() = ConnectionStats {
            tx_bytes: 0,
            rx_bytes: 0,
            connected_peers: 0,
            public_endpoint: None,
            connection_type: "unknown".to_string(),
        };

        log::info!("VPN disconnected");
        Ok(())
    }

    /// Get current connection status
    pub fn get_status(&self) -> ConnectionStatus {
        self.status.read().clone()
    }

    /// Get connection statistics
    pub fn get_stats(&self) -> ConnectionStats {
        self.stats.read().clone()
    }

    /// Update peer endpoint for direct P2P connection
    pub async fn update_peer_endpoint(&self, public_key: &str, endpoint: SocketAddr) -> Result<(), String> {
        if let Some(tunnel) = self.wg_tunnel.lock().await.as_ref() {
            let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
                .decode(public_key)
                .map_err(|e| format!("Invalid public key: {}", e))?
                .try_into()
                .map_err(|_| "Public key must be 32 bytes")?;

            tunnel.update_peer_endpoint(&key_bytes, endpoint);
            Ok(())
        } else {
            Err("Not connected".to_string())
        }
    }
}

impl Default for TunnelManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tauri Commands
// ============================================================================

#[tauri::command]
pub async fn connect_vpn(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    network_id: String,
    exit_node_type: Option<String>,
    exit_node_id: Option<String>,
) -> Result<(), String> {
    log::info!("========== VPN CONNECTION START ==========");
    log::info!("[STEP 1/6] connect_vpn command: device={}, network={}", device_id, network_id);
    log::info!("[STEP 1/6] Exit node: type={:?}, id={:?}", exit_node_type, exit_node_id);
    log::info!("[STEP 1/6] API base URL: {}", state.api_client.base_url);

    // Get stored token
    log::info!("[STEP 2/6] Retrieving stored auth token...");
    let token = match crate::config::get_stored_token_internal(&app).await {
        Ok(t) => {
            log::info!("[STEP 2/6] ✓ Token retrieved (length: {} chars)", t.len());
            t
        }
        Err(e) => {
            log::error!("[STEP 2/6] ✗ FAILED to get token: {}", e);
            return Err(format!("Failed to get auth token: {}", e));
        }
    };

    // Get device configuration from API
    log::info!("[STEP 3/6] Fetching device config from API...");
    let config_response = match state.api_client.get_device_config(&token, &device_id).await {
        Ok(c) => {
            log::info!("[STEP 3/6] ✓ Device config received");
            log::info!("[STEP 3/6]   - has_private_key: {}", c.has_private_key);
            log::info!("[STEP 3/6]   - config length: {} bytes", c.config.len());
            c
        }
        Err(e) => {
            log::error!("[STEP 3/6] ✗ FAILED to get device config: {}", e);
            return Err(format!("Failed to get device config: {}", e));
        }
    };

    if !config_response.has_private_key {
        log::error!("[STEP 3/6] ✗ Device config missing private key");
        return Err("Device configuration does not include private key. Please use a device with auto-generated keys.".to_string());
    }

    // Log WireGuard config details (without secrets)
    log::info!("[STEP 4/6] Parsing WireGuard config...");
    for line in config_response.config.lines() {
        let line = line.trim();
        if line.starts_with("[") || line.starts_with("Address") || line.starts_with("DNS") ||
           line.starts_with("Endpoint") || line.starts_with("AllowedIPs") || line.starts_with("PersistentKeepalive") {
            log::info!("[STEP 4/6]   {}", line);
        } else if line.starts_with("PublicKey") {
            log::info!("[STEP 4/6]   PublicKey = [PRESENT]");
        } else if line.starts_with("PrivateKey") {
            log::info!("[STEP 4/6]   PrivateKey = [PRESENT]");
        }
    }

    // Connect using the tunnel manager
    log::info!("[STEP 5/6] Acquiring tunnel manager lock...");
    let tunnel_manager = state.tunnel_manager.lock().await;
    log::info!("[STEP 5/6] ✓ Lock acquired, starting connection...");

    // Determine if we should route all traffic through VPN (exit node)
    let use_exit_node = exit_node_type.as_deref() == Some("relay") || exit_node_type.as_deref() == Some("device");
    log::info!("[STEP 6/6] Calling tunnel_manager.connect() with exit_node={}...", use_exit_node);
    match tunnel_manager.connect(
        &config_response.config,
        &device_id,
        &network_id,
        &state.api_client.base_url,
        &token,
        use_exit_node,
    ).await {
        Ok(()) => {
            log::info!("========== VPN CONNECTION SUCCESS ==========");
            Ok(())
        }
        Err(e) => {
            log::error!("[STEP 6/6] ✗ tunnel_manager.connect() FAILED: {}", e);
            log::error!("========== VPN CONNECTION FAILED ==========");
            Err(e)
        }
    }
}

#[tauri::command]
pub async fn disconnect_vpn(state: State<'_, AppState>) -> Result<(), String> {
    log::info!("disconnect_vpn command");
    let tunnel_manager = state.tunnel_manager.lock().await;
    tunnel_manager.disconnect().await
}

#[tauri::command]
pub async fn get_connection_status(state: State<'_, AppState>) -> Result<ConnectionStatus, String> {
    let tunnel_manager = state.tunnel_manager.lock().await;
    Ok(tunnel_manager.get_status())
}

#[tauri::command]
pub async fn get_connection_stats(state: State<'_, AppState>) -> Result<ConnectionStats, String> {
    let tunnel_manager = state.tunnel_manager.lock().await;
    Ok(tunnel_manager.get_stats())
}

/// Legacy config parser (kept for compatibility)
pub fn parse_wireguard_config(config_str: &str) -> Result<WireGuardConfig, String> {
    let mut private_key = String::new();
    let mut address = String::new();
    let mut dns = None;
    let mut peers = Vec::new();
    let mut current_peer: Option<PeerConfig> = None;

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
            current_peer = Some(PeerConfig {
                public_key: String::new(),
                endpoint: None,
                allowed_ips: Vec::new(),
                persistent_keepalive: None,
            });
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "PrivateKey" => private_key = value.to_string(),
                "Address" => address = value.to_string(),
                "DNS" => dns = Some(value.to_string()),
                "PublicKey" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.public_key = value.to_string();
                    }
                }
                "Endpoint" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.endpoint = Some(value.to_string());
                    }
                }
                "AllowedIPs" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.allowed_ips = value.split(',').map(|s| s.trim().to_string()).collect();
                    }
                }
                "PersistentKeepalive" => {
                    if let Some(ref mut peer) = current_peer {
                        peer.persistent_keepalive = value.parse().ok();
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(peer) = current_peer {
        peers.push(peer);
    }

    if private_key.is_empty() {
        return Err("Missing PrivateKey".to_string());
    }
    if address.is_empty() {
        return Err("Missing Address".to_string());
    }

    Ok(WireGuardConfig {
        private_key,
        address,
        dns,
        peers,
    })
}

/// Legacy WireGuard config types (kept for compatibility)
#[derive(Debug)]
pub struct WireGuardConfig {
    pub private_key: String,
    pub address: String,
    pub dns: Option<String>,
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug)]
pub struct PeerConfig {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    pub persistent_keepalive: Option<u16>,
}
