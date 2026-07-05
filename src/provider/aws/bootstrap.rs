//! Region-level bootstrap: instance-type viability, AMI, default-VPC public
//! subnets, per-VM security group. A port of SkyPilot's
//! `sky/provision/aws/config.py` `bootstrap_instances`, minus IAM (no on-VM
//! agent → no instance profile), plus the zone-availability filtering SkyPilot
//! gets from its catalog (`valid_on_region_zones`).
//!
//! Ordering is cheapest-check-first so a non-viable region fails over cleanly
//! before anything is created: instance-type metadata → zone offerings → AMI
//! (root device + min disk) → subnets ∩ offering zones → security group.

use std::collections::{HashMap, HashSet};

use aws_sdk_ec2::types::{
    IpPermission, IpRange, ResourceType, RouteTable, SubnetState, Tag, TagSpecification,
};

use crate::provider::{ProvisionError, ProvisionRequest};

use super::instance::TAG_MANAGED;
use super::{ami, error, filter, instance_type};

/// AWS region prerequisites resolved by `bootstrap`, consumed by
/// `run_instance`. AWS-shaped and kept below the trait — the engine treats
/// provisioning as one opaque step.
#[derive(Debug, Clone)]
pub struct Bootstrapped {
    /// Usable public subnets offering the instance type, one per zone.
    pub subnets: Vec<SubnetInfo>,
    pub security_group_id: String,
    pub resolved_ami: String,
    /// The AMI's root device name (drives `BlockDeviceMappings`).
    pub root_device_name: String,
}

#[derive(Debug, Clone)]
pub struct SubnetInfo {
    pub subnet_id: String,
    pub zone: String,
}

