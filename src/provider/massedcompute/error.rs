//! Massed Compute HTTP failures → `ProviderError` (the failover taxonomy).
//!
//! The VM API documents its deploy failures by message rather than by a
//! machine-readable code (see the "Deployment Errors" page), so those are
//! recognized by wording: "Insufficient Funds", "No Availability", "You have
//! reached your limit of running instances", "SSH Key does not exist",
//! "User does not exist, is disabled, …". Nothing is retried here —
//! throttling and server errors come back as `Transient` and the shared core
//! retries the step.

use crate::provider::{ErrorKind, ProviderError};

/// Billing problems: account-wide and not fixed by another offer, so they end
/// the launch with a clear message. Checked first, since "insufficient funds"
/// must not read as a stock problem.
const BALANCE_KEYWORDS: &[&str] = &["insufficient funds", "add credit", "account balance"];

/// The account's running-instance cap (tied to the billing recharge amount):
/// another offer won't help until the account changes.
const QUOTA_KEYWORDS: &[&str] = &["limit of running instances", "recharge amount"];

/// The requested product has no free machines right now.
const CAPACITY_KEYWORDS: &[&str] = &[
    "no availability",
    "not available",
    "no capacity",
    "out of stock",
    "insufficient",
];

/// The user account itself cannot deploy.
const ACCOUNT_KEYWORDS: &[&str] = &[
    "user does not exist",
    "is disabled",
    "not verified",
    "suspended",
    "invalid token",
    "unauthorized",
];

/// Which API call failed — decides what an unclassified 4xx means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Inventory / image lookups at plan time → `InvalidRequest`.
    Plan,
    /// Key registration, stock re-check, launch: nothing was created →
    /// `Unavailable`.
    Launch,
    /// Listing or terminating instances, possibly after creation → `Failed`.
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

    /// Whether the server says the thing addressed does not exist (an
    /// already-terminated instance, for idempotent deletes).
    pub fn is_not_found(&self) -> bool {
        match self {
            ApiError::Status { status: 404, .. } => true,
            ApiError::Status { message, .. } => {
                let lower = message.to_lowercase();
                lower.contains("not found") || lower.contains("does not exist")
            }
            _ => false,
        }
    }

    /// Classify into the failover taxonomy for a call of kind `op`.
    pub fn into_provider_error(self, op: Op) -> ProviderError {
        match self {
            ApiError::Status { status, message } => classify(status, &message, op),
            ApiError::Transport { message, transient } => {
                if transient {
                    ProviderError::transient(format!("Massed Compute API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Massed Compute API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected Massed Compute API response: {message}"))
            }
        }
    }
}

/// Map an HTTP status + error message to a `ProviderError`.
pub fn classify(status: u16, message: &str, op: Op) -> ProviderError {
    let msg = format!("Massed Compute API {status}: {message}");
    let lower = message.to_lowercase();
    if status == 401 || status == 403 || ACCOUNT_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::credentials(format!(
            "{msg} (check the API token, its scope must be Full access to launch, at \
             https://vm.massedcompute.com/settings/api)"
        ));
    }
    if status == 429 || status >= 500 {
        return ProviderError::transient(msg);
    }
    if BALANCE_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return ProviderError::invalid(format!(
            "Massed Compute account balance too low to launch: {msg}"
        ));
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

/// A human-readable message from an error body: `message`, `error` or
/// `detail` (the shapes the API uses), else the raw (truncated) text.
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
    ["message", "error", "detail", "response"]
        .iter()
        .find_map(|key| {
            json.get(*key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BlockScope;

    #[test]
    fn auth_and_server_errors() {
        assert_eq!(classify(401, "bad", Op::Plan).kind, ErrorKind::Credentials);
        assert_eq!(classify(403, "no", Op::Manage).kind, ErrorKind::Credentials);
        for status in [429, 500, 502, 503] {
            assert_eq!(classify(status, "x", Op::Launch).kind, ErrorKind::Transient);
        }
    }

    #[test]
    fn documented_deploy_errors() {
        let stock = classify(
            400,
            "No Availability. Select a different quantity or GPU type.",
            Op::Launch,
        );
        assert_eq!(stock.kind, ErrorKind::Capacity);
        assert_eq!(stock.block_scope(), BlockScope::Offer);

        // Billing comes before capacity even though both say "insufficient".
        let broke = classify(
            400,
            "Insufficient Funds. Please add credit to your account.",
            Op::Launch,
        );
        assert_eq!(broke.kind, ErrorKind::InvalidRequest);
        assert!(broke.msg.contains("balance too low"), "{}", broke.msg);

        let cap = classify(
            400,
            "You have reached your limit of running instances. Please increase your \
             'Recharge Amount' in billing to continue.",
            Op::Launch,
        );
        assert_eq!(cap.kind, ErrorKind::Quota);

        let user = classify(
            400,
            "User does not exist, is disabled, not verified, or suspended",
            Op::Launch,
        );
        assert_eq!(user.kind, ErrorKind::Credentials);

        assert_eq!(
            classify(400, "SSH Key does not exist", Op::Launch).kind,
            ErrorKind::Unavailable
        );
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
        // Stock wording outside a launch is not a capacity signal.
        assert_eq!(
            classify(400, "No Availability", Op::Plan).kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn not_found_detection() {
        let gone = ApiError::Status {
            status: 400,
            message: "Instance does not exist".into(),
        };
        assert!(gone.is_not_found());
        assert!(ApiError::Status {
            status: 404,
            message: String::new()
        }
        .is_not_found());
        assert!(!ApiError::Status {
            status: 400,
            message: "bad".into()
        }
        .is_not_found());
    }

    #[test]
    fn error_message_reads_known_shapes() {
        assert_eq!(
            error_message(r#"{"message":"No Availability","code":"x"}"#),
            "No Availability"
        );
        assert_eq!(
            error_message(r#"{"error":"Invalid token"}"#),
            "Invalid token"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "no error details");
    }
}
