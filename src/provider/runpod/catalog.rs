//! Planning for RunPod: live GPU stock → ordered offers.
//!
//! RunPod's stock changes minute to minute, so plans come from the live
//! catalog (`GET /v2/catalog/gpus?include=AVAILABILITY`) rather than a static
//! table. Each (GPU type × cloud tier × data center with stock) becomes one
//! offer:
//!
//! - `instance_type` = `"<gpu id> x<count> <secure|community>"`;
//! - `region` = the data center's continent (`north_america`, `europe`, …);
//! - `zone` = the data center id (`US-TX-3`), so `--zone` pins one;
//! - ordered cheapest first, ties broken by stock level.
//!
//! `--region` accepts continents (filtered here) and ISO country codes
//! (passed to the API as `countryCodes`); no regions means anywhere.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::provider::{GpuRequest, Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{Availability, Cloud, GpuType};

/// Launch details carried in `Offer::detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub gpu_id: String,
    pub count: u32,
    pub cloud: Cloud,
    pub data_center_id: String,
    pub memory_gb: u32,
}

/// The ask resolved into what the catalog is queried for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuAsk {
    /// GPU shorthand (`H100:8`), matched against type ids and names.
    Shorthand(GpuRequest),
    /// An exact RunPod GPU type id, one GPU.
    Exact(String),
}

impl GpuAsk {
    pub fn from_request(ask: &ResourceAsk) -> GpuAsk {
        match ask {
            ResourceAsk::Gpus(gpus) => GpuAsk::Shorthand(gpus.clone()),
            ResourceAsk::InstanceType(id) => GpuAsk::Exact(id.clone()),
        }
    }

    pub fn count(&self) -> u32 {
        match self {
            GpuAsk::Shorthand(gpus) => gpus.count,
            GpuAsk::Exact(_) => 1,
        }
    }

    pub fn matches(&self, gpu: &GpuType) -> bool {
        match self {
            GpuAsk::Shorthand(gpus) => gpu_name_matches(&gpus.name, gpu),
            GpuAsk::Exact(id) => gpu.id.eq_ignore_ascii_case(id),
        }
    }
}

/// Split `--region` values into ISO country codes (two uppercase letters, sent
/// to the API) and continent names (lowercased, filtered client-side).
pub fn split_regions(regions: &[String]) -> (Vec<String>, Vec<String>) {
    let mut countries = Vec::new();
    let mut continents = Vec::new();
    for region in regions {
        if region.len() == 2 && region.chars().all(|c| c.is_ascii_uppercase()) {
            countries.push(region.clone());
        } else {
            continents.push(region.to_lowercase());
        }
    }
    (countries, continents)
}

/// Uppercase tokens of a GPU id or name (split on space, `-`, `_`).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .collect()
}

