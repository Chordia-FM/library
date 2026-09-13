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

/// Floor between outbound JWKS fetches, independent of whether the last one helped.
///
/// Without it, a cache miss always meant a network call, and the `kid` is attacker-chosen: an
/// unauthenticated caller could send tokens carrying random `kid`s and turn each one into a request
/// from this library to the Hub. That is an amplified DoS against the Hub, from any host that can
/// reach a library.
const JWKS_MIN_REFRESH_SECS: u64 = 10;

struct CacheInner {
    /// kid → (raw 32-byte public key bytes stored as DecodingKey)
    keys: HashMap<String, DecodingKey>,
    refreshed_at: Option<Instant>,
    /// When a fetch was last *attempted*, successful or not. Distinct from `refreshed_at`, which
    /// only moves on success — a failing Hub must not license unlimited retries either.
    last_attempt: Option<Instant>,
}

pub struct JwksCache {
    /// Absent when this library runs with no Hub, which makes every Hub-signed token unverifiable
    /// and therefore rejected. See `Config::backend_url`.
    hub_url: Option<String>,
    client: reqwest::Client,
    inner: Mutex<CacheInner>,
}

impl JwksCache {
    pub fn new(hub_url: Option<String>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            hub_url,
            client,
            inner: Mutex::new(CacheInner {
                keys: HashMap::new(),
                refreshed_at: None,
                last_attempt: None,
            }),
        })
    }

    /// Return the `DecodingKey` for `kid`, refreshing the JWKS if stale.
    ///
    /// A miss only reaches the network when no fetch has been attempted in the last
    /// `JWKS_MIN_REFRESH_SECS`. Because the attempt is stamped *before* the lock is released, a
    /// burst of concurrent misses also collapses to a single in-flight fetch rather than one per
    /// request.
    pub async fn decoding_key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        {
            let mut inner = self.inner.lock().await;
            if let Some(t) = inner.refreshed_at {
                if t.elapsed() < Duration::from_secs(JWKS_TTL_SECS) {
                    if let Some(k) = inner.keys.get(kid) {
                        return Ok(k.clone());
                    }
                }
            }
            if let Some(t) = inner.last_attempt {
                if t.elapsed() < Duration::from_secs(JWKS_MIN_REFRESH_SECS) {
                    // Someone just looked and this kid still is not there. Answer from what we have
                    // rather than asking the Hub again.
                    return inner
                        .keys
                        .get(kid)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("unknown kid '{kid}'"));
                }
            }
            inner.last_attempt = Some(Instant::now());
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
        // No Hub means no signing keys, which means no Hub-issued capability token can ever
        // validate here. That is the correct outcome rather than a degraded one: a library running
        // standalone has no relationship in which such a token could have been minted.
        let hub_url = self.hub_url.as_deref().ok_or_else(|| {
            anyhow::anyhow!("this library is configured with no Hub (backend_url)")
        })?;
        let url = format!("{}/.well-known/jwks.json", hub_url.trim_end_matches('/'));
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

/// The credential a Hub-less library accepts in place of a capability token.
///
/// A capability token is a statement by the Hub that a particular user may do a particular thing to
/// a particular library. With no Hub there is nobody to make that statement — and nobody who needs
/// it: an embedded library is started by the application that is about to read from it, inside the
/// same process, on the user's own machine, over a socket bound to loopback.
///
/// So the credential is not a claim about anyone. It is a secret minted at boot, handed to the one
/// client that is entitled to it, and never written to disk. Losing the process loses the token,
/// which is exactly right — it authorises nothing beyond the run that created it.
///
/// The three conditions that must ALL hold before it is honoured (see the extractor below) are the
/// whole of its security: the session exists at all (only [`crate::embedded`] creates one), the
/// request arrived over loopback, and the presented bytes match.
pub struct LocalSession {
    token: String,
}

