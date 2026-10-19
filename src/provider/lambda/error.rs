//! Lambda HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The API reports errors as `{"error": {"code", "message", "suggestion"}}`
//! with namespaced codes (`global/invalid-api-key`,
//! `instance-operations/launch/insufficient-capacity`, `global/quota-exceeded`,
//! …), so classification keys on the code first and the HTTP status second.
//! Nothing is retried here — throttling and server errors come back as
//! `Transient` and the shared core retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Which API call failed — decides what an otherwise unclassified error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Catalog lookups at plan time: a rejected query is the user's ask →
    /// `InvalidRequest`.
    Plan,
    /// Per-launch prerequisites (SSH key, firewall ruleset, capacity
    /// re-check): nothing was launched → `Unavailable`.
    Prepare,
    /// `POST /api/v1/instance-operations/launch`: other rejections launched
    /// nothing → `Unavailable`.
    Launch,
    /// Listing, terminating or deleting, possibly after creation → `Failed`.
    Manage,
}

/// A failed API call, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered with a non-success status.
    Status {
        status: u16,
        /// The `error.code` field of the body, when present.
        code: Option<String>,
        message: String,
    },
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

    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Status { code, .. } => code.as_deref(),
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
                    ProviderError::transient(format!("Lambda API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Lambda API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected Lambda API response: {message}"))
            }
        }
    }
}

/// Map an HTTP status + error `code` + message to a `ProviderError`.
pub fn classify(status: u16, code: Option<&str>, message: &str, op: Op) -> ProviderError {
    let msg = match code {
        Some(code) => format!("Lambda API {status} {code}: {message}"),
        None => format!("Lambda API {status}: {message}"),
    };
    let code = code.unwrap_or("");
    if status == 401
        || matches!(
            code,
            "global/invalid-api-key" | "global/account-inactive" | "global/forbidden"
        )
        || (status == 403 && code != "global/invalid-address")
    {
        return ProviderError::credentials(format!(
            "{msg} (check the API key at https://cloud.lambda.ai/api-keys and that the account \
             is active)"
        ));
    }
    // The account may not launch from this address: no offer fixes that.
    if code == "global/invalid-address" {
        return ProviderError::invalid(msg);
    }
    if code == "instance-operations/launch/insufficient-capacity" {
        return ProviderError::capacity(msg);
    }
    if code == "global/quota-exceeded" {
        return ProviderError::quota(msg);
    }
    // A ruleset still attached to a terminating instance frees up shortly.
    if code == "firewall-rulesets/firewall-ruleset-in-use" {
        return ProviderError::transient(msg);
    }
    if status == 429
        || status >= 500
        || matches!(code, "global/rate-limited" | "global/internal-error")
    {
        return ProviderError::transient(msg);
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Prepare | Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// Split an error body into `(code, message)`, falling back to the raw
/// (truncated) text when it is not the documented `{"error": {...}}` shape.
/// A `suggestion`, when present, is appended to the message.
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, fallback());
    };
    let error = &value["error"];
    let code = error["code"].as_str().map(str::to_string);
    let message = match (error["message"].as_str(), error["suggestion"].as_str()) {
        (Some(m), Some(s)) if !s.is_empty() => format!("{m} ({s})"),
        (Some(m), _) => m.to_string(),
        _ => fallback(),
    };
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(status: u16, code: &str, op: Op) -> ErrorKind {
        classify(status, Some(code), "x", op).kind
    }

    #[test]
    fn codes_map_to_the_taxonomy() {
        assert_eq!(
            kind(401, "global/invalid-api-key", Op::Plan),
            ErrorKind::Credentials
        );
        assert_eq!(
            kind(403, "global/account-inactive", Op::Launch),
            ErrorKind::Credentials
        );
        assert_eq!(
            kind(
                400,
                "instance-operations/launch/insufficient-capacity",
                Op::Launch
            ),
            ErrorKind::Capacity
        );
        assert_eq!(
            kind(400, "global/quota-exceeded", Op::Launch),
            ErrorKind::Quota
        );
        assert_eq!(
            kind(429, "global/rate-limited", Op::Manage),
            ErrorKind::Transient
        );
        assert_eq!(
            kind(409, "global/internal-error", Op::Manage),
            ErrorKind::Transient
        );
        assert_eq!(
            kind(400, "firewall-rulesets/firewall-ruleset-in-use", Op::Manage),
            ErrorKind::Transient
        );
        assert_eq!(
            classify(503, None, "down", Op::Plan).kind,
            ErrorKind::Transient
        );
        // Launching from a disallowed address is not fixed by another offer.
        assert_eq!(
            kind(403, "global/invalid-address", Op::Launch),
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn unclassified_rejections_depend_on_the_operation() {
        let code = "global/invalid-parameters";
        assert_eq!(kind(400, code, Op::Plan), ErrorKind::InvalidRequest);
        assert_eq!(kind(400, code, Op::Prepare), ErrorKind::Unavailable);
        assert_eq!(kind(400, code, Op::Launch), ErrorKind::Unavailable);
        assert_eq!(kind(400, code, Op::Manage), ErrorKind::Failed);
        assert_eq!(
            kind(
                400,
                "instance-operations/launch/file-system-in-wrong-region",
                Op::Launch
            ),
            ErrorKind::Unavailable
        );
    }

    #[test]
    fn error_bodies_are_parsed_with_suggestions() {
        let (code, message) = error_parts(
            r#"{"error": {"code": "global/quota-exceeded", "message": "Quota exceeded",
                "suggestion": "Contact support"}}"#,
        );
        assert_eq!(code.as_deref(), Some("global/quota-exceeded"));
        assert_eq!(message, "Quota exceeded (Contact support)");
        let (code, message) = error_parts("<html>bad gateway</html>");
        assert_eq!(code, None);
        assert_eq!(message, "<html>bad gateway</html>");
    }
}
