//! EC2 instance lifecycle: find-or-create, wait, describe, terminate.
//! A port of SkyPilot's `sky/provision/aws/instance.py` for a single VM.

use std::time::Duration;

use aws_sdk_ec2::types::{
    BlockDeviceMapping, EbsBlockDevice, Filter, HttpTokensState, Instance,
    InstanceMarketOptionsRequest, InstanceMetadataOptionsRequest,
    InstanceNetworkInterfaceSpecification, InstanceStateName, InstanceType, MarketType,
    ResourceType, Tag, TagSpecification, VolumeType,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::provider::{Bootstrapped, ProvisionError, ProvisionRecord, ProvisionRequest, VmInfo};

use super::bootstrap::security_group_name;
use super::error::{self, AwsErrorClass};

/// Identity tag: every instance of a VM carries `gpuprov-vm-name=<name>`
/// (the `ray-cluster-name` analog). All queries filter on it.
pub const TAG_VM_NAME: &str = "gpuprov-vm-name";
pub const TAG_MANAGED: &str = "gpuprov-managed";

/// SkyPilot's `BOTO_CREATE_MAX_RETRIES`: budget for the subnet/zone retry loop.
const CREATE_MAX_RETRIES: usize = 5;
/// Server-side-error retry budget (SkyPilot's `BOTO_MAX_RETRIES`).
const SERVER_RETRY_MAX: usize = 12;
const SERVER_RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const SERVER_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Waiter: poll every 5s, up to 120 attempts (~10 min), like the boto3
/// `instance_running` waiter SkyPilot uses.
const WAIT_DELAY: Duration = Duration::from_secs(5);
const WAIT_MAX_ATTEMPTS: usize = 120;
/// Security-group deletion retries on `DependencyViolation` (ENI detach lag).
const SG_DELETE_MAX_ATTEMPTS: usize = 6;
const SG_DELETE_BACKOFF: Duration = Duration::from_secs(5);

const ACTIVE_STATES: &[&str] = &["pending", "running"];
const NON_TERMINATED_STATES: &[&str] = &["pending", "running", "stopping", "stopped"];
const ALL_LIVE_STATES: &[&str] = &["pending", "running", "shutting-down", "stopping", "stopped"];

/// Find-or-create the VM's instance. Reuses a live instance with the identity
/// tag (idempotent re-provision); otherwise creates one, retrying across the
/// bootstrapped subnets/zones on capacity errors.
pub async fn run_instance(
    ec2: &aws_sdk_ec2::Client,
    ec2_fail_fast: &aws_sdk_ec2::Client,
    req: &ProvisionRequest,
    bootstrapped: &Bootstrapped,
) -> Result<ProvisionRecord, ProvisionError> {
    let region = &req.region;
    let existing = find_instances(ec2, region, &req.vm_name, ACTIVE_STATES, false).await?;

    // Leak guard (SkyPilot: "more instances than requested").
    if existing.len() > 1 {
        return Err(ProvisionError::Other {
            region: region.clone(),
            msg: format!(
                "found {} live instances tagged {TAG_VM_NAME}={} — refusing to proceed; \
                 terminate the extras or run `gpuprov down {}`",
                existing.len(),
                req.vm_name,
                req.vm_name
            ),
        });
    }
    if let Some(instance) = existing.first() {
        let record = record_from_instance(region, instance, true)?;
        tracing::info!(
            "[{region}] reusing existing instance {} in {}",
            record.instance_id,
            record.zone
        );
        return Ok(record);
    }

    // Subnet/zone retry loop (SkyPilot's `_create_instances` scheme).
    let subnets = &bootstrapped.subnets;
    let num_subnets = subnets.len().max(1);
    let max_tries = std::cmp::max(
        num_subnets * (CREATE_MAX_RETRIES / num_subnets),
        num_subnets,
    );
    let per_subnet = std::cmp::max(max_tries / num_subnets, 1);
    let mut last_err: Option<ProvisionError> = None;
    for attempt in 0..max_tries {
        let subnet = &subnets[(attempt / per_subnet).min(num_subnets - 1)];
        match create_instance(ec2_fail_fast, req, bootstrapped, &subnet.subnet_id).await {
            Ok(instance) => {
                let mut record = record_from_instance(region, &instance, false)?;
                if record.zone.is_empty() {
                    record.zone = subnet.zone.clone();
                }
                tracing::info!(
                    "[{region}] launched instance {} in {}",
                    record.instance_id,
                    record.zone
                );
                return Ok(record);
            }
            Err(e @ ProvisionError::Credentials { .. }) => return Err(e),
            Err(e) => {
                tracing::warn!(
                    "[{region}] launch attempt {}/{max_tries} in {} failed: {e}",
                    attempt + 1,
                    subnet.zone
                );
                last_err = Some(match e {
                    // Attach the zone we were trying.
                    ProvisionError::Capacity { region, msg, .. } => ProvisionError::Capacity {
                        region,
                        zone: Some(subnet.zone.clone()),
                        msg,
                    },
                    other => other,
                });
            }
        }
    }
    Err(last_err.unwrap_or_else(|| ProvisionError::Other {
        region: region.clone(),
        msg: "instance creation failed: no subnets to try".to_string(),
    }))
}

/// One `RunInstances` call against one subnet, retrying only server-side
/// errors (request limits / internal errors) with backoff. Capacity errors
/// surface immediately — the client has SDK retries disabled — so the subnet
/// loop can fail over quickly (SkyPilot's fail-fast `max_attempts=0` client).
async fn create_instance(
    ec2_fail_fast: &aws_sdk_ec2::Client,
    req: &ProvisionRequest,
    bootstrapped: &Bootstrapped,
    subnet_id: &str,
) -> Result<Instance, ProvisionError> {
    let region = &req.region;
    let mut backoff = SERVER_RETRY_INITIAL_BACKOFF;
    for attempt in 0..SERVER_RETRY_MAX {
        let mut call = ec2_fail_fast
            .run_instances()
            .min_count(1)
            .max_count(1)
            .image_id(&bootstrapped.resolved_ami)
            .instance_type(InstanceType::from(req.instance_type.as_str()))
            .network_interfaces(
                InstanceNetworkInterfaceSpecification::builder()
                    .device_index(0)
                    .subnet_id(subnet_id)
                    .groups(&bootstrapped.security_group_id)
                    .associate_public_ip_address(true)
                    .build(),
            )
            .block_device_mappings(
                BlockDeviceMapping::builder()
                    .device_name("/dev/sda1")
                    .ebs(
                        // SkyPilot's MEDIUM disk tier: gp3 @ 3500 IOPS / 218 MBps.
                        EbsBlockDevice::builder()
                            .volume_size(req.disk_gb)
                            .volume_type(VolumeType::Gp3)
                            .iops(3500)
                            .throughput(218)
                            .delete_on_termination(true)
                            .build(),
                    )
                    .build(),
            )
            .metadata_options(
                // IMDSv2 required, matching SkyPilot's template.
                InstanceMetadataOptionsRequest::builder()
                    .http_tokens(HttpTokensState::Required)
                    .build(),
            )
            .tag_specifications(instance_tags(&req.vm_name))
            .user_data(BASE64.encode(req.user_data.as_bytes()));
        if req.spot {
            call = call.instance_market_options(
                InstanceMarketOptionsRequest::builder()
                    .market_type(MarketType::Spot)
                    .build(),
            );
        }
        match call.send().await {
            Ok(resp) => {
                return resp
                    .instances()
                    .first()
                    .cloned()
                    .ok_or_else(|| ProvisionError::Other {
                        region: region.clone(),
                        msg: "RunInstances succeeded but returned no instances".to_string(),
                    })
            }
            Err(e) => {
                let class = error::classify(&e);
                if class == AwsErrorClass::ServerRetryable && attempt + 1 < SERVER_RETRY_MAX {
                    tracing::warn!(
                        "[{region}] transient AWS error ({}), retrying in {}s",
                        error::describe(&e),
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, SERVER_RETRY_MAX_BACKOFF);
                    continue;
                }
                return Err(error::to_provision_error(e, region, None, false));
            }
        }
    }
    unreachable!("server-retry loop always returns")
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

/// Poll until every live instance of the VM is `running` (5s × 120).
pub async fn wait_running(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vm_name: &str,
) -> Result<(), ProvisionError> {
    for _ in 0..WAIT_MAX_ATTEMPTS {
        let instances = find_instances(ec2, region, vm_name, ACTIVE_STATES, false).await?;
        if instances.is_empty() {
            // Instance vanished (e.g. spot reclaimed immediately) or state
            // lagging; the failover layer retries this a few times.
            return Err(ProvisionError::Other {
                region: region.to_string(),
                msg: format!("no live instances found for '{vm_name}' while waiting for running"),
            });
        }
        if instances.iter().all(is_running) {
            return Ok(());
        }
        tokio::time::sleep(WAIT_DELAY).await;
    }
    Err(ProvisionError::Other {
        region: region.to_string(),
        msg: format!(
            "instance not running after {}s",
            WAIT_DELAY.as_secs() * WAIT_MAX_ATTEMPTS as u64
        ),
    })
}

/// Inventory of the VM's running instance (the `get_cluster_info` analog).
pub async fn describe(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vm_name: &str,
) -> Result<Option<VmInfo>, ProvisionError> {
    let instances = find_instances(ec2, region, vm_name, &["running"], false).await?;
    Ok(instances.first().map(|i| VmInfo {
        instance_id: i.instance_id().unwrap_or_default().to_string(),
        public_ip: i.public_ip_address().map(str::to_string),
        private_ip: i.private_ip_address().map(str::to_string),
        state: "running".to_string(),
    }))
}

/// Terminate all of the VM's instances, wait for them to disappear, then
/// delete the managed per-VM security group. Safe when nothing exists.
pub async fn terminate(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vm_name: &str,
) -> Result<(), ProvisionError> {
    let instances = find_instances(ec2, region, vm_name, NON_TERMINATED_STATES, false).await?;
    let ids: Vec<String> = instances
        .iter()
        .filter_map(|i| i.instance_id().map(str::to_string))
        .collect();
    if !ids.is_empty() {
        tracing::info!("[{region}] terminating instance(s): {}", ids.join(", "));
        ec2.terminate_instances()
            .set_instance_ids(Some(ids))
            .send()
            .await
            .map_err(|e| error::to_provision_error(e, region, None, false))?;
        for _ in 0..WAIT_MAX_ATTEMPTS {
            let left = find_instances(ec2, region, vm_name, ALL_LIVE_STATES, true).await?;
            if left.is_empty() {
                break;
            }
            tokio::time::sleep(WAIT_DELAY).await;
        }
    }
    delete_security_group(ec2, region, vm_name).await;
    Ok(())
}

/// Best-effort deletion of the per-VM SG. A leftover (free) security group is
/// logged loudly rather than failing teardown — it is retried by the next
/// `terminate` for this name.
async fn delete_security_group(ec2: &aws_sdk_ec2::Client, region: &str, vm_name: &str) {
    let sg_name = security_group_name(vm_name);
    let found = ec2
        .describe_security_groups()
        .filters(
            Filter::builder()
                .name("group-name")
                .values(&sg_name)
                .build(),
        )
        .send()
        .await;
    let groups = match found {
        Ok(resp) => resp.security_groups().to_vec(),
        Err(e) => {
            tracing::warn!(
                "[{region}] could not look up security group '{sg_name}': {}",
                error::describe(&e)
            );
            return;
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
        for attempt in 0..SG_DELETE_MAX_ATTEMPTS {
            match ec2.delete_security_group().group_id(group_id).send().await {
                Ok(_) => {
                    tracing::info!("[{region}] deleted security group {group_id} ({sg_name})");
                    break;
                }
                Err(e) => match error::error_code(&e).as_deref() {
                    // ENIs can lag behind instance termination.
                    Some("DependencyViolation") if attempt + 1 < SG_DELETE_MAX_ATTEMPTS => {
                        tokio::time::sleep(SG_DELETE_BACKOFF).await;
                    }
                    Some("InvalidGroup.NotFound") => break,
                    _ => {
                        tracing::warn!(
                            "[{region}] failed to delete security group {group_id} \
                             ({sg_name}) — delete it manually if unwanted: {}",
                            error::describe(&e)
                        );
                        break;
                    }
                },
            }
        }
    }
}

/// All instances carrying the VM identity tag in the given states.
async fn find_instances(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vm_name: &str,
    states: &[&str],
    in_teardown: bool,
) -> Result<Vec<Instance>, ProvisionError> {
    let mut state_filter = Filter::builder().name("instance-state-name");
    for state in states {
        state_filter = state_filter.values(*state);
    }
    let resp = ec2
        .describe_instances()
        .filters(
            Filter::builder()
                .name(format!("tag:{TAG_VM_NAME}"))
                .values(vm_name)
                .build(),
        )
        .filters(state_filter.build())
        .send()
        .await
        .map_err(|e| {
            let err = error::to_provision_error(e, region, None, false);
            if in_teardown {
                // Keep teardown errors distinguishable for the failover layer.
                match err {
                    ProvisionError::Credentials { msg } => ProvisionError::Credentials { msg },
                    other => ProvisionError::Other {
                        region: region.to_string(),
                        msg: format!("teardown status check failed: {other}"),
                    },
                }
            } else {
                err
            }
        })?;
    let mut instances: Vec<Instance> = resp
        .reservations()
        .iter()
        .flat_map(|r| r.instances().iter().cloned())
        .collect();
    instances.sort_by(|a, b| a.instance_id().cmp(&b.instance_id()));
    Ok(instances)
}

fn is_running(instance: &Instance) -> bool {
    instance
        .state()
        .and_then(|s| s.name())
        .is_some_and(|n| *n == InstanceStateName::Running)
}

fn record_from_instance(
    region: &str,
    instance: &Instance,
    reused: bool,
) -> Result<ProvisionRecord, ProvisionError> {
    let instance_id = instance
        .instance_id()
        .ok_or_else(|| ProvisionError::Other {
            region: region.to_string(),
            msg: "instance without an id in DescribeInstances response".to_string(),
        })?
        .to_string();
    let zone = instance
        .placement()
        .and_then(|p| p.availability_zone())
        .unwrap_or_default()
        .to_string();
    Ok(ProvisionRecord {
        region: region.to_string(),
        zone,
        instance_id,
        reused,
    })
}