impl LocalSession {
    /// A fresh 256-bit secret. Not derived from anything, not persisted, not recoverable.
    pub fn generate() -> Arc<Self> {
        use rand::distributions::Alphanumeric;
        use rand::Rng;
        Arc::new(Self {
            token: rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(43)
                .map(char::from)
                .collect(),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Compare without leaking where the two differ.
    ///
    /// The length difference is not hidden and does not need to be: the length is a constant of
    /// this code, not a property of the secret.
    pub fn matches(&self, presented: &str) -> bool {
        let (a, b) = (self.token.as_bytes(), presented.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

/// Whether this request came from the machine it is running on.
///
/// Belt and braces. An embedded library binds to `127.0.0.1` and so cannot receive a request from
/// anywhere else — but the local session is the one credential in this server that is not a signed
/// statement about anybody, and it should stop being honoured the moment that assumption stops
/// holding, not the moment somebody notices. A request with no peer address recorded fails closed.
fn from_loopback(parts: &Parts) -> bool {
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .is_some_and(|info| info.0.ip().is_loopback())
}

/// Verified capability token, extracted from the `Authorization: Bearer …` header.
pub struct CapToken {
    pub claims: CapabilityClaims,
    /// True when this is the embedded [`LocalSession`] rather than a Hub-signed capability.
    ///
    /// Read by [`require_action`] and by the stream handler's library-scope check, both of which
    /// are asking a question — "may this user do this to this library?" — that only has an answer
    /// where a Hub exists to have answered it.
    pub local: bool,
}

/// The claims a local session stands in for.
///
/// Deliberately nil rather than plausible: nothing may branch on these. Every consumer that would
/// have read them is required to check `local` first, and a nil `library_id` matching no row is the
/// backstop if one ever forgets.
fn local_claims() -> CapabilityClaims {
    use chordia_contracts::auth::ResourceRef;
    use chordia_contracts::library::PermissionLevel;
    CapabilityClaims {
        sub: uuid::Uuid::nil(),
        aud: uuid::Uuid::nil(),
        library_id: uuid::Uuid::nil(),
        resource: ResourceRef::Library {
            library_id: uuid::Uuid::nil(),
        },
        action: CapabilityAction::StreamRead,
        permission_level: PermissionLevel::Download,
        room_id: None,
        jti: uuid::Uuid::nil(),
        iat: 0,
        exp: 0,
        kid: String::new(),
    }
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

        // The embedded path, checked first because it is the cheaper of the two and because in
        // embedded mode the JWKS path below cannot succeed anyway (there is no Hub to fetch keys
        // from). All three conditions have to hold: a session exists, the request came from this
        // machine, and the bytes match. `local_session` is `None` in every configuration except
        // `crate::embedded`, so for a normal server this is one `Option` check and out.
        if let Some(session) = &state.local_session {
            if from_loopback(parts) && session.matches(bearer) {
                return Ok(CapToken {
                    claims: local_claims(),
                    local: true,
                });
            }
        }

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

        Ok(CapToken {
            claims,
            local: false,
        })
    }
}

/// A credential an embedded library requires and a standalone one does not.
///
/// One endpoint genuinely has to answer without a credential on a real server: `/v1/ping` is what
/// the pairing wizard probes to learn a self-signed library's certificate fingerprint and whether it
/// is already paired, and at that moment no credential exists to present.
///
/// None of that is true in the desktop app. There is no wizard, nothing external is meant to find
/// the loopback port, and the body — paired status and folder count — is precisely the fingerprint a
/// web page would scan the ephemeral range for before asking what music is on the disk. So in
/// embedded mode the probe wants the session token like everything else.
pub struct EmbeddedSession;

impl FromRequestParts<AppState> for EmbeddedSession {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if state.local_session.is_some() {
            CapToken::from_request_parts(parts, state).await?;
        }
        Ok(Self)
    }
}

/// Convenience: assert the token authorizes a specific action and return its claims.
///
/// A local session is exempt, and the exemption is the point rather than a loophole: capability
/// actions are how the Hub narrows what one user may do to another user's library, and an embedded
/// library has exactly one user, who owns it, on their own machine. There is no narrower grant for
/// the Hub to have issued and nobody to have issued it.
pub fn require_action(
    token: &CapToken,
    action: CapabilityAction,
) -> Result<&CapabilityClaims, AppError> {
    if !token.local && token.claims.action != action {
        return Err(AppError::Forbidden);
    }
    Ok(&token.claims)
}
