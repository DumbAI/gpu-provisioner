//! AWS-native health diagnosis (`CloudProvider::diagnose`), read-only.
//!
//! On top of the default "is it listed" view this adds what EC2 itself knows:
//!
//! - **Why an instance stopped or terminated**, from its `StateReason`
//!   ([API_StateReason](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_StateReason.html))
//!   and `StateTransitionReason`. Terminated instances stay visible in
//!   `DescribeInstances` for about an hour, so a spot reclaim can be told
//!   apart from a user-initiated termination.
//! - **Status checks** (system / instance reachability) and **scheduled
//!   events** (retirement, reboot, maintenance) from
//!   [DescribeInstanceStatus](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeInstanceStatus.html);
//!   see the user guide on
//!   [status checks](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/monitoring-system-instance-status-check.html)
//!   and [scheduled events](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/monitoring-instances-status-check_sched.html).
//! - **Spot lifecycle** (`InstanceLifecycle`): AWS may reclaim it with a
//!   two-minute notice.

use aws_sdk_ec2::types::{
    Instance, InstanceLifecycleType, InstanceStateName, InstanceStatus as Ec2InstanceStatus,
    InstanceStatusSummary, StateReason, SummaryStatus,
};

use crate::provider::{Diagnosis, Finding, InstanceStatus, ProviderError};

use super::error;
use super::filter;
use super::instance::{to_observed, TAG_VM_NAME};

/// Diagnose the VM's instance: recorded id first, then any live replacement.
pub async fn diagnose(
    ec2: &aws_sdk_ec2::Client,
    vm_name: &str,
    recorded: Option<&str>,
) -> Result<Diagnosis, ProviderError> {
    // No state filter: terminated instances carry the reason we want.
    let resp = ec2
        .describe_instances()
        .filters(filter(&format!("tag:{TAG_VM_NAME}"), vm_name))
        .send()
        .await
        .map_err(error::at_runtime)?;
    let mut instances: Vec<Instance> = resp
        .reservations()
        .iter()
        .flat_map(|r| r.instances().iter().cloned())
        .collect();
    instances.sort_by(|a, b| a.instance_id().cmp(&b.instance_id()));

    let Some(chosen) = choose(&instances, recorded) else {
        return Ok(Diagnosis {
            instance: None,
            findings: Vec::new(),
        });
    };

    let state = chosen.state().and_then(|s| s.name()).cloned();
    if matches!(
        state,
        Some(InstanceStateName::Terminated) | Some(InstanceStateName::ShuttingDown)
    ) {
        return Ok(Diagnosis {
            instance: None,
            findings: vec![Finding::critical(termination_reason(chosen))],
        });
    }

    let mut findings = Vec::new();
    if chosen.instance_lifecycle() == Some(&InstanceLifecycleType::Spot) {
        findings.push(Finding::info(
            "spot instance (AWS may reclaim it with a 2-minute notice)",
        ));
    }
    let observed = to_observed(chosen);
    match observed.as_ref().map(|o| &o.status) {
        Some(InstanceStatus::Stopped) | Some(InstanceStatus::Stopping) => {
            findings.push(Finding::critical(stop_reason(chosen)));
        }
        Some(InstanceStatus::Running) | Some(InstanceStatus::Pending) => {
            if let Some(id) = chosen.instance_id() {
                let status = ec2
                    .describe_instance_status()
                    .instance_ids(id)
                    .include_all_instances(true)
                    .send()
                    .await
                    .map_err(error::at_runtime)?;
                for s in status.instance_statuses() {
                    findings.extend(status_findings(s));
                }
            }
        }
        _ => {}
    }
    Ok(Diagnosis {
        instance: observed,
        findings,
    })
}

/// The recorded instance when listed, else a live one, else the newest of
/// whatever is left (so a reclaim of the recorded one is attributed).
fn choose<'a>(instances: &'a [Instance], recorded: Option<&str>) -> Option<&'a Instance> {
    let live = |i: &&Instance| {
        !matches!(
            i.state().and_then(|s| s.name()),
            Some(InstanceStateName::Terminated) | Some(InstanceStateName::ShuttingDown)
        )
    };
    recorded
        .and_then(|id| instances.iter().find(|i| i.instance_id() == Some(id)))
        .filter(|i| live(i))
        .or_else(|| instances.iter().find(live))
        .or_else(|| {
            recorded
                .and_then(|id| instances.iter().find(|i| i.instance_id() == Some(id)))
                .or_else(|| instances.last())
        })
}

