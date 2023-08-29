use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, ClientId, ClientSecret, Scope, TokenResponse, TokenUrl};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub(crate) async fn new_tailscale_api(
    client_id: String,
    client_secret: String,
) -> anyhow::Result<TailscaleApi> {
    let client = BasicClient::new(
        ClientId::new(client_id),
        Some(ClientSecret::new(client_secret)),
        AuthUrl::new("https://api.tailscale.com/api/v2/oauth/token".to_string())?,
        Some(TokenUrl::new(
            "https://api.tailscale.com/api/v2/oauth/token".to_string(),
        )?),
    );

    // Ensure the initial token is expired to force a new one to be created on first use.
    let token_info = TailscaleTokenInfo {
        token: "fake".to_string(),
        expires_at: Instant::now() - Duration::from_secs(1),
    };

    Ok(TailscaleApi {
        oauth_client: client,
        previous_token_response: RwLock::new(token_info),
        http_client: reqwest::Client::new(),
    })
}

pub struct TailscaleApi {
    oauth_client: BasicClient,
    previous_token_response: RwLock<TailscaleTokenInfo>,
    http_client: reqwest::Client,
}

impl TailscaleApi {
    pub async fn create_auth_token(&self, tags: Vec<String>) -> anyhow::Result<String> {
        let auth_token = self.get_token().await?;

        let body = CreateTokenRequest {
            expiry_seconds: Some(60 * 10),
            description: None,
            capabilities: CreateTokenRequestCapabilities {
                devices: CreateTokenRequestCapabilitiesDevices {
                    create: CreateTokenRequestCapabilitiesDevicesCreate {
                        ephemeral: false,
                        reusable: false,
                        preauthorized: true,
                        tags,
                    },
                },
            },
        };

        let response = self
            .http_client
            .post("https://api.tailscale.com/api/v2/tailnet/-/keys")
            .bearer_auth(auth_token)
            .json(&body)
            .send()
            .await?;

        if response.status() == StatusCode::OK {
            let response: DeviceTokenResponse = response.json().await?;

            info!("Created tailscale device token: {:?}", response);

            Ok(response.key)
        } else {
            let error_message = response.text().await?;

            Err(anyhow::anyhow!(
                "Failed to create tailscale device token: {:?}",
                error_message
            ))
        }
    }

    async fn get_token(&self) -> anyhow::Result<String> {
        let res = self.previous_token_response.read().await;

        if res.expires_at > Instant::now() {
            Ok(res.token.clone())
        } else {
            drop(res);

            let mut res = self.previous_token_response.write().await;

            if res.expires_at < Instant::now() {
                let token = self
                    .oauth_client
                    .exchange_client_credentials()
                    .add_scope(Scope::new("devices:write".to_string()))
                    .request_async(async_http_client)
                    .await?;

                let duration = Duration::from_secs_f32(
                    token
                        .expires_in()
                        .map(|e| e.as_secs_f32() * 0.9)
                        .unwrap_or(1800f32),
                );
                let token_info = TailscaleTokenInfo {
                    token: token.access_token().secret().clone(),
                    expires_at: Instant::now() + duration,
                };

                *res = token_info;
            }
            Ok(res.token.clone())
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateTokenRequest {
    capabilities: CreateTokenRequestCapabilities,
    #[serde(rename = "expirySeconds")]
    expiry_seconds: Option<i32>,
    description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateTokenRequestCapabilities {
    devices: CreateTokenRequestCapabilitiesDevices,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateTokenRequestCapabilitiesDevices {
    create: CreateTokenRequestCapabilitiesDevicesCreate,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateTokenRequestCapabilitiesDevicesCreate {
    reusable: bool,
    ephemeral: bool,
    preauthorized: bool,
    tags: Vec<String>,
}

struct TailscaleTokenInfo {
    token: String,
    expires_at: Instant,
}

#[derive(Debug, Serialize, Deserialize)]

struct DeviceTokenResponse {
    key: String,
    capabilities: CreateTokenRequestCapabilities,
}
