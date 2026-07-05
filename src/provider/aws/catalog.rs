//! AWS accelerator catalog — the AWS-specific half of candidate planning:
//! GPU shorthand → EC2 instance-type ladder. The generic cross-product with
//! regions/zones lives in `provider::expand_candidates`; only the GPU→type
//! mapping is AWS's own here.

use crate::provider::{
    expand_candidates, Candidate, GpuRequest, ProvisionError, ResourceAsk, ResourceRequest,
};

/// One GPU/count ask can be satisfied by several EC2 types that differ only
/// in CPU/memory. Ladders are ordered smallest (cheapest) first — that order
/// becomes the failover preference. (CPU/memory constraints, when added,
/// will filter these ladders.)
struct AwsGpuOffering {
    gpu: &'static str,
    count: u32,
    /// Instance types, cheapest first.
    instance_types: &'static [&'static str],
}

#[rustfmt::skip]
const OFFERINGS: &[AwsGpuOffering] = &[
    AwsGpuOffering { gpu: "T4", count: 1, instance_types: &["g4dn.xlarge", "g4dn.2xlarge", "g4dn.4xlarge", "g4dn.8xlarge", "g4dn.16xlarge"] },
    AwsGpuOffering { gpu: "T4", count: 4, instance_types: &["g4dn.12xlarge"] },
    AwsGpuOffering { gpu: "T4", count: 8, instance_types: &["g4dn.metal"] },
    AwsGpuOffering { gpu: "A10G", count: 1, instance_types: &["g5.xlarge", "g5.2xlarge", "g5.4xlarge", "g5.8xlarge", "g5.16xlarge"] },
    AwsGpuOffering { gpu: "A10G", count: 4, instance_types: &["g5.12xlarge", "g5.24xlarge"] },
    AwsGpuOffering { gpu: "A10G", count: 8, instance_types: &["g5.48xlarge"] },
    AwsGpuOffering { gpu: "L4", count: 1, instance_types: &["g6.xlarge", "g6.2xlarge", "g6.4xlarge", "g6.8xlarge", "g6.16xlarge"] },
    AwsGpuOffering { gpu: "L4", count: 4, instance_types: &["g6.12xlarge", "g6.24xlarge"] },
    AwsGpuOffering { gpu: "L4", count: 8, instance_types: &["g6.48xlarge"] },
    AwsGpuOffering { gpu: "L40S", count: 1, instance_types: &["g6e.xlarge", "g6e.2xlarge", "g6e.4xlarge", "g6e.8xlarge", "g6e.16xlarge"] },
    AwsGpuOffering { gpu: "L40S", count: 4, instance_types: &["g6e.12xlarge", "g6e.24xlarge"] },
    AwsGpuOffering { gpu: "L40S", count: 8, instance_types: &["g6e.48xlarge"] },
    AwsGpuOffering { gpu: "A100", count: 8, instance_types: &["p4d.24xlarge"] },
    AwsGpuOffering { gpu: "A100-80GB", count: 8, instance_types: &["p4de.24xlarge"] },
    AwsGpuOffering { gpu: "H100", count: 8, instance_types: &["p5.48xlarge"] },
    AwsGpuOffering { gpu: "H200", count: 8, instance_types: &["p5e.48xlarge"] },
];

/// All EC2 instance types satisfying a GPU ask, cheapest first.
pub fn instance_types_for(gpus: &GpuRequest) -> Result<Vec<String>, ProvisionError> {
    OFFERINGS
        .iter()
        .find(|o| o.gpu == gpus.name && o.count == gpus.count)
        .map(|o| o.instance_types.iter().map(|t| t.to_string()).collect())
        .ok_or_else(|| ProvisionError::InvalidRequest {
            msg: format!(
                "no AWS instance type known for GPU '{gpus}'; available: {}",
                OFFERINGS
                    .iter()
                    .map(|o| format!("{}:{}", o.gpu, o.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
}

/// Resolve the resource ask into AWS instance types, then expand over the
/// candidate regions/zone.
pub fn plan_candidates(request: &ResourceRequest) -> Result<Vec<Candidate>, ProvisionError> {
    let instance_types = match &request.ask {
        ResourceAsk::InstanceType(instance_type) => vec![instance_type.clone()],
        ResourceAsk::Gpus(gpus) => instance_types_for(gpus)?,
    };
    expand_candidates(&instance_types, &request.regions, request.zone.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, count: u32) -> GpuRequest {
        GpuRequest {
            name: name.to_string(),
            count,
        }
    }

    #[test]
    fn gpu_resolves_to_type_ladder_cheapest_first() {
        let types = instance_types_for(&gpu("L40S", 1)).unwrap();
        assert_eq!(types[0], "g6e.xlarge");
        assert!(types.contains(&"g6e.8xlarge".to_string()));
        assert!(types.len() > 1, "one GPU ask should yield multiple sizes");

        assert_eq!(
            instance_types_for(&gpu("H100", 8)).unwrap(),
            vec!["p5.48xlarge"]
        );
    }

    #[test]
    fn unknown_gpu_is_invalid_request() {
        let err = instance_types_for(&gpu("B200", 1)).unwrap_err();
        assert!(matches!(err, ProvisionError::InvalidRequest { .. }));
        assert!(err.to_string().contains("available:"), "{err}");
    }

    #[test]
    fn gpu_ask_expands_to_ladder_over_regions() {
        let request = ResourceRequest {
            ask: ResourceAsk::Gpus(gpu("L40S", 4)),
            regions: vec!["us-east-2".into(), "us-west-2".into()],
            zone: None,
        };
        let rendered: Vec<String> = plan_candidates(&request)
            .unwrap()
            .iter()
            .map(|c| c.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "g6e.12xlarge@us-east-2",
                "g6e.12xlarge@us-west-2",
                "g6e.24xlarge@us-east-2",
                "g6e.24xlarge@us-west-2",
            ]
        );
    }
}