fn reason_code(instance: &Instance) -> Option<&str> {
    instance.state_reason().and_then(StateReason::code)
}

/// Explain a termination from `StateReason` (codes per API_StateReason).
pub fn termination_reason(instance: &Instance) -> String {
    let id = instance.instance_id().unwrap_or("the instance");
    let why = match reason_code(instance) {
        Some("Server.SpotInstanceTermination") => {
            "terminated by AWS: spot capacity reclaimed".to_string()
        }
        Some("Server.InternalError") => "terminated by AWS: internal error".to_string(),
        Some("Server.InsufficientInstanceCapacity") => {
            "terminated by AWS: insufficient capacity".to_string()
        }
        Some("Server.ScheduledStop") => "terminated by AWS: scheduled retirement".to_string(),
        Some("Client.UserInitiatedShutdown") | Some("Client.UserInitiatedHibernate") => {
            "terminated from outside gpuprov (user/API)".to_string()
        }
        Some("Client.InstanceInitiatedShutdown") => {
            "terminated after shutting itself down (OS shutdown)".to_string()
        }
        Some(code) => format!("terminated ({code}: {})", message(instance)),
        None => match instance.state_transition_reason().filter(|r| !r.is_empty()) {
            Some(reason) => format!("terminated ({reason})"),
            None => "terminated (no reason reported)".to_string(),
        },
    };
    format!("{id} {why}")
}

/// Explain a stop from `StateReason` (codes per API_StateReason).
pub fn stop_reason(instance: &Instance) -> String {
    let id = instance.instance_id().unwrap_or("the instance");
    let why = match reason_code(instance) {
        Some("Server.SpotInstanceShutdown") => "stopped by AWS: spot interruption".to_string(),
        Some("Server.ScheduledStop") => "stopped by AWS: scheduled retirement".to_string(),
        Some("Server.InternalError") => "stopped by AWS: internal error".to_string(),
        Some("Client.UserInitiatedShutdown") => {
            "stopped from outside gpuprov (user/API)".to_string()
        }
        Some("Client.InstanceInitiatedShutdown") => {
            "stopped after shutting itself down (OS shutdown)".to_string()
        }
        Some(code) => format!("stopped ({code}: {})", message(instance)),
        None => "stopped (no reason reported)".to_string(),
    };
    format!("{id} {why}")
}

fn message(instance: &Instance) -> &str {
    instance
        .state_reason()
        .and_then(StateReason::message)
        .unwrap_or_default()
}

/// Status checks and scheduled events from one `DescribeInstanceStatus` row.
pub fn status_findings(status: &Ec2InstanceStatus) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (label, summary) in [
        ("system status check", status.system_status()),
        ("instance status check", status.instance_status()),
    ] {
        if let Some(finding) = summary.and_then(|s| summary_finding(label, s)) {
            findings.push(finding);
        }
    }
    for event in status.events() {
        let description = event.description().unwrap_or_default();
        if description.starts_with("[Completed]") || description.starts_with("[Canceled]") {
            continue;
        }
        let code = event.code().map(|c| c.as_str()).unwrap_or("event");
        let when = event
            .not_before()
            .map(|t| format!(" not before {t}"))
            .unwrap_or_default();
        findings.push(Finding::warn(format!(
            "scheduled {code}{when}: {description}"
        )));
    }
    findings
}

