//! The cloud-agnostic half of planning: validate the `VmSpec` and
//! split it into (a) a `ResourceRequest` for the provider to resolve into
//! cloud-specific offers (`CloudProvider::plan`), (b) the offer-independent
//! `InstanceSpec`, and (c) the optional features the launch depends on. The
//! cloud-dependent parts — GPU→instance-type mapping and region/zone
//! conventions — live in each provider's catalog.
//!
//! Workload concerns (k3s ports, the installer) are not planned here: the
//! cluster layer (`cluster::k3s`) adapts the `InstanceSpec` to the chosen
//! provider's capabilities.

use crate::config::Defaults;
use crate::provider::{Capabilities, Feature, InstanceSpec, ResourceAsk, ResourceRequest};
use crate::spec::{SpecError, VmSpec};
use crate::userdata;

/// Output of [`prepare`].
#[derive(Debug, Clone)]
pub struct Plan {
    pub spec: InstanceSpec,
    pub request: ResourceRequest,
    /// Optional cloud features the user asked for — checked against the
    /// provider's capabilities before planning.
    pub required_features: Vec<Feature>,
    /// Make the VM a k3s cluster (spec/flags, else `[defaults] kubernetes`).
    pub kubernetes: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error("invalid --gpus value: {0}")]
    InvalidGpus(String),
}

/// Validate the spec and assemble the provider-facing request plus the
/// offer-independent instance fields. `default_regions` is the target cloud's
/// configured fallback; whether an empty region list is acceptable is the
/// provider's call (AWS/GCP require regions, Prime Intellect means "any").
pub fn prepare(
    spec: &VmSpec,
    defaults: &Defaults,
    default_regions: &[String],
    public_key_openssh: &str,
) -> Result<Plan, PlanError> {
    let vm_name = spec.validated_name()?;

    let ask = match (&spec.instance_type, &spec.gpus) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(SpecError::InstanceTypeXorGpus.into());
        }
        (Some(instance_type), None) => ResourceAsk::InstanceType(instance_type.clone()),
        (None, Some(gpus)) => ResourceAsk::Gpus(gpus.parse().map_err(PlanError::InvalidGpus)?),
    };

    let regions = if spec.regions.is_empty() {
        default_regions.to_vec()
    } else {
        spec.regions.clone()
    };

    let ssh_user = spec
        .ssh_user
        .clone()
        .unwrap_or_else(|| defaults.ssh_user.clone());
    let user_data = userdata::build_cloud_init(&ssh_user, public_key_openssh);

    // An explicit k8s ask is a hard requirement; the default is best-effort
    // (skipped on clouds that cannot host k3s, see `Plan::kubernetes_for`).
    let required_features = [
        (spec.ami.is_some(), Feature::ImageOverride),
        (spec.spot, Feature::Spot),
        (!spec.ports.is_empty(), Feature::OpenPorts),
        (spec.kubernetes == Some(true), Feature::Kubernetes),
    ]
    .into_iter()
    .filter_map(|(needed, feature)| needed.then_some(feature))
    .collect();

    Ok(Plan {
        spec: InstanceSpec {
            vm_name,
            image_id: spec.ami.clone(),
            disk_gb: spec.disk_gb.unwrap_or(defaults.disk_gb),
            spot: spec.spot,
            open_ports: spec.ports.clone(),
            user_data,
            ssh_public_key: public_key_openssh.trim().to_string(),
            ssh_user,
            boot_script: None,
        },
        request: ResourceRequest {
            ask,
            regions,
            zone: spec.zone.clone(),
            spot: spec.spot,
        },
        required_features,
        kubernetes: spec.kubernetes.unwrap_or(defaults.kubernetes),
    })
}

