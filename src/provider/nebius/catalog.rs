//! Planning against live Nebius data. Nebius projects are regional (one per
//! region by default), so the catalog is assembled per project: its
//! platforms (`gpu-h100-sxm`, …) and their presets (`1gpu-16vcpu-200gb`, …)
//! become one offer per platform × preset × region that matches the ask.
//!
//! Prices: on-demand offers are priced by the billing calculator (one batch
//! for every candidate); preemptible offers by the public spot-price list,
//! which quotes one GPU hour per platform × region. An offer that could not
//! be priced is kept but sorted last.
//!
//! The capacity advisor is a soft signal: an offer whose fresh advice says
//! nothing can launch (limit reached, or zero available within quota) is
//! dropped, and among equally priced offers the more available one goes
//! first. Without advice (no tenant id, or no permission) every offer stays.
//!
//! Boot images follow Nebius's recommendation: `ubuntu24.04-cuda13.0` for
//! every GPU platform, `ubuntu24.04-driverless` for CPU-only platforms
//! (<https://docs.nebius.com/compute/storage/boot-disk-images>).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{Availability, Platform, Preset, ResourceAdvice, SpotPrice};

/// Recommended image family for GPU platforms.
pub const IMAGE_GPU: &str = "ubuntu24.04-cuda13.0";
/// Image family for CPU-only platforms.
pub const IMAGE_CPU: &str = "ubuntu24.04-driverless";

/// What a Nebius offer carries from plan to launch (`Offer::detail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub project_id: String,
    pub region: String,
    pub platform: String,
    pub preset: String,
    /// Default image family (an `--image` override wins).
    pub image_family: String,
}

/// One project's platforms.
#[derive(Debug, Clone)]
pub struct RegionCatalog {
    pub region: String,
    pub project_id: String,
    pub platforms: Vec<Platform>,
}

/// A launchable configuration before pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub detail: OfferDetail,
    pub gpu_count: u32,
}

impl Candidate {
    /// `platform/preset` — the offer's instance type.
    pub fn instance_type(&self) -> String {
        format!("{}/{}", self.detail.platform, self.detail.preset)
    }

    /// Unique key of the configuration (also the calculator request name).
    pub fn key(&self) -> String {
        format!("{}@{}", self.instance_type(), self.detail.region)
    }
}

/// Uppercase tokens of a GPU name (split on space, `-`, `_`, parentheses;
/// trademark signs dropped).
fn tokens(text: &str) -> Vec<String> {
    text.replace(['®', '™'], "")
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '(' | ')' | ','))
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .collect()
}

/// Whether the user's GPU shorthand names this platform's GPU. Each user
/// token must equal a token of the platform id (`gpu-h100-sxm`) or of its
/// human-readable names — or adjacent tokens joined (`RTXPRO6000` ≙
/// `RTX PRO 6000`). Whole tokens keep `L40` off `L40S` and `B200` off `B300`.
pub fn gpu_name_matches(user: &str, platform: &Platform) -> bool {
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    [
        platform.metadata.name.as_str(),
        platform.spec.human_readable_name.as_str(),
        platform.spec.short_human_readable_name.as_str(),
    ]
    .iter()
    .filter(|text| !text.is_empty())
    .any(|text| {
        let have = tokens(text);
        let mut joined: Vec<String> = have
            .windows(2)
            .map(|w| format!("{}{}", w[0], w[1]))
            .collect();
        joined.extend(have.windows(3).map(|w| format!("{}{}{}", w[0], w[1], w[2])));
        wanted
            .iter()
            .all(|w| have.contains(w) || joined.contains(w))
    })
}

fn matches_ask(ask: &ResourceAsk, platform: &Platform, preset: &Preset) -> bool {
    match ask {
        // `platform/preset`, or a bare platform (any of its presets).
        ResourceAsk::InstanceType(wanted) => match wanted.split_once('/') {
            Some((p, s)) => {
                platform.metadata.name.eq_ignore_ascii_case(p)
                    && preset.name.eq_ignore_ascii_case(s)
            }
            None => platform.metadata.name.eq_ignore_ascii_case(wanted),
        },
        ResourceAsk::Gpus(gpus) => {
            preset.resources.gpu_count == gpus.count && gpu_name_matches(&gpus.name, platform)
        }
    }
}

