//! Verda HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The API reports errors as `{ code, message }` with a machine-readable
//! `code` (`invalid_request`, `unauthorized_request`, `insufficient_funds`,
//! `forbidden_action`, `not_found`, `conflict`, `server_error`,
//! `service_unavailable`), so classification keys on the code first and the
//! HTTP status second. Nothing is retried here — throttling and server
//! errors come back as `Transient` and the shared core retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Which API call failed — decides what an otherwise unclassified error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Catalog lookups at plan time: a rejected query is the user's ask
    /// (e.g. an unknown instance type) → `InvalidRequest`.
    Plan,
    /// Per-launch prerequisites (SSH key, startup script, stock re-check):
    /// nothing was launched → `Unavailable`; a 503 here is an outage, not a
    /// capacity signal.
    Prepare,
    /// `POST /v1/instances`: a 503 / `service_unavailable` is how Verda says
    /// "no capacity in this location"; other rejections launched nothing.
    Launch,
    /// Listing or deleting resources, possibly after creation → `Failed`.
    Manage,
}

/// A failed API call, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered with a non-success status.
    Status {
        status: u16,
        /// The `code` field of the error body, when present.
        code: Option<String>,
        message: String,
    },
    /// The request never got a usable answer (connect error, timeout).
    Transport { message: String, transient: bool },
    /// The response body did not match the expected schema.
    Decode(String),
    /// Exchanging the client credentials for a token was refused.
    Auth(String),
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
            ApiError::Status {
                status,
                code,
                message,
            } => classify(status, code.as_deref(), &message, op),
            ApiError::Transport { message, transient } => {
                if transient {
                    ProviderError::transient(format!("Verda API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Verda API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected Verda API response: {message}"))
            }
            ApiError::Auth(message) => ProviderError::credentials(format!(
                "Verda rejected the API credentials: {message} (check the client id and secret \
                 under Credentials in the Verda console)"
            )),
        }
    }
}

/// Map an HTTP status + error `code` + message to a `ProviderError`.
pub fn classify(status: u16, code: Option<&str>, message: &str, op: Op) -> ProviderError {
    let msg = match code {
        Some(code) => format!("Verda API {status} {code}: {message}"),
        None => format!("Verda API {status}: {message}"),
    };
    if status == 401
        || status == 403
        || matches!(code, Some("unauthorized_request" | "forbidden_action"))
    {
        return ProviderError::credentials(format!(
            "{msg} (check the API credentials and their permissions in the Verda console)"
        ));
    }
    // Billing is account-wide and no other offer fixes it: end the launch
    // with a clear message instead of walking every location.
    if code == Some("insufficient_funds") {
        return ProviderError::invalid(format!("Verda account balance too low to launch: {msg}"));
    }
    if op == Op::Launch
        && (status == 503
            || code == Some("service_unavailable")
            || message.to_lowercase().contains("capacity"))
    {
        return ProviderError::capacity(msg);
    }
    if status == 429
        || status >= 500
        || matches!(code, Some("server_error" | "service_unavailable"))
    {
        return ProviderError::transient(msg);
    }
    if code == Some("conflict") {
        return ProviderError::failed(msg);
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Prepare | Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// Split an error body into `(code, message)`, falling back to the raw
/// (truncated) text when it is not the documented `{code, message}` shape.
pub fn error_parts(body: &str) -> (Option<String>, String) {
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
        return (None, fallback());
    };
    let code = json
        .get("code")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let message = json
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(fallback);
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BlockScope;

    #[test]
    fn auth_errors_by_status_and_code() {
        for (status, code) in [
            (401, None),
            (403, None),
            (400, Some("unauthorized_request")),
            (400, Some("forbidden_action")),
        ] {
            assert_eq!(
                classify(status, code, "no", Op::Manage).kind,
                ErrorKind::Credentials,
                "{status} {code:?}"
            );
        }
        assert_eq!(
            ApiError::Auth("bad secret".into())
                .into_provider_error(Op::Plan)
                .kind,
            ErrorKind::Credentials
        );
    }

    #[test]
    fn create_503_is_capacity_but_elsewhere_transient() {
        let no_capacity = classify(
            503,
            Some("service_unavailable"),
            "No capacity available",
            Op::Launch,
        );
        assert_eq!(no_capacity.kind, ErrorKind::Capacity);
        assert_eq!(no_capacity.block_scope(), BlockScope::Offer);
        assert_eq!(
            classify(400, None, "Not enough capacity in FIN-01", Op::Launch).kind,
            ErrorKind::Capacity
        );
        for op in [Op::Plan, Op::Prepare, Op::Manage] {
            assert_eq!(
                classify(503, Some("service_unavailable"), "down", op).kind,
                ErrorKind::Transient,
                "{op:?}"
            );
        }
        assert_eq!(
            classify(429, None, "slow down", Op::Launch).kind,
            ErrorKind::Transient
        );
        assert_eq!(
            classify(500, Some("server_error"), "boom", Op::Manage).kind,
            ErrorKind::Transient
        );
    }

    #[test]
    fn insufficient_funds_is_fatal_with_a_clear_message() {
        let broke = classify(400, Some("insufficient_funds"), "Top up", Op::Launch);
        assert_eq!(broke.kind, ErrorKind::InvalidRequest);
        assert!(broke.msg.contains("balance too low"), "{}", broke.msg);
    }

    #[test]
    fn unclassified_errors_depend_on_the_call() {
        assert_eq!(
            classify(400, Some("invalid_request"), "x", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            classify(404, Some("not_found"), "x", Op::Prepare).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            classify(400, Some("invalid_request"), "x", Op::Launch).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            classify(404, Some("not_found"), "x", Op::Manage).kind,
            ErrorKind::Failed
        );
        assert_eq!(
            classify(409, Some("conflict"), "x", Op::Launch).kind,
            ErrorKind::Failed
        );
    }

    #[test]
    fn error_parts_read_code_and_message() {
        assert_eq!(
            error_parts(r#"{"code":"not_found","message":"Instance not found"}"#),
            (Some("not_found".into()), "Instance not found".into())
        );
        assert_eq!(error_parts("plain text"), (None, "plain text".into()));
        assert_eq!(error_parts(""), (None, "no error details".into()));
    }
}