pub async fn bootstrap(
    ec2: &aws_sdk_ec2::Client,
    ssm: &aws_sdk_ssm::Client,
    req: &ProvisionRequest,
) -> Result<Bootstrapped, ProvisionError> {
    let region = &req.region;

    // 1+2. Instance-type metadata (decides AMI flavor) and the zones offering
    //      it — independent, so query concurrently.
    let (meta, offering_zones) = tokio::try_join!(
        instance_type::instance_type_meta(ec2, region, &req.instance_type),
        instance_type::zones_offering(ec2, region, &req.instance_type),
    )?;
    tracing::info!(
        "[{region}] {}: {}, {}",
        req.instance_type,
        meta.gpu_desc.as_deref().unwrap_or("no GPU"),
        meta.arch.as_str()
    );
    if offering_zones.is_empty() {
        return Err(ProvisionError::Config {
            region: region.to_string(),
            msg: format!(
                "instance type '{}' is not offered in this region",
                req.instance_type
            ),
        });
    }

    // 3. AMI: default via SSM (or user override), then per-image facts.
    let ami_id = match &req.image_id {
        Some(image) => image.clone(),
        None => ami::resolve_ami(ssm, region, meta.arch, meta.is_gpu).await?,
    };
    let resolved = ami::describe_ami(ec2, region, &ami_id).await?;
    check_min_disk(region, &resolved, req.disk_gb)?;
    tracing::info!(
        "[{region}] using AMI {} (root device {}, min disk {} GB)",
        resolved.ami_id,
        resolved.root_device_name,
        resolved.min_disk_gb
    );

    // 4. Network: default-VPC public subnets, restricted to offering zones.
    let vpc_id = default_vpc_id(ec2, region).await?;
    let subnets = public_subnets(ec2, region, &vpc_id, req.zone.as_deref()).await?;
    let subnets = filter_to_offering_zones(subnets, &offering_zones, region, &req.instance_type)?;
    tracing::info!(
        "[{region}] default VPC {vpc_id}, {} public subnet(s) offering {}: {}",
        subnets.len(),
        req.instance_type,
        subnets
            .iter()
            .map(|s| s.zone.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 5. Security group — only created once the region is known to be viable.
    let security_group_id = ensure_security_group(ec2, region, &vpc_id, req).await?;
    tracing::info!("[{region}] security group ready: {security_group_id}");

    Ok(Bootstrapped {
        subnets,
        security_group_id,
        resolved_ami: resolved.ami_id,
        root_device_name: resolved.root_device_name,
    })
}

/// The AMI's root snapshot is the smallest legal root disk (SkyPilot errors
/// the same way rather than silently billing a bigger disk).
fn check_min_disk(
    region: &str,
    resolved: &ami::ResolvedAmi,
    requested_gb: i32,
) -> Result<(), ProvisionError> {
    if requested_gb < resolved.min_disk_gb {
        return Err(ProvisionError::Config {
            region: region.to_string(),
            msg: format!(
                "AMI {} requires a root disk of at least {} GB but {} GB was requested; \
                 pass --disk {} (or larger)",
                resolved.ami_id, resolved.min_disk_gb, requested_gb, resolved.min_disk_gb
            ),
        });
    }
    Ok(())
}

/// Keep only subnets in zones where the instance type is actually offered.
fn filter_to_offering_zones(
    subnets: Vec<SubnetInfo>,
    offering_zones: &HashSet<String>,
    region: &str,
    instance_type: &str,
) -> Result<Vec<SubnetInfo>, ProvisionError> {
    let (kept, dropped): (Vec<_>, Vec<_>) = subnets
        .into_iter()
        .partition(|s| offering_zones.contains(&s.zone));
    if kept.is_empty() {
        let mut offered: Vec<&str> = offering_zones.iter().map(String::as_str).collect();
        offered.sort_unstable();
        return Err(ProvisionError::Config {
            region: region.to_string(),
            msg: format!(
                "'{instance_type}' is offered in zone(s) [{}] but there is no usable \
                 public subnet there (subnets exist in: [{}])",
                offered.join(", "),
                dropped
                    .iter()
                    .map(|s| s.zone.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(kept)
}

async fn default_vpc_id(ec2: &aws_sdk_ec2::Client, region: &str) -> Result<String, ProvisionError> {
    let resp = ec2
        .describe_vpcs()
        .filters(filter("isDefault", "true"))
        .send()
        .await
        .map_err(error::in_bootstrap(region))?;
    resp.vpcs()
        .first()
        .and_then(|v| v.vpc_id())
        .map(str::to_string)
        .ok_or_else(|| ProvisionError::Config {
            region: region.to_string(),
            msg: "no default VPC in this region; create one (VPC console → Actions → \
                  Create default VPC) or use another region"
                .to_string(),
        })
}

/// Usable subnets: in the default VPC, `available`, **public** (their route
/// table — explicit or the VPC main table — has an `igw-*` route), matching
/// the zone pin if any. Sorted by zone for deterministic retry order.
async fn public_subnets(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vpc_id: &str,
    zone_pin: Option<&str>,
) -> Result<Vec<SubnetInfo>, ProvisionError> {
    // Subnets and route tables are independent lookups — fetch concurrently.
    let (subnets_resp, tables_resp) = tokio::try_join!(
        async {
            ec2.describe_subnets()
                .filters(filter("vpc-id", vpc_id))
                .send()
                .await
                .map_err(error::in_bootstrap(region))
        },
        async {
            ec2.describe_route_tables()
                .filters(filter("vpc-id", vpc_id))
                .send()
                .await
                .map_err(error::in_bootstrap(region))
        },
    )?;

    let tables = tables_resp.route_tables();
    let main_is_public = tables
        .iter()
        .find(|t| t.associations().iter().any(|a| a.main().unwrap_or(false)))
        .map(is_public_table)
        .unwrap_or(false);
    let mut explicit: HashMap<&str, bool> = HashMap::new();
    for table in tables {
        let public = is_public_table(table);
        for assoc in table.associations() {
            if let Some(subnet_id) = assoc.subnet_id() {
                explicit.insert(subnet_id, public);
            }
        }
    }

    let mut usable = Vec::new();
    for subnet in subnets_resp.subnets() {
        if subnet.state() != Some(&SubnetState::Available) {
            continue;
        }
        let (Some(subnet_id), Some(zone)) = (subnet.subnet_id(), subnet.availability_zone()) else {
            continue;
        };
        let public = *explicit.get(subnet_id).unwrap_or(&main_is_public);
        if !public {
            continue;
        }
        if zone_pin.is_some_and(|pin| pin != zone) {
            continue;
        }
        usable.push(SubnetInfo {
            subnet_id: subnet_id.to_string(),
            zone: zone.to_string(),
        });
    }
    usable.sort_by(|a, b| a.zone.cmp(&b.zone));

    if usable.is_empty() {
        let zone_note = zone_pin
            .map(|pin| format!(" in zone {pin}"))
            .unwrap_or_default();
        return Err(ProvisionError::Config {
            region: region.to_string(),
            msg: format!(
                "no available public subnet in default VPC {vpc_id}{zone_note} \
                 (public = route table has an internet-gateway route)"
            ),
        });
    }
    Ok(usable)
}

fn is_public_table(table: &RouteTable) -> bool {
    table
        .routes()
        .iter()
        .any(|r| r.gateway_id().is_some_and(|g| g.starts_with("igw-")))
}

/// Get-or-create the per-VM security group `gpuprov-<name>` with SSH (22) and
/// any extra ports open to 0.0.0.0/0. Rules are only authorized when the
/// group has none (idempotent re-provision).
async fn ensure_security_group(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vpc_id: &str,
    req: &ProvisionRequest,
) -> Result<String, ProvisionError> {
    let sg_name = security_group_name(&req.vm_name);

    let mut existing = find_security_group(ec2, region, vpc_id, &sg_name).await?;
    if existing.is_none() {
        let create = ec2
            .create_security_group()
            .group_name(&sg_name)
            // Note: EC2 SG descriptions forbid quote characters.
            .description(format!(
                "gpuprov managed security group for VM {}",
                req.vm_name
            ))
            .vpc_id(vpc_id)
            .tag_specifications(
                TagSpecification::builder()
                    .resource_type(ResourceType::SecurityGroup)
                    .tags(Tag::builder().key(TAG_MANAGED).value("true").build())
                    .build(),
            )
            .send()
            .await;
        match create {
            Ok(_) => {}
            // Lost a create race — the refetch below picks it up.
            Err(e) if error::has_code(&e, "InvalidGroup.Duplicate") => {}
            Err(e) => return Err(error::in_bootstrap(region)(e)),
        }
        existing = find_security_group(ec2, region, vpc_id, &sg_name).await?;
    }
    let (sg_id, rule_count) = existing.ok_or_else(|| ProvisionError::Config {
        region: region.to_string(),
        msg: format!("security group '{sg_name}' not found after creation"),
    })?;

    if rule_count == 0 {
        let mut ports: Vec<u16> = vec![22];
        ports.extend(&req.extra_ports);
        ports.sort_unstable();
        ports.dedup();
        let mut auth = ec2.authorize_security_group_ingress().group_id(&sg_id);
        for port in ports {
            auth = auth.ip_permissions(
                IpPermission::builder()
                    .ip_protocol("tcp")
                    .from_port(i32::from(port))
                    .to_port(i32::from(port))
                    .ip_ranges(IpRange::builder().cidr_ip("0.0.0.0/0").build())
                    .build(),
            );
        }
        match auth.send().await {
            Ok(_) => {}
            // Raced with a concurrent authorize — rules exist, which is what we want.
            Err(e) if error::has_code(&e, "InvalidPermission.Duplicate") => {}
            Err(e) => return Err(error::in_bootstrap(region)(e)),
        }
    }
    Ok(sg_id)
}

pub fn security_group_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// Returns `(group_id, ingress_rule_count)` if the group exists in the VPC.
async fn find_security_group(
    ec2: &aws_sdk_ec2::Client,
    region: &str,
    vpc_id: &str,
    sg_name: &str,
) -> Result<Option<(String, usize)>, ProvisionError> {
    let resp = ec2
        .describe_security_groups()
        .filters(filter("group-name", sg_name))
        .filters(filter("vpc-id", vpc_id))
        .send()
        .await
        .map_err(error::in_bootstrap(region))?;
    Ok(resp.security_groups().first().and_then(|sg| {
        sg.group_id()
            .map(|id| (id.to_string(), sg.ip_permissions().len()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subnet(id: &str, zone: &str) -> SubnetInfo {
        SubnetInfo {
            subnet_id: id.to_string(),
            zone: zone.to_string(),
        }
    }

    #[test]
    fn offering_filter_keeps_only_offered_zones() {
        let subnets = vec![
            subnet("s-a", "us-east-2a"),
            subnet("s-b", "us-east-2b"),
            subnet("s-c", "us-east-2c"),
        ];
        let offered: HashSet<String> = ["us-east-2a", "us-east-2c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let kept = filter_to_offering_zones(subnets, &offered, "us-east-2", "g6e.8xlarge").unwrap();
        let zones: Vec<&str> = kept.iter().map(|s| s.zone.as_str()).collect();
        assert_eq!(zones, vec!["us-east-2a", "us-east-2c"]);
    }

    #[test]
    fn offering_filter_errors_when_no_overlap() {
        let subnets = vec![subnet("s-a", "us-east-2a")];
        let offered: HashSet<String> = ["us-east-2b".to_string()].into_iter().collect();
        let err =
            filter_to_offering_zones(subnets, &offered, "us-east-2", "g6e.8xlarge").unwrap_err();
        match err {
            ProvisionError::Config { msg, .. } => {
                assert!(msg.contains("us-east-2b"), "{msg}");
                assert!(msg.contains("us-east-2a"), "{msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn min_disk_check() {
        let resolved = ami::ResolvedAmi {
            ami_id: "ami-1".into(),
            root_device_name: "/dev/sda1".into(),
            min_disk_gb: 75,
        };
        assert!(check_min_disk("us-east-2", &resolved, 100).is_ok());
        assert!(check_min_disk("us-east-2", &resolved, 75).is_ok());
        let err = check_min_disk("us-east-2", &resolved, 50).unwrap_err();
        assert!(err.to_string().contains("--disk 75"), "{err}");
    }
}
