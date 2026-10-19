//! OAuth client-credentials token cache. Verda API calls carry a short-lived
//! bearer token obtained from `POST /v1/oauth2/token`; it is fetched lazily,
//! reused until shortly before it expires, and dropped on a 401 so the next
//! call fetches a fresh one.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::error::{error_parts, ApiError};

/// Refresh this long before the token's stated expiry, so a token never
/// expires mid-request.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
struct TokenRequest<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    client_secret: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds.
    expires_in: f64,
}

#[derive(Debug, Clone)]
struct Cached {
    token: String,
    refresh_at: Instant,
}

/// The client credentials plus the current token.
pub struct TokenCache {
    client_id: String,
    client_secret: String,
    cached: Mutex<Option<Cached>>,
}

impl TokenCache {
    pub fn new(client_id: &str, client_secret: &str) -> Self {
        TokenCache {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            cached: Mutex::new(None),
        }
    }

    /// A valid access token, fetching a new one when none is cached or the
    /// cached one is about to expire.
    pub async fn token(&self, http: &reqwest::Client, base_url: &str) -> Result<String, ApiError> {
        let mut cached = self.cached.lock().await;
        if let Some(current) = cached.as_ref() {
            if Instant::now() < current.refresh_at {
                return Ok(current.token.clone());
            }
        }
        let fresh = self.fetch(http, base_url).await?;
        let token = fresh.token.clone();
        *cached = Some(fresh);
        Ok(token)
    }

    /// Forget the cached token (the API rejected it).
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    async fn fetch(&self, http: &reqwest::Client, base_url: &str) -> Result<Cached, ApiError> {
        let resp = http
            .post(format!("{base_url}/v1/oauth2/token"))
            .json(&TokenRequest {
                grant_type: "client_credentials",
                client_id: &self.client_id,
                client_secret: &self.client_secret,
            })
            .send()
            .await
            .map_err(ApiError::from_reqwest)?;
        let status = resp.status();
        let body = resp.text().await.map_err(ApiError::from_reqwest)?;
        if !status.is_success() {
            let (code, message) = error_parts(&body);
            // Bad credentials come back as 400/401; anything else (throttling,
            // outages) is an ordinary API error the core can retry.
            return Err(if matches!(status.as_u16(), 400 | 401 | 403) {
                ApiError::Auth(message)
            } else {
                ApiError::Status {
                    status: status.as_u16(),
                    code,
                    message,
                }
            });
        }
        let parsed: TokenResponse =
            serde_json::from_str(&body).map_err(|e| ApiError::Decode(e.to_string()))?;
        let lifetime = Duration::from_secs_f64(parsed.expires_in.max(0.0));
        Ok(Cached {
            token: parsed.access_token,
            refresh_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN),
        })
    }
}
