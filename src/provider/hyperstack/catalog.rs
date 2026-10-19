//! Planning against live Hyperstack data. An offer is emitted only when
//! three sources agree: the **flavor** exists in the region and reports
//! `stock_available`, the **stock** feed (`GET /core/stocks`) has a free
//! configuration of that GPU count (when it lists the model), and the
//! **pricebook** prices it. One offer per flavor × region, cheapest first.
//! Region = Hyperstack region (`CANADA-1`, `US-1`, `NORWAY-1`); no zones.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{Flavor, PriceEntry, RegionStock};

/// Regions searched when the request names none: the North American ones.
/// (`NORWAY-1` is opt-in via `--region NORWAY-1`.)
pub const DEFAULT_REGIONS: &[&str] = &["CANADA-1", "US-1"];

/// What a Hyperstack offer carries from plan to launch (`Offer::detail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub flavor_name: String,
    pub region: String,
    /// GPU model as named by the flavor/pricebook (empty for CPU-only).
    pub gpu: String,
    pub gpu_count: u32,
    pub spot: bool,
    /// Images the flavor is hard-restricted to (empty = any).
    pub hard_images: Vec<String>,
}

/// Uppercase tokens of a GPU name, split on space, `-`, `_`; `80GB` and
/// `80G` normalize to the same token, and a `SPOT` marker is dropped.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .filter(|t| t != "SPOT")
        .map(|t| match t.strip_suffix("GB") {
            Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => format!("{n}G"),
            _ => t,
        })
        .collect()
}

/// Whether the user's GPU shorthand names this flavor's GPU. Each user
/// token must equal a GPU token or two adjacent tokens joined
/// (`RTXA6000` ≙ `RTX-A6000`); whole tokens keep `A10` off `A100` and
/// `L40` off `L40S`. A bare model also matches its `RTX-` form (`A6000`).
pub fn gpu_name_matches(user: &str, flavor: &Flavor) -> bool {
    let names: Vec<&str> = [
        flavor.gpu.as_str(),
        flavor.display_name.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|n| !n.is_empty())
    .collect();
    if names.iter().any(|n| n.eq_ignore_ascii_case(user)) {
        return true;
    }
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    names.iter().any(|text| {
        let have = tokens(text);
        let joined: Vec<String> = have
            .windows(2)
            .map(|w| format!("{}{}", w[0], w[1]))
            .collect();
        wanted
            .iter()
            .all(|w| have.contains(w) || joined.contains(w))
    })
}

/// Spot flavors carry `spot` in their name (e.g. `A100-80G-PCIe-spot`).
pub fn is_spot(flavor: &Flavor) -> bool {
    let has = |s: &str| s.to_lowercase().contains("spot");
    has(&flavor.name) || has(&flavor.gpu) || flavor.labels.iter().any(|l| has(&l.label))
}

fn matches_ask(ask: &ResourceAsk, flavor: &Flavor) -> bool {
    match ask {
        ResourceAsk::InstanceType(wanted) => flavor.name.eq_ignore_ascii_case(wanted),
        ResourceAsk::Gpus(gpus) => {
            flavor.gpu_count == gpus.count && gpu_name_matches(&gpus.name, flavor)
        }
    }
}

/// Regions the request allows: its own list (case-insensitive), else
/// [`DEFAULT_REGIONS`]. A zone pin names a region too (Hyperstack has no
/// zones).
pub fn region_allowed(region: &str, request: &ResourceRequest) -> bool {
    if let Some(zone) = &request.zone {
        if !zone.eq_ignore_ascii_case(region) {
            return false;
        }
    }
    if request.regions.is_empty() {
        DEFAULT_REGIONS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(region))
    } else {
        request
            .regions
            .iter()
            .any(|r| r.eq_ignore_ascii_case(region))
    }
}

/// Canonical form for comparing GPU model names across feeds.
fn model_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase()
        .trim_end_matches("SPOT")
        .to_string()
}

