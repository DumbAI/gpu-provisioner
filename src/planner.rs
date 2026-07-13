//! The cloud-agnostic half of planning ("Phase 3"): validate the `VmSpec` and
//! split it into (a) a `ResourceRequest` for the provider to resolve into
//! cloud-specific `{instance_type, region, zone}` candidates
//! (`CloudProvider::plan_candidates`), and (b) the candidate-independent
//! `LaunchSpec`. The cloud-dependent parts — GPU→instance-type mapping and
//! region/zone conventions — live in each provider (AWS:
//! `provider/aws/catalog.rs`), mirroring how SkyPilot's optimizer defers to
//! per-cloud catalogs.

use crate::config::Defaults;
use crate::kube;
use crate::provider::{Candidate, ProvisionRequest, ResourceAsk, ResourceRequest};
use crate::spec::{SpecError, VmSpec};
use crate::userdata::{self, Workload};

/// Ports opened (at security-group creation) for a k3s node: the Kubernetes API
/// server plus the k3s-bundled Traefik ingress / ServiceLB HTTP(S) endpoints.
const K3S_PORTS: [u16; 3] = [6443, 80, 443];

/// Everything needed to provision that does NOT vary per candidate.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub vm_name: String,
    pub image_id: Option<String>,
    pub disk_gb: i32,
    pub spot: bool,
    pub extra_ports: Vec<u16>,
    pub ssh_user: String,
    pub user_data: String,
}

/// Output of `prepare`: the invariants plus the resource ask for the provider.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub launch: LaunchSpec,
    pub request: ResourceRequest,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error("invalid --gpus value: {0}")]
    InvalidGpus(String),
    #[error("no candidate regions: pass --region (repeatable) or set defaults.regions in the config file")]
    NoRegions,
}

/// Validate the spec and assemble the provider-facing resource request plus
/// the candidate-independent launch fields.
pub fn prepare(
    spec: &VmSpec,
    defaults: &Defaults,
    public_key_openssh: &str,
) -> Result<Prepared, PlanError> {
    let vm_name = spec.validated_name()?;

    let ask = match (&spec.instance_type, &spec.gpus) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(SpecError::InstanceTypeXorGpus.into());
        }
        (Some(instance_type), None) => ResourceAsk::InstanceType(instance_type.clone()),
        (None, Some(gpus)) => ResourceAsk::Gpus(gpus.parse().map_err(PlanError::InvalidGpus)?),
    };

    let regions = if spec.regions.is_empty() {
        defaults.regions.clone()
    } else {
        spec.regions.clone()
    };
    if regions.is_empty() {
        return Err(PlanError::NoRegions);
    }

    let ssh_user = spec
        .ssh_user
        .clone()
        .unwrap_or_else(|| defaults.ssh_user.clone());

    // The k8s ask (opt-in via the spec) both opens the k3s ports and swaps the
    // cloud-init workload for the k3s installer. GPU-ness is decided at boot by
    // the script itself, so the plan stays candidate-independent here.
    let mut extra_ports = spec.ports.clone();
    let workload = if spec.kubernetes {
        extra_ports.extend(K3S_PORTS);
        Workload::K3s {
            install_script: kube::K3S_INSTALL_SCRIPT,
        }
    } else {
        Workload::Bare
    };
    let user_data = userdata::build_cloud_init(&ssh_user, public_key_openssh, &workload);

    Ok(Prepared {
        launch: LaunchSpec {
            vm_name,
            image_id: spec.ami.clone(),
            disk_gb: spec.disk_gb.unwrap_or(defaults.disk_gb),
            spot: spec.spot,
            extra_ports,
            ssh_user,
            user_data,
        },
        request: ResourceRequest {
            ask,
            regions,
            zone: spec.zone.clone(),
        },
    })
}