/// Every configuration in `catalogs` that satisfies the ask (and, for
/// spot, allows preemptible VMs). Errors when nothing matches at all.
pub fn candidates(
    catalogs: &[RegionCatalog],
    request: &ResourceRequest,
) -> Result<Vec<Candidate>, ProviderError> {
    let mut out = Vec::new();
    let mut known: Vec<String> = Vec::new();
    let mut spot_refused = false;
    for catalog in catalogs {
        for platform in &catalog.platforms {
            for preset in &platform.spec.presets {
                if preset.resources.gpu_count > 0 {
                    known.push(format!("{}/{}", platform.metadata.name, preset.name));
                }
                if !matches_ask(&request.ask, platform, preset) {
                    continue;
                }
                if request.spot && !platform.status.allowed_for_preemptibles {
                    spot_refused = true;
                    continue;
                }
                let gpu_count = preset.resources.gpu_count;
                out.push(Candidate {
                    detail: OfferDetail {
                        project_id: catalog.project_id.clone(),
                        region: catalog.region.clone(),
                        platform: platform.metadata.name.clone(),
                        preset: preset.name.clone(),
                        image_family: if gpu_count > 0 { IMAGE_GPU } else { IMAGE_CPU }.to_string(),
                    },
                    gpu_count,
                });
            }
        }
    }
    if out.is_empty() {
        let ask = match &request.ask {
            ResourceAsk::InstanceType(t) => format!("instance type '{t}'"),
            ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
        };
        if spot_refused {
            return Err(ProviderError::invalid(format!(
                "no Nebius platform matching {ask} allows preemptible (spot) VMs"
            )));
        }
        known.sort();
        known.dedup();
        known.truncate(12);
        let regions: Vec<&str> = catalogs.iter().map(|c| c.region.as_str()).collect();
        return Err(ProviderError::invalid(format!(
            "no Nebius platform/preset matches {ask} in {}; GPU presets there include: {}",
            regions.join(", "),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )));
    }
    Ok(out)
}

/// Spot price of one GPU hour by `(region, platform)`.
pub fn spot_price_index(prices: &[SpotPrice]) -> HashMap<(String, String), f64> {
    prices
        .iter()
        .filter(|p| p.price.currency.is_empty() || p.price.currency.eq_ignore_ascii_case("usd"))
        .filter_map(|p| {
            let value: f64 = p.price.value.parse().ok()?;
            Some((
                (p.region.clone(), p.compute_instance.platform.clone()),
                value,
            ))
        })
        .collect()
}

/// Higher is more likely to launch.
fn availability_rank(a: &Availability) -> u8 {
    match a.availability_level.as_str() {
        "AVAILABILITY_LEVEL_HIGH" => 3,
        "AVAILABILITY_LEVEL_MEDIUM" => 2,
        "AVAILABILITY_LEVEL_LOW" => 1,
        _ => 0,
    }
}

/// Fresh advice that nothing of this kind can launch now.
fn advice_rules_out(a: &Availability) -> bool {
    a.data_state == "DATA_STATE_FRESH"
        && (a.availability_level == "AVAILABILITY_LEVEL_LIMIT_REACHED" || a.available <= 0)
}

/// Advice by `(region, platform, preset)` for on-demand or preemptible VMs.
fn advice_index(
    advice: &[ResourceAdvice],
    spot: bool,
) -> HashMap<(String, String, String), Availability> {
    advice
        .iter()
        .map(|a| {
            let instance = &a.spec.compute_instance;
            let availability = if spot {
                a.status.preemptible.clone()
            } else {
                a.status.on_demand.clone()
            };
            (
                (
                    a.spec.region.clone(),
                    instance.platform.clone(),
                    instance.preset.name.clone(),
                ),
                availability,
            )
        })
        .collect()
}

