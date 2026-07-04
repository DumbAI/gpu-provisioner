//! AWS adapter: credential resolution (CLI config file → profile → default
//! chain), per-region client cache, and the `CloudProvider` implementation.

pub mod ami;
pub mod bootstrap;
pub mod error;
pub mod instance;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};

use crate::config::AwsConfig;
use crate::provider::{
    Bootstrapped, CloudProvider, ProvisionError, ProvisionRecord, ProvisionRequest, VmInfo,
};

pub struct AwsProvider {
    base: aws_config::SdkConfig,
    clients: Mutex<HashMap<String, RegionClients>>,
}

#[derive(Clone)]
struct RegionClients {
    ec2: aws_sdk_ec2::Client,
    /// SDK retries disabled so capacity errors surface immediately
    /// (SkyPilot's fail-fast `max_attempts=0` client for create/start).
    ec2_fail_fast: aws_sdk_ec2::Client,
    ssm: aws_sdk_ssm::Client,
}

impl AwsProvider {
    /// Credential resolution order: explicit static keys from the config file
    /// → named profile from the config file → default AWS chain (env vars,
    /// `~/.aws/credentials`, IMDS, ...).
    pub async fn new(cfg: &AwsConfig) -> Self {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let (Some(access_key), Some(secret_key)) = (&cfg.access_key_id, &cfg.secret_access_key) {
            tracing::debug!("using static AWS credentials from the config file");
            loader = loader.credentials_provider(aws_credential_types::Credentials::from_keys(
                access_key.clone(),
                secret_key.clone(),
                cfg.session_token.clone(),
            ));
        } else if let Some(profile) = &cfg.profile {
            tracing::debug!("using AWS profile '{profile}' from the config file");
            loader = loader.profile_name(profile);
        }
        let base = loader.load().await;
        Self {
            base,
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn clients(&self, region: &str) -> RegionClients {
        let mut cache = self.clients.lock().expect("client cache poisoned");
        if let Some(clients) = cache.get(region) {
            return clients.clone();
        }
        let region_obj = Region::new(region.to_string());
        let retries = aws_sdk_ec2::config::retry::RetryConfig::standard().with_max_attempts(12);
        let ec2_conf = aws_sdk_ec2::config::Builder::from(&self.base)
            .region(region_obj.clone())
            .retry_config(retries.clone())
            .build();
        let ec2_fail_fast_conf = aws_sdk_ec2::config::Builder::from(&self.base)
            .region(region_obj.clone())
            .retry_config(aws_sdk_ec2::config::retry::RetryConfig::disabled())
            .build();
        let ssm_conf = aws_sdk_ssm::config::Builder::from(&self.base)
            .region(region_obj)
            .retry_config(retries)
            .build();
        let clients = RegionClients {
            ec2: aws_sdk_ec2::Client::from_conf(ec2_conf),
            ec2_fail_fast: aws_sdk_ec2::Client::from_conf(ec2_fail_fast_conf),
            ssm: aws_sdk_ssm::Client::from_conf(ssm_conf),
        };
        cache.insert(region.to_string(), clients.clone());
        clients
    }
}

#[async_trait]
impl CloudProvider for AwsProvider {
    async fn bootstrap(&self, req: &ProvisionRequest) -> Result<Bootstrapped, ProvisionError> {
        let clients = self.clients(&req.region);
        bootstrap::bootstrap(&clients.ec2, &clients.ssm, req).await
    }

    async fn run_instance(
        &self,
        req: &ProvisionRequest,
        bootstrapped: &Bootstrapped,
    ) -> Result<ProvisionRecord, ProvisionError> {
        let clients = self.clients(&req.region);
        instance::run_instance(&clients.ec2, &clients.ec2_fail_fast, req, bootstrapped).await
    }

    async fn wait_running(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError> {
        instance::wait_running(&self.clients(region).ec2, region, vm_name).await
    }

    async fn describe(
        &self,
        region: &str,
        vm_name: &str,
    ) -> Result<Option<VmInfo>, ProvisionError> {
        instance::describe(&self.clients(region).ec2, region, vm_name).await
    }

    async fn terminate(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError> {
        instance::terminate(&self.clients(region).ec2, region, vm_name).await
    }
}
