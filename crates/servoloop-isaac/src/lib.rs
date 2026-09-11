//! Versioned, authenticated HTTP bridge for a simulation-owned Isaac process.
//! The bridge deliberately does not expose arbitrary USD loading or model control.

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use servoloop_core::{Error, Result};
use servoloop_robot::{CommandReceipt, RobotCommand, RobotDriver, RobotState};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageFrame {
    pub camera: String,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub data_base64: String,
    pub simulation_time_s: f64,
    pub wall_time_unix_ms: u64,
    pub frame: String,
    pub units: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    command_id: String,
    episode_id: u64,
    command: RobotCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandResult {
    accepted: bool,
    outcome: String,
    message: String,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    pub min_version: u16,
    pub max_version: u16,
    pub scene: String,
}

/// HTTP client. A token is supplied out-of-band and never included in Debug or errors.
#[derive(Clone)]
pub struct IsaacDriver {
    client: Client,
    endpoint: String,
    token: Arc<String>,
    state: Arc<Mutex<(u64, u64)>>,
    command_prefix: Arc<String>,
}

impl IsaacDriver {
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::with_timeout(endpoint, token, Duration::from_secs(5))
    }

    pub fn with_timeout(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let endpoint = endpoint.into().trim_end_matches('/').to_owned();
        let url = reqwest::Url::parse(&endpoint)
            .map_err(|_| Error::InvalidInput("bridge endpoint is not a URL".into()))?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if (url.scheme() != "https" && !loopback)
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidInput(
                "remote bridge endpoints must use HTTPS".into(),
            ));
        }
        let token = token.into();
        if token.is_empty() {
            return Err(Error::InvalidInput("bridge token is empty".into()));
        }
        Ok(Self {
            client: Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| Error::Model(e.to_string()))?,
            endpoint,
            token: Arc::new(token),
            state: Arc::new(Mutex::new((0, 0))),
            command_prefix: Arc::new(format!("cmd-{}-{}", std::process::id(), uuid_like())),
        })
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let request_id = format!("req-{}", uuid_like());
        let envelope = body.map(|body| serde_json::json!({"schema_version": PROTOCOL_VERSION, "request_id": request_id, "body": body}));
        let mut req = self
            .client
            .request(method, format!("{}{}", self.endpoint, path))
            .bearer_auth(self.token.as_str())
            .header("accept", "application/json");
        if let Some(v) = envelope {
            req = req.json(&v);
        }
        let response = req
            .send()
            .await
            .map_err(|e| Error::Reconciliation(format!("bridge request failed: {e}")))?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::Model("bridge authentication failed".into()));
        }
        if !response.status().is_success() {
            return Err(Error::Reconciliation(format!(
                "bridge returned HTTP {}",
                response.status()
            )));
        }
        if response.content_length().unwrap_or(0) > MAX_RESPONSE_BYTES as u64 {
            return Err(Error::InvalidInput(
                "bridge response exceeds size limit".into(),
            ));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Reconciliation(e.to_string()))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(Error::InvalidInput(
                    "bridge response exceeds size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::Reconciliation(format!("invalid bridge response: {e}")))
    }
    pub async fn handshake(&self, scene: &str) -> Result<Handshake> {
        let result: Handshake = self.request(reqwest::Method::POST, "/v1/handshake", Some(serde_json::json!({"min_version": PROTOCOL_VERSION, "max_version": PROTOCOL_VERSION, "scene": scene}))).await?;
        if result.min_version > PROTOCOL_VERSION || result.max_version < PROTOCOL_VERSION {
            return Err(Error::InvalidInput(
                "bridge does not support protocol version 1".into(),
            ));
        }
        Ok(result)
    }
    pub async fn reset(&self) -> Result<()> {
        let result: serde_json::Value = self
            .request(reqwest::Method::POST, "/v1/episode/reset", None)
            .await?;
        let episode = result
            .get("episode_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Error::Reconciliation("bridge reset omitted episode_id".into()))?;
        self.state.lock().await.1 = episode;
        Ok(())
    }
    pub async fn step(&self, steps: u32) -> Result<()> {
        if !(1..=1000).contains(&steps) {
            return Err(Error::InvalidInput("step count must be 1..=1000".into()));
        }
        let result: serde_json::Value = self
            .request(
                reqwest::Method::POST,
                "/v1/simulation/step",
                Some(serde_json::json!({"steps":steps})),
            )
            .await?;
        if result
            .get("steps")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(Error::Reconciliation("bridge step omitted steps".into()));
        }
        Ok(())
    }
    pub async fn camera(&self, camera: &str) -> Result<ImageFrame> {
        if camera.is_empty() || camera.contains('/') || camera.contains('?') {
            return Err(Error::InvalidInput("invalid camera name".into()));
        }
        let frame: ImageFrame = self
            .request(reqwest::Method::GET, &format!("/v1/cameras/{camera}"), None)
            .await?;
        if frame.camera != camera
            || frame.width == 0
            || frame.height == 0
            || frame.data_base64.is_empty()
        {
            return Err(Error::Reconciliation("invalid camera response".into()));
        }
        Ok(frame)
    }
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[async_trait]
impl RobotDriver for IsaacDriver {
    async fn observe(&self) -> Result<RobotState> {
        self.request(reqwest::Method::GET, "/v1/observation", None)
            .await
    }
    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
        let mut state = self.state.lock().await;
        state.0 += 1;
        let result: CommandResult = self
            .request(
                reqwest::Method::POST,
                "/v1/commands",
                Some(
                    serde_json::to_value(CommandBody {
                        command_id: format!("{}-{}", self.command_prefix, state.0),
                        episode_id: state.1,
                        command,
                    })
                    .unwrap(),
                ),
            )
            .await?;
        if !result.accepted || result.outcome != "completed" {
            return Err(Error::Reconciliation(
                "bridge did not complete command".into(),
            ));
        }
        Ok(CommandReceipt {
            accepted: result.accepted,
            message: format!("{} ({})", result.message, result.outcome),
            metadata: result.metadata,
        })
    }
    async fn stop(&self) -> Result<()> {
        let result: serde_json::Value = self
            .request(reqwest::Method::POST, "/v1/stop", None)
            .await?;
        if result.get("stopped") != Some(&serde_json::Value::Bool(true)) {
            return Err(Error::Reconciliation(
                "bridge did not acknowledge stop".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_public_plain_http() {
        assert!(IsaacDriver::new("http://0.0.0.0:8000", "x").is_err());
    }
    #[test]
    fn accepts_loopback() {
        assert!(IsaacDriver::new("http://127.0.0.1:8000", "x").is_ok());
    }
}
