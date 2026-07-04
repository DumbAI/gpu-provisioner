//! AMI resolution via AWS SSM public parameters — replaces SkyPilot's CSV
//! image catalog (`skypilot:custom-gpu-ubuntu-*` tags) with two well-known,
//! always-current lookups. Failure → `Config` error → failover to the next
//! region.

use crate::provider::ProvisionError;

use super::error;

/// Canonical Ubuntu 22.04 LTS (x86_64) — CPU instances.
pub const CPU_AMI_SSM_PARAM: &str =
    "/aws/service/canonical/ubuntu/server/22.04/stable/current/amd64/hvm/ebs-gp2/ami-id";

/// AWS Deep Learning Base AMI (OSS NVIDIA driver, Ubuntu 22.04, x86_64) —
/// GPU instances; NVIDIA driver preinstalled so `nvidia-smi` works on boot.
pub const GPU_AMI_SSM_PARAM: &str =
    "/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-ubuntu-22.04/latest/ami-id";

pub async fn resolve_ami(
    ssm: &aws_sdk_ssm::Client,
    region: &str,
    needs_gpu: bool,
) -> Result<String, ProvisionError> {
    let param = if needs_gpu {
        GPU_AMI_SSM_PARAM
    } else {
        CPU_AMI_SSM_PARAM
    };
    let resp =
        ssm.get_parameter()
            .name(param)
            .send()
            .await
            .map_err(|e| match error::classify(&e) {
                error::AwsErrorClass::Credentials => ProvisionError::Credentials {
                    msg: error::describe(&e),
                },
                _ => ProvisionError::Config {
                    region: region.to_string(),
                    msg: format!(
                        "failed to resolve AMI via SSM parameter '{param}': {}",
                        error::describe(&e)
                    ),
                },
            })?;
    resp.parameter()
        .and_then(|p| p.value())
        .map(str::to_string)
        .ok_or_else(|| ProvisionError::Config {
            region: region.to_string(),
            msg: format!("SSM parameter '{param}' returned no value"),
        })
}
