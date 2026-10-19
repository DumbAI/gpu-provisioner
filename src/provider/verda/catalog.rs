//! Planning against live Verda data: instance types (with prices and the OS
//! images they boot) × where each type can launch right now
//! (`GET /v1/instance-availability`) → one offer per type × location,
//! cheapest first. Region = location code (`FIN-01`); there are no zones.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{InstanceType, LocationAvailability, VerdaLocation};

/// What a Verda offer carries from plan to launch (`Offer::detail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub instance_type: String,
    pub location_code: String,
    pub is_spot: bool,
    /// Image types this instance type can boot.
    pub supported_os: Vec<String>,
    /// Whether the type has GPUs (decides the default image).
    pub gpu: bool,
}

/// Uppercase tokens of a GPU name (split on space, `-`, `_`).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .collect()
}

/// Whether the user's GPU shorthand names this type's GPU. Each user token
/// must equal a token of the type's model, name or display name, or two
/// adjacent tokens joined (`RTX6000` ≙ `RTX 6000`). Whole-token comparison
/// keeps `A10` off `A100`, `H200` off `GH200` and `L40` off `L40S`.
pub fn gpu_name_matches(user: &str, ty: &InstanceType) -> bool {
    let names: Vec<&str> = [
        ty.model.as_str(),
        ty.name.as_str(),
        ty.display_name.as_deref().unwrap_or(""),
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

fn matches_ask(ask: &ResourceAsk, ty: &InstanceType) -> bool {
    match ask {
        ResourceAsk::InstanceType(wanted) => ty.instance_type.eq_ignore_ascii_case(wanted),
        ResourceAsk::Gpus(gpus) => {
            ty.gpu.number_of_gpus == gpus.count && gpu_name_matches(&gpus.name, ty)
        }
    }
}

/// Locations allowed by the request's region filter: location codes match
/// exactly (case-insensitive), two-letter entries match a location's
/// country code; no filter = everywhere. A zone pin is a location code too.
fn location_allowed(code: &str, country: Option<&str>, request: &ResourceRequest) -> bool {
    if let Some(zone) = &request.zone {
        if !zone.eq_ignore_ascii_case(code) {
            return false;
        }
    }
    request.regions.is_empty()
        || request.regions.iter().any(|r| {
            r.eq_ignore_ascii_case(code)
                || (r.len() == 2 && country.is_some_and(|c| c.eq_ignore_ascii_case(r)))
        })
}

/// Build ordered offers from the catalog, availability and locations.
pub fn offers_from(
    types: &[InstanceType],
    availability: &[LocationAvailability],
    locations: &[VerdaLocation],
    request: &ResourceRequest,
) -> Result<Vec<Offer>, ProviderError> {
    let matching: Vec<&InstanceType> = types
        .iter()
        .filter(|t| matches_ask(&request.ask, t))
        .collect();
    if matching.is_empty() {
        let mut known: Vec<String> = types
            .iter()
            .filter(|t| t.gpu.number_of_gpus > 0 && !t.model.is_empty())
            .map(|t| format!("{}:{}", t.model, t.gpu.number_of_gpus))
            .collect();
        known.sort();
        known.dedup();
        known.truncate(12);
        let ask = match &request.ask {
            ResourceAsk::InstanceType(t) => format!("instance type '{t}'"),
            ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
        };
        return Err(ProviderError::invalid(format!(
            "no Verda instance type matches {ask}; known GPUs include: {}",
            known.join(", ")
        )));
    }
    let countries: HashMap<&str, &str> = locations
        .iter()
        .map(|l| (l.code.as_str(), l.country_code.as_str()))
        .collect();
    let mut offers = Vec::new();
    for ty in matching {
        let price = if request.spot {
            ty.spot_price
        } else {
            ty.price_per_hour
        };
        for loc in availability {
            if !loc.availabilities.contains(&ty.instance_type) {
                continue;
            }
            let country = countries.get(loc.location_code.as_str()).copied();
            if !location_allowed(&loc.location_code, country, request) {
                continue;
            }
            let detail = OfferDetail {
                instance_type: ty.instance_type.clone(),
                location_code: loc.location_code.clone(),
                is_spot: request.spot,
                supported_os: ty.supported_os.clone(),
                gpu: ty.gpu.number_of_gpus > 0,
            };
            let mut offer = Offer::new(&ty.instance_type, &loc.location_code);
            offer.hourly_price = price.filter(|p| *p > 0.0);
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

/// Numeric version after `prefix-` in an image type, e.g. `24.04` from
/// `ubuntu-24.04-cuda-12.8-open-docker` with prefix `ubuntu`.
fn version_after(image: &str, prefix: &str) -> Vec<u32> {
    let parts: Vec<&str> = image.split('-').collect();
    parts
        .windows(2)
        .find(|w| w[0].eq_ignore_ascii_case(prefix))
        .map(|w| w[1].split('.').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_default()
}

/// The default OS image for an instance type: GPU types prefer CUDA images
/// (newest Ubuntu, then newest CUDA); CPU types prefer plain Ubuntu (newest).
/// Falls back to any Ubuntu image, then to the first supported image.
pub fn choose_image(supported_os: &[String], gpu: bool) -> Option<String> {
    let ubuntu: Vec<&String> = supported_os
        .iter()
        .filter(|i| i.to_lowercase().starts_with("ubuntu"))
        .collect();
    let preferred: Vec<&String> = ubuntu
        .iter()
        .copied()
        .filter(|i| i.to_lowercase().contains("cuda") == gpu)
        .collect();
    let pool = if preferred.is_empty() {
        ubuntu
    } else {
        preferred
    };
    let best = pool.into_iter().fold(None::<&String>, |best, candidate| {
        let rank = |i: &str| (version_after(i, "ubuntu"), version_after(i, "cuda"));
        match best {
            Some(b) if rank(b) >= rank(candidate) => Some(b),
            _ => Some(candidate),
        }
    });
    best.or(supported_os.first()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::GpuRequest;

    fn ty(
        instance_type: &str,
        model: &str,
        name: &str,
        gpus: u32,
        price: f64,
        spot: f64,
    ) -> InstanceType {
        serde_json::from_value(serde_json::json!({
            "instance_type": instance_type, "model": model, "name": name,
            "display_name": format!("NVIDIA {name}"), "gpu": {"number_of_gpus": gpus},
            "price_per_hour": price.to_string(), "spot_price": spot.to_string(),
            "supported_os": ["ubuntu-22.04-cuda-12.3", "ubuntu-24.04-cuda-12.8-open-docker", "ubuntu-24.04"]
        }))
        .unwrap()
    }

    fn named(model: &str, name: &str) -> InstanceType {
        ty("X", model, name, 1, 1.0, 0.5)
    }

    #[test]
    fn gpu_names_match_on_whole_tokens() {
        assert!(gpu_name_matches("H100", &named("H100", "H100 SXM5 80GB")));
        assert!(gpu_name_matches(
            "A100-80GB",
            &named("A100", "A100 SXM4 80GB")
        ));
        assert!(gpu_name_matches(
            "RTX6000-Ada",
            &named("RTX6000ADA", "RTX 6000 Ada 48GB")
        ));
        assert!(gpu_name_matches("l40s", &named("L40S", "L40S 48GB")));
        assert!(!gpu_name_matches("A10", &named("A100", "A100 SXM4 80GB")));
        assert!(!gpu_name_matches("H200", &named("GH200", "GH200 96GB")));
        assert!(!gpu_name_matches("L40", &named("L40S", "L40S 48GB")));
        assert!(!gpu_name_matches(
            "A100-40GB",
            &named("A100", "A100 SXM4 80GB")
        ));
    }

    fn catalog() -> (
        Vec<InstanceType>,
        Vec<LocationAvailability>,
        Vec<VerdaLocation>,
    ) {
        let types = vec![
            ty("1H100.80S.30V", "H100", "H100 SXM5 80GB", 1, 3.60, 0.9),
            ty("1H100.80S.22V", "H100", "H100 SXM5 80GB", 1, 3.17, 1.2),
            ty("2H100.80S.60V", "H100", "H100 SXM5 80GB", 2, 6.34, 1.6),
            ty("1A100.22V", "A100", "A100 SXM4 80GB", 1, 1.29, 0.4),
            serde_json::from_value(serde_json::json!({
                "instance_type": "CPU.4V.16G", "model": "", "name": "CPU",
                "price_per_hour": "0.03", "supported_os": ["ubuntu-24.04"]
            }))
            .unwrap(),
        ];
        let availability = vec![
            LocationAvailability {
                location_code: "FIN-01".into(),
                availabilities: vec!["1H100.80S.22V".into(), "1H100.80S.30V".into()],
            },
            LocationAvailability {
                location_code: "ICE-01".into(),
                availabilities: vec!["1H100.80S.22V".into(), "CPU.4V.16G".into()],
            },
        ];
        let locations = vec![
            VerdaLocation {
                code: "FIN-01".into(),
                country_code: "FI".into(),
            },
            VerdaLocation {
                code: "ICE-01".into(),
                country_code: "IS".into(),
            },
        ];
        (types, availability, locations)
    }

    fn request(
        ask: ResourceAsk,
        regions: &[&str],
        zone: Option<&str>,
        spot: bool,
    ) -> ResourceRequest {
        ResourceRequest {
            ask,
            regions: regions.iter().map(|r| r.to_string()).collect(),
            zone: zone.map(str::to_string),
            spot,
        }
    }

    fn h100(count: u32) -> ResourceAsk {
        ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count,
        })
    }

    fn rendered(offers: &[Offer]) -> Vec<String> {
        offers.iter().map(|o| o.to_string()).collect()
    }

    #[test]
    fn offers_cover_available_locations_cheapest_first() {
        let (types, avail, locs) = catalog();
        let offers =
            offers_from(&types, &avail, &locs, &request(h100(1), &[], None, false)).unwrap();
        assert_eq!(
            rendered(&offers),
            [
                "1H100.80S.22V@FIN-01 ($3.17/h)",
                "1H100.80S.22V@ICE-01 ($3.17/h)",
                "1H100.80S.30V@FIN-01 ($3.60/h)",
            ]
        );
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.location_code, "FIN-01");
        assert!(detail.gpu && !detail.is_spot);
        assert_eq!(detail.supported_os.len(), 3);
        // Count must match: 2x H100 is a different type, unavailable here.
        assert!(
            offers_from(&types, &avail, &locs, &request(h100(2), &[], None, false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn spot_prices_reorder_offers() {
        let (types, avail, locs) = catalog();
        let offers =
            offers_from(&types, &avail, &locs, &request(h100(1), &[], None, true)).unwrap();
        assert_eq!(offers[0].to_string(), "1H100.80S.30V@FIN-01 ($0.90/h)");
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert!(detail.is_spot);
    }

    #[test]
    fn region_country_and_zone_filters() {
        let (types, avail, locs) = catalog();
        let by_code = offers_from(
            &types,
            &avail,
            &locs,
            &request(h100(1), &["ice-01"], None, false),
        )
        .unwrap();
        assert_eq!(rendered(&by_code), ["1H100.80S.22V@ICE-01 ($3.17/h)"]);
        let by_country = offers_from(
            &types,
            &avail,
            &locs,
            &request(h100(1), &["FI"], None, false),
        )
        .unwrap();
        assert!(by_country.iter().all(|o| o.region == "FIN-01"));
        assert_eq!(by_country.len(), 2);
        let pinned = offers_from(
            &types,
            &avail,
            &locs,
            &request(h100(1), &[], Some("ICE-01"), false),
        )
        .unwrap();
        assert_eq!(rendered(&pinned), ["1H100.80S.22V@ICE-01 ($3.17/h)"]);
    }

    #[test]
    fn exact_instance_types_including_cpu_and_unknown_asks() {
        let (types, avail, locs) = catalog();
        let cpu = offers_from(
            &types,
            &avail,
            &locs,
            &request(
                ResourceAsk::InstanceType("CPU.4V.16G".into()),
                &[],
                None,
                false,
            ),
        )
        .unwrap();
        assert_eq!(rendered(&cpu), ["CPU.4V.16G@ICE-01 ($0.03/h)"]);
        let detail: OfferDetail = serde_json::from_value(cpu[0].detail.clone()).unwrap();
        assert!(!detail.gpu);

        let err = offers_from(
            &types,
            &avail,
            &locs,
            &request(
                ResourceAsk::Gpus(GpuRequest {
                    name: "B200".into(),
                    count: 8,
                }),
                &[],
                None,
                false,
            ),
        )
        .unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("H100:1"), "{}", err.msg);
    }

    #[test]
    fn default_image_prefers_newest_cuda_ubuntu_for_gpus() {
        let images: Vec<String> = [
            "ubuntu-22.04-cuda-12.3",
            "ubuntu-24.04-cuda-12.6-open-docker",
            "ubuntu-24.04-cuda-12.8-open-docker",
            "ubuntu-24.04",
            "ubuntu-22.04",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            choose_image(&images, true).as_deref(),
            Some("ubuntu-24.04-cuda-12.8-open-docker")
        );
        assert_eq!(
            choose_image(&images, false).as_deref(),
            Some("ubuntu-24.04")
        );
        // No CUDA image: any Ubuntu; nothing Ubuntu: the first entry.
        assert_eq!(
            choose_image(&["ubuntu-22.04".to_string()], true).as_deref(),
            Some("ubuntu-22.04")
        );
        assert_eq!(
            choose_image(&["debian-12".to_string()], true).as_deref(),
            Some("debian-12")
        );
        assert_eq!(choose_image(&[], true), None);
    }
}
