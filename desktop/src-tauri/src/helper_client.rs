//! Client for communicating with the PLE7 privileged helper daemon
//!
//! This module handles:
//! - Checking if helper is installed
//! - Installing helper with admin privileges
//! - Sending commands to the helper daemon

use std::io::{Read, Write, BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const SOCKET_PATH: &str = "/var/run/ple7-helper.sock";
const HELPER_PATH: &str = "/Library/PrivilegedHelperTools/ple7-helper";
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.ple7.vpn.helper.plist";

#[derive(Debug, Serialize)]
#[serde(tag = "command")]
pub enum HelperCommand {
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
    #[serde(rename = "read_packet")]
    ReadPacket {
        tun_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "write_packet")]
    WritePacket {
        tun_name: String,
        data: String, // Base64 encoded
    },
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Deserialize)]
pub struct HelperResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

pub struct HelperClient {
    stream: Option<UnixStream>,
}

impl HelperClient {
    pub fn new() -> Self {
        Self { stream: None }
    }

    /// Check if the helper daemon is installed and running
    pub fn is_installed() -> bool {
        Path::new(HELPER_PATH).exists() && Path::new(PLIST_PATH).exists()
    }

    /// Check if the helper daemon is running
    pub fn is_running() -> bool {
        Path::new(SOCKET_PATH).exists()
    }

    /// Install the helper daemon (requires admin privileges)
    /// Returns the AppleScript command to run with admin privileges
    pub fn get_install_script(helper_binary_path: &str, plist_path: &str) -> String {
        format!(
            r#"do shell script "
# Create directories
mkdir -p /Library/PrivilegedHelperTools
mkdir -p /Library/LaunchDaemons

# Copy helper binary
cp '{}' /Library/PrivilegedHelperTools/ple7-helper
chmod 755 /Library/PrivilegedHelperTools/ple7-helper
chown root:wheel /Library/PrivilegedHelperTools/ple7-helper

# Copy launchd plist
cp '{}' /Library/LaunchDaemons/com.ple7.vpn.helper.plist
chmod 644 /Library/LaunchDaemons/com.ple7.vpn.helper.plist
chown root:wheel /Library/LaunchDaemons/com.ple7.vpn.helper.plist

# Load the daemon
launchctl unload /Library/LaunchDaemons/com.ple7.vpn.helper.plist 2>/dev/null || true
launchctl load /Library/LaunchDaemons/com.ple7.vpn.helper.plist

echo 'Helper installed successfully'
" with administrator privileges"#,
            helper_binary_path, plist_path
        )
    }

    /// Install the helper using osascript (will prompt for admin password)
    pub async fn install_helper() -> Result<(), String> {
        log::info!("Installing PLE7 helper daemon...");

        // Get paths to bundled helper files
        let exe_path = std::env::current_exe()
            .map_err(|e| format!("Failed to get executable path: {}", e))?;

        let resources_dir = exe_path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("Resources"))
            .ok_or("Failed to find Resources directory")?;

        let helper_binary = resources_dir.join("ple7-helper");
        let plist_file = resources_dir.join("com.ple7.vpn.helper.plist");

        if !helper_binary.exists() {
            return Err(format!("Helper binary not found at {:?}", helper_binary));
        }

        if !plist_file.exists() {
            return Err(format!("Plist file not found at {:?}", plist_file));
        }

        let script = Self::get_install_script(
            helper_binary.to_str().unwrap(),
            plist_file.to_str().unwrap(),
        );

        log::debug!("Running install script via osascript");

        let output = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .map_err(|e| format!("Failed to run osascript: {}", e))?;

        if output.status.success() {
            log::info!("Helper installed successfully");

            // Wait for daemon to start
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if Self::is_running() {
                    log::info!("Helper daemon is now running");
                    return Ok(());
                }
            }

            Err("Helper installed but daemon not starting".to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);

