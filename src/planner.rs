//! The mini-optimizer ("Phase 3"): turn a validated `VmSpec` into an ordered
//! list of failover candidates plus the per-candidate `ProvisionRequest`
//! builder — the analog of SkyPilot's `Optimizer` +
//! `make_launchables_for_valid_region_zones` + `make_deploy_resources_variables`,
//! without a pricing catalog: the user's region order is the preference order.

use crate::catalog::{self, CatalogError};
use crate::config::Defaults;
use crate::provider::ProvisionRequest;
use crate::spec::{SpecError, VmSpec};
use crate::userdata;

/// One failover candidate: a region, optionally pinned to specific zones.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub region: String,
    pub zones: Vec<String>,
}

/// Everything needed to provision, independent of the candidate region.
#[derive(Debug, Clone)]
pub struct Plan {
    pub vm_name: String,
    pub candidates: Vec<Candidate>,
    pub instance_type: String,
    pub needs_gpu_ami: bool,
    pub ami: Option<String>,
    pub disk_gb: i32,
    pub spot: bool,
    pub extra_ports: Vec<u16>,
    pub ssh_user: String,
    pub user_data: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("no candidate regions: pass --region (repeatable) or set defaults.regions in the config file")]
    NoRegions,
    #[error("zone '{zone}' does not belong to any candidate region ({regions})")]
    ZoneOutsideRegions { zone: String, regions: String },
}

pub fn plan(
    spec: &VmSpec,
    defaults: &Defaults,
    public_key_openssh: &str,
) -> Result<Plan, PlanError> {
    let vm_name = spec.validated_name()?;

    let (instance_type, needs_gpu_ami) = match (&spec.instance_type, &spec.gpus) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(SpecError::InstanceTypeXorGpus.into());
        }
        (Some(it), None) => (it.clone(), catalog::is_gpu_instance_type(it)),
        (None, Some(gpus)) => {
            let offering = catalog::resolve_gpus(gpus)?;
            (offering.instance_type.to_string(), true)
        }
    };

    let regions = if spec.regions.is_empty() {
        defaults.regions.clone()
    } else {
        spec.regions.clone()
    };
    if regions.is_empty() {
        return Err(PlanError::NoRegions);
    }

    // A zone pin implies its region (SkyPilot: `Resources(zone=..)` fixes the
    // region); all other candidate regions drop out.
    let candidates = match &spec.zone {
        None => regions
            .iter()
            .map(|r| Candidate {
                region: r.clone(),
                zones: Vec::new(),
            })
            .collect(),
        Some(zone) => {
            let owner = regions.iter().find(|r| zone.starts_with(r.as_str()));
            match owner {
                Some(region) => vec![Candidate {
                    region: region.clone(),
                    zones: vec![zone.clone()],
                }],
                None => {
                    return Err(PlanError::ZoneOutsideRegions {
                        zone: zone.clone(),
                        regions: regions.join(", "),
                    })
                }
            }
        }
    };

    let ssh_user = spec
        .ssh_user
        .clone()
        .unwrap_or_else(|| defaults.ssh_user.clone());
    let user_data = userdata::build_cloud_init(&ssh_user, public_key_openssh);

    Ok(Plan {
        vm_name,
        candidates,
        instance_type,
        needs_gpu_ami,
        ami: spec.ami.clone(),
        disk_gb: spec.disk_gb.unwrap_or(defaults.disk_gb),
        spot: spec.spot,
        extra_ports: spec.ports.clone(),
        ssh_user,
        user_data,
    })
}

pub fn build_request(plan: &Plan, candidate: &Candidate) -> ProvisionRequest {
    ProvisionRequest {
        vm_name: plan.vm_name.clone(),
        region: candidate.region.clone(),
        zones: candidate.zones.clone(),
        instance_type: plan.instance_type.clone(),
        ami_id: plan.ami.clone(),
        needs_gpu_ami: plan.needs_gpu_ami,
        disk_gb: plan.disk_gb,
        spot: plan.spot,
        extra_ports: plan.extra_ports.clone(),
        user_data: plan.user_data.clone(),
        ssh_user: plan.ssh_user.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAA test";

    fn base_spec() -> VmSpec {
        VmSpec {
            name: Some("vm1".into()),
            instance_type: Some("t3.micro".into()),
            regions: vec!["us-west-2".into(), "us-east-1".into()],
            ..Default::default()
        }
    }

    #[test]
    fn expands_regions_in_order() {
        let plan = plan(&base_spec(), &Defaults::default(), KEY).unwrap();
        let regions: Vec<&str> = plan.candidates.iter().map(|c| c.region.as_str()).collect();
        assert_eq!(regions, vec!["us-west-2", "us-east-1"]);
        assert!(!plan.needs_gpu_ami);
        assert!(plan.user_data.contains(KEY));
    }

    #[test]
    fn gpus_shorthand_resolves_and_needs_gpu_ami() {
        let mut spec = base_spec();
        spec.instance_type = None;
        spec.gpus = Some("A10G:1".into());
        let plan = plan(&spec, &Defaults::default(), KEY).unwrap();
        assert_eq!(plan.instance_type, "g5.xlarge");
        assert!(plan.needs_gpu_ami);
    }

    #[test]
    fn gpu_instance_type_detected() {
        let mut spec = base_spec();
        spec.instance_type = Some("g4dn.xlarge".into());
        let plan = plan(&spec, &Defaults::default(), KEY).unwrap();
        assert!(plan.needs_gpu_ami);
    }

    #[test]
    fn zone_pin_restricts_to_owning_region() {
        let mut spec = base_spec();
        spec.zone = Some("us-east-1b".into());
        let plan = plan(&spec, &Defaults::default(), KEY).unwrap();
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].region, "us-east-1");
        assert_eq!(plan.candidates[0].zones, vec!["us-east-1b"]);
    }

    #[test]
    fn zone_outside_regions_is_rejected() {
        let mut spec = base_spec();
        spec.zone = Some("eu-west-1a".into());
        assert!(matches!(
            plan(&spec, &Defaults::default(), KEY),
            Err(PlanError::ZoneOutsideRegions { .. })
        ));
    }

    #[test]
    fn falls_back_to_default_regions_and_errors_when_none() {
        let mut spec = base_spec();
        spec.regions = Vec::new();
        let defaults = Defaults {
            regions: vec!["eu-central-1".into()],
            ..Default::default()
        };
        let p = plan(&spec, &defaults, KEY).unwrap();
        assert_eq!(p.candidates[0].region, "eu-central-1");

        assert!(matches!(
            plan(&spec, &Defaults::default(), KEY),
            Err(PlanError::NoRegions)
        ));
    }

    #[test]
    fn instance_type_xor_gpus() {
        let mut spec = base_spec();
        spec.gpus = Some("T4:1".into());
        assert!(matches!(
            plan(&spec, &Defaults::default(), KEY),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
        spec.instance_type = None;
        spec.gpus = None;
        assert!(matches!(
            plan(&spec, &Defaults::default(), KEY),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
    }
}
