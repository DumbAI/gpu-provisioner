//! AMI resolution without a bundled image catalog: SSM public-parameter
//! lookups keyed on architecture × GPU, then a `DescribeImages` call for the
//! per-AMI facts a launch needs: the root device name and the minimum root
//! disk size.
//! Failures → `Config` error → failover to the next region.

use crate::provider::ProviderError;

use super::error;
use super::instance_type::Arch;

/// Canonical Ubuntu 22.04 LTS — CPU instances.
pub const CPU_X86_AMI_SSM_PARAM: &str =
    "/aws/service/canonical/ubuntu/server/22.04/stable/current/amd64/hvm/ebs-gp2/ami-id";
pub const CPU_ARM64_AMI_SSM_PARAM: &str =
    "/aws/service/canonical/ubuntu/server/22.04/stable/current/arm64/hvm/ebs-gp2/ami-id";

/// AWS Deep Learning Base AMI (OSS NVIDIA driver, Ubuntu 22.04) — GPU
/// instances; NVIDIA driver preinstalled so `nvidia-smi` works on boot.
pub const GPU_X86_AMI_SSM_PARAM: &str =
    "/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-ubuntu-22.04/latest/ami-id";
pub const GPU_ARM64_AMI_SSM_PARAM: &str =
    "/aws/service/deeplearning/ami/arm64/base-oss-nvidia-driver-gpu-ubuntu-22.04/latest/ami-id";

pub const DEFAULT_ROOT_DEVICE_NAME: &str = "/dev/sda1";

/// The arch × GPU AMI matrix.
pub fn pick_ssm_param(arch: Arch, is_gpu: bool) -> &'static str {
    match (arch, is_gpu) {
        (Arch::X86_64, false) => CPU_X86_AMI_SSM_PARAM,
        (Arch::Arm64, false) => CPU_ARM64_AMI_SSM_PARAM,
        (Arch::X86_64, true) => GPU_X86_AMI_SSM_PARAM,
        (Arch::Arm64, true) => GPU_ARM64_AMI_SSM_PARAM,
    }
}

/// A resolved AMI plus the per-image facts needed to launch correctly.
#[derive(Debug, Clone)]
pub struct ResolvedAmi {
    pub ami_id: String,
    /// Root device name for `BlockDeviceMappings` (wrong name = extra volume
    /// instead of a resized root disk).
    pub root_device_name: String,
    /// Snapshot size of the root volume — the smallest legal `--disk`.
    pub min_disk_gb: i32,
}

/// Resolve the default AMI for this region via SSM public parameters.
pub async fn resolve_ami(
    ssm: &aws_sdk_ssm::Client,
    arch: Arch,
    is_gpu: bool,
) -> Result<String, ProviderError> {
    let param = pick_ssm_param(arch, is_gpu);
    let resp =
        ssm.get_parameter()
            .name(param)
            .send()
            .await
            .map_err(|e| match error::classify(&e) {
                error::AwsErrorClass::Credentials => {
                    ProviderError::credentials(error::describe(&e))
                }
                _ => ProviderError::unavailable(format!(
                    "failed to resolve AMI via SSM parameter '{param}': {} \
                         (pass --ami to use a specific image)",
                    error::describe(&e)
                )),
            })?;
    resp.parameter()
        .and_then(|p| p.value())
        .map(str::to_string)
        .ok_or_else(|| {
            ProviderError::unavailable(format!("SSM parameter '{param}' returned no value"))
        })
}

/// Look up the AMI's root device name and minimum root disk size; also
/// validates that the image (default or user-supplied `--ami`) exists in the
/// region.
pub async fn describe_ami(
    ec2: &aws_sdk_ec2::Client,
    ami_id: &str,
) -> Result<ResolvedAmi, ProviderError> {
    let resp = ec2
        .describe_images()
        .image_ids(ami_id)
        .send()
        .await
        .map_err(error::in_bootstrap)?;
    let Some(image) = resp.images().first() else {
        return Err(ProviderError::unavailable(format!(
            "AMI '{ami_id}' not found in this region"
        )));
    };
    let root_device_name = image
        .root_device_name()
        .unwrap_or(DEFAULT_ROOT_DEVICE_NAME)
        .to_string();
    let min_disk_gb = image
        .block_device_mappings()
        .iter()
        .find(|m| m.device_name() == Some(root_device_name.as_str()))
        .and_then(|m| m.ebs())
        .and_then(|e| e.volume_size())
        .unwrap_or(0);
    Ok(ResolvedAmi {
        ami_id: ami_id.to_string(),
        root_device_name,
        min_disk_gb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssm_param_matrix() {
        assert_eq!(pick_ssm_param(Arch::X86_64, false), CPU_X86_AMI_SSM_PARAM);
        assert_eq!(pick_ssm_param(Arch::Arm64, false), CPU_ARM64_AMI_SSM_PARAM);
        assert_eq!(pick_ssm_param(Arch::X86_64, true), GPU_X86_AMI_SSM_PARAM);
        assert_eq!(pick_ssm_param(Arch::Arm64, true), GPU_ARM64_AMI_SSM_PARAM);
        // GPU params must point at the OSS-NVIDIA-driver DLAMI.
        assert!(pick_ssm_param(Arch::X86_64, true).contains("base-oss-nvidia-driver"));
    }
}
