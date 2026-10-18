//! GCP error classification → `ProviderError` (the failover taxonomy).
//!
//! Two error channels exist on Compute Engine: synchronous HTTP errors
//! (`{"error": {"code", "message", "errors": [{"reason"}]}}`) and errors
//! inside a finished long-running `Operation` (`error.errors[].code`) — the
//! latter is where zonal stockouts surface. Stockouts block the zone (offers
//! are zonal, so `BlockScope::Offer`); quotas block the region.

use crate::provider::{BlockScope, ProviderError};

/// A failed HTTP exchange. `status == 0` means the request never got a
/// response (connect/timeout/body errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// First `errors[].reason` (e.g. `quotaExceeded`, `notFound`).
    pub reason: Option<String>,
    pub message: String,
}

impl ApiError {
    pub fn transport(message: impl Into<String>) -> Self {
        ApiError {
            status: 0,
            reason: None,
            message: message.into(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status == 404
    }

    /// Parse GCP's JSON error envelope; falls back to the raw body.
    pub fn from_response(status: u16, body: &str) -> Self {
        let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
        let error = parsed.as_ref().and_then(|v| v.get("error"));
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let mut b = body.trim().to_string();
                b.truncate(400);
                b
            });
        let reason = error
            .and_then(|e| e.get("errors"))
            .and_then(|e| e.get(0))
            .and_then(|e| e.get("reason"))
            .and_then(|r| r.as_str())
            .map(str::to_string);
        ApiError {
            status,
            reason,
            message,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.status, &self.reason) {
            (0, _) => write!(f, "request failed: {}", self.message),
            (s, Some(r)) => write!(f, "HTTP {s} {r}: {}", self.message),
            (s, None) => write!(f, "HTTP {s}: {}", self.message),
        }
    }
}

/// Where an HTTP error happened — decides what an unclassified client error
/// means for failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Catalog lookups: a bad region/type is the user's ask → `InvalidRequest`.
    Plan,
    /// Before/while creating, nothing exists yet → `Unavailable`.
    Create,
    /// After an instance may exist → `Failed` (triggers compensation).
    Runtime,
}

/// Classify a synchronous HTTP error.
pub fn classify_http(err: &ApiError, phase: Phase) -> ProviderError {
    let msg = err.to_string();
    match (err.status, err.reason.as_deref()) {
        (0, _) | (429, _) | (500..=599, _) => ProviderError::transient(msg),
        (401, _) => ProviderError::credentials(msg),
        (403, Some("quotaExceeded")) => ProviderError::quota(msg),
        (403, Some("rateLimitExceeded" | "userRateLimitExceeded")) => ProviderError::transient(msg),
        (403, _) => ProviderError::credentials(msg),
        _ => match phase {
            Phase::Plan => ProviderError::invalid(msg),
            Phase::Create => ProviderError::unavailable(msg),
            Phase::Runtime => ProviderError::failed(msg),
        },
    }
}

/// Classify an error code from a finished `Operation` (instance insert).
pub fn classify_operation(code: &str, message: &str) -> ProviderError {
    let msg = format!("{code}: {message}");
    match code {
        // Region-wide (or global — `GPUS_ALL_REGIONS`; the closest scope we
        // have is the region, and the next region will fail fast too).
        "QUOTA_EXCEEDED" | "quotaExceeded" => ProviderError::quota(msg),
        // Zonal stockouts. UNSUPPORTED_OPERATION is observed when a VM is
        // preempted during creation; RESOURCE_NOT_FOUND / RESOURCE_NOT_READY /
        // RESOURCE_OPERATION_RATE_EXCEEDED are zone-blocked as well
        // (retrying them in place risks an endless loop).
        "ZONE_RESOURCE_POOL_EXHAUSTED"
        | "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS"
        | "UNSUPPORTED_OPERATION"
        | "insufficientCapacity"
        | "RESOURCE_NOT_READY"
        | "RESOURCE_NOT_FOUND"
        | "RESOURCE_OPERATION_RATE_EXCEEDED" => ProviderError::capacity(msg),
        // The VPC is project-global: no region will work.
        "VPC_NOT_FOUND" => ProviderError::unavailable(msg).with_block(BlockScope::Region),
        _ => ProviderError::failed(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ErrorKind;

    fn http(status: u16, reason: Option<&str>) -> ApiError {
        ApiError {
            status,
            reason: reason.map(str::to_string),
            message: "boom".into(),
        }
    }

    #[test]
    fn http_errors_map_to_taxonomy() {
        let kind = |status, reason, phase| classify_http(&http(status, reason), phase).kind;
        assert_eq!(kind(0, None, Phase::Create), ErrorKind::Transient);
        assert_eq!(kind(503, None, Phase::Runtime), ErrorKind::Transient);
        assert_eq!(kind(429, None, Phase::Plan), ErrorKind::Transient);
        assert_eq!(kind(401, None, Phase::Create), ErrorKind::Credentials);
        assert_eq!(
            kind(403, Some("quotaExceeded"), Phase::Create),
            ErrorKind::Quota
        );
        assert_eq!(
            kind(403, Some("rateLimitExceeded"), Phase::Create),
            ErrorKind::Transient
        );
        assert_eq!(
            kind(403, Some("forbidden"), Phase::Create),
            ErrorKind::Credentials
        );
        assert_eq!(kind(404, None, Phase::Plan), ErrorKind::InvalidRequest);
        assert_eq!(kind(404, None, Phase::Create), ErrorKind::Unavailable);
        assert_eq!(kind(400, None, Phase::Runtime), ErrorKind::Failed);
    }

    #[test]
    fn operation_codes_map_to_taxonomy_and_scope() {
        let e = classify_operation("ZONE_RESOURCE_POOL_EXHAUSTED", "no L4 in us-central1-a");
        assert_eq!(e.kind, ErrorKind::Capacity);
        assert_eq!(e.block_scope(), BlockScope::Offer);
        assert!(e.msg.contains("us-central1-a"));
        let e = classify_operation("QUOTA_EXCEEDED", "Quota 'GPUS_ALL_REGIONS' exceeded");
        assert_eq!(e.kind, ErrorKind::Quota);
        assert_eq!(e.block_scope(), BlockScope::Region);
        assert_eq!(
            classify_operation("UNSUPPORTED_OPERATION", "").kind,
            ErrorKind::Capacity
        );
        assert_eq!(
            classify_operation("VPC_NOT_FOUND", "").block_scope(),
            BlockScope::Region
        );
        assert_eq!(
            classify_operation("SOMETHING_ELSE", "").kind,
            ErrorKind::Failed
        );
    }

    #[test]
    fn parses_gcp_error_envelope() {
        let body = r#"{"error":{"code":403,"message":"Quota 'CPUS' exceeded.",
            "errors":[{"message":"Quota 'CPUS' exceeded.","domain":"usageLimits",
            "reason":"quotaExceeded"}]}}"#;
        let e = ApiError::from_response(403, body);
        assert_eq!(e.reason.as_deref(), Some("quotaExceeded"));
        assert_eq!(e.message, "Quota 'CPUS' exceeded.");
        let e = ApiError::from_response(502, "<html>bad gateway</html>");
        assert_eq!(e.message, "<html>bad gateway</html>");
        assert!(e.reason.is_none());
    }
}