impl Plan {
    /// Whether to set up k3s on a provider with `caps`. An explicit request
    /// is kept (and rejected by the capability check if unsupported); the
    /// default is dropped where k3s cannot run (e.g. the Docker test cloud).
    pub fn kubernetes_for(&self, caps: &Capabilities) -> bool {
        self.kubernetes
            && (self.required_features.contains(&Feature::Kubernetes)
                || caps.supports(Feature::Kubernetes))
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

    fn plan(spec: &VmSpec) -> Result<Plan, PlanError> {
        prepare(spec, &Defaults::default(), &[], KEY)
    }

    #[test]
    fn splits_spec_into_instance_spec_and_request() {
        let plan = plan(&base_spec()).unwrap();
        assert_eq!(plan.spec.vm_name, "vm1");
        assert!(plan.spec.user_data.contains(KEY));
        assert_eq!(plan.spec.ssh_public_key, KEY);
        assert!(matches!(
            &plan.request.ask,
            ResourceAsk::InstanceType(it) if it == "t3.micro"
        ));
        assert_eq!(plan.request.regions, vec!["us-west-2", "us-east-1"]);
        // Zone resolution is the provider's job; the pin passes through.
        assert!(plan.request.zone.is_none());
        assert!(plan.required_features.is_empty());
    }

    #[test]
    fn kubernetes_does_not_touch_the_instance_plan() {
        // The cluster layer adds k3s ports/boot script per provider capability.
        let mut spec = base_spec();
        spec.kubernetes = Some(true);
        let plan = plan(&spec).unwrap();
        assert!(plan.spec.open_ports.is_empty());
        assert!(!plan.spec.user_data.contains("k3s"));
    }

    #[test]
    fn kubernetes_defaults_on_and_is_skipped_only_when_implicit() {
        let no_k3s = Capabilities {
            unsupported: &[(Feature::Kubernetes, "no systemd")],
            ..Capabilities::default()
        };
        let full = Capabilities::default();

        // Default on; dropped silently where k3s cannot run.
        let implicit = plan(&base_spec()).unwrap();
        assert!(implicit.kubernetes);
        assert!(!implicit.required_features.contains(&Feature::Kubernetes));
        assert!(implicit.kubernetes_for(&full));
        assert!(!implicit.kubernetes_for(&no_k3s));

        // Explicit: kept, and required (so an unsupported cloud rejects it).
        let mut spec = base_spec();
        spec.kubernetes = Some(true);
        let explicit = plan(&spec).unwrap();
        assert!(explicit.required_features.contains(&Feature::Kubernetes));
        assert!(explicit.kubernetes_for(&no_k3s));

        // Opt-outs: the spec, or the config default.
        spec.kubernetes = Some(false);
        assert!(!plan(&spec).unwrap().kubernetes);
        let off = Defaults {
            kubernetes: false,
            ..Defaults::default()
        };
        assert!(!prepare(&base_spec(), &off, &[], KEY).unwrap().kubernetes);
    }

    #[test]
    fn required_features_reflect_the_spec() {
        let mut spec = base_spec();
        spec.ami = Some("ami-1".into());
        spec.spot = true;
        spec.ports = vec![8080];
        let plan = plan(&spec).unwrap();
        assert_eq!(
            plan.required_features,
            vec![Feature::ImageOverride, Feature::Spot, Feature::OpenPorts]
        );
        assert_eq!(plan.spec.open_ports, vec![8080]);
        assert!(plan.request.spot);
    }

    #[test]
    fn gpus_shorthand_is_parsed_not_resolved() {
        let mut spec = base_spec();
        spec.instance_type = None;
        spec.gpus = Some("l40s:4".into());
        match &plan(&spec).unwrap().request.ask {
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
        assert!(matches!(plan(&spec), Err(PlanError::InvalidGpus(_))));
    }

    #[test]
    fn falls_back_to_provider_default_regions_and_allows_none() {
        let mut spec = base_spec();
        spec.regions = Vec::new();
        let plan_with =
            prepare(&spec, &Defaults::default(), &["eu-central-1".into()], KEY).unwrap();
        assert_eq!(plan_with.request.regions, vec!["eu-central-1"]);
        // No regions at all is the provider's call to accept or reject.
        assert!(plan(&spec).unwrap().request.regions.is_empty());
    }

    #[test]
    fn instance_type_xor_gpus() {
        let mut spec = base_spec();
        spec.gpus = Some("T4:1".into());
        assert!(matches!(
            plan(&spec),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
        spec.instance_type = None;
        spec.gpus = None;
        assert!(matches!(
            plan(&spec),
            Err(PlanError::Spec(SpecError::InstanceTypeXorGpus))
        ));
    }
}
