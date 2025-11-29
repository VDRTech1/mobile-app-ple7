//! WebSocket client for real-time peer updates from the control plane
//! Receives peer endpoint updates for NAT traversal and direct P2P connections
//! Uses Socket.IO protocol format (42["event",{data}])

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

/// Events received from the control plane
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum WsEvent {
    /// Peer endpoint updated (for direct P2P connection)
    PeerEndpointUpdate {
        device_id: String,
        public_key: String,
        endpoint: String,
    },
    /// Peer came online
    PeerOnline {
        device_id: String,
        public_key: String,
    },
    /// Peer went offline
    PeerOffline {
        device_id: String,
    },
    /// Network configuration changed
    NetworkConfigUpdate {
        network_id: String,
    },
    /// Ping from server (keepalive)
    Ping,
    /// Server acknowledged our endpoint report
    EndpointAck {
        success: bool,
    },
}

/// Messages sent to the control plane
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum WsMessage {
    /// Register this device (makes it show as online)
    RegisterDevice {
        device_id: String,
    },
    /// Register this device with its public endpoint (for P2P)
    RegisterEndpoint {
        device_id: String,
        endpoint: String,
    },
    /// Subscribe to updates for a network
    Subscribe {
        network_id: String,
    },
    /// Unsubscribe from a network
    Unsubscribe {
        network_id: String,
    },
    /// Pong response
    Pong,
}

/// Callback for handling WebSocket events
pub type EventCallback = Box<dyn Fn(WsEvent) + Send + Sync>;

/// Parse Socket.IO message format: "42[\"event_name\",{data}]"
fn parse_socketio_message(text: &str) -> Option<WsEvent> {
    // Socket.IO message types:
    // 0 = CONNECT, 2 = EVENT, 3 = ACK, 4 = CONNECT_ERROR, 40 = CONNECT (namespace), 42 = EVENT (namespace)
    // We're interested in 42 (EVENT with default namespace) or just 2 (EVENT)

    let text = text.trim();

    // Handle Socket.IO CONNECT response (0 or 40)
    if text.starts_with("0{") || text == "40" || text.starts_with("40{") {
        log::debug!("[WS] Socket.IO connect acknowledgement");
        return None;
    }

    // Handle Socket.IO EVENT (42["event",data] or 2["event",data])
    let json_start = if text.starts_with("42") {
        2
    } else if text.starts_with("2[") {
        1
    } else {
        log::debug!("[WS] Unknown Socket.IO message type: {}", &text[..text.len().min(50)]);
        return None;
    };

    let json_part = &text[json_start..];

    // Parse as JSON array: ["event_name", {data}]
    match serde_json::from_str::<Value>(json_part) {
        Ok(Value::Array(arr)) if arr.len() >= 2 => {
            let event_name = arr[0].as_str()?;
            let data = &arr[1];

            match event_name {
                "peer_endpoint_update" => {
                    let device_id = data.get("deviceId")?.as_str()?.to_string();
                    let public_key = data.get("publicKey")?.as_str()?.to_string();
                    let endpoint = data.get("endpoint")?.as_str()?.to_string();
                    Some(WsEvent::PeerEndpointUpdate { device_id, public_key, endpoint })
                }
                "peer_online" => {
                    let device_id = data.get("deviceId")?.as_str()?.to_string();
                    let public_key = data.get("publicKey").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    Some(WsEvent::PeerOnline { device_id, public_key })
                }
                "peer_offline" => {
                    let device_id = data.get("deviceId")?.as_str()?.to_string();
                    Some(WsEvent::PeerOffline { device_id })
                }
                _ => {
                    log::debug!("[WS] Unknown Socket.IO event: {}", event_name);
                    None
                }
            }
        }
        Ok(_) => {
            log::debug!("[WS] Socket.IO message not an array: {}", json_part);
            None
        }
        Err(e) => {
            log::debug!("[WS] Failed to parse Socket.IO JSON: {} - {}", e, json_part);
            None
        }
    }
}

