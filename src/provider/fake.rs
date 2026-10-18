//! Test-only simulated cloud: an in-memory instance table with scripted
//! per-offer behaviors and a call log. Shared by the `vm` and `cluster`
//! machine tests (extend it rather than writing ad-hoc fakes).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use super::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceAsk, ResourceRequest, SshEndpoint,
};

/// What happens when an offer (keyed by `region` or `region:instance_type`,
/// most specific wins) is attempted.
#[derive(Debug, Clone)]
pub enum Behavior {
    /// Created, Pending for `boot_polls` observations, then Running.
    Boots { boot_polls: u32 },
    /// `prepare` fails with this error (nothing created).
    PrepareFails(ProviderError),
    /// `create` fails with this error (nothing created).
    CreateFails(ProviderError),
    /// `create` succeeds server-side but the call reports this error (e.g. a
    /// timeout after the request landed).
    CreatedButErrors(ProviderError),
    /// Created, then reports `Failed` while booting.
    BootFails,
    /// Created, never leaves Pending.
    NeverBoots,
}

#[derive(Debug, Clone)]
struct Instance {
    id: String,
    vm: String,
    region: String,
    status: InstanceStatus,
    polls_left: u32,
    /// Observations left in Terminating before disappearing.
    terminating_polls: u32,
}

pub struct FakeProvider {
    behaviors: Vec<(String, Behavior)>,
    instances: Mutex<Vec<Instance>>,
    next_id: Mutex<u32>,
    calls: Mutex<Vec<String>>,
    /// `delete` errors: `Some(n)` fails the next n calls, `u32::MAX` forever.
    pub delete_failures: Mutex<u32>,
    pub unsupported: &'static [(Feature, &'static str)],
    /// Reported as `Capabilities::cloud_init`.
    pub cloud_init: bool,
    pub ssh_user: Option<String>,
    pub ssh_port: u16,
    /// Max observed concurrent live instances per VM (leak/duplicate check).
    pub max_live: Mutex<HashMap<String, usize>>,
}

impl FakeProvider {
    pub fn new(behaviors: Vec<(&str, Behavior)>) -> Self {
        FakeProvider {
            behaviors: behaviors
                .into_iter()
                .map(|(k, b)| (k.to_string(), b))
                .collect(),
            instances: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
            delete_failures: Mutex::new(0),
            unsupported: &[],
            cloud_init: false,
            ssh_user: None,
            ssh_port: 22,
            max_live: Mutex::new(HashMap::new()),
        }
    }

    fn behavior(&self, offer: &Offer) -> Behavior {
        let specific = format!("{}:{}", offer.region, offer.instance_type);
        self.behaviors
            .iter()
            .find(|(k, _)| *k == specific)
            .or_else(|| self.behaviors.iter().find(|(k, _)| *k == offer.region))
            .map(|(_, b)| b.clone())
            .unwrap_or(Behavior::Boots { boot_polls: 1 })
    }

    fn log(&self, entry: String) {
        self.calls.lock().unwrap().push(entry);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count_calls(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    /// Live (not yet gone) instances, for leak assertions.
    pub fn live(&self) -> usize {
        self.instances.lock().unwrap().len()
    }

    /// Simulate an instance created out-of-band (e.g. by a crashed run).
    pub fn seed_instance(&self, vm: &str, region: &str, status: InstanceStatus) -> String {
        let id = self.new_id();
        self.instances.lock().unwrap().push(Instance {
            id: id.clone(),
            vm: vm.to_string(),
            region: region.to_string(),
            status,
            polls_left: 0,
            terminating_polls: 0,
        });
        id
    }

    fn new_id(&self) -> String {
        let mut n = self.next_id.lock().unwrap();
        *n += 1;
        format!("i-{n}")
    }

    fn spawn(&self, offer: &Offer, vm: &str, behavior: &Behavior) -> String {
        let id = self.new_id();
        let (status, polls_left) = match behavior {
            Behavior::Boots { boot_polls } => (InstanceStatus::Pending, *boot_polls),
            Behavior::BootFails => (InstanceStatus::Pending, 1),
            _ => (InstanceStatus::Pending, u32::MAX),
        };
        let mut instances = self.instances.lock().unwrap();
        instances.push(Instance {
            id: id.clone(),
            vm: vm.to_string(),
            region: offer.region.clone(),
            status,
            polls_left,
            terminating_polls: 0,
        });
        let live = instances.iter().filter(|i| i.vm == vm).count();
        let mut max = self.max_live.lock().unwrap();
        let entry = max.entry(vm.to_string()).or_default();
        *entry = (*entry).max(live);
        // BootFails: remember to flip to Failed instead of Running.
        if matches!(behavior, Behavior::BootFails) {
            self.log(format!("bootfails:{id}"));
        }
        id
    }
}

#[async_trait]
impl CloudProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: self.unsupported,
            cloud_init: self.cloud_init,
            boot_timeout: Duration::from_secs(60),
            delete_timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(1),
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let types: Vec<String> = match &request.ask {
            ResourceAsk::InstanceType(it) => it.split(',').map(str::to_string).collect(),
            ResourceAsk::Gpus(g) => vec![format!("fake-{}", g.name)],
        };
        super::expand_offers(&types, &request.regions, request.zone.as_deref())
    }