/// The stock feed's verdict for this flavor: `Some(n)` free configurations
/// of its GPU count, or `None` when the feed does not list the model there.
pub fn stock_count(flavor: &Flavor, stocks: &[RegionStock]) -> Option<i64> {
    if flavor.gpu.is_empty() {
        return None;
    }
    let region = stocks
        .iter()
        .find(|s| s.region.eq_ignore_ascii_case(&flavor.region_name))?;
    let wanted = model_key(&flavor.gpu);
    let model = region.models.iter().find(|m| {
        let have = model_key(&m.model);
        !have.is_empty() && (have == wanted || have.ends_with(&wanted) || wanted.ends_with(&have))
    })?;
    model
        .configurations
        .get(&format!("{}x", flavor.gpu_count))
        .copied()
        .or(Some(0))
}

/// Whether the flavor can be launched now: `stock_available` must not be
/// false, and the stock feed (when it lists the model) must have a free
/// configuration of this GPU count.
pub fn in_stock(flavor: &Flavor, stocks: &[RegionStock]) -> bool {
    if flavor.stock_available == Some(false) {
        return false;
    }
    match stock_count(flavor, stocks) {
        Some(n) => n > 0,
        None => flavor.stock_available.unwrap_or(false) || flavor.gpu.is_empty(),
    }
}

/// Hourly USD price of a flavor: per-GPU rate × GPU count for GPU flavors
/// (spot GPUs are priced under `<gpu>-spot`); for CPU-only flavors the sum
/// of the `(cpu-only-flavors)` vCPU, RAM and storage rates.
pub fn hourly_price(flavor: &Flavor, prices: &HashMap<String, f64>) -> Option<f64> {
    let rate = |name: &str| prices.get(&name.to_lowercase()).copied();
    if flavor.gpu_count == 0 || flavor.gpu.is_empty() {
        let cpu = rate("vCPU (cpu-only-flavors)")?;
        let ram = rate("RAM (cpu-only-flavors)")?;
        let disk = rate("hypervisor-local-storage (cpu-only-flavors)").unwrap_or(0.0);
        return Some(cpu * flavor.cpu as f64 + ram * flavor.ram + disk * flavor.disk as f64);
    }
    let per_gpu = if is_spot(flavor) && !flavor.gpu.to_lowercase().ends_with("-spot") {
        rate(&format!("{}-spot", flavor.gpu)).or_else(|| rate(&flavor.gpu))
    } else {
        rate(&flavor.gpu)
    }?;
    Some(per_gpu * flavor.gpu_count as f64)
}

/// Pricebook rows keyed by lowercase name.
pub fn price_index(entries: &[PriceEntry]) -> HashMap<String, f64> {
    entries
        .iter()
        .map(|e| (e.name.to_lowercase(), e.value))
        .collect()
}

/// Build the ordered offers for `request`.
pub fn offers_from(
    flavors: &[Flavor],
    stocks: &[RegionStock],
    prices: &HashMap<String, f64>,
    request: &ResourceRequest,
) -> Result<Vec<Offer>, ProviderError> {
    let matching: Vec<&Flavor> = flavors
        .iter()
        .filter(|f| matches_ask(&request.ask, f))
        .filter(|f| is_spot(f) == request.spot)
        .collect();
    if matching.is_empty() {
        let mut known: Vec<String> = flavors
            .iter()
            .filter(|f| f.gpu_count > 0)
            .map(|f| format!("{}:{}", f.gpu, f.gpu_count))
            .collect();
        known.sort();
        known.dedup();
        known.truncate(12);
        return Err(ProviderError::invalid(format!(
            "no Hyperstack flavor matches {}{} (known: {})",
            describe(&request.ask),
            if request.spot { " (spot)" } else { "" },
            known.join(", ")
        )));
    }
    let mut offers: Vec<Offer> = matching
        .into_iter()
        .filter(|f| region_allowed(&f.region_name, request))
        .filter(|f| in_stock(f, stocks))
        .map(|f| {
            let detail = OfferDetail {
                flavor_name: f.name.clone(),
                region: f.region_name.clone(),
                gpu: f.gpu.clone(),
                gpu_count: f.gpu_count,
                spot: is_spot(f),
                hard_images: f.hard_images(),
            };
            let mut offer = Offer::new(&f.name, &f.region_name);
            offer.hourly_price = hourly_price(f, prices);
            offer.detail = serde_json::to_value(detail).expect("offer detail serializes");
            offer
        })
        .collect();
    offers.sort_by(|a, b| {
        match (a.hourly_price, b.hourly_price) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        .then_with(|| a.region.cmp(&b.region))
        .then_with(|| a.instance_type.cmp(&b.instance_type))
    });
    Ok(offers)
}

