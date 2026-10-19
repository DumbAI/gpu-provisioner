//! RunPod HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The v2 API reports errors as `ErrorResponse { title, status, detail,
//! errors[] }` without machine-readable capacity codes, so capacity, quota
//! and billing problems are recognized by keywords in the message. Nothing
//! is retried here — throttling and server errors come back as `Transient`
//! and the shared core retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Billing problems: account-wide and not fixed by another offer, so they end
/// the launch with a clear message instead of walking every data center.
/// Checked first, since "insufficient funds" would otherwise read as capacity.
const BALANCE_KEYWORDS: &[&str] = &[
    "insufficient funds",
    "insufficient balance",
    "balance too low",
    "low balance",
    "spend limit",
];

/// Account limits (checked before capacity: "limit exceeded" is not stock).
const QUOTA_KEYWORDS: &[&str] = &["quota", "limit exceeded"];

/// The requested GPU / data center has no free machines right now.
const CAPACITY_KEYWORDS: &[&str] = &[
    "no longer any instances available",
    "no available",
    "not enough",
    "insufficient",
    "unavailable",
    "capacity",
    "out of stock",
];

/// Which API call failed — decides what an unclassified 4xx means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Catalog lookups at plan time: a rejected query is the user's ask
    /// (e.g. a malformed country code) → `InvalidRequest`.
    Plan,
    /// Stock re-check / pod creation: nothing was created → `Unavailable`.
    Launch,
    /// Listing or deleting pods, possibly after creation → `Failed`.
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
                    ProviderError::transient(format!("RunPod API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("RunPod API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected RunPod API response: {message}"))
            }
        }
    }
}

/// Map an HTTP status + error message to a `ProviderError`.
pub fn classify(status: u16, message: &str, op: Op) -> ProviderError {
    let msg = format!("RunPod API {status}: {message}");
    if status == 401 || status == 403 {
        return ProviderError::credentials(format!(
            "{msg} (check the API key and its permissions at \
             https://www.runpod.io/console/user/settings)"
        ));
    }
    if status == 429 || status >= 500 {
        return ProviderError::transient(msg);
    }
    let lower = message.to_lowercase();
    if BALANCE_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::invalid(format!("RunPod account balance too low to launch: {msg}"));
    }
    if QUOTA_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::quota(msg);
    }
    if op == Op::Launch && CAPACITY_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::capacity(msg);
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// Extract a human-readable message from an `ErrorResponse` body (`detail`,
/// plus any validation `errors`), falling back to `title` or the raw
/// (truncated) text.
pub fn error_message(body: &str) -> String {
    let fallback = || {
        let mut text = body.trim().to_string();
        text.truncate(400);
        if text.is_empty() {
            "no error details".to_string()
        } else {
            text
        }
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return fallback();
    };
    let detail = json
        .get("detail")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| json.get("title").and_then(|v| v.as_str()))
        .map(str::to_string);
    let errors: Vec<String> = json
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    match (detail, errors.is_empty()) {
        (Some(detail), true) => detail,
        (Some(detail), false) => format!("{detail} ({})", errors.join("; ")),
        (None, false) => errors.join("; "),
        (None, true) => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BlockScope;

    #[test]
    fn auth_and_server_errors() {
        assert_eq!(
            classify(401, "bad key", Op::Plan).kind,
            ErrorKind::Credentials
        );
        assert_eq!(classify(403, "no", Op::Manage).kind, ErrorKind::Credentials);
        for status in [429, 500, 502, 503] {
            assert_eq!(classify(status, "x", Op::Launch).kind, ErrorKind::Transient);
        }
    }

    #[test]
    fn capacity_quota_and_balance_on_create() {
        let capacity = classify(
            409,
            "There are no longer any instances available with the requested specifications.",
            Op::Launch,
        );
        assert_eq!(capacity.kind, ErrorKind::Capacity);
        assert_eq!(capacity.block_scope(), BlockScope::Offer);

        let quota = classify(400, "GPU quota exceeded for this account", Op::Launch);
        assert_eq!(quota.kind, ErrorKind::Quota);
        assert_eq!(quota.block_scope(), BlockScope::Region);

        // "insufficient funds" is billing, not stock — and it is fatal.
        let broke = classify(402, "Insufficient funds to deploy this pod", Op::Launch);
        assert_eq!(broke.kind, ErrorKind::InvalidRequest);
        assert!(broke.msg.contains("balance too low"), "{}", broke.msg);
    }

    #[test]
    fn unclassified_4xx_depends_on_the_call() {
        assert_eq!(
            classify(422, "bad", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            classify(400, "bad", Op::Launch).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(classify(404, "bad", Op::Manage).kind, ErrorKind::Failed);
        // Capacity wording outside a launch is not a capacity signal.
        assert_eq!(
            classify(400, "unavailable", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn error_message_reads_problem_details() {
        assert_eq!(
            error_message(r#"{"title":"Not Found","status":404,"detail":"pod not found"}"#),
            "pod not found"
        );
        assert_eq!(
            error_message(
                r#"{"title":"Bad Request","status":422,"detail":"invalid body","errors":["$: additional properties 'bogus' not allowed"]}"#
            ),
            "invalid body ($: additional properties 'bogus' not allowed)"
        );
        assert_eq!(error_message(r#"{"title":"Conflict"}"#), "Conflict");
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "no error details");
    }
}
