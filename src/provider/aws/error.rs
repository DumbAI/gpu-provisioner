//! AWS error-code classification → `ProvisionError` (the failover taxonomy).
//!
//! Mirrors SkyPilot: credential codes from `sky/provision/aws/utils.py`
//! `handle_boto_error`; server-side retryables from
//! `_ec2_call_with_retry_on_server_error`; everything else during creation is
//! a capacity-class failure for the zone/region.

use aws_sdk_ec2::error::{ProvideErrorMetadata, SdkError};

use crate::provider::ProvisionError;

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
    "InstanceLimitExceeded",
    "VcpuLimitExceeded",
    "MaxSpotInstanceCountExceeded",
    "SpotMaxPriceTooLow",
    "Unsupported",
];

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
    ServerRetryable,
    Other,
}

pub fn classify<E, R>(err: &SdkError<E, R>) -> AwsErrorClass
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    if let Some(code) = err.code() {
        if CREDENTIAL_CODES.contains(&code) {
            return AwsErrorClass::Credentials;
        }
        if CAPACITY_CODES.contains(&code) {
            return AwsErrorClass::Capacity;
        }
        if SERVER_RETRYABLE_CODES.contains(&code) {
            return AwsErrorClass::ServerRetryable;
        }
        return AwsErrorClass::Other;
    }
    // No service error code: dispatch/construction failures. Credential
    // resolution problems (no profile, no keys) surface here.
    let debug = format!("{err:?}").to_lowercase();
    if debug.contains("credential") {
        AwsErrorClass::Credentials
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

/// `map_err` helper for calls made *before* any instance exists (planning,
/// network/image/SG prep): a non-capacity failure means nothing was launched
/// → `Config` (no teardown needed).
pub fn in_bootstrap<E, R>(region: &str) -> impl Fn(SdkError<E, R>) -> ProvisionError + '_
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    move |err| to_provision_error(err, region, true)
}

/// `map_err` helper for calls made *after* an instance may exist: a
/// non-capacity failure is `Other` (triggers teardown).
pub fn at_runtime<E, R>(region: &str) -> impl Fn(SdkError<E, R>) -> ProvisionError + '_
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    move |err| to_provision_error(err, region, false)
}

/// Map an SDK error to the failover taxonomy. Capacity errors get the zone
/// attached by the caller (the subnet loop) since only it knows which zone was
/// being tried.
fn to_provision_error<E, R>(err: SdkError<E, R>, region: &str, in_bootstrap: bool) -> ProvisionError
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    let msg = describe(&err);
    match classify(&err) {
        AwsErrorClass::Credentials => ProvisionError::Credentials { msg },
        AwsErrorClass::Capacity => ProvisionError::Capacity {
            region: region.to_string(),
            zone: None,
            msg,
        },
        AwsErrorClass::ServerRetryable | AwsErrorClass::Other => {
            let region = region.to_string();
            if in_bootstrap {
                ProvisionError::Config { region, msg }
            } else {
                ProvisionError::Other { region, msg }
            }
        }
    }
}
