//! Nebius API failures → `ProviderError` (the failover taxonomy).
//!
//! The REST gateway answers a failed call with an HTTP status and
//! `{"code": <gRPC canonical code>, "message", "details": [...]}`
//! (<https://docs.nebius.com/rest-api/errors>). `details` usually holds a
//! `nebius.common.v1.ServiceError` whose oneof names the failure precisely —
//! `notEnoughResources`, `quotaFailure`, `tooManyRequests`, … — plus a
//! `retryType` (`CALL` = just retry). A finished long-running operation
//! carries the same `{code, message, details}` as its `status`. Both are
//! classified here; nothing is retried in the adapter — `Transient` errors go
//! back to the shared core.

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{ErrorKind, ProviderError};

/// Which API call failed — decides what an otherwise unclassified error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Account and catalog lookups at plan time: a rejected query is the
    /// user's ask or setup → `InvalidRequest`.
    Plan,
    /// Per-launch prerequisites (subnet lookup): nothing was created →
    /// `Unavailable`.
    Prepare,
    /// `POST /compute/v1/instances` rejected synchronously: nothing was
    /// created → `Unavailable`.
    Launch,
    /// The create operation finished with an error: the instance may exist →
    /// `Failed`.
    LaunchResult,
    /// Listing or deleting, possibly after creation → `Failed`.
    Manage,
}

/// The `ServiceError` detail kinds that change the classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    NotEnoughResources,
    QuotaFailure,
    TooManyRequests,
    PermissionDenied,
    ResourceNotFound,
}

/// A Nebius error (`google.rpc.Status` with `ServiceError` details), from an
/// HTTP response body or a finished operation's `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// gRPC canonical code (`5` NOT_FOUND, `8` RESOURCE_EXHAUSTED, …).
    pub code: Option<i32>,
    pub message: String,
    pub detail: Option<Detail>,
    /// `ServiceError.retryType` == `CALL`: the same call may simply be retried.
    pub retry_call: bool,
}

/// A failed API call, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The server answered with a non-success HTTP status.
    Http { status: u16, error: Status },
    /// A long-running operation finished unsuccessfully.
    Operation { id: String, error: Status },
    /// The request never got a usable answer (connect error, timeout).
    Transport { message: String, transient: bool },
    /// The response body did not match the expected schema.
    Decode(String),
    /// The access token could not be obtained (no config/env token and the
    /// `nebius` CLI failed).
    Token(String),
}

impl ApiError {
    pub fn from_reqwest(err: reqwest::Error) -> Self {
        let transient = err.is_timeout() || err.is_connect() || err.is_request();
        ApiError::Transport {
            message: err.to_string(),
            transient,
        }
    }

    /// Whether the resource does not exist (HTTP 404 / gRPC `NOT_FOUND`).
    pub fn is_not_found(&self) -> bool {
        match self {
            ApiError::Http { status, error } => *status == 404 || error.code == Some(5),
            _ => false,
        }
    }

    /// Classify into the failover taxonomy for a call of kind `op`.
    pub fn into_provider_error(self, op: Op) -> ProviderError {
        match self {
            ApiError::Http { status, error } => classify(Some(status), &error, op),
            ApiError::Operation { id, error } => {
                let mut err = classify(None, &error, op);
                err.msg = format!("{} (operation {id})", err.msg);
                err
            }
            ApiError::Transport { message, transient } => {
                if transient {
                    ProviderError::transient(format!("Nebius API unreachable: {message}"))
                } else {
                    ProviderError::failed(format!("Nebius API request failed: {message}"))
                }
            }
            ApiError::Decode(message) => {
                ProviderError::failed(format!("unexpected Nebius API response: {message}"))
            }
            ApiError::Token(message) => ProviderError::credentials(format!(
                "no Nebius access token: {message} (set nebius.token in the config file or \
                 $NEBIUS_IAM_TOKEN, or install the nebius CLI and run `nebius profile create`)"
            )),
        }
    }
}

/// Wording of a stockout ("Not enough resources" is the error Nebius
/// documents for GPU shortages; see
/// <https://docs.nebius.com/compute/virtual-machines/not-enough-resources>).
const CAPACITY_WORDS: &[&str] = &[
    "not enough resources",
    "notenoughresources",
    "insufficient capacity",
    "no capacity",
    "out of capacity",
];

fn mentions(message: &str, words: &[&str]) -> bool {
    let lower = message.to_lowercase();
    words.iter().any(|w| lower.contains(w))
}