/// Whether the user's GPU shorthand names this GPU type. Each user token must
/// equal a token of the type's id or display name, or two adjacent tokens
/// joined (`RTX4090` ≙ `RTX 4090`). Whole-token comparison keeps `A10` off
/// `A100`, `H200` off `GH200` and `L40` off `L40S`. An exact id/name match
/// always counts.
pub fn gpu_name_matches(user: &str, gpu: &GpuType) -> bool {
    if gpu.id.eq_ignore_ascii_case(user) || gpu.name.eq_ignore_ascii_case(user) {
        return true;
    }
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    [gpu.id.as_str(), gpu.name.as_str()].iter().any(|text| {
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

/// Build ordered offers from per-tier catalog responses.
pub fn offers_from(
    stock: &[(Cloud, Vec<GpuType>)],
    continents_by_dc: &HashMap<String, String>,
    request: &ResourceRequest,
) -> Result<Vec<Offer>, ProviderError> {
    let ask = GpuAsk::from_request(&request.ask);
    let count = ask.count();
    let (_, continents) = split_regions(&request.regions);
    let mut matched_any = false;
    let mut ranked: Vec<(f64, Availability, Offer)> = Vec::new();

    for (cloud, gpus) in stock {
        for gpu in gpus.iter().filter(|g| ask.matches(g)) {
            matched_any = true;
            if gpu.max_count(*cloud) < count {
                continue;
            }
            for dc in &gpu.data_centers {
                if dc.availability == Availability::None {
                    continue;
                }
                if request
                    .zone
                    .as_ref()
                    .is_some_and(|z| !z.eq_ignore_ascii_case(&dc.id))
                {
                    continue;
                }
                let continent = continents_by_dc
                    .get(&dc.id)
                    .map(|r| r.to_lowercase())
                    .unwrap_or_else(|| "unknown".to_string());
                if !continents.is_empty() && !continents.contains(&continent) {
                    continue;
                }
                let price = gpu.price(*cloud) * f64::from(count);
                let detail = OfferDetail {
                    gpu_id: gpu.id.clone(),
                    count,
                    cloud: *cloud,
                    data_center_id: dc.id.clone(),
                    memory_gb: gpu.memory,
                };
                let mut offer = Offer::new(
                    format!("{} x{count} {}", gpu.id, cloud.as_str().to_lowercase()),
                    continent,
                )
                .with_zone(Some(dc.id.clone()));
                offer.hourly_price = (price > 0.0).then_some(price);
                offer.detail = serde_json::to_value(&detail).expect("offer detail serializes");
                ranked.push((price, dc.availability, offer));
            }
        }
    }

    if !matched_any {
        let known: Vec<&str> = stock
            .iter()
            .flat_map(|(_, gpus)| gpus.iter().map(|g| g.id.as_str()))
            .take(8)
            .collect();
        return Err(ProviderError::invalid(format!(
            "no RunPod GPU type matches {}; known types include: {}",
            match &ask {
                GpuAsk::Shorthand(g) => format!("'{g}'"),
                GpuAsk::Exact(id) => format!("instance type '{id}'"),
            },
            known.join(", ")
        )));
    }

    // Cheapest first; among equal prices, the better-stocked data center.
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    Ok(ranked.into_iter().map(|(_, _, offer)| offer).collect())
}

#[cfg(test)]
mod tests {
    use super::super::api::{DataCenterStock, TierMaxCount, TierPrice};
    use super::*;

    fn gpu(id: &str, name: &str, secure: f64, dcs: &[(&str, Availability)]) -> GpuType {
        GpuType {
            id: id.into(),
            name: name.into(),
            memory: 80,
            price: TierPrice {
                secure,
                community: secure / 2.0,
            },
            max_count: TierMaxCount {
                secure: 8,
                community: 2,
            },
            data_centers: dcs
                .iter()
                .map(|(id, availability)| DataCenterStock {
                    id: id.to_string(),
                    availability: *availability,
                })
                .collect(),
        }
    }

    fn named(id: &str, name: &str) -> GpuType {
        gpu(id, name, 1.0, &[])
    }

    #[test]
    fn gpu_names_match_on_whole_tokens() {
        let cases = [
            ("H100", "NVIDIA H100 80GB HBM3", "H100 SXM", true),
            ("H100", "NVIDIA H100 PCIe", "H100 PCIe", true),
            ("H100", "NVIDIA H100 NVL", "H100 NVL", true),
            ("A100-80GB", "NVIDIA A100 80GB PCIe", "A100 PCIe", true),
            ("A100-80GB", "NVIDIA A100-SXM4-80GB", "A100 SXM", true),
            ("RTX4090", "NVIDIA GeForce RTX 4090", "RTX 4090", true),
            (
                "RTX6000-Ada",
                "NVIDIA RTX 6000 Ada Generation",
                "RTX 6000 Ada",
                true,
            ),
            ("L40S", "NVIDIA L40S", "L40S", true),
            // Negative cases: no prefix matches.
            ("A10", "NVIDIA A100 80GB PCIe", "A100 PCIe", false),
            ("H200", "NVIDIA GH200 480GB", "GH200", false),
            ("L40", "NVIDIA L40S", "L40S", false),
            ("A100-40GB", "NVIDIA A100 80GB PCIe", "A100 PCIe", false),
        ];
        for (user, id, name, expected) in cases {
            assert_eq!(
                gpu_name_matches(user, &named(id, name)),
                expected,
                "{user} vs {id}"
            );
        }
        // Exact id or display name, case-insensitively.
        assert!(gpu_name_matches(
            "nvidia h100 nvl",
            &named("NVIDIA H100 NVL", "H100 NVL")
        ));
        assert!(gpu_name_matches(
            "h100 sxm",
            &named("NVIDIA H100 80GB HBM3", "H100 SXM")
        ));
    }

    #[test]
    fn regions_split_into_countries_and_continents() {
        let (countries, continents) = split_regions(&[
            "US".into(),
            "north_america".into(),
            "EUROPE".into(),
            "ca".into(),
        ]);
        assert_eq!(countries, vec!["US"]);
        // Lowercase two-letter values are continents-or-typos, not countries.
        assert_eq!(continents, vec!["north_america", "europe", "ca"]);
    }

    fn continents() -> HashMap<String, String> {
        [
            ("US-TX-3", "NORTH_AMERICA"),
            ("CA-MTL-1", "NORTH_AMERICA"),
            ("EU-RO-1", "EUROPE"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    fn request(ask: ResourceAsk, regions: &[&str], zone: Option<&str>) -> ResourceRequest {
        ResourceRequest {
            ask,
            regions: regions.iter().map(|r| r.to_string()).collect(),
            zone: zone.map(str::to_string),
            spot: false,
        }
    }

    fn stock() -> Vec<(Cloud, Vec<GpuType>)> {
        let h100_sxm = gpu(
            "NVIDIA H100 80GB HBM3",
            "H100 SXM",
            2.99,
            &[
                ("US-TX-3", Availability::High),
                ("EU-RO-1", Availability::Low),
                ("CA-MTL-1", Availability::None),
            ],
        );
        let h100_pcie = gpu(
            "NVIDIA H100 PCIe",
            "H100 PCIe",
            1.99,
            &[("CA-MTL-1", Availability::Medium)],
        );
        let a40 = gpu("NVIDIA A40", "A40", 0.4, &[("US-TX-3", Availability::High)]);
        vec![
            (
                Cloud::Secure,
                vec![h100_sxm.clone(), h100_pcie.clone(), a40.clone()],
            ),
            (Cloud::Community, vec![h100_sxm, h100_pcie, a40]),
        ]
    }

    #[test]
    fn offers_cover_tiers_and_stocked_dcs_cheapest_first() {
        let req = request(ResourceAsk::Gpus("H100:1".parse().unwrap()), &[], None);
        let offers = offers_from(&stock(), &continents(), &req).unwrap();
        let rendered: Vec<String> = offers.iter().map(|o| o.to_string()).collect();
        assert_eq!(
            rendered,
            vec![
                "NVIDIA H100 PCIe x1 community@north_america/CA-MTL-1 ($0.99/h)",
                "NVIDIA H100 80GB HBM3 x1 community@north_america/US-TX-3 ($1.50/h)",
                "NVIDIA H100 80GB HBM3 x1 community@europe/EU-RO-1 ($1.50/h)",
                "NVIDIA H100 PCIe x1 secure@north_america/CA-MTL-1 ($1.99/h)",
                "NVIDIA H100 80GB HBM3 x1 secure@north_america/US-TX-3 ($2.99/h)",
                "NVIDIA H100 80GB HBM3 x1 secure@europe/EU-RO-1 ($2.99/h)",
            ],
            "sold-out CA-MTL-1 for the SXM is skipped; equal prices prefer HIGH stock"
        );
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(
            detail,
            OfferDetail {
                gpu_id: "NVIDIA H100 PCIe".into(),
                count: 1,
                cloud: Cloud::Community,
                data_center_id: "CA-MTL-1".into(),
                memory_gb: 80,
            }
        );
    }

    #[test]
    fn continent_zone_and_max_count_filters() {
        let europe = request(
            ResourceAsk::Gpus("H100:1".parse().unwrap()),
            &["europe"],
            None,
        );
        let offers = offers_from(&stock(), &continents(), &europe).unwrap();
        assert!(offers.iter().all(|o| o.region == "europe"));
        assert_eq!(offers.len(), 2);

        let pinned = request(
            ResourceAsk::Gpus("H100:1".parse().unwrap()),
            &[],
            Some("us-tx-3"),
        );
        let offers = offers_from(&stock(), &continents(), &pinned).unwrap();
        assert!(offers.iter().all(|o| o.zone.as_deref() == Some("US-TX-3")));

        // Community tier tops out at 2 GPUs per pod in this catalog.
        let eight = request(ResourceAsk::Gpus("H100:8".parse().unwrap()), &[], None);
        let offers = offers_from(&stock(), &continents(), &eight).unwrap();
        assert!(!offers.is_empty());
        assert!(offers
            .iter()
            .all(|o| o.instance_type.ends_with("x8 secure")));
        assert_eq!(offers[0].hourly_price, Some(1.99 * 8.0));
    }

    #[test]
    fn exact_instance_type_and_unknown_gpu() {
        let exact = request(ResourceAsk::InstanceType("nvidia a40".into()), &[], None);
        let offers = offers_from(&stock(), &continents(), &exact).unwrap();
        assert_eq!(offers.len(), 2);
        assert!(offers
            .iter()
            .all(|o| o.instance_type.starts_with("NVIDIA A40 x1")));

        let unknown = request(ResourceAsk::Gpus("B999:1".parse().unwrap()), &[], None);
        let err = offers_from(&stock(), &continents(), &unknown).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("NVIDIA A40"), "{}", err.msg);

        // A known GPU that is simply sold out everywhere: no offers, no error.
        let sold_out = vec![(
            Cloud::Secure,
            vec![gpu(
                "NVIDIA B200",
                "B200",
                5.0,
                &[("US-TX-3", Availability::None)],
            )],
        )];
        let b200 = request(ResourceAsk::Gpus("B200:1".parse().unwrap()), &[], None);
        assert!(offers_from(&sold_out, &continents(), &b200)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_data_centers_land_in_an_unknown_region() {
        let stock = vec![(
            Cloud::Secure,
            vec![gpu(
                "NVIDIA A40",
                "A40",
                0.4,
                &[("XX-NEW-1", Availability::Low)],
            )],
        )];
        let req = request(ResourceAsk::Gpus("A40:1".parse().unwrap()), &[], None);
        let offers = offers_from(&stock, &continents(), &req).unwrap();
        assert_eq!(offers[0].region, "unknown");
    }
}
