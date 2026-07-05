//! The long-running launch operation ("Phase 3" stage machine):
//! **Plan → Provision-with-failover → Wait-for-SSH → Persist**.
//! Blocking by design — the CLI runs it start to finish and the end state is
//! a `UP` record in the local DB with the VM's IP and SSH config.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::AppConfig;
use crate::db::{Db, UpFields, VmState};
use crate::failover::{self, FailoverError};
use crate::planner::{self, LaunchSpec, PlanError};
use crate::provider::{CloudProvider, ProvisionError, VmInfo};
use crate::spec::VmSpec;
use crate::ssh::config_file;
use crate::ssh::keys::{self, KeyError, KeyPair};
use crate::ssh::options::SshOptions;
use crate::ssh::wait::{self, WaitError};
use crate::util::Paths;

/// Gap between full failover rounds with `--retry-until-up`
/// (SkyPilot's `_RETRY_UNTIL_UP_INIT_GAP_SECONDS`).
const RETRY_UNTIL_UP_GAP: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct LaunchOutcome {
    pub name: String,
    pub region: String,
    pub zone: String,
    pub instance_id: String,
    pub public_ip: String,
    pub ssh_user: String,
    pub ssh_key_path: PathBuf,
    pub ssh_config_path: PathBuf,
    /// True when an existing live instance was reused.
    pub reused: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("state db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Provision(#[from] ProvisionError),
    #[error(transparent)]
    Failover(#[from] FailoverError),
    #[error(transparent)]
    SshWait(#[from] WaitError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Provision (or reconcile) the VM described by `spec` and block until it is
/// SSH-ready and recorded in the DB.
pub async fn launch(
    provider: &dyn CloudProvider,
    config: &AppConfig,
    db: &Db,
    paths: &Paths,
    spec: &VmSpec,
    retry_until_up: bool,
) -> Result<LaunchOutcome, LaunchError> {
    let keypair = keys::ensure_keypair(&paths.keys_dir())?;
    let prepared = planner::prepare(spec, &config.defaults, &keypair.public_key_openssh)?;
    let launch = &prepared.launch;

    // Reconcile: an UP record whose instance is still running just gets
    // refreshed (find-or-create keeps this idempotent at the cloud level too).
    if let Some(record) = db.get(&launch.vm_name)? {
        if record.state == VmState::Up {
            if let Some(region) = record.region.clone() {
                tracing::info!(
                    "record for '{}' is UP in {region}; verifying against the cloud",
                    launch.vm_name
                );
                if let Some(info) = provider.describe(&region, &launch.vm_name).await? {
                    let zone = record.zone.clone().unwrap_or_default();
                    return finalize(db, paths, launch, &keypair, &region, &zone, info, true).await;
                }
                tracing::warn!(
                    "no running instance found for '{}' in {region}; re-provisioning",
                    launch.vm_name
                );
            }
        }
    }

    // The provider resolves the resource ask into cloud-specific candidates
    // (instance types × regions, in preference order).
    let candidates = provider.plan_candidates(&prepared.request).await?;
    let preview: Vec<String> = candidates.iter().take(6).map(|c| c.to_string()).collect();
    tracing::info!(
        "stage 1/4 plan: vm '{}' on {}, {} candidate(s): {}{}{}",
        launch.vm_name,
        provider.name(),
        candidates.len(),
        preview.join(", "),
        if candidates.len() > preview.len() {
            format!(", … +{} more", candidates.len() - preview.len())
        } else {
            String::new()
        },
        if launch.spot { " (spot)" } else { "" },
    );

    let spec_json = serde_json::to_string(spec)
        .map_err(|e| LaunchError::Other(format!("cannot serialize spec: {e}")))?;

    tracing::info!("stage 2/4 provision: starting failover loop");
    let record = loop {
        match failover::provision_with_failover(provider, launch, &candidates, db, &spec_json).await
        {
            Ok(record) => break record,
            Err(e @ FailoverError::Exhausted { .. }) if retry_until_up => {
                tracing::warn!("{e}");
                tracing::info!(
                    "--retry-until-up: next round in {}s",
                    RETRY_UNTIL_UP_GAP.as_secs()
                );
                tokio::time::sleep(RETRY_UNTIL_UP_GAP).await;
            }
            Err(e) => return Err(e.into()),
        }
    };

    let info = provider
        .describe(&record.region, &launch.vm_name)
        .await?
        .ok_or_else(|| {
            LaunchError::Other(format!(
                "instance for '{}' disappeared right after provisioning in {}",
                launch.vm_name, record.region
            ))
        })?;
    finalize(
        db,
        paths,
        launch,
        &keypair,
        &record.region,
        &record.zone,
        info,
        record.reused,
    )
    .await
}

/// Stages 3–4: wait for SSH, then persist the record + SSH config.
#[allow(clippy::too_many_arguments)]
async fn finalize(
    db: &Db,
    paths: &Paths,
    launch: &LaunchSpec,
    keypair: &KeyPair,
    region: &str,
    zone: &str,
    info: VmInfo,
    reused: bool,
) -> Result<LaunchOutcome, LaunchError> {
    let public_ip = info.public_ip.clone().ok_or_else(|| {
        LaunchError::Other(format!(
            "instance {} has no public IP; clean up with `gpuprov down {}`",
            info.instance_id, launch.vm_name
        ))
    })?;

    tracing::info!(
        "stage 3/4 wait-ssh: probing {}@{public_ip}",
        launch.ssh_user
    );
    let ssh_opts = SshOptions::new(keypair.private_path.clone());
    wait::wait_for_ssh(
        &public_ip,
        &launch.ssh_user,
        &ssh_opts,
        wait::DEFAULT_TIMEOUT,
    )
    .await?;

    tracing::info!("stage 4/4 persist: writing SSH config and DB record");
    let ssh_config_path = config_file::write_vm_host_entry(
        &paths.ssh_dir(),
        &launch.vm_name,
        &public_ip,
        &launch.ssh_user,
        &keypair.private_path,
    )?;
    if let Some(user_ssh_config) = Paths::user_ssh_config_path() {
        config_file::ensure_include(&user_ssh_config, &paths.ssh_dir())?;
    }
    db.mark_up(
        &launch.vm_name,
        &UpFields {
            region: region.to_string(),
            zone: zone.to_string(),
            instance_id: info.instance_id.clone(),
            public_ip: public_ip.clone(),
            private_ip: info.private_ip.clone(),
            ssh_user: launch.ssh_user.clone(),
            ssh_key_path: keypair.private_path.display().to_string(),
            ssh_config_path: ssh_config_path.display().to_string(),
        },
    )?;

    Ok(LaunchOutcome {
        name: launch.vm_name.clone(),
        region: region.to_string(),
        zone: zone.to_string(),
        instance_id: info.instance_id,
        public_ip,
        ssh_user: launch.ssh_user.clone(),
        ssh_key_path: keypair.private_path.clone(),
        ssh_config_path,
        reused,
    })
}
