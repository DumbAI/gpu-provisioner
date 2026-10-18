//! Prime Intellect HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The API has no machine-readable capacity codes, so capacity and quota are
//! recognized by keywords in the error message. Nothing is retried here —
//! throttling and server errors come back as `Transient` and the shared core
//! retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Message fragments marking an account limit (checked before capacity, since
/// "limit exceeded" would otherwise read as generic unavailability).
const QUOTA_KEYWORDS: &[&str] = &["quota exceeded", "limit exceeded"];

/// Message keywords that mean the offer has no capacity right now.
const CAPACITY_KEYWORDS: &[&str] = &[
    "no capacity",
    "capacity",
    "unavailable",
    "out of stock",
    "insufficient",
    "not available",
];

/// Which API call failed — decides what an unclassified 4xx means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Availability lookup at plan time: a rejected query is the user's ask
    /// (e.g. an unknown region id) → `InvalidRequest`.
    Plan,
    /// Key registration / offer re-validation / pod creation: nothing was
    /// created → `Unavailable`.
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
                    ProviderError::transient(format!("Prime Intellect API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Prime Intellect API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => ProviderError::failed(format!(
                "unexpected Prime Intellect API response: {message}"
            )),
        }
    }
}

/// Map an HTTP status + error message to a `ProviderError`.
pub fn classify(status: u16, message: &str, op: Op) -> ProviderError {
    let msg = format!("Prime Intellect API {status}: {message}");
    if status == 401 || status == 403 {
        return ProviderError::credentials(format!(
            "{msg} (check the API key's permissions, generate a new one at \
             https://app.primeintellect.ai/dashboard/tokens, or run `prime login`)"
        ));
    }
    if status == 429 || status >= 500 {
        return ProviderError::transient(msg);
    }
    let lower = message.to_lowercase();
    if QUOTA_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::quota(msg);
    }
    if CAPACITY_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::capacity(msg);
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// Extract a human-readable message from an error body: `message`, `error`,
/// `detail` (string or FastAPI-style list), or 422 `errors[].details`;
/// falls back to the raw (truncated) text.
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
    for key in ["message", "error"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    match json.get("detail") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => return s.clone(),
        Some(serde_json::Value::Array(items)) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|i| i.get("msg").and_then(|m| m.as_str()).map(str::to_string))
                .collect();
            if !parts.is_empty() {
                return parts.join("; ");
            }
        }
        _ => {}
    }
    if let Some(errors) = json.get("errors").and_then(|v| v.as_array()) {
        let parts: Vec<String> = errors
            .iter()
            .filter_map(|e| {
                let details = e.get("details").and_then(|d| d.as_str())?;
                Some(match e.get("param").and_then(|p| p.as_str()) {
                    Some(param) => format!("{param}: {details}"),
                    None => details.to_string(),
                })
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("; ");
        }
    }
    fallback()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_statuses_are_credentials() {
        for status in [401, 403] {
            let e = classify(status, "nope", Op::Launch);
            assert_eq!(e.kind, ErrorKind::Credentials);
            assert!(e.msg.contains("prime login"));
        }
    }

    #[test]
    fn throttling_and_server_errors_are_transient() {
        assert_eq!(
            classify(429, "slow down", Op::Launch).kind,
            ErrorKind::Transient
        );
        assert_eq!(classify(503, "", Op::Manage).kind, ErrorKind::Transient);
    }

    #[test]
    fn keywords_select_capacity_and_quota() {
        assert_eq!(
            classify(400, "No capacity for this GPU type", Op::Launch).kind,
            ErrorKind::Capacity
        );
        assert_eq!(
            classify(400, "Out of stock", Op::Launch).kind,
            ErrorKind::Capacity
        );
        assert_eq!(
            classify(400, "Team quota exceeded", Op::Launch).kind,
            ErrorKind::Quota
        );
        assert_eq!(
            classify(400, "Spend limit exceeded", Op::Launch).kind,
            ErrorKind::Quota
        );
    }

    #[test]
    fn unclassified_4xx_depends_on_the_operation() {
        assert_eq!(
            classify(422, "bad region", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            classify(400, "bad socket", Op::Launch).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            classify(409, "conflict", Op::Manage).kind,
            ErrorKind::Failed
        );
    }

    #[test]
    fn transport_and_decode_errors() {
        let t = ApiError::Transport {
            message: "timed out".into(),
            transient: true,
        };
        assert_eq!(t.into_provider_error(Op::Launch).kind, ErrorKind::Transient);
        let d = ApiError::Decode("missing field".into());
        assert_eq!(d.into_provider_error(Op::Plan).kind, ErrorKind::Failed);
    }

    #[test]
    fn extracts_messages_from_common_body_shapes() {
        assert_eq!(error_message(r#"{"message":"m"}"#), "m");
        assert_eq!(error_message(r#"{"error":"e"}"#), "e");
        assert_eq!(error_message(r#"{"detail":"d"}"#), "d");
        assert_eq!(
            error_message(r#"{"detail":[{"msg":"a"},{"msg":"b"}]}"#),
            "a; b"
        );
        assert_eq!(
            error_message(r#"{"errors":[{"param":"socket","details":"invalid"}]}"#),
            "socket: invalid"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "no error details");
    }
}
