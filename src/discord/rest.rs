//! A thin Discord REST client for the few calls that must **not** wait out a rate limit.
//!
//! serenity's `Http` handles Discord's rate limits by sleeping and retrying, which is exactly right
//! for a message edit and exactly wrong for uploading 26 emojis or changing the bot's avatar:
//! those buckets are small (avatar changes are a handful per hour), a 429 can say "come back in
//! forty minutes", and a management request cannot sit in a sleep for that long. This client makes
//! one attempt and returns the retry-after as data, so the caller can persist it, warn the owner,
//! and try again from a timer.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

const API: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "DiscordBot (https://github.com/chordia-fm/library, 0.1.0)";

#[derive(Debug, thiserror::Error)]
pub enum RestError {
    /// Discord asked us to wait. `global` means every route, not just this one.
    #[error("rate limited by Discord; retry after {}s", retry_after.as_secs())]
    RateLimited { retry_after: Duration, global: bool },
    #[error("Discord answered {status}: {message}")]
    Status { status: u16, message: String },
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

pub struct Rest {
    http: reqwest::Client,
    token: String,
}

impl Rest {
    pub fn new(http: reqwest::Client, token: impl Into<String>) -> Self {
        Self {
            http,
            token: token.into(),
        }
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, RestError> {
        let mut req = self
            .http
            .request(method, format!("{API}{path}"))
            .header("Authorization", format!("Bot {}", self.token))
            .header("User-Agent", USER_AGENT);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.as_u16() == 429 {
            let header_secs = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok());
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            let secs = body
                .get("retry_after")
                .and_then(Value::as_f64)
                .or(header_secs)
                .unwrap_or(60.0);
            return Err(RestError::RateLimited {
                retry_after: Duration::from_secs_f64(secs.max(1.0)),
                global: body.get("global").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            return Err(RestError::Status {
                status: status.as_u16(),
                message,
            });
        }
        if status.as_u16() == 204 {
            // No body: the caller asked for `()` (serde deserializes unit from `null`).
            return serde_json::from_value(Value::Null).map_err(|e| RestError::Status {
                status: 204,
                message: e.to_string(),
            });
        }
        Ok(resp.json::<T>().await?)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, RestError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, RestError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, RestError> {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<(), RestError> {
        self.send::<()>(reqwest::Method::DELETE, path, None).await
    }
}