/// Price, filter and order candidates: cheapest first, unpriced last, more
/// available first among equal prices. `on_demand` maps [`Candidate::key`] to
/// an hourly price; `spot` maps `(region, platform)` to a GPU-hour price.
pub fn offers_from(
    candidates: Vec<Candidate>,
    spot_request: bool,
    on_demand: &BTreeMap<String, f64>,
    spot: &HashMap<(String, String), f64>,
    advice: Option<&[ResourceAdvice]>,
) -> Vec<Offer> {
    let advice = advice.map(|a| advice_index(a, spot_request));
    let mut ranked: Vec<(Offer, u8)> = Vec::new();
    for candidate in candidates {
        let detail = &candidate.detail;
        let availability = advice.as_ref().and_then(|index| {
            index.get(&(
                detail.region.clone(),
                detail.platform.clone(),
                detail.preset.clone(),
            ))
        });
        if let Some(a) = availability {
            if advice_rules_out(a) {
                tracing::info!(
                    "skipping {}: the capacity advisor reports no {} capacity ({})",
                    candidate.key(),
                    if spot_request {
                        "preemptible"
                    } else {
                        "on-demand"
                    },
                    a.availability_level
                );
                continue;
            }
        }
        let price = if spot_request {
            spot.get(&(detail.region.clone(), detail.platform.clone()))
                .map(|gpu_hour| gpu_hour * f64::from(candidate.gpu_count.max(1)))
        } else {
            on_demand.get(&candidate.key()).copied()
        };
        let mut offer = Offer::new(candidate.instance_type(), &detail.region);
        offer.hourly_price = price.filter(|p| *p > 0.0);
        offer.detail = serde_json::to_value(detail).expect("detail serializes");
        ranked.push((offer, availability.map_or(0, availability_rank)));
    }
    ranked.sort_by(|(a, rank_a), (b, rank_b)| {
        let price = |o: &Offer| o.hourly_price.unwrap_or(f64::INFINITY);
        price(a)
            .total_cmp(&price(b))
            .then_with(|| rank_b.cmp(rank_a))
            .then_with(|| a.region.cmp(&b.region))
            .then_with(|| a.instance_type.cmp(&b.instance_type))
    });
    ranked.into_iter().map(|(offer, _)| offer).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::provider::GpuRequest;

    fn platform(name: &str, human: &str, presets: &[(&str, u32)], preemptible: bool) -> Platform {
        let presets: Vec<_> = presets
            .iter()
            .map(|(n, g)| json!({"name": n, "resources": {"vcpuCount": 16, "memoryGibibytes": 200, "gpuCount": g}}))
            .collect();
        serde_json::from_value(json!({
            "metadata": {"id": format!("computeplatform-{name}"), "parentId": "project-x", "name": name},
            "spec": {"humanReadableName": human, "presets": presets},
            "status": {"allowedForPreemptibles": preemptible}
        }))
        .unwrap()
    }

    fn catalogs() -> Vec<RegionCatalog> {
        let h100 = platform(
            "gpu-h100-sxm",
            "NVIDIA® H100 NVLink with Intel Sapphire Rapids",
            &[("1gpu-16vcpu-200gb", 1), ("8gpu-128vcpu-1600gb", 8)],
            true,
        );
        let h200 = platform(
            "gpu-h200-sxm",
            "NVIDIA® H200 NVLink with Intel Sapphire Rapids",
            &[("1gpu-16vcpu-200gb", 1)],
            true,
        );
        let rtx = platform(
            "gpu-rtx6000",
            "NVIDIA® RTX PRO™ 6000 with Intel Granite Rapids",
            &[("1gpu-24vcpu-200gb", 1)],
            false,
        );
        let l40s = platform(
            "gpu-l40s-a",
            "NVIDIA® L40S PCIe with Intel Ice Lake",
            &[("1gpu-8vcpu-32gb", 1)],
            true,
        );
        let cpu = platform(
            "cpu-d3",
            "Non-GPU AMD EPYC Genoa",
            &[("4vcpu-16gb", 0)],
            false,
        );
        vec![
            RegionCatalog {
                region: "eu-north1".into(),
                project_id: "project-e00north".into(),
                platforms: vec![h100, h200.clone(), l40s, cpu.clone()],
            },
            RegionCatalog {
                region: "us-central1".into(),
                project_id: "project-e00central".into(),
                platforms: vec![h200, rtx, cpu],
            },
        ]
    }

    fn gpus(name: &str, count: u32) -> ResourceRequest {
        ResourceRequest {
            ask: ResourceAsk::Gpus(GpuRequest {
                name: name.into(),
                count,
            }),
            regions: Vec::new(),
            zone: None,
            spot: false,
        }
    }

    fn keys(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(Candidate::key).collect()
    }

    #[test]
    fn gpu_names_match_whole_tokens_of_platform_names() {
        let all = catalogs();
        let find = |name: &str| {
            all.iter()
                .flat_map(|c| &c.platforms)
                .find(|p| p.metadata.name == name)
                .unwrap()
        };
        assert!(gpu_name_matches("H100", find("gpu-h100-sxm")));
        assert!(!gpu_name_matches("H100", find("gpu-h200-sxm")));
        assert!(gpu_name_matches("L40S", find("gpu-l40s-a")));
        assert!(!gpu_name_matches("L40", find("gpu-l40s-a")));
        assert!(gpu_name_matches("RTX6000", find("gpu-rtx6000")));
        assert!(gpu_name_matches("RTXPRO6000", find("gpu-rtx6000")));
        assert!(gpu_name_matches("RTX-PRO-6000", find("gpu-rtx6000")));
        assert!(!gpu_name_matches("H100", find("cpu-d3")));
    }

    #[test]
    fn candidates_span_regions_and_match_counts() {
        let found = candidates(&catalogs(), &gpus("H200", 1)).unwrap();
        assert_eq!(
            keys(&found),
            [
                "gpu-h200-sxm/1gpu-16vcpu-200gb@eu-north1",
                "gpu-h200-sxm/1gpu-16vcpu-200gb@us-central1"
            ]
        );
        assert_eq!(found[1].detail.project_id, "project-e00central");
        assert_eq!(found[0].detail.image_family, IMAGE_GPU);
        let eight = candidates(&catalogs(), &gpus("H100", 8)).unwrap();
        assert_eq!(keys(&eight), ["gpu-h100-sxm/8gpu-128vcpu-1600gb@eu-north1"]);
    }

    #[test]
    fn instance_type_asks_take_platform_slash_preset_or_a_platform() {
        let ask = |t: &str| ResourceRequest {
            ask: ResourceAsk::InstanceType(t.into()),
            ..gpus("x", 1)
        };
        let exact = candidates(&catalogs(), &ask("gpu-h100-sxm/1gpu-16vcpu-200gb")).unwrap();
        assert_eq!(keys(&exact), ["gpu-h100-sxm/1gpu-16vcpu-200gb@eu-north1"]);
        assert_eq!(
            candidates(&catalogs(), &ask("gpu-h100-sxm")).unwrap().len(),
            2
        );
        let cpu = candidates(&catalogs(), &ask("cpu-d3/4vcpu-16gb")).unwrap();
        assert_eq!(cpu[0].detail.image_family, IMAGE_CPU);
    }

    #[test]
    fn spot_needs_a_preemptible_platform_and_unknown_gpus_are_invalid() {
        let mut spot = gpus("RTX6000", 1);
        spot.spot = true;
        let err = candidates(&catalogs(), &spot).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("preemptible"), "{}", err.msg);

        let err = candidates(&catalogs(), &gpus("B300", 1)).unwrap_err();
        assert!(
            err.msg.contains("gpu-h100-sxm/1gpu-16vcpu-200gb"),
            "{}",
            err.msg
        );
        assert!(err.msg.contains("eu-north1, us-central1"), "{}", err.msg);
    }

    #[test]
    fn offers_are_priced_ordered_and_filtered_by_fresh_advice() {
        let found = candidates(&catalogs(), &gpus("H200", 1)).unwrap();
        let on_demand: BTreeMap<String, f64> = [
            ("gpu-h200-sxm/1gpu-16vcpu-200gb@eu-north1".to_string(), 3.5),
            (
                "gpu-h200-sxm/1gpu-16vcpu-200gb@us-central1".to_string(),
                3.5,
            ),
        ]
        .into();
        let advice: Vec<ResourceAdvice> = serde_json::from_value(json!([
            {"spec": {"region": "us-central1", "computeInstance": {"platform": "gpu-h200-sxm", "preset": {"name": "1gpu-16vcpu-200gb"}}},
             "status": {"onDemand": {"available": "5", "availabilityLevel": "AVAILABILITY_LEVEL_HIGH", "dataState": "DATA_STATE_FRESH"}}},
            {"spec": {"region": "eu-north1", "computeInstance": {"platform": "gpu-h200-sxm", "preset": {"name": "1gpu-16vcpu-200gb"}}},
             "status": {"onDemand": {"available": "1", "availabilityLevel": "AVAILABILITY_LEVEL_LOW", "dataState": "DATA_STATE_FRESH"}}}
        ]))
        .unwrap();
        // Equal prices: the more available region first.
        let offers = offers_from(
            found.clone(),
            false,
            &on_demand,
            &HashMap::new(),
            Some(&advice),
        );
        let regions: Vec<&str> = offers.iter().map(|o| o.region.as_str()).collect();
        assert_eq!(regions, ["us-central1", "eu-north1"]);
        assert_eq!(offers[0].instance_type, "gpu-h200-sxm/1gpu-16vcpu-200gb");
        assert_eq!(offers[0].hourly_price, Some(3.5));
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.project_id, "project-e00central");

        // Fresh "limit reached" drops the offer; stale advice does not.
        let mut blocked = advice.clone();
        blocked[0].status.on_demand.availability_level = "AVAILABILITY_LEVEL_LIMIT_REACHED".into();
        let offers = offers_from(
            found.clone(),
            false,
            &on_demand,
            &HashMap::new(),
            Some(&blocked),
        );
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].region, "eu-north1");
        blocked[0].status.on_demand.data_state = "DATA_STATE_STALE".into();
        assert_eq!(
            offers_from(
                found.clone(),
                false,
                &on_demand,
                &HashMap::new(),
                Some(&blocked)
            )
            .len(),
            2
        );

        // Unpriced offers sort last; no advice keeps everything.
        let partial: BTreeMap<String, f64> = [(
            "gpu-h200-sxm/1gpu-16vcpu-200gb@us-central1".to_string(),
            4.0,
        )]
        .into();
        let offers = offers_from(found, false, &partial, &HashMap::new(), None);
        assert_eq!(offers[0].region, "us-central1");
        assert_eq!(offers[1].hourly_price, None);
    }

    #[test]
    fn spot_offers_use_gpu_hour_prices_times_gpus() {
        let mut request = gpus("H100", 8);
        request.spot = true;
        let found = candidates(&catalogs(), &request).unwrap();
        let prices: Vec<SpotPrice> = serde_json::from_value(json!([
            {"skuId": "sku-1", "productTitle": "Preemptible NVIDIA® H100 NVLink", "region": "eu-north1",
             "computeInstance": {"platform": "gpu-h100-sxm"},
             "price": {"value": "0.79", "currency": "usd", "unitTitle": "GPU hour"}}
        ]))
        .unwrap();
        let offers = offers_from(
            found,
            true,
            &BTreeMap::new(),
            &spot_price_index(&prices),
            None,
        );
        assert_eq!(offers.len(), 1);
        assert!((offers[0].hourly_price.unwrap() - 6.32).abs() < 1e-9);
    }
}
