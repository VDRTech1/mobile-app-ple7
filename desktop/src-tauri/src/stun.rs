//! STUN client for NAT traversal
//! Discovers public IP:port for direct peer-to-peer connections

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;
use stun_codec::rfc5389::attributes::XorMappedAddress;
use stun_codec::rfc5389::methods::BINDING;
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};
use bytecodec::{DecodeExt, EncodeExt};
use rand::Rng;

/// Public STUN servers for NAT traversal
const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun.stunprotocol.org:3478",
];

/// Result of STUN query - our public endpoint as seen by the STUN server
#[derive(Debug, Clone)]
pub struct StunResult {
    pub public_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub stun_server: String,
}

/// STUN client for discovering public IP:port
pub struct StunClient {
    timeout: Duration,
}

impl StunClient {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Discover our public endpoint using STUN
    /// Tries multiple servers until one succeeds
    pub fn discover_public_endpoint(&self) -> Result<StunResult, String> {
        // Bind to any available port
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind UDP socket: {}", e))?;

        socket.set_read_timeout(Some(self.timeout))
            .map_err(|e| format!("Failed to set socket timeout: {}", e))?;

        let local_addr = socket.local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?;

        // Try each STUN server until one works
        let mut errors = Vec::new();
        for (i, server) in STUN_SERVERS.iter().enumerate() {
            log::info!("[STUN] Trying server {}/{}: {}", i + 1, STUN_SERVERS.len(), server);
            match self.query_stun_server(&socket, server) {
                Ok(public_addr) => {
                    log::info!("[STUN] ✓ Success! {} -> {} (via {})",
                        local_addr, public_addr, server);
                    return Ok(StunResult {
                        public_addr,
                        local_addr,
                        stun_server: server.to_string(),
                    });
                }
                Err(e) => {
                    log::warn!("[STUN] ✗ Server {} failed: {}", server, e);
                    errors.push(format!("{}: {}", server, e));
                    continue;
                }
            }
        }

        let error_summary = errors.join("; ");
        log::error!("[STUN] All {} servers failed. Details: {}", STUN_SERVERS.len(), error_summary);
        Err(format!("All STUN servers failed: {}", error_summary))
    }

    /// Discover public endpoint using a specific local port
    /// This is important for WireGuard - we want to know the public mapping of our WG port
    pub fn discover_for_port(&self, local_port: u16) -> Result<StunResult, String> {
        let bind_addr = format!("0.0.0.0:{}", local_port);
        let socket = UdpSocket::bind(&bind_addr)
            .map_err(|e| format!("Failed to bind to port {}: {}", local_port, e))?;

        socket.set_read_timeout(Some(self.timeout))
            .map_err(|e| format!("Failed to set socket timeout: {}", e))?;

        let local_addr = socket.local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?;

        for server in STUN_SERVERS {
            match self.query_stun_server(&socket, server) {
                Ok(public_addr) => {
                    log::info!("STUN discovery for port {}: {} -> {} (via {})",
                        local_port, local_addr, public_addr, server);
                    return Ok(StunResult {
                        public_addr,
                        local_addr,
                        stun_server: server.to_string(),
                    });
                }
                Err(e) => {
                    log::debug!("STUN server {} failed for port {}: {}", server, local_port, e);
                    continue;
                }
            }
        }

        Err(format!("All STUN servers failed for port {}", local_port))
    }

    fn query_stun_server(&self, socket: &UdpSocket, server: &str) -> Result<SocketAddr, String> {
        // Resolve server address
        let server_addr: SocketAddr = server
            .parse()
            .or_else(|_| {
                // Try DNS resolution
                std::net::ToSocketAddrs::to_socket_addrs(&server)
                    .map_err(|e| format!("DNS resolution failed: {}", e))?
                    .next()
                    .ok_or_else(|| "No addresses found".to_string())
            })?;

        // Create STUN binding request
        let transaction_id = self.generate_transaction_id();
        let request = Message::<stun_codec::rfc5389::Attribute>::new(
            MessageClass::Request,
            BINDING,
            transaction_id,
        );

        // Encode and send
        let mut encoder = MessageEncoder::new();
        let request_bytes = encoder
            .encode_into_bytes(request)
            .map_err(|e| format!("Failed to encode STUN request: {}", e))?;

        socket.send_to(&request_bytes, server_addr)
            .map_err(|e| format!("Failed to send STUN request: {}", e))?;

        // Receive response
        let mut buf = [0u8; 1024];
        let (len, _) = socket.recv_from(&mut buf)
            .map_err(|e| format!("Failed to receive STUN response: {}", e))?;

        // Decode response
        let mut decoder = MessageDecoder::<stun_codec::rfc5389::Attribute>::new();
        let response = decoder
            .decode_from_bytes(&buf[..len])
            .map_err(|e| format!("Failed to decode STUN response: {}", e))?
            .map_err(|e| format!("Incomplete STUN response: {:?}", e))?;

        // Verify transaction ID
        if response.transaction_id() != transaction_id {
            return Err("Transaction ID mismatch".to_string());
        }

        // Extract XOR-MAPPED-ADDRESS
        for attr in response.attributes() {
            if let stun_codec::rfc5389::Attribute::XorMappedAddress(xma) = attr {
                return Ok(xma.address());
            }
        }

        // Try regular MAPPED-ADDRESS as fallback
        for attr in response.attributes() {
            if let stun_codec::rfc5389::Attribute::MappedAddress(ma) = attr {
                return Ok(ma.address());
            }
        }

        Err("No mapped address in STUN response".to_string())
    }

    fn generate_transaction_id(&self) -> TransactionId {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 12];
        rng.fill(&mut bytes);
        TransactionId::new(bytes)
    }
}

impl Default for StunClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Async version of STUN client
pub struct AsyncStunClient {
    timeout: Duration,
}

impl AsyncStunClient {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }

    /// Discover public endpoint asynchronously
    pub async fn discover_public_endpoint(&self) -> Result<StunResult, String> {
        // Run sync STUN client in blocking task
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || {
            let client = StunClient::with_timeout(timeout);
            client.discover_public_endpoint()
        })
        .await
        .map_err(|e| format!("STUN task failed: {}", e))?
    }

    /// Discover public endpoint for specific port asynchronously
    pub async fn discover_for_port(&self, local_port: u16) -> Result<StunResult, String> {
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || {
            let client = StunClient::with_timeout(timeout);
            client.discover_for_port(local_port)
        })
        .await
        .map_err(|e| format!("STUN task failed: {}", e))?
    }
}

impl Default for AsyncStunClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stun_discovery() {
        let client = StunClient::new();
        match client.discover_public_endpoint() {
            Ok(result) => {
                println!("Public endpoint: {}", result.public_addr);
                println!("Local endpoint: {}", result.local_addr);
                println!("STUN server: {}", result.stun_server);
            }
            Err(e) => {
                println!("STUN failed (may be expected in CI): {}", e);
            }
        }
    }
}