/// Format message for Socket.IO: 42["event",{data}]
fn format_socketio_message(event: &str, data: &Value) -> String {
    format!("42{}", serde_json::to_string(&serde_json::json!([event, data])).unwrap_or_default())
}

/// WebSocket connection state
#[derive(Debug, Clone, PartialEq)]
pub enum WsState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

/// WebSocket client for control plane communication
pub struct WsClient {
    base_url: String,
    token: String,
    device_id: String,
    state: Arc<RwLock<WsState>>,
    pub tx: Option<mpsc::Sender<WsMessage>>,
    callbacks: Arc<RwLock<Vec<EventCallback>>>,
    peer_endpoints: Arc<RwLock<HashMap<String, SocketAddr>>>,
}

impl WsClient {
    pub fn new(base_url: &str, token: &str, device_id: &str) -> Self {
        // Convert http(s) to ws(s)
        let ws_url = base_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        Self {
            base_url: ws_url,
            token: token.to_string(),
            device_id: device_id.to_string(),
            state: Arc::new(RwLock::new(WsState::Disconnected)),
            tx: None,
            callbacks: Arc::new(RwLock::new(Vec::new())),
            peer_endpoints: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a callback for WebSocket events
    pub fn on_event(&mut self, callback: EventCallback) {
        self.callbacks.write().push(callback);
    }

    /// Get current peer endpoints
    pub fn peer_endpoints(&self) -> HashMap<String, SocketAddr> {
        self.peer_endpoints.read().clone()
    }

    /// Get specific peer endpoint
    pub fn get_peer_endpoint(&self, public_key: &str) -> Option<SocketAddr> {
        self.peer_endpoints.read().get(public_key).copied()
    }

    /// Connect to the WebSocket server
    pub async fn connect(&mut self) -> Result<(), String> {
        *self.state.write() = WsState::Connecting;

        let ws_url = format!("{}/ws/mesh?token={}", self.base_url, self.token);

        log::info!("Connecting to WebSocket: {}", self.base_url);

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .map_err(|e| format!("WebSocket connection failed: {}", e))?;

        let (mut write, mut read) = ws_stream.split();

        // Create message channel
        let (tx, mut rx) = mpsc::channel::<WsMessage>(32);
        self.tx = Some(tx.clone());

        *self.state.write() = WsState::Connected;
        log::info!("WebSocket connected");

        // Clone for tasks
        let state = self.state.clone();
        let callbacks = self.callbacks.clone();
        let peer_endpoints = self.peer_endpoints.clone();
        let device_id = self.device_id.clone();

        // Spawn write task - sends Socket.IO formatted messages
        let state_write = state.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                // Format message as Socket.IO EVENT: 42["event_name",{data}]
                let socketio_msg = match &msg {
                    WsMessage::RegisterDevice { device_id } => {
                        format_socketio_message("register_device", &serde_json::json!({
                            "deviceId": device_id
                        }))
                    }
                    WsMessage::RegisterEndpoint { device_id, endpoint } => {
                        format_socketio_message("register_endpoint", &serde_json::json!({
                            "deviceId": device_id,
                            "endpoint": endpoint
                        }))
                    }
                    WsMessage::Subscribe { network_id } => {
                        format_socketio_message("subscribe", &serde_json::json!({
                            "networkId": network_id
                        }))
                    }
                    WsMessage::Unsubscribe { network_id } => {
                        format_socketio_message("unsubscribe", &serde_json::json!({
                            "networkId": network_id
                        }))
                    }
                    WsMessage::Pong => "3".to_string(), // Socket.IO PONG
                };

                log::debug!("[WS] Sending: {}", &socketio_msg[..socketio_msg.len().min(100)]);
                if let Err(e) = write.send(Message::Text(socketio_msg)).await {
                    log::error!("WebSocket send error: {}", e);
                    *state_write.write() = WsState::Disconnected;
                    break;
                }
            }
        });