fn summary_finding(label: &str, summary: &InstanceStatusSummary) -> Option<Finding> {
    let details: Vec<String> = summary
        .details()
        .iter()
        .filter_map(|d| {
            let name = d.name().map(|n| n.as_str())?;
            let status = d.status().map(|s| s.as_str()).unwrap_or("?");
            Some(format!("{name}={status}"))
        })
        .collect();
    let detail = if details.is_empty() {
        String::new()
    } else {
        format!(": {}", details.join(", "))
    };
    match summary.status()? {
        SummaryStatus::Impaired => Some(Finding::critical(format!("{label} impaired{detail}"))),
        SummaryStatus::InsufficientData => {
            Some(Finding::info(format!("{label}: insufficient data yet")))
        }
        SummaryStatus::Initializing => Some(Finding::info(format!("{label} initializing"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_ec2::types::{
        EventCode, InstanceState, InstanceStatusDetails, InstanceStatusEvent, StatusName,
        StatusType,
    };

    use super::*;
    use crate::provider::Severity;

    fn instance(state: InstanceStateName, code: Option<&str>) -> Instance {
        let mut b = Instance::builder()
            .instance_id("i-1")
            .state(InstanceState::builder().name(state).build());
        if let Some(code) = code {
            b = b.state_reason(StateReason::builder().code(code).message("msg").build());
        }
        b.build()
    }

    #[test]
    fn termination_reasons_are_attributed() {
        let spot = instance(
            InstanceStateName::Terminated,
            Some("Server.SpotInstanceTermination"),
        );
        assert_eq!(
            termination_reason(&spot),
            "i-1 terminated by AWS: spot capacity reclaimed"
        );
        let user = instance(
            InstanceStateName::Terminated,
            Some("Client.UserInitiatedShutdown"),
        );
        assert!(termination_reason(&user).contains("outside gpuprov"));
        let other = instance(InstanceStateName::Terminated, Some("Server.Weird"));
        assert!(termination_reason(&other).contains("Server.Weird: msg"));
        let none = instance(InstanceStateName::Terminated, None);
        assert!(termination_reason(&none).contains("no reason"));
    }

    #[test]
    fn stop_reasons_are_attributed() {
        let spot = instance(
            InstanceStateName::Stopped,
            Some("Server.SpotInstanceShutdown"),
        );
        assert_eq!(stop_reason(&spot), "i-1 stopped by AWS: spot interruption");
    }

    #[test]
    fn choose_prefers_live_recorded_then_live_then_terminated() {
        let dead = instance(InstanceStateName::Terminated, None);
        let live = Instance::builder()
            .instance_id("i-2")
            .state(
                InstanceState::builder()
                    .name(InstanceStateName::Running)
                    .build(),
            )
            .build();
        let all = vec![dead.clone(), live];
        assert_eq!(
            choose(&all, Some("i-1")).and_then(|i| i.instance_id()),
            Some("i-2"),
            "a live replacement beats the terminated recorded one"
        );
        assert_eq!(
            choose(std::slice::from_ref(&dead), Some("i-1")).and_then(|i| i.instance_id()),
            Some("i-1")
        );
        assert!(choose(&[], Some("i-1")).is_none());
    }

    #[test]
    fn status_checks_and_events_map_to_findings() {
        let impaired = InstanceStatusSummary::builder()
            .status(SummaryStatus::Impaired)
            .details(
                InstanceStatusDetails::builder()
                    .name(StatusName::Reachability)
                    .status(StatusType::Failed)
                    .build(),
            )
            .build();
        let ok = InstanceStatusSummary::builder()
            .status(SummaryStatus::Ok)
            .build();
        let status = Ec2InstanceStatus::builder()
            .system_status(impaired)
            .instance_status(ok)
            .events(
                InstanceStatusEvent::builder()
                    .code(EventCode::InstanceRetirement)
                    .description("The instance is running on degraded hardware")
                    .build(),
            )
            .events(
                InstanceStatusEvent::builder()
                    .code(EventCode::SystemReboot)
                    .description("[Completed] reboot done")
                    .build(),
            )
            .build();
        let findings = status_findings(&status);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Critical);
        assert_eq!(
            findings[0].message,
            "system status check impaired: reachability=failed"
        );
        assert_eq!(findings[1].severity, Severity::Warn);
        assert!(findings[1].message.contains("instance-retirement"));
    }

    #[test]
    fn initializing_checks_are_informational() {
        let status = Ec2InstanceStatus::builder()
            .system_status(
                InstanceStatusSummary::builder()
                    .status(SummaryStatus::Initializing)
                    .build(),
            )
            .build();
        let findings = status_findings(&status);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);
    }
}
