//! Hyperstack HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! Errors arrive as `{ status: false, message, error_reason }` with no
//! machine-readable code, so classification keys on the HTTP status first
//! and on well-known phrases in the message second. Nothing is retried here
//! — throttling and server errors come back as `Transient` and the shared
//! core retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Which API call failed — decides what an otherwise unclassified error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Catalog lookups at plan time: a rejected query is the user's ask →
    /// `InvalidRequest`.
    Plan,
    /// Per-launch prerequisites (environment, key pair, image, stock
    /// re-check): nothing was launched → `Unavailable`.
    Prepare,
    /// `POST /core/virtual-machines`: stock-out phrasing is a capacity
    /// signal; other rejections launched nothing.
    Launch,
    /// Listing or deleting VMs, possibly after creation → `Failed`; a `409`
    /// (the VM is mid-transition) is worth retrying.
    Manage,
}

/// A failed API call, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered with a non-success status.
    Status { status: u16, message: String },
    /// The request never got a usable answer (connect error, timeout).
    Transport { message: String, transient: bool },
    /// The response body did not match the expected schema.
    Decode(String),
}

impl ApiError {
    pub fn from_reqwest(err: reqwest::Error) -> Self {
        let transient = err.is_timeout() || err.is_connect() || err.is_request();
        ApiError::Transport {
            message: err.to_string(),
            transient,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Classify into the failover taxonomy for a call of kind `op`.
    pub fn into_provider_error(self, op: Op) -> ProviderError {
        match self {
            ApiError::Status { status, message } => classify(status, &message, op),
            ApiError::Transport { message, transient } => {
                if transient {
                    ProviderError::transient(format!("Hyperstack API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Hyperstack API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected Hyperstack API response: {message}"))
            }
        }
    }
}

/// Phrases meaning "this flavor has no free hardware here right now".
const CAPACITY_PHRASES: &[&str] = &[
    "capacity",
    "out of stock",
    "no stock",
    "not enough",
    "no available",
    "not available",
    "unavailable",
    "insufficient resources",
];

/// Phrases meaning an account limit was hit (blocks the whole region).
const QUOTA_PHRASES: &[&str] = &["quota", "limit exceeded", "maximum number"];

/// Phrases meaning the account cannot pay — no other offer fixes that.
const BILLING_PHRASES: &[&str] = &[
    "insufficient credit",
    "insufficient balance",
    "insufficient funds",
    "low balance",
    "add credit",
    "payment",
];

/// Map an HTTP status + message to a `ProviderError`.
pub fn classify(status: u16, message: &str, op: Op) -> ProviderError {
    let msg = format!("Hyperstack API {status}: {message}");
    let lower = message.to_lowercase();
    let has = |phrases: &[&str]| phrases.iter().any(|p| lower.contains(p));
    if status == 401 || status == 403 {
        return ProviderError::credentials(format!(
            "{msg} (check the API key and its permissions in the Hyperstack console)"
        ));
    }
    // Checked before the capacity phrases: "insufficient balance" is not a
    // stock-out, and walking every other region would not help.
    if has(BILLING_PHRASES) {
        return ProviderError::invalid(format!(
            "Hyperstack account balance too low to launch: {msg}"
        ));
    }
    if op == Op::Launch {
        if has(QUOTA_PHRASES) {
            return ProviderError::quota(msg);
        }
        if has(CAPACITY_PHRASES) {
            return ProviderError::capacity(msg);
        }
    }
    if status == 429 || status >= 500 {
        return ProviderError::transient(msg);
    }
    if status == 409 {
        return match op {
            Op::Manage => ProviderError::transient(msg),
            _ => ProviderError::failed(msg),
        };
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Prepare | Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// The human-readable part of an error body: `message` (plus
/// `error_reason` when it adds detail), else the raw (truncated) text.
pub fn error_message(body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error_reason: Option<String>,
    }
    if let Ok(parsed) = serde_json::from_str::<Body>(body) {
        let message = parsed.message.filter(|m| !m.is_empty());
        let reason = parsed.error_reason.filter(|r| !r.is_empty());
        match (message, reason) {
            (Some(m), Some(r)) if !m.contains(&r) => return format!("{m} ({r})"),
            (Some(m), _) => return m,
            (None, Some(r)) => return r,
            (None, None) => {}
        }
    }
    let mut text = body.trim().to_string();
    text.truncate(400);
    if text.is_empty() {
        "no error details".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BlockScope;

    #[test]
    fn auth_and_billing_end_the_launch() {
        assert_eq!(
            classify(401, "invalid api key", Op::Plan).kind,
            ErrorKind::Credentials
        );
        assert_eq!(
            classify(403, "forbidden", Op::Launch).kind,
            ErrorKind::Credentials
        );
        let billing = classify(400, "Insufficient credit to deploy VM", Op::Launch);
        assert_eq!(billing.kind, ErrorKind::InvalidRequest);
        assert!(billing.msg.contains("balance too low"), "{}", billing.msg);
    }

    #[test]
    fn launch_phrases_map_to_capacity_and_quota() {
        let stock = classify(400, "Flavor is out of stock in this region", Op::Launch);
        assert_eq!(stock.kind, ErrorKind::Capacity);
        let quota = classify(400, "GPU quota exceeded for organization", Op::Launch);
        assert_eq!(quota.kind, ErrorKind::Quota);
        assert_eq!(quota.block_scope(), BlockScope::Region);
        // The same words outside a launch are not stock signals.
        assert_eq!(
            classify(400, "out of stock", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn status_fallbacks_by_operation() {
        assert_eq!(
            classify(429, "slow down", Op::Plan).kind,
            ErrorKind::Transient
        );
        assert_eq!(
            classify(502, "bad gateway", Op::Launch).kind,
            ErrorKind::Transient
        );
        assert_eq!(classify(409, "busy", Op::Manage).kind, ErrorKind::Transient);
        assert_eq!(
            classify(409, "name taken", Op::Launch).kind,
            ErrorKind::Failed
        );
        assert_eq!(
            classify(404, "flavor not found", Op::Launch).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            classify(400, "bad", Op::Prepare).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(classify(400, "bad", Op::Manage).kind, ErrorKind::Failed);
    }

    #[test]
    fn error_bodies_are_summarized() {
        assert_eq!(
            error_message(
                r#"{"status":false,"message":"Bad flavor","error_reason":"no such flavor"}"#
            ),
            "Bad flavor (no such flavor)"
        );
        assert_eq!(
            error_message(r#"{"status":false,"message":"Gone"}"#),
            "Gone"
        );
        assert_eq!(error_message("<html>oops</html>"), "<html>oops</html>");
        assert_eq!(error_message(""), "no error details");
    }
}