fn describe(ask: &ResourceAsk) -> String {
    match ask {
        ResourceAsk::InstanceType(t) => format!("flavor '{t}'"),
        ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
    }
}

/// A `major.minor` version, comparable as a tuple.
type Version = (u32, u32);

/// Parse `Ubuntu Server 24.04 LTS R570 CUDA 12.8 …` into
/// `(ubuntu version, cuda version)`.
fn image_rank(name: &str) -> Option<(Version, Option<Version>)> {
    let words: Vec<&str> = name.split_whitespace().collect();
    if !words.first()?.eq_ignore_ascii_case("ubuntu") {
        return None;
    }
    let version = |s: &str| -> Option<Version> {
        let (a, b) = s.split_once('.')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    let ubuntu = words.iter().find_map(|w| version(w))?;
    let cuda = words
        .windows(2)
        .find(|w| w[0].eq_ignore_ascii_case("cuda"))
        .and_then(|w| version(w[1]));
    Some((ubuntu, cuda))
}

/// Pick the boot image: the flavor's hard restriction if any; else for GPU
/// flavors the newest Ubuntu with the newest CUDA (preferring the variant
/// without Docker on a tie — k3s brings its own runtime); for CPU-only
/// flavors the newest plain Ubuntu.
pub fn choose_image(available: &[String], detail: &OfferDetail) -> Option<String> {
    if let Some(first) = detail.hard_images.first() {
        return Some(
            detail
                .hard_images
                .iter()
                .find(|i| available.contains(i))
                .unwrap_or(first)
                .clone(),
        );
    }
    let gpu = detail.gpu_count > 0;
    available
        .iter()
        .filter_map(|name| image_rank(name).map(|rank| (name, rank)))
        .filter(|(_, (_, cuda))| cuda.is_some() == gpu)
        .max_by(|(a, ra), (b, rb)| {
            ra.cmp(rb)
                .then_with(|| docker_last(b).cmp(&docker_last(a)))
                .then_with(|| b.cmp(a))
        })
        .map(|(name, _)| name.clone())
}

fn docker_last(name: &str) -> bool {
    name.to_lowercase().contains("docker")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::GpuRequest;
    use crate::provider::ResourceAsk;

    fn flavor(name: &str, gpu: &str, count: u32, region: &str, stock: Option<bool>) -> Flavor {
        serde_json::from_value(serde_json::json!({
            "name": name, "gpu": gpu, "gpu_count": count, "region_name": region,
            "cpu": 8, "ram": 32.0, "disk": 100, "stock_available": stock
        }))
        .unwrap()
    }

    fn request(gpu: &str, count: u32, regions: &[&str], spot: bool) -> ResourceRequest {
        ResourceRequest {
            ask: ResourceAsk::Gpus(GpuRequest {
                name: gpu.to_uppercase(),
                count,
            }),
            regions: regions.iter().map(|r| r.to_string()).collect(),
            zone: None,
            spot,
        }
    }

    fn stocks(region: &str, model: &str, one_x: i64) -> Vec<RegionStock> {
        serde_json::from_value(serde_json::json!([
            {"region": region, "models": [{"model": model, "configurations": {"1x": one_x, "8x": 0}}]}
        ]))
        .unwrap()
    }

    #[test]
    fn gpu_names_match_on_whole_tokens() {
        let a6000 = flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "CANADA-1", Some(true));
        assert!(gpu_name_matches("A6000", &a6000));
        assert!(gpu_name_matches("RTXA6000", &a6000));
        assert!(gpu_name_matches("rtx-a6000", &a6000));
        let a100 = flavor("n3-A100x1", "A100-80G-PCIe", 1, "CANADA-1", Some(true));
        assert!(gpu_name_matches("A100", &a100));
        assert!(gpu_name_matches("A100-80GB", &a100));
        assert!(!gpu_name_matches("A10", &a100));
        let l40 = flavor("n3-L40x1", "L40", 1, "CANADA-1", Some(true));
        assert!(gpu_name_matches("L40", &l40));
        assert!(!gpu_name_matches("L40S", &l40));
        let l40s = flavor("n3-L40Sx1", "L40S", 1, "CANADA-1", Some(true));
        assert!(!gpu_name_matches("L40", &l40s));
        let pro = flavor(
            "n3-RTX-PRO-6000x1",
            "RTX-PRO-6000",
            1,
            "CANADA-1",
            Some(true),
        );
        assert!(gpu_name_matches("RTX-PRO-6000", &pro));
        assert!(!gpu_name_matches("A6000", &pro));
    }

    #[test]
    fn regions_default_to_north_america() {
        let any = request("A6000", 1, &[], false);
        assert!(region_allowed("CANADA-1", &any));
        assert!(region_allowed("US-1", &any));
        assert!(!region_allowed("NORWAY-1", &any));
        let norway = request("A6000", 1, &["norway-1"], false);
        assert!(region_allowed("NORWAY-1", &norway));
        let mut pinned = request("A6000", 1, &["CANADA-1", "US-1"], false);
        pinned.zone = Some("US-1".into());
        assert!(!region_allowed("CANADA-1", &pinned));
        assert!(region_allowed("US-1", &pinned));
    }

    #[test]
    fn stock_join_requires_both_signals() {
        let f = flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "CANADA-1", Some(true));
        assert!(in_stock(&f, &stocks("CANADA-1", "RTX-A6000", 3)));
        // The flavor says yes but the stock feed has no 1x left.
        assert!(!in_stock(&f, &stocks("CANADA-1", "RTX-A6000", 0)));
        // The feed matches models loosely across naming styles.
        assert!(in_stock(&f, &stocks("CANADA-1", "A6000", 2)));
        // Not listed in the feed: trust the flavor's flag.
        assert!(in_stock(&f, &[]));
        let sold = flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "CANADA-1", Some(false));
        assert!(!in_stock(&sold, &stocks("CANADA-1", "RTX-A6000", 3)));
        let unknown = flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "CANADA-1", None);
        assert!(!in_stock(&unknown, &[]));
    }

    #[test]
    fn prices_are_per_gpu_times_count_and_spot_suffixed() {
        let prices = price_index(&[
            PriceEntry {
                name: "RTX-A6000".into(),
                value: 0.5,
            },
            PriceEntry {
                name: "A100-80G-PCIe-spot".into(),
                value: 0.9,
            },
            PriceEntry {
                name: "vCPU (cpu-only-flavors)".into(),
                value: 0.01,
            },
            PriceEntry {
                name: "RAM (cpu-only-flavors)".into(),
                value: 0.002,
            },
        ]);
        let two = flavor("n3-RTX-A6000x2", "RTX-A6000", 2, "CANADA-1", Some(true));
        assert_eq!(hourly_price(&two, &prices), Some(1.0));
        let spot = flavor("n3-A100x1-spot", "A100-80G-PCIe", 1, "CANADA-1", Some(true));
        assert_eq!(hourly_price(&spot, &prices), Some(0.9));
        let cpu = flavor("n1-cpu-small", "", 0, "CANADA-1", Some(true));
        assert!((hourly_price(&cpu, &prices).unwrap() - (0.08 + 0.064)).abs() < 1e-9);
        let unpriced = flavor("n3-L40x1", "L40", 1, "CANADA-1", Some(true));
        assert_eq!(hourly_price(&unpriced, &prices), None);
    }

    #[test]
    fn offers_join_filter_and_sort() {
        let flavors = vec![
            flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "CANADA-1", Some(true)),
            flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "US-1", Some(true)),
            flavor("n3-RTX-A6000x1", "RTX-A6000", 1, "NORWAY-1", Some(true)),
            flavor("n3-RTX-A6000x2", "RTX-A6000", 2, "CANADA-1", Some(true)),
            flavor(
                "n3-RTX-A6000x1-spot",
                "RTX-A6000",
                1,
                "CANADA-1",
                Some(true),
            ),
        ];
        let stock: Vec<RegionStock> = serde_json::from_value(serde_json::json!([
            {"region": "CANADA-1", "models": [{"model": "RTX-A6000", "configurations": {"1x": 4}}]},
            {"region": "US-1", "models": [{"model": "RTX-A6000", "configurations": {"1x": 0}}]},
        ]))
        .unwrap();
        let prices = price_index(&[
            PriceEntry {
                name: "RTX-A6000".into(),
                value: 0.5,
            },
            PriceEntry {
                name: "RTX-A6000-spot".into(),
                value: 0.2,
            },
        ]);
        let offers =
            offers_from(&flavors, &stock, &prices, &request("A6000", 1, &[], false)).unwrap();
        // Norway excluded by default, US-1 sold out, x2 and spot don't match.
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].region, "CANADA-1");
        assert_eq!(offers[0].hourly_price, Some(0.5));
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.flavor_name, "n3-RTX-A6000x1");
        assert!(!detail.spot);

        let spot = offers_from(&flavors, &stock, &prices, &request("A6000", 1, &[], true)).unwrap();
        assert_eq!(spot.len(), 1);
        assert_eq!(spot[0].instance_type, "n3-RTX-A6000x1-spot");
        assert_eq!(spot[0].hourly_price, Some(0.2));

        let err =
            offers_from(&flavors, &stock, &prices, &request("H100", 8, &[], false)).unwrap_err();
        assert!(err.msg.contains("RTX-A6000:1"), "{}", err.msg);
    }

    #[test]
    fn exact_flavor_name_is_an_instance_type_ask() {
        let flavors = vec![flavor(
            "n3-RTX-A6000x1",
            "RTX-A6000",
            1,
            "CANADA-1",
            Some(true),
        )];
        let mut req = request("x", 1, &[], false);
        req.ask = ResourceAsk::InstanceType("N3-RTX-A6000X1".into());
        let offers = offers_from(&flavors, &[], &HashMap::new(), &req).unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].hourly_price, None);
    }

    #[test]
    fn image_choice_prefers_newest_cuda_without_docker() {
        let images: Vec<String> = [
            "Ubuntu Server 22.04 LTS R535 CUDA 12.2",
            "Ubuntu Server 24.04 LTS R570 CUDA 12.8",
            "Ubuntu Server 24.04 LTS R570 CUDA 12.8 with Docker",
            "Ubuntu Server 22.04 LTS (Jammy Jellyfish)",
            "Ubuntu Server 24.04 LTS (Noble Numbat)",
            "Debian 12 Plain",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let gpu = OfferDetail {
            flavor_name: "f".into(),
            region: "CANADA-1".into(),
            gpu: "RTX-A6000".into(),
            gpu_count: 1,
            spot: false,
            hard_images: Vec::new(),
        };
        assert_eq!(
            choose_image(&images, &gpu).as_deref(),
            Some("Ubuntu Server 24.04 LTS R570 CUDA 12.8")
        );
        let cpu = OfferDetail {
            gpu_count: 0,
            gpu: String::new(),
            ..gpu.clone()
        };
        assert_eq!(
            choose_image(&images, &cpu).as_deref(),
            Some("Ubuntu Server 24.04 LTS (Noble Numbat)")
        );
        let restricted = OfferDetail {
            hard_images: vec!["Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker".into()],
            ..gpu
        };
        assert_eq!(
            choose_image(&images, &restricted).as_deref(),
            Some("Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker")
        );
        // A newer CUDA beats an older one.
        let newer: Vec<String> = vec![
            "Ubuntu Server 24.04 LTS R570 CUDA 12.8".into(),
            "Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker".into(),
        ];
        assert_eq!(
            choose_image(
                &newer,
                &OfferDetail {
                    hard_images: Vec::new(),
                    ..restricted.clone()
                }
            )
            .as_deref(),
            Some("Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker")
        );
    }
}