/// Combine the launch invariants with one provider-produced candidate.
pub fn build_request(launch: &LaunchSpec, candidate: &Candidate) -> ProvisionRequest {
    ProvisionRequest {
        vm_name: launch.vm_name.clone(),
        region: candidate.region.clone(),
        zone: candidate.zone.clone(),
        instance_type: candidate.instance_type.clone(),
        image_id: launch.image_id.clone(),
        disk_gb: launch.disk_gb,
        spot: launch.spot,
        extra_ports: launch.extra_ports.clone(),
        user_data: launch.user_data.clone(),
        ssh_user: launch.ssh_user.clone(),
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
    fn splits_spec_into_launch_and_request() {
        let prepared = prepare(&base_spec(), &Defaults::default(), KEY).unwrap();
        assert_eq!(prepared.launch.vm_name, "vm1");
        assert!(prepared.launch.user_data.contains(KEY));
        assert!(matches!(
            &prepared.request.ask,
            ResourceAsk::InstanceType(it) if it == "t3.micro"
        ));
        assert_eq!(prepared.request.regions, vec!["us-west-2", "us-east-1"]);
        // Zone resolution is the provider's job; the pin passes through.
        assert!(prepared.request.zone.is_none());
    }

    #[test]
    fn kubernetes_opens_k3s_ports_and_embeds_installer() {
        let mut spec = base_spec();
        spec.kubernetes = true;
        spec.ports = vec![8080];
        let prepared = prepare(&spec, &Defaults::default(), KEY).unwrap();
        // User ports are preserved and the k3s ports are added.
        for p in [8080u16, 6443, 80, 443] {
            assert!(prepared.launch.extra_ports.contains(&p), "missing port {p}");
        }
        // The real installer embeds cleanly and the whole document stays valid YAML.
        assert!(prepared.launch.user_data.contains("get.k3s.io"));
        assert!(prepared.launch.user_data.contains("--tls-san"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&prepared.launch.user_data).unwrap();
        assert!(parsed.get("runcmd").is_some());
    }

    #[test]
    fn non_kubernetes_leaves_ports_and_userdata_bare() {
        let mut spec = base_spec();
        spec.ports = vec![8080];
        let prepared = prepare(&spec, &Defaults::default(), KEY).unwrap();
        assert_eq!(prepared.launch.extra_ports, vec![8080]);
        assert!(!prepared.launch.user_data.contains("k3s"));
        assert!(!prepared.launch.user_data.contains("write_files"));
    }

    #[test]
    fn gpus_shorthand_is_parsed_not_resolved() {
        let mut spec = base_spec();
        spec.instance_type = None;
        spec.gpus = Some("l40s:4".into());
        let prepared = prepare(&spec, &Defaults::default(), KEY).unwrap();
        match &prepared.request.ask {
            ResourceAsk::Gpus(gpus) => {
                assert_eq!(gpus.name, "L40S");
                assert_eq!(gpus.count, 4);
            }
            other => panic!("expected Gpus ask, got {other:?}"),
        }
    }

    #[test]
    fn malformed_gpus_is_rejected() {
        let mut spec = base_spec();
        spec.instance_type = None;
        spec.gpus = Some("L40S:x".into());
        assert!(matches!(
            prepare(&spec, &Defaults::default(), KEY),
            Err(PlanError::InvalidGpus(_))
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
        let prepared = prepare(&spec, &defaults, KEY).unwrap();
        assert_eq!(prepared.request.regions, vec!["eu-central-1"]);

        assert!(matches!(
            prepare(&spec, &Defaults::default(), KEY),
            Err(PlanError::NoRegions)
        ));
    }

    #[test]
    fn instance_type_xor_gpus() {
        let mut spec = base_spec();
        spec.gpus = Some("T4:1".into());
        assert!(matches!(
            prepare(&spec, &Defaults::default(), KEY),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
        spec.instance_type = None;
        spec.gpus = None;
        assert!(matches!(
            prepare(&spec, &Defaults::default(), KEY),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
    }

    #[test]
    fn build_request_combines_launch_and_candidate() {
        let prepared = prepare(&base_spec(), &Defaults::default(), KEY).unwrap();
        let candidate = Candidate {
            instance_type: "g6e.8xlarge".into(),
            region: "us-east-2".into(),
            zone: Some("us-east-2a".into()),
        };
        let request = build_request(&prepared.launch, &candidate);
        assert_eq!(request.vm_name, "vm1");
        assert_eq!(request.instance_type, "g6e.8xlarge");
        assert_eq!(request.region, "us-east-2");
        assert_eq!(request.zone.as_deref(), Some("us-east-2a"));
    }
}
