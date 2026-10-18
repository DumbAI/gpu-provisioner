//! AWS error-code classification → `ProviderError` (the failover taxonomy).
//!
//! Credential codes abort; server-side errors (throttling, internal errors)
//! become `Transient` and are retried by the shared core; capacity vs quota
//! are split so an account limit blocks the whole
//! region instead of being retried per instance type.

use aws_sdk_ec2::error::{ProvideErrorMetadata, SdkError};

use crate::provider::{BlockScope, ErrorKind, ProviderError};

const CREDENTIAL_CODES: &[&str] = &[
    "ExpiredToken",
    "ExpiredTokenException",
    "RequestExpired",
    "InvalidClientTokenId",
    "InvalidClientToken",
    "AuthFailure",
    "UnauthorizedOperation",
];

const CAPACITY_CODES: &[&str] = &[
    "InsufficientInstanceCapacity",
    "InsufficientCapacity",
    "InsufficientFreeAddressesInSubnet",
    "SpotMaxPriceTooLow",
];

/// Account limits: per region (and per instance family), so they block every
/// type in the region rather than one offer.
const QUOTA_CODES: &[&str] = &[
    "InstanceLimitExceeded",
    "VcpuLimitExceeded",
    "MaxSpotInstanceCountExceeded",
];

/// The instance type is not supported in the requested zone(s).
const UNSUPPORTED_CODES: &[&str] = &["Unsupported"];

pub const SERVER_RETRYABLE_CODES: &[&str] = &[
    "RequestLimitExceeded",
    "ServerInternal",
    "ServiceUnavailable",
    "InternalError",
    "Unavailable",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsErrorClass {
    Credentials,
    Capacity,
    Quota,
    Unsupported,
    ServerRetryable,
    Other,
}

/// Classify by service error code (pure; unit-tested via [`classify_code`]).
pub fn classify<E, R>(err: &SdkError<E, R>) -> AwsErrorClass
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    if let Some(code) = err.code() {
        return classify_code(code);
    }
    // No service error code: dispatch/construction failures. Credential
    // resolution problems (no profile, no keys) surface here; timeouts and
    // connection errors are transient.
    let debug = format!("{err:?}").to_lowercase();
    if debug.contains("credential") {
        AwsErrorClass::Credentials
    } else if matches!(
        err,
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_)
    ) {
        AwsErrorClass::ServerRetryable
    } else {
        AwsErrorClass::Other
    }
}

pub fn classify_code(code: &str) -> AwsErrorClass {
    if CREDENTIAL_CODES.contains(&code) {
        AwsErrorClass::Credentials
    } else if CAPACITY_CODES.contains(&code) {
        AwsErrorClass::Capacity
    } else if QUOTA_CODES.contains(&code) {
        AwsErrorClass::Quota
    } else if UNSUPPORTED_CODES.contains(&code) {
        AwsErrorClass::Unsupported
    } else if SERVER_RETRYABLE_CODES.contains(&code) {
        AwsErrorClass::ServerRetryable
    } else {
        AwsErrorClass::Other
    }
}

/// Human-readable `code: message` for an SDK error.
pub fn describe<E, R>(err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    match (err.code(), err.message()) {
        (Some(code), Some(msg)) => format!("{code}: {msg}"),
        (Some(code), None) => code.to_string(),
        _ => {
            let mut s = format!("{err:?}");
            s.truncate(400);
            s
        }
    }
}

pub fn error_code<E, R>(err: &SdkError<E, R>) -> Option<String>
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    err.code().map(str::to_string)
}

/// Whether an SDK error carries a specific service error code.
pub fn has_code<E, R>(err: &SdkError<E, R>, code: &str) -> bool
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    err.code() == Some(code)
}

/// `map_err` helper for calls made *before* any instance exists (network,
/// image, SG prep): an unclassified failure means nothing was launched →
/// `Unavailable` (no compensation needed).
pub fn in_bootstrap<E, R>(err: SdkError<E, R>) -> ProviderError
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    to_provider_error(classify(&err), describe(&err), ErrorKind::Unavailable)
}

/// `map_err` helper for calls made *after* an instance may exist: an
/// unclassified failure is `Failed` (triggers compensation).
pub fn at_runtime<E, R>(err: SdkError<E, R>) -> ProviderError
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    to_provider_error(classify(&err), describe(&err), ErrorKind::Failed)
}

fn to_provider_error(class: AwsErrorClass, msg: String, unclassified: ErrorKind) -> ProviderError {
    match class {
        AwsErrorClass::Credentials => ProviderError::credentials(msg),
        AwsErrorClass::Capacity => ProviderError::capacity(msg),
        AwsErrorClass::Quota => ProviderError::quota(msg),
        AwsErrorClass::Unsupported => {
            ProviderError::unavailable(msg).with_block(BlockScope::TypeInRegion)
        }
        AwsErrorClass::ServerRetryable => ProviderError::transient(msg),
        AwsErrorClass::Other => ProviderError::new(unclassified, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_map_to_taxonomy() {
        assert_eq!(classify_code("ExpiredToken"), AwsErrorClass::Credentials);
        assert_eq!(
            classify_code("InsufficientInstanceCapacity"),
            AwsErrorClass::Capacity
        );
        assert_eq!(classify_code("VcpuLimitExceeded"), AwsErrorClass::Quota);
        assert_eq!(classify_code("Unsupported"), AwsErrorClass::Unsupported);
        assert_eq!(
            classify_code("RequestLimitExceeded"),
            AwsErrorClass::ServerRetryable
        );
        assert_eq!(
            classify_code("InvalidAMIID.Malformed"),
            AwsErrorClass::Other
        );
    }

    #[test]
    fn classes_map_to_kinds_and_scopes() {
        let quota = to_provider_error(AwsErrorClass::Quota, "x".into(), ErrorKind::Failed);
        assert_eq!(quota.kind, ErrorKind::Quota);
        assert_eq!(quota.block_scope(), BlockScope::Region);
        let unsupported =
            to_provider_error(AwsErrorClass::Unsupported, "x".into(), ErrorKind::Failed);
        assert_eq!(unsupported.block_scope(), BlockScope::TypeInRegion);
        assert_eq!(
            to_provider_error(AwsErrorClass::Other, "x".into(), ErrorKind::Unavailable).kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            to_provider_error(
                AwsErrorClass::ServerRetryable,
                "x".into(),
                ErrorKind::Failed
            )
            .kind,
            ErrorKind::Transient
        );
    }
}
