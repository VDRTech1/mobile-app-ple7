use serde::{Deserialize, Serialize};
use tauri::State;

use crate::tunnel::AppState;

pub struct ApiClient {
    pub base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub plan: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(rename = "mfaEnabled", default)]
    pub mfa_enabled: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LoginResult {
    Success {
        #[serde(rename = "access_token")]
        token: String,
        user: User,
    },
    MfaRequired {
        #[serde(rename = "requiresMfa")]
        requires_mfa: bool,
        #[serde(rename = "userId")]
        user_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub user: User,
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub ip_range: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub ip_address: String,
    pub public_key: String,
    pub is_online: bool,
    pub is_exit_node: bool,
    pub platform: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub config: String,
    #[serde(rename = "hasPrivateKey")]
    pub has_private_key: bool,
}

impl ApiClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: reqwest::Client::new(),
        }
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<LoginResponse, String> {
        let response = self
            .client
            .post(format!("{}/api/auth/login", self.base_url))
            .json(&serde_json::json!({
                "email": email,
                "password": password
            }))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(format!("Login failed: {}", error_text));
        }

        let result = response
            .json::<LoginResult>()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        match result {
            LoginResult::Success { token, user } => Ok(LoginResponse { user, token }),
            LoginResult::MfaRequired { .. } => {
                Err("MFA is enabled. Please use the web app to login with MFA.".to_string())
            }
        }
    }

    pub async fn verify_token(&self, token: &str) -> Result<User, String> {
        let response = self
            .client
            .get(format!("{}/api/auth/me", self.base_url))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            return Err("Invalid or expired token".to_string());
        }

        response
            .json::<User>()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    }

    pub async fn get_networks(&self, token: &str) -> Result<Vec<Network>, String> {
        let response = self
            .client
            .get(format!("{}/api/mesh/networks", self.base_url))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            return Err("Failed to fetch networks".to_string());
        }

        response
            .json::<Vec<Network>>()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    }

    pub async fn get_devices(&self, token: &str, network_id: &str) -> Result<Vec<Device>, String> {
        let response = self
            .client
            .get(format!(
                "{}/api/mesh/networks/{}/devices",
                self.base_url, network_id
            ))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            return Err("Failed to fetch devices".to_string());
        }

        response
            .json::<Vec<Device>>()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    }

    pub async fn get_device_config(
        &self,
        token: &str,
        device_id: &str,
    ) -> Result<DeviceConfig, String> {
        let response = self
            .client
            .get(format!(
                "{}/api/mesh/devices/{}/config",
                self.base_url, device_id
            ))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            return Err("Failed to fetch device config".to_string());
        }

        response
            .json::<DeviceConfig>()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    }
}

// Tauri commands
#[tauri::command]
pub async fn login(
    state: State<'_, AppState>,
    email: String,
    password: String,
) -> Result<LoginResponse, String> {
    state.api_client.login(&email, &password).await
}

#[tauri::command]
pub async fn verify_token(state: State<'_, AppState>, token: String) -> Result<User, String> {
    state.api_client.verify_token(&token).await
}

#[tauri::command]
pub async fn get_networks(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<Network>, String> {
    let token = crate::config::get_stored_token_internal(&app).await?;
    state.api_client.get_networks(&token).await
}

#[tauri::command]
pub async fn get_devices(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    network_id: String,
) -> Result<Vec<Device>, String> {
    let token = crate::config::get_stored_token_internal(&app).await?;
    state.api_client.get_devices(&token, &network_id).await
}

#[tauri::command]
pub async fn get_device_config(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
) -> Result<DeviceConfig, String> {
    let token = crate::config::get_stored_token_internal(&app).await?;
    state.api_client.get_device_config(&token, &device_id).await
}
