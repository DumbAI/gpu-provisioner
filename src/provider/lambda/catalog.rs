//! Planning against live Lambda data: `GET /api/v1/instance-types` returns
//! every type with its price and the regions that can launch it right now,
//! so one call yields one offer per type × region with capacity, cheapest
//! first. Region = Lambda region (`us-east-1`); there are no zones.
//!
//! Without an explicit region filter, only US regions (`us-*`) are
//! considered: Lambda serves here as a US allocation option, and launching
//! across an ocean by default would surprise. `--region` overrides that.

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{InstanceType, InstanceTypeEntry};

/// Region prefix used when the request names no region.
pub const DEFAULT_REGION_PREFIX: &str = "us-";

/// What a Lambda offer carries from plan to launch (`Offer::detail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub instance_type: String,
    pub region: String,
}

/// Uppercase tokens of a GPU name (split on space, `-`, `_`, parentheses).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '(' | ')' | ','))
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .collect()
}

/// Whether the user's GPU shorthand names this type's GPU. Each user token
/// must equal a token of the GPU description (`A100 (80 GB SXM4)`), the
/// type description or the type name — or two adjacent tokens joined
/// (`80GB` ≙ `80 GB`). Whole-token comparison keeps `A10` off `A100`,
/// `H200` off `GH200`, and `A6000` matches `RTX A6000`.
pub fn gpu_name_matches(user: &str, ty: &InstanceType) -> bool {
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    [
        ty.gpu_description.as_str(),
        ty.description.as_str(),
        ty.name.as_str(),
    ]
    .iter()
    .filter(|text| !text.is_empty())
    .any(|text| {
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

fn matches_ask(ask: &ResourceAsk, ty: &InstanceType) -> bool {
    match ask {
        ResourceAsk::InstanceType(wanted) => ty.name.eq_ignore_ascii_case(wanted),
        ResourceAsk::Gpus(gpus) => ty.specs.gpus == gpus.count && gpu_name_matches(&gpus.name, ty),
    }
}

/// Regions allowed by the request: explicit regions (case-insensitive), a
/// zone pin (Lambda has no zones, so it pins a region), or by default the
/// US regions.
fn region_allowed(region: &str, request: &ResourceRequest) -> bool {
    if let Some(pin) = &request.zone {
        if !pin.eq_ignore_ascii_case(region) {
            return false;
        }
    }
    if request.regions.is_empty() {
        return request.zone.is_some() || region.starts_with(DEFAULT_REGION_PREFIX);
    }
    request
        .regions
        .iter()
        .any(|r| r.eq_ignore_ascii_case(region))
}

/// Build ordered offers from the instance-type catalog.
pub fn offers_from(
    entries: &[InstanceTypeEntry],
    request: &ResourceRequest,
) -> Result<Vec<Offer>, ProviderError> {
    let matching: Vec<&InstanceTypeEntry> = entries
        .iter()
        .filter(|e| matches_ask(&request.ask, &e.instance_type))
        .collect();
    if matching.is_empty() {
        let mut known: Vec<String> = entries
            .iter()
            .filter(|e| e.instance_type.specs.gpus > 0)
            .map(|e| {
                format!(
                    "{} ({})",
                    e.instance_type.name, e.instance_type.gpu_description
                )
            })
            .collect();
        known.sort();
        known.truncate(12);
        let ask = match &request.ask {
            ResourceAsk::InstanceType(t) => format!("instance type '{t}'"),
            ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
        };
        return Err(ProviderError::invalid(format!(
            "no Lambda instance type matches {ask}; known types include: {}",
            known.join(", ")
        )));
    }
    let mut offers = Vec::new();
    for entry in matching {
        let ty = &entry.instance_type;
        for region in &entry.regions_with_capacity_available {
            if !region_allowed(&region.name, request) {
                continue;
            }
            let detail = OfferDetail {
                instance_type: ty.name.clone(),
                region: region.name.clone(),
            };
            let mut offer = Offer::new(&ty.name, &region.name);
            offer.hourly_price =
                (ty.price_cents_per_hour > 0).then(|| ty.price_cents_per_hour as f64 / 100.0);
            offer.detail = serde_json::to_value(&detail).expect("detail serializes");
            offers.push(offer);
        }
    }
    offers.sort_by(|a, b| {
        let price = |o: &Offer| o.hourly_price.unwrap_or(f64::INFINITY);
        price(a)
            .total_cmp(&price(b))
            .then_with(|| a.region.cmp(&b.region))
            .then_with(|| a.instance_type.cmp(&b.instance_type))
    });
    Ok(offers)
}

/// Whether `instance_type` can launch in `region` per a fresh catalog.
pub fn has_capacity(entries: &[InstanceTypeEntry], instance_type: &str, region: &str) -> bool {
    entries.iter().any(|e| {
        e.instance_type.name == instance_type
            && e.regions_with_capacity_available
                .iter()
                .any(|r| r.name == region)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::GpuRequest;

    fn entry(name: &str, gpu: &str, gpus: u32, cents: u64, regions: &[&str]) -> InstanceTypeEntry {
        serde_json::from_value(serde_json::json!({
            "instance_type": {
                "name": name,
                "description": format!("{gpus}x {gpu}"),
                "gpu_description": gpu,
                "price_cents_per_hour": cents,
                "specs": {"vcpus": 30, "memory_gib": 200, "storage_gib": 1400, "gpus": gpus},
                "architecture": "x86_64"
            },
            "regions_with_capacity_available": regions
                .iter()
                .map(|r| serde_json::json!({"name": r, "description": r}))
                .collect::<Vec<_>>()
        }))
        .unwrap()
    }

    fn catalog() -> Vec<InstanceTypeEntry> {
        vec![
            entry(
                "gpu_1x_a10",
                "A10 (24 GB PCIe)",
                1,
                129,
                &["us-east-1", "us-west-1"],
            ),
            entry("gpu_1x_a100", "A100 (40 GB SXM4)", 1, 179, &["us-east-1"]),
            entry(
                "gpu_1x_a100_sxm4_80gb",
                "A100 (80 GB SXM4)",
                1,
                229,
                &["us-midwest-1"],
            ),
            entry(
                "gpu_1x_a6000",
                "RTX A6000 (48 GB)",
                1,
                109,
                &["us-south-1", "europe-central-1"],
            ),
            entry(
                "gpu_1x_h100_pcie",
                "H100 (80 GB PCIe)",
                1,
                329,
                &["us-west-3"],
            ),
            entry(
                "gpu_8x_h100_sxm5",
                "H100 (80 GB SXM5)",
                8,
                2392,
                &["us-east-1"],
            ),
            entry("gpu_1x_gh200", "GH200 (96 GB)", 1, 149, &["us-east-3"]),
            entry("cpu_4x_general", "", 0, 20, &["us-east-1"]),
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

    fn names(offers: &[Offer]) -> Vec<String> {
        offers
            .iter()
            .map(|o| format!("{}@{}", o.instance_type, o.region))
            .collect()
    }

    #[test]
    fn gpu_matching_uses_whole_tokens() {
        let types = catalog();
        let ty = |n: &str| {
            &types
                .iter()
                .find(|e| e.instance_type.name == n)
                .unwrap()
                .instance_type
        };
        assert!(gpu_name_matches("A10", ty("gpu_1x_a10")));
        assert!(!gpu_name_matches("A10", ty("gpu_1x_a100")));
        assert!(gpu_name_matches("A100", ty("gpu_1x_a100")));
        assert!(gpu_name_matches("A100-80GB", ty("gpu_1x_a100_sxm4_80gb")));
        assert!(!gpu_name_matches("A100-80GB", ty("gpu_1x_a100")));
        assert!(gpu_name_matches("A6000", ty("gpu_1x_a6000")));
        assert!(gpu_name_matches("RTXA6000", ty("gpu_1x_a6000")));
        assert!(gpu_name_matches("H100", ty("gpu_1x_h100_pcie")));
        assert!(gpu_name_matches("H100", ty("gpu_8x_h100_sxm5")));
        assert!(!gpu_name_matches("H200", ty("gpu_1x_gh200")));
        assert!(!gpu_name_matches("H100", ty("gpu_1x_gh200")));
    }

    #[test]
    fn offers_are_per_region_with_capacity_cheapest_first_us_only_by_default() {
        let offers = offers_from(&catalog(), &gpus("A10", 1)).unwrap();
        assert_eq!(
            names(&offers),
            ["gpu_1x_a10@us-east-1", "gpu_1x_a10@us-west-1"]
        );
        assert_eq!(offers[0].hourly_price, Some(1.29));

        // A6000 has capacity in us-south-1 and europe-central-1: Europe is
        // dropped by default...
        let offers = offers_from(&catalog(), &gpus("A6000", 1)).unwrap();
        assert_eq!(names(&offers), ["gpu_1x_a6000@us-south-1"]);
        // ...and kept when asked for.
        let mut request = gpus("A6000", 1);
        request.regions = vec!["europe-central-1".into()];
        let offers = offers_from(&catalog(), &request).unwrap();
        assert_eq!(names(&offers), ["gpu_1x_a6000@europe-central-1"]);
    }

    #[test]
    fn gpu_count_and_ladder_ordering() {
        // H100:1 → only the single-GPU PCIe type; H100:8 → the SXM5 box.
        let one = offers_from(&catalog(), &gpus("H100", 1)).unwrap();
        assert_eq!(names(&one), ["gpu_1x_h100_pcie@us-west-3"]);
        let eight = offers_from(&catalog(), &gpus("H100", 8)).unwrap();
        assert_eq!(names(&eight), ["gpu_8x_h100_sxm5@us-east-1"]);
        assert_eq!(eight[0].hourly_price, Some(23.92));
        // A100 (no size) spans both sizes, cheapest first.
        let a100 = offers_from(&catalog(), &gpus("A100", 1)).unwrap();
        assert_eq!(
            names(&a100),
            [
                "gpu_1x_a100@us-east-1",
                "gpu_1x_a100_sxm4_80gb@us-midwest-1"
            ]
        );
    }

    #[test]
    fn zone_pins_a_region_and_instance_type_asks_are_exact() {
        let mut request = gpus("A10", 1);
        request.zone = Some("us-west-1".into());
        let offers = offers_from(&catalog(), &request).unwrap();
        assert_eq!(names(&offers), ["gpu_1x_a10@us-west-1"]);

        let request = ResourceRequest {
            ask: ResourceAsk::InstanceType("cpu_4x_general".into()),
            regions: Vec::new(),
            zone: None,
            spot: false,
        };
        let offers = offers_from(&catalog(), &request).unwrap();
        assert_eq!(names(&offers), ["cpu_4x_general@us-east-1"]);
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.region, "us-east-1");
    }

    #[test]
    fn unknown_gpu_is_an_invalid_request_listing_known_types() {
        let err = offers_from(&catalog(), &gpus("B200", 1)).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("gpu_1x_a10"), "{}", err.msg);
        // Matching types without capacity in allowed regions: no offers, not an error.
        let mut request = gpus("A100-80GB", 1);
        request.regions = vec!["us-east-1".into()];
        assert!(offers_from(&catalog(), &request).unwrap().is_empty());
    }

    #[test]
    fn capacity_recheck() {
        assert!(has_capacity(&catalog(), "gpu_1x_a10", "us-west-1"));
        assert!(!has_capacity(&catalog(), "gpu_1x_a10", "us-south-1"));
    }
}
