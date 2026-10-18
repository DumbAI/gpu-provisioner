//! EC2 instance primitives: create, observe, delete, cleanup — with no
//! wait/retry loops, which the shared launch/teardown machines own.

use aws_sdk_ec2::types::{
    BlockDeviceMapping, EbsBlockDevice, HttpTokensState, Instance, InstanceMarketOptionsRequest,
    InstanceMetadataOptionsRequest, InstanceNetworkInterfaceSpecification, InstanceStateName,
    InstanceType, MarketType, ResourceType, Tag, TagSpecification, VolumeType,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::provider::{
    ErrorKind, InstanceSpec, InstanceStatus, Observed, Offer, ProviderError, SshEndpoint,
};

use super::bootstrap::{security_group_name, Bootstrapped};
use super::error;
use super::{filter, filter_values};

/// Identity tag: every instance of a VM carries `gpuprov-vm-name=<name>`
/// (the `ray-cluster-name` analog). All queries filter on it.
pub const TAG_VM_NAME: &str = "gpuprov-vm-name";
pub const TAG_MANAGED: &str = "gpuprov-managed";

/// Every state short of `terminated`.
const LIVE_STATES: &[&str] = &["pending", "running", "shutting-down", "stopping", "stopped"];

/// One creation attempt: `RunInstances` on the fail-fast (no SDK retry)
/// client, walking the bootstrapped subnets (zones) in order on capacity
/// errors, so one attempt covers every usable zone. Server-side
/// errors return `Transient` so the core retries the whole step (which first
/// observes, so a request that landed is adopted, not duplicated).
pub async fn create(
    ec2_fail_fast: &aws_sdk_ec2::Client,
    offer: &Offer,
    prepared: &Bootstrapped,
    spec: &InstanceSpec,
) -> Result<String, ProviderError> {
    let mut last_err: Option<ProviderError> = None;
    for subnet in &prepared.subnets {
        match run_instance(ec2_fail_fast, offer, prepared, spec, &subnet.subnet_id).await {
            Ok(instance_id) => {
                tracing::info!(
                    "[{}] launched instance {instance_id} in {}",
                    offer.region,
                    subnet.zone
                );
                return Ok(instance_id);
            }
            Err(e) if e.kind == ErrorKind::Capacity => {
                tracing::warn!(
                    "[{}] no capacity in {}: {}",
                    offer.region,
                    subnet.zone,
                    e.msg
                );
                last_err = Some(ProviderError::capacity(format!(
                    "{}: {}",
                    subnet.zone, e.msg
                )));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| ProviderError::unavailable("no subnets to launch into")))
}

async fn run_instance(
    ec2_fail_fast: &aws_sdk_ec2::Client,
    offer: &Offer,
    prepared: &Bootstrapped,
    spec: &InstanceSpec,
    subnet_id: &str,
) -> Result<String, ProviderError> {
    let mut call = ec2_fail_fast
        .run_instances()
        .min_count(1)
        .max_count(1)
        .image_id(&prepared.resolved_ami)
        .instance_type(InstanceType::from(offer.instance_type.as_str()))
        .network_interfaces(
            InstanceNetworkInterfaceSpecification::builder()
                .device_index(0)
                .subnet_id(subnet_id)
                .groups(&prepared.security_group_id)
                .associate_public_ip_address(true)
                .build(),
        )
        .block_device_mappings(
            BlockDeviceMapping::builder()
                .device_name(&prepared.root_device_name)
                .ebs(
                    EbsBlockDevice::builder()
                        .volume_size(spec.disk_gb)
                        .volume_type(VolumeType::Gp3)
                        .iops(3500)
                        .throughput(218)
                        .delete_on_termination(true)
                        .build(),
                )
                .build(),
        )
        .metadata_options(
            InstanceMetadataOptionsRequest::builder()
                .http_tokens(HttpTokensState::Required)
                .build(),
        )
        .tag_specifications(instance_tags(&spec.vm_name))
        .user_data(BASE64.encode(spec.user_data.as_bytes()));
    if spec.spot {
        call = call.instance_market_options(
            InstanceMarketOptionsRequest::builder()
                .market_type(MarketType::Spot)
                .build(),
        );
    }
    let resp = call.send().await.map_err(error::at_runtime)?;
    resp.instances()
        .first()
        .and_then(|i| i.instance_id())
        .map(str::to_string)
        .ok_or_else(|| ProviderError::failed("RunInstances succeeded but returned no instance"))
}

fn instance_tags(vm_name: &str) -> TagSpecification {
    TagSpecification::builder()
        .resource_type(ResourceType::Instance)
        .tags(
            Tag::builder()
                .key("Name")
                .value(format!("gpuprov-{vm_name}"))
                .build(),
        )
        .tags(Tag::builder().key(TAG_VM_NAME).value(vm_name).build())
        .tags(Tag::builder().key(TAG_MANAGED).value("true").build())
        .build()
}

/// All non-terminated instances carrying the VM identity tag.
pub async fn observe(
    ec2: &aws_sdk_ec2::Client,
    vm_name: &str,
) -> Result<Vec<Observed>, ProviderError> {
    let resp = ec2
        .describe_instances()
        .filters(filter(&format!("tag:{TAG_VM_NAME}"), vm_name))
        .filters(filter_values(
            "instance-state-name",
            LIVE_STATES.iter().copied(),
        ))
        .send()
        .await
        .map_err(error::at_runtime)?;
    let mut observed: Vec<Observed> = resp
        .reservations()
        .iter()
        .flat_map(|r| r.instances())
        .filter_map(to_observed)
        .collect();
    observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    Ok(observed)
}

fn to_observed(instance: &Instance) -> Option<Observed> {
    let status = match instance.state().and_then(|s| s.name())? {
        InstanceStateName::Pending => InstanceStatus::Pending,
        InstanceStateName::Running => InstanceStatus::Running,
        InstanceStateName::ShuttingDown => InstanceStatus::Terminating,
        InstanceStateName::Stopping => InstanceStatus::Stopping,
        InstanceStateName::Stopped => InstanceStatus::Stopped,
        _ => return None,
    };
    Some(Observed {
        instance_id: instance.instance_id()?.to_string(),
        status,
        zone: instance
            .placement()
            .and_then(|p| p.availability_zone())
            .map(str::to_string),
        private_ip: instance.private_ip_address().map(str::to_string),
        // The login user comes from our cloud-init; sshd on the default port.
        endpoint: instance.public_ip_address().map(|ip| SshEndpoint {
            host: ip.to_string(),
            port: 22,
            user: None,
        }),
    })
}

/// Request termination; already-gone ids are success.
pub async fn delete(ec2: &aws_sdk_ec2::Client, ids: &[String]) -> Result<(), ProviderError> {
    if ids.is_empty() {
        return Ok(());
    }
    match ec2
        .terminate_instances()
        .set_instance_ids(Some(ids.to_vec()))
        .send()
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if error::has_code(&e, "InvalidInstanceID.NotFound") => Ok(()),
        Err(e) => Err(error::at_runtime(e)),
    }
}

/// Delete the managed per-VM security group. `DependencyViolation` (ENIs lag
/// behind instance termination) is `Transient` so the core retries; other
/// failures are logged and swallowed — a leftover SG is free and is retried
/// by the next teardown of the same name.
pub async fn delete_security_group(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vm_name: &str,
) -> Result<(), ProviderError> {
    let sg_name = security_group_name(vm_name);
    let groups = match ec2
        .describe_security_groups()
        .filters(filter("group-name", &sg_name))
        .send()
        .await
    {
        Ok(resp) => resp.security_groups().to_vec(),
        Err(e) => {
            tracing::warn!(
                "[{region}] could not look up security group '{sg_name}': {}",
                error::describe(&e)
            );
            return Ok(());
        }
    };
    for sg in groups {
        let managed = sg
            .tags()
            .iter()
            .any(|t| t.key() == Some(TAG_MANAGED) && t.value() == Some("true"));
        let Some(group_id) = sg.group_id() else {
            continue;
        };
        if !managed {
            tracing::info!("[{region}] leaving unmanaged security group {group_id} in place");
            continue;
        }
        match ec2.delete_security_group().group_id(group_id).send().await {
            Ok(_) => tracing::info!("[{region}] deleted security group {group_id} ({sg_name})"),
            Err(e) if error::has_code(&e, "InvalidGroup.NotFound") => {}
            Err(e) if error::has_code(&e, "DependencyViolation") => {
                return Err(ProviderError::transient(format!(
                    "security group {group_id} still in use: {}",
                    error::describe(&e)
                )));
            }
            Err(e) => tracing::warn!(
                "[{region}] failed to delete security group {group_id} ({sg_name}) — delete it \
                 manually if unwanted: {}",
                error::describe(&e)
            ),
        }
    }
    Ok(())
}
