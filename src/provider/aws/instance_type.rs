//! Live instance-type metadata — the catalog-less analog of SkyPilot's
//! instance catalog: `DescribeInstanceTypes` supplies GPU/architecture facts
//! (SkyPilot: `get_accelerators_from_instance_type` /
//! `get_arch_from_instance_type` from CSVs), and
//! `DescribeInstanceTypeOfferings` supplies per-AZ availability (SkyPilot:
//! `valid_on_region_zones` zone filtering).

use std::collections::HashSet;

use aws_sdk_ec2::types::{InstanceType, LocationType};

use crate::provider::ProvisionError;

use super::{error, filter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Arm64,
}

impl Arch {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// What we need to know about an instance type to pick an AMI.
#[derive(Debug, Clone)]
pub struct InstanceTypeMeta {
    pub arch: Arch,
    /// Carries an NVIDIA GPU → use the DLAMI (OSS NVIDIA driver).
    pub is_gpu: bool,
    /// Human-readable GPU summary, e.g. `"1x NVIDIA L40S"`.
    pub gpu_desc: Option<String>,
}

/// Pre-Turing GPUs are not supported by the OSS NVIDIA kernel driver in the
/// default DLAMI — out of scope (warn only; users can pass `--ami`).
const PRE_TURING_GPUS: &[&str] = &["K80", "V100", "M60"];

/// Query GPU/arch facts for an instance type. Unknown types produce a clean
/// `Config` error (nothing launched → failover); transient API failures fall
/// back to a static family heuristic so provisioning can still proceed.
pub async fn instance_type_meta(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    instance_type: &str,
) -> Result<InstanceTypeMeta, ProvisionError> {
    let resp = ec2
        .describe_instance_types()
        .instance_types(InstanceType::from(instance_type))
        .send()
        .await;
    let info = match resp {
        Ok(resp) => match resp.instance_types().first() {
            Some(info) => info.clone(),
            None => {
                return Err(ProvisionError::Config {
                    region: region.to_string(),
                    msg: format!("unknown instance type '{instance_type}'"),
                })
            }
        },
        Err(e) => {
            let code = error::error_code(&e);
            if matches!(
                code.as_deref(),
                Some("InvalidInstanceType") | Some("InvalidParameterValue")
            ) {
                return Err(ProvisionError::Config {
                    region: region.to_string(),
                    msg: format!(
                        "unknown instance type '{instance_type}': {}",
                        error::describe(&e)
                    ),
                });
            }
            if error::classify(&e) == error::AwsErrorClass::Credentials {
                return Err(ProvisionError::Credentials {
                    msg: error::describe(&e),
                });
            }
            tracing::warn!(
                "[{region}] DescribeInstanceTypes failed ({}); falling back to \
                 static family heuristics for '{instance_type}'",
                error::describe(&e)
            );
            return Ok(fallback_meta(instance_type));
        }
    };

    let archs = info
        .processor_info()
        .map(|p| p.supported_architectures())
        .unwrap_or_default();
    let arch = if archs.iter().any(|a| a.as_str() == "x86_64") {
        Arch::X86_64
    } else if archs.iter().any(|a| a.as_str() == "arm64") {
        Arch::Arm64
    } else {
        tracing::warn!(
            "[{region}] no supported architecture reported for '{instance_type}'; assuming x86_64"
        );
        Arch::X86_64
    };

    let gpus: Vec<_> = info
        .gpu_info()
        .map(|g| g.gpus())
        .unwrap_or_default()
        .iter()
        .filter(|g| g.manufacturer() == Some("NVIDIA"))
        .cloned()
        .collect();
    let is_gpu = !gpus.is_empty();
    let gpu_desc = if is_gpu {
        Some(
            gpus.iter()
                .map(|g| {
                    format!(
                        "{}x NVIDIA {}",
                        g.count().unwrap_or(1),
                        g.name().unwrap_or("GPU")
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    } else {
        None
    };

    let pre_turing: Vec<&str> = gpus
        .iter()
        .filter_map(|g| g.name())
        .filter(|n| PRE_TURING_GPUS.contains(n))
        .collect();
    if !pre_turing.is_empty() {
        tracing::warn!(
            "[{region}] '{instance_type}' has pre-Turing GPU(s) ({}) — the default AMI's \
             OSS NVIDIA driver does not support them; pass --ami with a suitable image",
            pre_turing.join(", ")
        );
    }

    Ok(InstanceTypeMeta {
        arch,
        is_gpu,
        gpu_desc,
    })
}

/// Static fallback when the metadata API is unavailable: NVIDIA GPU families
/// by prefix, x86_64 assumed. (The former `catalog::is_gpu_instance_type`.)
pub fn fallback_meta(instance_type: &str) -> InstanceTypeMeta {
    let family = instance_type.split('.').next().unwrap_or("");
    let is_gpu = matches!(
        family,
        "p2" | "p3"
            | "p3dn"
            | "p4d"
            | "p4de"
            | "p5"
            | "p5e"
            | "p5en"
            | "p6"
            | "p6e"
            | "g3"
            | "g3s"
            | "g4dn"
            | "g4ad"
            | "g5"
            | "g6"
            | "g6e"
            | "g6f"
            | "gr6"
    );
    InstanceTypeMeta {
        arch: Arch::X86_64,
        is_gpu,
        gpu_desc: None,
    }
}

/// Availability zones (by name) where the instance type is offered in this
/// region. Empty set = the type is not sold in the region at all.
pub async fn zones_offering(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    instance_type: &str,
) -> Result<HashSet<String>, ProvisionError> {
    let resp = ec2
        .describe_instance_type_offerings()
        .location_type(LocationType::AvailabilityZone)
        .filters(filter("instance-type", instance_type))
        .send()
        .await
        .map_err(error::in_bootstrap(region))?;
    Ok(resp
        .instance_type_offerings()
        .iter()
        .filter_map(|o| o.location().map(str::to_string))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_detects_gpu_families() {
        assert!(fallback_meta("g5.xlarge").is_gpu);
        assert!(fallback_meta("g6e.8xlarge").is_gpu);
        assert!(fallback_meta("p4d.24xlarge").is_gpu);
        assert!(!fallback_meta("t3.micro").is_gpu);
        assert!(!fallback_meta("m7i.large").is_gpu);
        assert_eq!(fallback_meta("g5.xlarge").arch, Arch::X86_64);
    }
}