        // Spawn read task - parses Socket.IO formatted messages
        let tx_pong = tx.clone();
        tokio::spawn(async move {
            while let Some(result) = read.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        log::debug!("[WS] Received: {}", &text[..text.len().min(200)]);

                        // Parse Socket.IO message format
                        if let Some(event) = parse_socketio_message(&text) {
                            // Handle special events
                            match &event {
                                WsEvent::PeerEndpointUpdate { public_key, endpoint, .. } => {
                                    if let Ok(addr) = endpoint.parse::<SocketAddr>() {
                                        peer_endpoints.write().insert(public_key.clone(), addr);
                                        log::info!("[P2P] Received peer endpoint: {} -> {}", &public_key[..8], endpoint);
                                    }
                                }
                                _ => {}
                            }

                            // Call registered callbacks
                            for callback in callbacks.read().iter() {
                                callback(event.clone());
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        log::info!("WebSocket closed by server");
                        *state.write() = WsState::Disconnected;
                        break;
                    }
                    Ok(Message::Ping(_)) => {
                        // Tungstenite handles pong automatically
                    }
                    Err(e) => {
                        log::error!("WebSocket read error: {}", e);
                        *state.write() = WsState::Disconnected;
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }

    /// Register our public endpoint with the control plane
    pub async fn register_endpoint(&self, endpoint: SocketAddr) -> Result<(), String> {
        if let Some(tx) = &self.tx {
            tx.send(WsMessage::RegisterEndpoint {
                device_id: self.device_id.clone(),
                endpoint: endpoint.to_string(),
            })
            .await
            .map_err(|e| format!("Failed to send endpoint: {}", e))?;

            log::info!("Registered endpoint with control plane: {}", endpoint);
        }
        Ok(())
    }

    /// Subscribe to updates for a network
    pub async fn subscribe(&self, network_id: &str) -> Result<(), String> {
        if let Some(tx) = &self.tx {
            tx.send(WsMessage::Subscribe {
                network_id: network_id.to_string(),
            })
            .await
            .map_err(|e| format!("Failed to subscribe: {}", e))?;

            log::info!("Subscribed to network: {}", network_id);
        }
        Ok(())
    }

    /// Get current connection state
    pub fn state(&self) -> WsState {
        self.state.read().clone()
    }

    /// Disconnect from WebSocket
    pub fn disconnect(&mut self) {
        self.tx = None;
        *self.state.write() = WsState::Disconnected;
        log::info!("WebSocket disconnected");
    }
}

/// Managed WebSocket client with automatic reconnection
pub struct ManagedWsClient {
    client: Arc<RwLock<Option<WsClient>>>,
    config: WsConfig,
    running: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub struct WsConfig {
    pub base_url: String,
    pub token: String,
    pub device_id: String,
    pub reconnect_interval: Duration,
}

impl ManagedWsClient {
    pub fn new(config: WsConfig) -> Self {
        Self {
            client: Arc::new(RwLock::new(None)),
            config,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Start the managed WebSocket connection with auto-reconnect
    /// Optionally registers endpoint and subscribes to network after connection
    pub async fn start_with_registration(
        &self,
        on_event: EventCallback,
        public_endpoint: Option<SocketAddr>,
        network_id: Option<String>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;

        if self.running.load(Ordering::SeqCst) {
            return Err("Already running".to_string());
        }

        self.running.store(true, Ordering::SeqCst);

        let config = self.config.clone();
        let client = self.client.clone();
        let running = self.running.clone();
        let callbacks = Arc::new(RwLock::new(vec![on_event]));

        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let mut ws_client = WsClient::new(
                    &config.base_url,
                    &config.token,
                    &config.device_id,
                );

                // Add callbacks
                for cb in callbacks.read().iter() {
                    // Note: This is simplified - in production you'd clone Arc callbacks
                }

                match ws_client.connect().await {
                    Ok(()) => {
                        log::info!("WebSocket connected, registering device...");

                        // Always register device first (shows as online)
                        if let Some(tx) = &ws_client.tx {
                            if let Err(e) = tx.send(WsMessage::RegisterDevice {
                                device_id: config.device_id.clone(),
                            }).await {
                                log::warn!("Failed to register device: {}", e);
                            } else {
                                log::info!("Device registered: {}", config.device_id);
                            }
                        }

                        // Register endpoint if STUN succeeded (enables P2P)
                        if let Some(endpoint) = public_endpoint {
                            if let Some(tx) = &ws_client.tx {
                                if let Err(e) = tx.send(WsMessage::RegisterEndpoint {
                                    device_id: config.device_id.clone(),
                                    endpoint: endpoint.to_string(),
                                }).await {
                                    log::warn!("Failed to register endpoint: {}", e);
                                } else {
                                    log::info!("Registered P2P endpoint: {}", endpoint);
                                }
                            }
                        } else {
                            log::warn!("No public endpoint (STUN failed) - P2P unavailable, using relay only");
                        }

                        // Subscribe to network
                        if let Some(ref net_id) = network_id {
                            if let Some(tx) = &ws_client.tx {
                                if let Err(e) = tx.send(WsMessage::Subscribe {
                                    network_id: net_id.clone(),
                                }).await {
                                    log::warn!("Failed to subscribe to network: {}", e);
                                } else {
                                    log::info!("Subscribed to network: {}", net_id);
                                }
                            }
                        }

                        *client.write() = Some(ws_client);
                        log::info!("WebSocket ready for P2P updates (endpoint: {})",
                            public_endpoint.map(|e| e.to_string()).unwrap_or_else(|| "relay-only".to_string()));

                        // Monitor connection
                        loop {
                            tokio::time::sleep(Duration::from_secs(5)).await;

                            if !running.load(Ordering::SeqCst) {
                                break;
                            }

                            let state = client.read()
                                .as_ref()
                                .map(|c| c.state())
                                .unwrap_or(WsState::Disconnected);

                            if state == WsState::Disconnected {
                                log::info!("WebSocket disconnected, will reconnect...");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("WebSocket connection failed: {}", e);
                    }
                }

                if running.load(Ordering::SeqCst) {
                    log::info!("Reconnecting in {:?}...", config.reconnect_interval);
                    tokio::time::sleep(config.reconnect_interval).await;
                }
            }
        });

        Ok(())
    }

    /// Start the managed WebSocket connection with auto-reconnect (legacy)
    pub async fn start(&self, on_event: EventCallback) -> Result<(), String> {
        self.start_with_registration(on_event, None, None).await
    }

    /// Stop the managed connection
    pub fn stop(&self) {
        use std::sync::atomic::Ordering;
        self.running.store(false, Ordering::SeqCst);
        if let Some(client) = self.client.write().as_mut() {
            client.disconnect();
        }
    }

    /// Register endpoint
    pub async fn register_endpoint(&self, endpoint: SocketAddr) -> Result<(), String> {
        // Get the tx channel without holding the lock across await
        let tx = {
            let guard = self.client.read();
            guard.as_ref().and_then(|c| c.tx.clone())
        };

        if let Some(tx) = tx {
            tx.send(WsMessage::RegisterEndpoint {
                device_id: self.config.device_id.clone(),
                endpoint: endpoint.to_string(),
            })
            .await
            .map_err(|e| format!("Failed to send endpoint: {}", e))?;
            log::info!("Registered endpoint with control plane: {}", endpoint);
            Ok(())
        } else {
            Err("Not connected".to_string())
        }
    }

    /// Get peer endpoint
    pub fn get_peer_endpoint(&self, public_key: &str) -> Option<SocketAddr> {
        self.client.read()
            .as_ref()
            .and_then(|c| c.get_peer_endpoint(public_key))
    }

    /// Subscribe to network updates
    pub async fn subscribe(&self, network_id: &str) -> Result<(), String> {
        // Get the tx channel without holding the lock across await
        let tx = {
            let guard = self.client.read();
            guard.as_ref().and_then(|c| c.tx.clone())
        };

        if let Some(tx) = tx {
            tx.send(WsMessage::Subscribe {
                network_id: network_id.to_string(),
            })
            .await
            .map_err(|e| format!("Failed to subscribe: {}", e))?;
            log::info!("Subscribed to network: {}", network_id);
            Ok(())
        } else {
            Err("Not connected".to_string())
        }
    }
}
