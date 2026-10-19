//! AWS adapter: credential resolution (CLI config file → profile → default
//! chain), per-region client cache, and the `CloudProvider` implementation
//! (prepare = `bootstrap`, create/observe/delete/cleanup = `instance`).

pub mod ami;
pub mod bootstrap;
pub mod catalog;
pub mod diagnose;
pub mod error;
pub mod instance;
pub mod instance_type;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::types::Filter;

use crate::config::AwsConfig;
use crate::provider::{
    Capabilities, CloudProvider, Diagnosis, InstanceSpec, Location, Observed, Offer, ProviderError,
    ResourceRequest,
};

/// A single-valued EC2 describe filter — the AWS SDK's builder chain is
/// verbose enough to be worth wrapping (used across bootstrap/instance queries).
/// Canonical provider id recorded on VM rows.
pub const PROVIDER_ID: &str = "aws";

pub(crate) fn filter(name: &str, value: &str) -> Filter {
    Filter::builder().name(name).values(value).build()
}

/// A multi-valued EC2 describe filter (e.g. a set of instance states).
pub(crate) fn filter_values<'a>(name: &str, values: impl IntoIterator<Item = &'a str>) -> Filter {
    values
        .into_iter()
        .fold(Filter::builder().name(name), |b, v| b.values(v))
        .build()
}

pub struct AwsProvider {
    base: aws_config::SdkConfig,
    clients: Mutex<HashMap<String, RegionClients>>,
}

#[derive(Clone)]
struct RegionClients {
    ec2: aws_sdk_ec2::Client,
    /// SDK retries disabled so capacity errors surface immediately on
    /// `RunInstances` (the core decides whether to retry or fail over).
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
        if let Some(endpoint) = &cfg.endpoint_url {
            tracing::debug!("sending AWS API calls to {endpoint}");
            loader = loader.endpoint_url(endpoint);
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
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        // Everything is supported, and the Ubuntu / Deep Learning AMIs run
        // cloud-init. EC2 boots in ~1 min; the default 10-minute boot budget
        // matches the boto3 `instance_running` waiter.
        Capabilities {
            boot_script: true,
            ..Capabilities::default()
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        catalog::plan(request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let clients = self.clients(&offer.region);
        let prepared = bootstrap::bootstrap(&clients.ec2, &clients.ssm, offer, spec).await?;
        serde_json::to_value(prepared)
            .map_err(|e| ProviderError::failed(format!("serializing bootstrap state: {e}")))
    }

    async fn create(
        &self,
        offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let prepared: bootstrap::Bootstrapped = serde_json::from_value(prepared.clone())
            .map_err(|e| ProviderError::failed(format!("corrupt bootstrap state: {e}")))?;
        instance::create(
            &self.clients(&offer.region).ec2_fail_fast,
            offer,
            &prepared,
            spec,
        )
        .await
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        instance::observe(&self.clients(&at.region).ec2, vm_name).await
    }

    async fn delete(
        &self,
        at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        instance::delete(&self.clients(&at.region).ec2, instance_ids).await
    }

    async fn cleanup(&self, at: &Location, vm_name: &str) -> Result<(), ProviderError> {
        instance::delete_security_group(&self.clients(&at.region).ec2, &at.region, vm_name).await
    }

    /// EC2-native diagnosis: state reasons, status checks, scheduled events
    /// (see `diagnose`).
    async fn diagnose(
        &self,
        at: &Location,
        vm_name: &str,
        instance_id: Option<&str>,
    ) -> Result<Diagnosis, ProviderError> {
        diagnose::diagnose(&self.clients(&at.region).ec2, vm_name, instance_id).await
    }
}
