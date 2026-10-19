//! DigitalOcean HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The API reports errors as `{"id", "message", "request_id"}`, where `id`
//! names the HTTP status (`unauthorized`, `not_found`,
//! `unprocessable_entity`, `too_many_requests`, …; spec
//! `components/schemas/error`). The ids are too coarse to tell capacity from
//! a bad request, and `POST /v2/droplets` documents no capacity response at
//! all, so a rejected create is classified by its message wording (see
//! [`classify`]). Nothing is retried here — throttling and server errors come
//! back as `Transient` and the shared core retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Which API call failed — decides what an otherwise unclassified error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Catalog lookups at plan time: a rejected query is the user's ask →
    /// `InvalidRequest`.
    Plan,
    /// Per-launch prerequisites (SSH key, tag, firewall, availability
    /// re-check): nothing was created → `Unavailable`.
    Prepare,
    /// `POST /v2/droplets`: other rejections created nothing → `Unavailable`.
    Launch,
    /// Listing or deleting, possibly after creation → `Failed`.
    Manage,
}

/// A failed API call, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered with a non-success status.
    Status {
        status: u16,
        /// The `id` field of the body, when present.
        id: Option<String>,
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

    /// Classify into the failover taxonomy for a call of kind `op`.
    pub fn into_provider_error(self, op: Op) -> ProviderError {
        match self {
            ApiError::Status {
                status,
                id,
                message,
            } => classify(status, id.as_deref(), &message, op),
            ApiError::Transport { message, transient } => {
                if transient {
                    ProviderError::transient(format!("DigitalOcean API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("DigitalOcean API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected DigitalOcean API response: {message}"))
            }
        }
    }
}

/// Wording of a create rejected for lack of stock. Assumed, not documented:
/// the spec lists no capacity response for `POST /v2/droplets`; DigitalOcean
/// answers an unavailable size/region with `422` "… is not available …".
const CAPACITY_WORDS: &[&str] = &[
    "not available",
    "unavailable",
    "capacity",
    "out of stock",
    "insufficient",
];

/// Wording of the account Droplet limit ("… will exceed your droplet
/// limit"; the limit is `GET /v2/account` → `droplet_limit`).
const QUOTA_WORDS: &[&str] = &["droplet limit", "exceed"];

/// Map an HTTP status + error `id` + message to a `ProviderError`.
pub fn classify(status: u16, id: Option<&str>, message: &str, op: Op) -> ProviderError {
    let msg = match id {
        Some(id) => format!("DigitalOcean API {status} {id}: {message}"),
        None => format!("DigitalOcean API {status}: {message}"),
    };
    if status == 401 || status == 403 {
        return ProviderError::credentials(format!(
            "{msg} (check the token at https://cloud.digitalocean.com/account/api/tokens \
             and that it has read and write scope)"
        ));
    }
    if status == 429 || status >= 500 {
        return ProviderError::transient(msg);
    }
    if op == Op::Launch {
        let lower = message.to_lowercase();
        if QUOTA_WORDS.iter().any(|w| lower.contains(w)) {
            return ProviderError::quota(msg);
        }
        if CAPACITY_WORDS.iter().any(|w| lower.contains(w)) {
            return ProviderError::capacity(msg);
        }
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Prepare | Op::Launch => ErrorKind::Unavailable,
        Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

/// Split an error body into `(id, message)`, falling back to the raw
/// (truncated) text when it is not the documented `{"id", "message"}` shape.
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
    let id = value["id"].as_str().map(str::to_string);
    let message = value["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(fallback);
    (id, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(status: u16, message: &str, op: Op) -> ErrorKind {
        classify(status, Some("unprocessable_entity"), message, op).kind
    }

    #[test]
    fn statuses_map_to_the_taxonomy() {
        assert_eq!(
            classify(
                401,
                Some("unauthorized"),
                "Unable to authenticate you",
                Op::Plan
            )
            .kind,
            ErrorKind::Credentials
        );
        assert_eq!(
            classify(403, Some("forbidden"), "read-only token", Op::Launch).kind,
            ErrorKind::Credentials
        );
        assert_eq!(
            classify(
                429,
                Some("too_many_requests"),
                "API rate limit exceeded",
                Op::Launch
            )
            .kind,
            ErrorKind::Transient
        );
        assert_eq!(
            classify(503, None, "down", Op::Manage).kind,
            ErrorKind::Transient
        );
    }

    #[test]
    fn create_rejections_are_read_by_wording() {
        let launch = |m| kind(422, m, Op::Launch);
        assert_eq!(
            launch("Size is not available in this region."),
            ErrorKind::Capacity
        );
        assert_eq!(
            launch("There is insufficient capacity for this Droplet size"),
            ErrorKind::Capacity
        );
        assert_eq!(
            launch("creating this/these droplet(s) will exceed your droplet limit"),
            ErrorKind::Quota
        );
        assert_eq!(launch("Name is invalid"), ErrorKind::Unavailable);
        // Wording only matters on create: elsewhere the operation decides.
        assert_eq!(
            kind(422, "Size is not available", Op::Plan),
            ErrorKind::InvalidRequest
        );
        assert_eq!(kind(422, "x", Op::Prepare), ErrorKind::Unavailable);
        assert_eq!(kind(422, "x", Op::Manage), ErrorKind::Failed);
    }

    #[test]
    fn error_bodies_are_parsed() {
        let (id, message) = error_parts(
            r#"{"id": "not_found", "message": "The resource you were accessing could not be found.",
                "request_id": "4851a473-1621-42ea-b2f9-5071c0ea8414"}"#,
        );
        assert_eq!(id.as_deref(), Some("not_found"));
        assert_eq!(
            message,
            "The resource you were accessing could not be found."
        );
        let (id, message) = error_parts("<html>bad gateway</html>");
        assert_eq!(id, None);
        assert_eq!(message, "<html>bad gateway</html>");
    }
}