    async fn prepare(
        &self,
        offer: &Offer,
        _spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        self.log(format!("prepare:{}:{}", offer.region, offer.instance_type));
        match self.behavior(offer) {
            Behavior::PrepareFails(e) => Err(e),
            _ => Ok(serde_json::json!({"prepared": offer.region})),
        }
    }

    async fn create(
        &self,
        offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        assert_eq!(prepared["prepared"], offer.region.as_str());
        self.log(format!("create:{}:{}", offer.region, offer.instance_type));
        let behavior = self.behavior(offer);
        match &behavior {
            Behavior::CreateFails(e) => Err(e.clone()),
            Behavior::CreatedButErrors(e) => {
                self.spawn(offer, &spec.vm_name, &Behavior::Boots { boot_polls: 1 });
                Err(e.clone())
            }
            other => Ok(self.spawn(offer, &spec.vm_name, other)),
        }
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        self.log(format!("observe:{}", at.region));
        let bootfails: Vec<String> = self
            .calls()
            .iter()
            .filter_map(|c| c.strip_prefix("bootfails:").map(str::to_string))
            .collect();
        let mut instances = self.instances.lock().unwrap();
        // Advance the simulation one tick per observation.
        instances.retain_mut(|i| {
            if i.region != at.region || i.vm != vm_name {
                return true;
            }
            match i.status {
                InstanceStatus::Pending if i.polls_left == u32::MAX => {}
                InstanceStatus::Pending if i.polls_left > 0 => i.polls_left -= 1,
                InstanceStatus::Pending => {
                    i.status = if bootfails.contains(&i.id) {
                        InstanceStatus::Failed("kernel panic".into())
                    } else {
                        InstanceStatus::Running
                    }
                }
                InstanceStatus::Terminating if i.terminating_polls == 0 => return false,
                InstanceStatus::Terminating => i.terminating_polls -= 1,
                _ => {}
            }
            true
        });
        Ok(instances
            .iter()
            .filter(|i| i.region == at.region && i.vm == vm_name)
            .map(|i| Observed {
                instance_id: i.id.clone(),
                status: i.status.clone(),
                zone: Some(format!("{}a", i.region)),
                private_ip: Some("10.0.0.1".into()),
                endpoint: (i.status == InstanceStatus::Running).then(|| SshEndpoint {
                    host: format!("203.0.113.{}", i.id.trim_start_matches("i-")),
                    port: self.ssh_port,
                    user: self.ssh_user.clone(),
                }),
            })
            .collect())
    }

    async fn delete(
        &self,
        at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        self.log(format!("delete:{}:{}", at.region, instance_ids.join(",")));
        {
            let mut failures = self.delete_failures.lock().unwrap();
            if *failures > 0 {
                if *failures != u32::MAX {
                    *failures -= 1;
                }
                return Err(ProviderError::transient("api down"));
            }
        }
        for i in self.instances.lock().unwrap().iter_mut() {
            if instance_ids.contains(&i.id) && i.status != InstanceStatus::Terminating {
                i.status = InstanceStatus::Terminating;
                i.terminating_polls = 1;
            }
        }
        Ok(())
    }

    async fn cleanup(&self, at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        self.log(format!("cleanup:{}", at.region));
        Ok(())
    }
}
