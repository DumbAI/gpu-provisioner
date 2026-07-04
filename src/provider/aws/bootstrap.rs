//! Region-level bootstrap: AMI, default-VPC public subnets, per-VM security
//! group. A port of SkyPilot's `sky/provision/aws/config.py`
//! `bootstrap_instances`, minus IAM (no on-VM agent → no instance profile).

use std::collections::HashMap;

use aws_sdk_ec2::types::{
    Filter, IpPermission, IpRange, ResourceType, RouteTable, SubnetState, Tag, TagSpecification,
};

use crate::provider::{Bootstrapped, ProvisionError, ProvisionRequest, SubnetInfo};

use super::instance::TAG_MANAGED;
use super::{ami, error};

pub async fn bootstrap(
    ec2: &aws_sdk_ec2::Client,
    ssm: &aws_sdk_ssm::Client,
    req: &ProvisionRequest,
) -> Result<Bootstrapped, ProvisionError> {
    let region = &req.region;

    let resolved_ami = match &req.ami_id {
        Some(ami) => ami.clone(),
        None => ami::resolve_ami(ssm, region, req.needs_gpu_ami).await?,
    };
    tracing::info!("[{region}] using AMI {resolved_ami}");

    let vpc_id = default_vpc_id(ec2, region).await?;
    let subnets = public_subnets(ec2, region, &vpc_id, &req.zones).await?;
    tracing::info!(
        "[{region}] default VPC {vpc_id}, {} usable public subnet(s): {}",
        subnets.len(),
        subnets
            .iter()
            .map(|s| s.zone.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let security_group_id = ensure_security_group(ec2, region, &vpc_id, req).await?;
    tracing::info!("[{region}] security group ready: {security_group_id}");

    Ok(Bootstrapped {
        subnets,
        security_group_id,
        resolved_ami,
    })
}

async fn default_vpc_id(ec2: &aws_sdk_ec2::Client, region: &str) -> Result<String, ProvisionError> {
    let resp = ec2
        .describe_vpcs()
        .filters(Filter::builder().name("isDefault").values("true").build())
        .send()
        .await
        .map_err(|e| error::to_provision_error(e, region, None, true))?;
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
    zone_pins: &[String],
) -> Result<Vec<SubnetInfo>, ProvisionError> {
    let vpc_filter = Filter::builder().name("vpc-id").values(vpc_id).build();
    let subnets_resp = ec2
        .describe_subnets()
        .filters(vpc_filter.clone())
        .send()
        .await
        .map_err(|e| error::to_provision_error(e, region, None, true))?;
    let tables_resp = ec2
        .describe_route_tables()
        .filters(vpc_filter)
        .send()
        .await
        .map_err(|e| error::to_provision_error(e, region, None, true))?;

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
        if !zone_pins.is_empty() && !zone_pins.iter().any(|z| z == zone) {
            continue;
        }
        usable.push(SubnetInfo {
            subnet_id: subnet_id.to_string(),
            zone: zone.to_string(),
        });
    }
    usable.sort_by(|a, b| a.zone.cmp(&b.zone));

    if usable.is_empty() {
        let zone_note = if zone_pins.is_empty() {
            String::new()
        } else {
            format!(" in zone(s) {}", zone_pins.join(", "))
        };
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
            Err(e) if error::error_code(&e).as_deref() == Some("InvalidGroup.Duplicate") => {}
            Err(e) => return Err(error::to_provision_error(e, region, None, true)),
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
            Err(e) if error::error_code(&e).as_deref() == Some("InvalidPermission.Duplicate") => {}
            Err(e) => return Err(error::to_provision_error(e, region, None, true)),
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
        .filters(Filter::builder().name("group-name").values(sg_name).build())
        .filters(Filter::builder().name("vpc-id").values(vpc_id).build())
        .send()
        .await
        .map_err(|e| error::to_provision_error(e, region, None, true))?;
    Ok(resp.security_groups().first().and_then(|sg| {
        sg.group_id()
            .map(|id| (id.to_string(), sg.ip_permissions().len()))
    }))
}