/// Map an error to a `ProviderError`. `http` is the HTTP status of a direct
/// call (`None` for an operation result).
pub fn classify(http: Option<u16>, error: &Status, op: Op) -> ProviderError {
    let code = error.code.unwrap_or(-1);
    let label = match (http, error.code) {
        (Some(h), Some(c)) => format!("Nebius API {h} (code {c})"),
        (Some(h), None) => format!("Nebius API {h}"),
        (None, Some(c)) => format!("Nebius operation failed (code {c})"),
        (None, None) => "Nebius operation failed".to_string(),
    };
    let msg = format!("{label}: {}", error.message);
    if code == 16 || http == Some(401) {
        return ProviderError::credentials(format!(
            "{msg} (the access token is invalid or expired; check nebius.token / \
             $NEBIUS_IAM_TOKEN or `nebius iam get-access-token`)"
        ));
    }
    if code == 7 || http == Some(403) || error.detail == Some(Detail::PermissionDenied) {
        return ProviderError::credentials(format!(
            "{msg} (the account lacks a role here; an `editor` group membership in the \
             tenant or project is enough)"
        ));
    }
    let launching = matches!(op, Op::Launch | Op::LaunchResult);
    if error.detail == Some(Detail::NotEnoughResources)
        || (launching && mentions(&error.message, CAPACITY_WORDS))
    {
        return ProviderError::capacity(msg);
    }
    if error.detail == Some(Detail::QuotaFailure) {
        return ProviderError::quota(msg);
    }
    if error.detail == Some(Detail::TooManyRequests) || error.retry_call {
        return ProviderError::transient(msg);
    }
    // RESOURCE_EXHAUSTED without a detail: on a create it is the account's
    // quota (Nebius maps quota exhaustion to it); elsewhere throttling.
    if code == 8 || http == Some(429) {
        return if launching {
            ProviderError::quota(msg)
        } else {
            ProviderError::transient(msg)
        };
    }
    // CANCELLED, DEADLINE_EXCEEDED, ABORTED, INTERNAL, UNAVAILABLE.
    if matches!(code, 1 | 4 | 10 | 13 | 14) || http.is_some_and(|h| h >= 500) {
        return ProviderError::transient(msg);
    }
    let kind = match op {
        Op::Plan => ErrorKind::InvalidRequest,
        Op::Prepare | Op::Launch => ErrorKind::Unavailable,
        Op::LaunchResult | Op::Manage => ErrorKind::Failed,
    };
    ProviderError::new(kind, msg)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawStatus {
    code: Option<i32>,
    message: Option<String>,
    details: Vec<Value>,
}

impl Status {
    /// Read a `google.rpc.Status` JSON value (an error body or an
    /// operation's `status`).
    pub fn from_value(value: &Value) -> Status {
        let raw: RawStatus = serde_json::from_value(value.clone()).unwrap_or_default();
        let mut status = Status {
            code: raw.code,
            message: raw.message.unwrap_or_default(),
            detail: None,
            retry_call: false,
        };
        for detail in &raw.details {
            let is_service_error = detail["@type"]
                .as_str()
                .is_some_and(|t| t.ends_with("nebius.common.v1.ServiceError"));
            if !is_service_error {
                continue;
            }
            status.retry_call |= detail["retryType"].as_str() == Some("CALL");
            let kinds = [
                ("notEnoughResources", Detail::NotEnoughResources),
                ("quotaFailure", Detail::QuotaFailure),
                ("tooManyRequests", Detail::TooManyRequests),
                ("permissionDenied", Detail::PermissionDenied),
                ("resourceNotFound", Detail::ResourceNotFound),
            ];
            if let Some((_, kind)) = kinds.iter().find(|(key, _)| !detail[*key].is_null()) {
                status.detail.get_or_insert(*kind);
                // The violation messages say what is short; keep them.
                for violation in detail["notEnoughResources"]["violations"]
                    .as_array()
                    .into_iter()
                    .chain(detail["quotaFailure"]["violations"].as_array())
                    .flatten()
                {
                    if let Some(text) = violation["message"].as_str() {
                        if !status.message.contains(text) {
                            status.message = format!("{} ({text})", status.message);
                        }
                    }
                }
            }
        }
        status
    }

    /// Read an error response body, falling back to the raw (truncated) text
    /// when it is not the documented JSON shape.
    pub fn from_body(body: &str) -> Status {
        match serde_json::from_str::<Value>(body) {
            Ok(value) if value.get("code").is_some() || value.get("message").is_some() => {
                Status::from_value(&value)
            }
            _ => {
                let mut text = body.trim().to_string();
                text.truncate(400);
                Status {
                    code: None,
                    message: if text.is_empty() {
                        "no error details".into()
                    } else {
                        text
                    },
                    detail: None,
                    retry_call: false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn status(code: i32, message: &str) -> Status {
        Status {
            code: Some(code),
            message: message.into(),
            detail: None,
            retry_call: false,
        }
    }

    fn kind(http: u16, code: i32, message: &str, op: Op) -> ErrorKind {
        classify(Some(http), &status(code, message), op).kind
    }

    #[test]
    fn auth_failures_are_credentials() {
        // Live answers of the gateway to a bad / missing token.
        assert_eq!(
            kind(401, 16, "invalid token", Op::Plan),
            ErrorKind::Credentials
        );
        assert_eq!(
            kind(403, 7, "you do not have permission", Op::Launch),
            ErrorKind::Credentials
        );
        let err = ApiError::Token("nebius: not found".into()).into_provider_error(Op::Plan);
        assert_eq!(err.kind, ErrorKind::Credentials);
        assert!(err.msg.contains("NEBIUS_IAM_TOKEN"), "{}", err.msg);
    }

    #[test]
    fn service_error_details_decide_capacity_quota_and_throttling() {
        let body = json!({
            "code": 8, "message": "resource exhausted",
            "details": [{
                "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                "service": "compute", "code": "NotEnoughResources",
                "notEnoughResources": {"violations": [
                    {"resourceType": "gpu", "message": "not enough gpu-h100-sxm in eu-north1", "requested": "1"}
                ]},
                "retryType": "UNIT_OF_WORK"
            }]
        });
        let parsed = Status::from_value(&body);
        assert_eq!(parsed.detail, Some(Detail::NotEnoughResources));
        assert!(parsed.message.contains("not enough gpu-h100-sxm"));
        assert_eq!(
            classify(Some(429), &parsed, Op::Launch).kind,
            ErrorKind::Capacity
        );

        let quota = Status::from_value(&json!({
            "code": 8, "message": "quota exceeded",
            "details": [{"@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                         "quotaFailure": {"violations": [{"quota": "compute.gpu.h100", "limit": "8", "requested": "16"}]}}]
        }));
        assert_eq!(
            classify(Some(429), &quota, Op::Launch).kind,
            ErrorKind::Quota
        );

        let throttled = Status::from_value(&json!({
            "code": 8, "message": "slow down",
            "details": [{"@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                         "tooManyRequests": {"violation": "rps"}, "retryType": "CALL"}]
        }));
        assert_eq!(
            classify(Some(429), &throttled, Op::Launch).kind,
            ErrorKind::Transient
        );
    }

    #[test]
    fn codes_without_details() {
        // RESOURCE_EXHAUSTED on a create is the account quota; on reads, throttling.
        assert_eq!(kind(429, 8, "exhausted", Op::Launch), ErrorKind::Quota);
        assert_eq!(kind(429, 8, "exhausted", Op::Manage), ErrorKind::Transient);
        // Capacity wording on a create.
        assert_eq!(
            kind(
                400,
                9,
                "Not enough resources to start the instance",
                Op::Launch
            ),
            ErrorKind::Capacity
        );
        for code in [4, 10, 13, 14] {
            assert_eq!(kind(503, code, "x", Op::Manage), ErrorKind::Transient);
        }
        // Everything else falls to the operation's default.
        assert_eq!(kind(400, 3, "bad", Op::Plan), ErrorKind::InvalidRequest);
        assert_eq!(kind(400, 3, "bad", Op::Prepare), ErrorKind::Unavailable);
        assert_eq!(kind(400, 3, "bad", Op::Launch), ErrorKind::Unavailable);
        assert_eq!(kind(400, 9, "bad", Op::Manage), ErrorKind::Failed);
    }

    #[test]
    fn operation_results_may_have_created_something() {
        let failed = ApiError::Operation {
            id: "computeoperation-e00x".into(),
            error: status(9, "boot disk image is incompatible"),
        }
        .into_provider_error(Op::LaunchResult);
        assert_eq!(failed.kind, ErrorKind::Failed);
        assert!(failed.msg.contains("computeoperation-e00x"));
        let capacity = ApiError::Operation {
            id: "op".into(),
            error: status(8, "Not enough resources: gpu-h200-sxm"),
        }
        .into_provider_error(Op::LaunchResult);
        assert_eq!(capacity.kind, ErrorKind::Capacity);
    }

    #[test]
    fn bodies_and_not_found() {
        // Live answer for a malformed parent id.
        let parsed = Status::from_body(
            r#"{"code":3, "message":"parent_id is invalid", "details":[{"@type":"type.googleapis.com/nebius.common.v1.ServiceError", "service":"compute", "code":"BadRequest", "badRequest":{"violations":[{"field":"parent_id", "message":"parse"}]}, "retryType":"NOTHING"}]}"#,
        );
        assert_eq!(parsed.code, Some(3));
        assert_eq!(parsed.message, "parent_id is invalid");
        assert!(!parsed.retry_call);
        let raw = Status::from_body("<html>bad gateway</html>");
        assert_eq!(raw.code, None);
        assert_eq!(raw.message, "<html>bad gateway</html>");

        let missing = ApiError::Http {
            status: 404,
            error: status(5, "instance not found"),
        };
        assert!(missing.is_not_found());
    }
}
