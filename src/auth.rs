//! Capability-token validation: fetch + cache the Hub JWKS, verify EdDSA signatures offline.
//!
//! Every protected endpoint requires a `CapToken` extractor.  The JWKS is refreshed at most once
//! per `JWKS_TTL_SECS` so all validation is **offline** - no per-request Hub call.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine;
use chordia_contracts::auth::{CapabilityAction, CapabilityClaims};
use ed25519_dalek::VerifyingKey;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use tokio::sync::Mutex;

use crate::error::AppError;
use crate::http::AppState;

const JWKS_TTL_SECS: u64 = 300;

struct CacheInner {
    /// kid → (raw 32-byte public key bytes stored as DecodingKey)
    keys: HashMap<String, DecodingKey>,
    refreshed_at: Option<Instant>,
}

pub struct JwksCache {
    hub_url: String,
    client: reqwest::Client,
    inner: Mutex<CacheInner>,
}

impl JwksCache {
    pub fn new(hub_url: String, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            hub_url,
            client,
            inner: Mutex::new(CacheInner {
                keys: HashMap::new(),
                refreshed_at: None,
            }),
        })
    }

    /// Return the `DecodingKey` for `kid`, refreshing the JWKS if stale.
    pub async fn decoding_key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        {
            let inner = self.inner.lock().await;
            if let Some(t) = inner.refreshed_at {
                if t.elapsed() < Duration::from_secs(JWKS_TTL_SECS) {
                    if let Some(k) = inner.keys.get(kid) {
                        return Ok(k.clone());
                    }
                }
            }
        }
        self.refresh().await?;
        let inner = self.inner.lock().await;
        inner
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown kid '{kid}'"))
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        let url = format!(
            "{}/.well-known/jwks.json",
            self.hub_url.trim_end_matches('/')
        );
        let body: serde_json::Value = self.client.get(&url).send().await?.json().await?;
        let keys_arr = body["keys"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("JWKS missing 'keys' array"))?;

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut new_keys = HashMap::new();
        for jwk in keys_arr {
            let kid = jwk["kid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("JWK missing kid"))?;
            let x = jwk["x"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("JWK missing x"))?;
            let raw = b64.decode(x)?;
            if raw.len() != 32 {
                anyhow::bail!("JWK x field is not 32 bytes");
            }
            let vk = VerifyingKey::from_bytes(raw[..32].try_into().unwrap())?;
            // jsonwebtoken rust_crypto EdDSA verifier reads the raw 32-byte public key
            let dk = DecodingKey::from_ed_der(&vk.to_bytes());
            new_keys.insert(kid.to_string(), dk);
        }

        let mut inner = self.inner.lock().await;
        inner.keys = new_keys;
        inner.refreshed_at = Some(Instant::now());
        Ok(())
    }
}

/// Verified capability token, extracted from the `Authorization: Bearer …` header.
pub struct CapToken {
    pub claims: CapabilityClaims,
}

impl FromRequestParts<AppState> for CapToken {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Primary: Authorization: Bearer <token>
        // Fallback: ?token=<token> query param so <audio src="…?token=…"> works in the browser.
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_owned());

        let query_token = parts.uri.query().and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "token" {
                    Some(v.to_owned())
                } else {
                    None
                }
            })
        });

        let bearer = auth_header.or(query_token).ok_or(AppError::Unauthorized)?;
        let bearer = bearer.as_str();

        // Peek at the header to get the `kid` without full verification yet.
        let header = jsonwebtoken::decode_header(bearer).map_err(|_| AppError::Unauthorized)?;
        let kid = header.kid.ok_or(AppError::Unauthorized)?;

        let dk = state
            .jwks
            .decoding_key(&kid)
            .await
            .map_err(|_| AppError::Unauthorized)?;

        let mut validation = Validation::new(Algorithm::EdDSA);
        // exp is stored in epoch-milliseconds; jsonwebtoken expects epoch-seconds by default.
        // We validate exp manually below, so disable the built-in check.
        validation.validate_exp = false;
        // aud is a UUID string; we check it ourselves against the configured server_id.
        validation.validate_aud = false;

        let data = jsonwebtoken::decode::<CapabilityClaims>(bearer, &dk, &validation)
            .map_err(|_| AppError::Unauthorized)?;

        let claims = data.claims;

        // Manual expiry check (millis).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        if claims.exp < now_ms {
            return Err(AppError::Unauthorized);
        }

        // Audience must match this server's server_id (enforced once paired).
        if let Some(ref creds) = *state.credentials.read().await {
            if claims.aud != creds.server_id {
                return Err(AppError::Unauthorized);
            }
        }

        Ok(CapToken { claims })
    }
}

/// Convenience: assert the token authorizes a specific action and return its claims.
pub fn require_action(
    token: &CapToken,
    action: CapabilityAction,
) -> Result<&CapabilityClaims, AppError> {
    if token.claims.action != action {
        return Err(AppError::Forbidden);
    }
    Ok(&token.claims)
}
