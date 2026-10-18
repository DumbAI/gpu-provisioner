//! Pure request/response shaping for Compute Engine instances: the
//! `instances.insert` body, the per-VM firewall rule, and status/endpoint
//! normalization of listed instances.

use serde_json::{json, Value};

use crate::provider::{InstanceSpec, InstanceStatus, Observed, SshEndpoint};

use super::api::Instance;
use super::catalog::{basename, TypeChoice};

/// Label carrying the VM identity (the AWS tag analog; label keys allow `-`).
pub const LABEL_VM_NAME: &str = "gpuprov-vm-name";
pub const LABEL_MANAGED: &str = "gpuprov-managed";

/// Instance names must be unique per zone and start with a letter.
pub fn instance_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// Per-VM firewall rule name, also used as the instance's network tag.
pub fn network_tag(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// List filter selecting a VM's instances.
pub fn label_filter(vm_name: &str) -> String {
    format!("labels.{LABEL_VM_NAME} = \"{vm_name}\"")
}

/// Body for `instances.insert` in `zone`.
pub fn insert_body(
    zone: &str,
    choice: &TypeChoice,
    image: &str,
    tag: &str,
    spec: &InstanceSpec,
) -> Value {
    let scheduling = if spec.spot {
        // Spot VMs cannot restart automatically; delete on preemption so a
        // preempted VM shows up as gone rather than as a stopped husk.
        json!({
            "provisioningModel": "SPOT",
            "instanceTerminationAction": "DELETE",
            "onHostMaintenance": "TERMINATE",
            "automaticRestart": false,
        })
    } else if choice.gpu {
        // GPU VMs cannot live-migrate.
        json!({"onHostMaintenance": "TERMINATE", "automaticRestart": true})
    } else {
        json!({"onHostMaintenance": "MIGRATE", "automaticRestart": true})
    };
    let mut body = json!({
        "name": instance_name(&spec.vm_name),
        "machineType": format!("zones/{zone}/machineTypes/{}", choice.machine_type),
        "labels": {
            LABEL_VM_NAME: spec.vm_name,
            LABEL_MANAGED: "true",
        },
        "tags": {"items": [tag]},
        "disks": [{
            "boot": true,
            "autoDelete": true,
            "initializeParams": {
                "sourceImage": image,
                "diskSizeGb": spec.disk_gb.to_string(),
            },
        }],
        "networkInterfaces": [{
            "network": "global/networks/default",
            "accessConfigs": [{"name": "External NAT", "type": "ONE_TO_ONE_NAT"}],
        }],
        "metadata": {"items": [
            // The guest agent creates the user from `ssh-keys`; cloud-init
            // (where the image runs it) applies `user-data` as on AWS.
            {"key": "ssh-keys", "value": format!("{}:{}", spec.ssh_user, spec.ssh_public_key.trim())},
            {"key": "user-data", "value": spec.user_data},
        ]},
        "scheduling": scheduling,
    });
    if let Some(acc) = &choice.accelerator {
        body["guestAccelerators"] = json!([{
            "acceleratorType": format!("zones/{zone}/acceleratorTypes/{}", acc.kind),
            "acceleratorCount": acc.count,
        }]);
    }
    body
}

/// Body for the per-VM ingress firewall rule on the default network.
pub fn firewall_body(tag: &str, open_ports: &[u16]) -> Value {
    json!({
        "name": tag,
        "network": "global/networks/default",
        "direction": "INGRESS",
        "sourceRanges": ["0.0.0.0/0"],
        "targetTags": [tag],
        "allowed": [{"IPProtocol": "tcp", "ports": firewall_ports(open_ports)}],
        "description": "gpuprov managed firewall rule",
    })
}

/// SSH plus the requested ports, sorted and deduplicated, as strings.
pub fn firewall_ports(open_ports: &[u16]) -> Vec<String> {
    let mut ports: Vec<u16> = vec![22];
    ports.extend(open_ports);
    ports.sort_unstable();
    ports.dedup();
    ports.iter().map(u16::to_string).collect()
}

/// Ports an existing firewall rule allows over TCP (for drift detection).
pub fn allowed_tcp_ports(firewall: &Value) -> Vec<String> {
    let mut ports: Vec<String> = firewall
        .get("allowed")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| a.get("IPProtocol").and_then(Value::as_str) == Some("tcp"))
        .flat_map(|a| {
            a.get("ports")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    ports.sort_by_key(|p| p.parse::<u32>().unwrap_or(u32::MAX));
    ports
}

/// Map GCP's instance status. Note GCP's `TERMINATED` means *stopped* (the
/// disk remains); a deleted instance simply disappears from lists.
pub fn status(gcp_status: &str) -> InstanceStatus {
    match gcp_status {
        "PROVISIONING" | "STAGING" | "REPAIRING" => InstanceStatus::Pending,
        "RUNNING" => InstanceStatus::Running,
        "STOPPING" | "SUSPENDING" => InstanceStatus::Stopping,
        "STOPPED" | "SUSPENDED" | "TERMINATED" => InstanceStatus::Stopped,
        other => InstanceStatus::Failed(format!("unknown GCP status {other}")),
    }
}

pub fn to_observed(instance: &Instance) -> Observed {
    let nic = instance.network_interfaces.first();
    let nat_ip = nic
        .and_then(|n| n.access_configs.first())
        .and_then(|a| a.nat_ip.clone());
    Observed {
        instance_id: instance.name.clone(),
        status: status(&instance.status),
        zone: (!instance.zone.is_empty()).then(|| basename(&instance.zone).to_string()),
        private_ip: nic.and_then(|n| n.network_ip.clone()),
        // User comes from the `ssh-keys` metadata (the spec's ssh_user).
        endpoint: nat_ip.map(|host| SshEndpoint {
            host,
            port: 22,
            user: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::catalog::Accelerator;
    use super::*;

    fn spec(spot: bool) -> InstanceSpec {
        InstanceSpec {
            vm_name: "dev".into(),
            image_id: None,
            disk_gb: 150,
            spot,
            open_ports: vec![8080],
            user_data: "#cloud-config\n".into(),
            ssh_public_key: "ssh-ed25519 AAAA key\n".into(),
            ssh_user: "ubuntu".into(),
        }
    }

    fn t4() -> TypeChoice {
        TypeChoice {
            machine_type: "n1-standard-4".into(),
            gpu: true,
            accelerator: Some(Accelerator {
                kind: "nvidia-tesla-t4".into(),
                count: 1,
            }),
        }
    }

    #[test]
    fn insert_body_carries_identity_key_and_accelerator() {
        let body = insert_body(
            "us-west1-b",
            &t4(),
            "projects/x/global/images/img",
            "gpuprov-dev",
            &spec(false),
        );
        assert_eq!(body["name"], "gpuprov-dev");
        assert_eq!(
            body["machineType"],
            "zones/us-west1-b/machineTypes/n1-standard-4"
        );
        assert_eq!(body["labels"][LABEL_VM_NAME], "dev");
        assert_eq!(body["labels"][LABEL_MANAGED], "true");
        assert_eq!(body["tags"]["items"][0], "gpuprov-dev");
        assert_eq!(body["disks"][0]["initializeParams"]["diskSizeGb"], "150");
        assert_eq!(
            body["networkInterfaces"][0]["accessConfigs"][0]["type"],
            "ONE_TO_ONE_NAT"
        );
        let items = body["metadata"]["items"].as_array().unwrap();
        assert_eq!(items[0]["key"], "ssh-keys");
        assert_eq!(items[0]["value"], "ubuntu:ssh-ed25519 AAAA key");
        assert_eq!(items[1]["key"], "user-data");
        assert_eq!(
            body["guestAccelerators"][0]["acceleratorType"],
            "zones/us-west1-b/acceleratorTypes/nvidia-tesla-t4"
        );
        assert_eq!(body["guestAccelerators"][0]["acceleratorCount"], 1);
        assert_eq!(body["scheduling"]["onHostMaintenance"], "TERMINATE");
    }

    #[test]
    fn spot_and_cpu_scheduling() {
        let spot = insert_body("z", &t4(), "img", "tag", &spec(true));
        assert_eq!(spot["scheduling"]["provisioningModel"], "SPOT");
        assert_eq!(spot["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(spot["scheduling"]["automaticRestart"], false);
        let cpu = TypeChoice {
            machine_type: "e2-medium".into(),
            gpu: false,
            accelerator: None,
        };
        let body = insert_body("z", &cpu, "img", "tag", &spec(false));
        assert_eq!(body["scheduling"]["onHostMaintenance"], "MIGRATE");
        assert!(body.get("guestAccelerators").is_none());
    }

    #[test]
    fn firewall_allows_ssh_plus_ports() {
        let body = firewall_body("gpuprov-dev", &[8080, 22, 443]);
        assert_eq!(body["targetTags"][0], "gpuprov-dev");
        assert_eq!(body["direction"], "INGRESS");
        let ports = allowed_tcp_ports(&body);
        assert_eq!(ports, vec!["22", "443", "8080"]);
        assert_eq!(firewall_ports(&[]), vec!["22"]);
    }

    #[test]
    fn statuses_normalize() {
        assert_eq!(status("STAGING"), InstanceStatus::Pending);
        assert_eq!(status("RUNNING"), InstanceStatus::Running);
        assert_eq!(status("STOPPING"), InstanceStatus::Stopping);
        assert_eq!(status("TERMINATED"), InstanceStatus::Stopped);
        assert!(matches!(status("WAT"), InstanceStatus::Failed(_)));
    }

    #[test]
    fn observed_reads_nat_ip_and_zone() {
        let instance: Instance = serde_json::from_value(json!({
            "name": "gpuprov-dev",
            "status": "RUNNING",
            "zone": "https://compute.googleapis.com/compute/v1/projects/p/zones/us-west1-b",
            "networkInterfaces": [{"networkIP": "10.0.0.2",
                                   "accessConfigs": [{"natIP": "34.1.2.3"}]}],
        }))
        .unwrap();
        let o = to_observed(&instance);
        assert!(o.is_ready());
        assert_eq!(o.zone.as_deref(), Some("us-west1-b"));
        assert_eq!(o.private_ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(o.endpoint.unwrap().host, "34.1.2.3");
        // No NAT IP yet → not ready.
        let booting: Instance =
            serde_json::from_value(json!({"name": "x", "status": "RUNNING"})).unwrap();
        assert!(!to_observed(&booting).is_ready());
    }

    #[test]
    fn label_filter_quotes_the_name() {
        assert_eq!(label_filter("dev"), "labels.gpuprov-vm-name = \"dev\"");
    }
}