            if stderr.contains("User canceled") || stdout.contains("User canceled") {
                Err("Installation cancelled by user".to_string())
            } else {
                Err(format!("Failed to install helper: {} {}", stdout, stderr))
            }
        }
    }

    /// Connect to the helper daemon
    pub fn connect(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }

        let stream = UnixStream::connect(SOCKET_PATH)
            .map_err(|e| format!("Failed to connect to helper: {}", e))?;

        stream.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("Failed to set read timeout: {}", e))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("Failed to set write timeout: {}", e))?;

        self.stream = Some(stream);
        Ok(())
    }

    /// Send a command to the helper daemon
    pub fn send_command(&mut self, cmd: HelperCommand) -> Result<HelperResponse, String> {
        self.connect()?;

        let stream = self.stream.as_mut().unwrap();

        // Send command
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| format!("Failed to serialize command: {}", e))?;

        stream.write_all(cmd_json.as_bytes())
            .map_err(|e| format!("Failed to send command: {}", e))?;

        // Read response
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)
            .map_err(|e| format!("Failed to read response: {}", e))?;

        serde_json::from_str(&response_line)
            .map_err(|e| format!("Failed to parse response: {}", e))
    }

    /// Create a TUN device
    pub fn create_tun(&mut self, name: &str, address: &str, netmask: &str) -> Result<HelperResponse, String> {
        self.send_command(HelperCommand::CreateTun {
            name: name.to_string(),
            address: address.to_string(),
            netmask: netmask.to_string(),
        })
    }

    /// Destroy a TUN device
    pub fn destroy_tun(&mut self, name: &str) -> Result<HelperResponse, String> {
        self.send_command(HelperCommand::DestroyTun {
            name: name.to_string(),
        })
    }

    /// Add a route
    pub fn add_route(&mut self, destination: &str, prefix_len: u8, gateway: &str) -> Result<HelperResponse, String> {
        self.send_command(HelperCommand::AddRoute {
            destination: destination.to_string(),
            prefix_len,
            gateway: gateway.to_string(),
        })
    }

    /// Set default gateway for exit node
    pub fn set_default_gateway(&mut self, gateway: &str) -> Result<HelperResponse, String> {
        self.send_command(HelperCommand::SetDefaultGateway {
            gateway: gateway.to_string(),
        })
    }

    /// Restore original default gateway
    pub fn restore_default_gateway(&mut self) -> Result<HelperResponse, String> {
        self.send_command(HelperCommand::RestoreDefaultGateway)
    }

    /// Ping the helper to check if it's responsive
    pub fn ping(&mut self) -> Result<bool, String> {
        let response = self.send_command(HelperCommand::Ping)?;
        Ok(response.success && response.message == "pong")
    }

    /// Read a packet from the TUN device
    pub fn read_packet(&mut self, tun_name: &str, timeout_ms: Option<u64>) -> Result<Option<Vec<u8>>, String> {
        use base64::Engine as _;

        let response = self.send_command(HelperCommand::ReadPacket {
            tun_name: tun_name.to_string(),
            timeout_ms,
        })?;

        if !response.success {
            return Err(response.message);
        }

        // Check for timeout
        if response.message == "timeout" {
            return Ok(None);
        }

        // Extract packet data from response
        if let Some(data) = response.data {
            if let Some(packet_b64) = data.get("packet").and_then(|p| p.as_str()) {
                let packet = base64::engine::general_purpose::STANDARD
                    .decode(packet_b64)
                    .map_err(|e| format!("Failed to decode packet: {}", e))?;
                return Ok(Some(packet));
            }
        }

        Err("No packet data in response".to_string())
    }

    /// Write a packet to the TUN device
    pub fn write_packet(&mut self, tun_name: &str, data: &[u8]) -> Result<(), String> {
        use base64::Engine as _;

        let data_b64 = base64::engine::general_purpose::STANDARD.encode(data);

        let response = self.send_command(HelperCommand::WritePacket {
            tun_name: tun_name.to_string(),
            data: data_b64,
        })?;

        if response.success {
            Ok(())
        } else {
            Err(response.message)
        }
    }
}

impl Default for HelperClient {
    fn default() -> Self {
        Self::new()
    }
}
