//! GCP accelerator catalog — the GCP-specific half of planning: GPU shorthand →
//! machine-type ladder (cheapest first), plus zone helpers.
//!
//! GCP differs from AWS in two ways that shape the offers:
//! - most GPUs come **built into** accelerator-optimized machine types (G2 =
//!   L4, A2 = A100, A3 = H100), but T4s are **attached** to N1 machines via
//!   `guestAccelerators`, so a ladder entry may carry an accelerator;
//! - capacity and instances are **zonal**, so offers are per zone (type-major
//!   × region × zone) and a stockout blocks only that zone.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::provider::{GpuRequest, Offer, ProviderError, ResourceAsk};

/// An accelerator attached to an N1 machine (`guestAccelerators`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    /// GCP accelerator type id, e.g. `nvidia-tesla-t4`.
    #[serde(rename = "type")]
    pub kind: String,
    pub count: u32,
}

/// One machine-type choice for an ask; travels in `Offer::detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeChoice {
    pub machine_type: String,
    /// GPU machine (built-in or attached): needs `onHostMaintenance=TERMINATE`
    /// and the Deep Learning VM image by default.
    pub gpu: bool,
    pub accelerator: Option<Accelerator>,
}

struct GcpGpuOffering {
    gpu: &'static str,
    count: u32,
    /// Machine types, cheapest first.
    machine_types: &'static [&'static str],
    /// Attached accelerator type (N1 + T4); `None` = built into the type.
    accelerator: Option<&'static str>,
}

#[rustfmt::skip]
const OFFERINGS: &[GcpGpuOffering] = &[
    GcpGpuOffering { gpu: "T4", count: 1, machine_types: &["n1-standard-4", "n1-standard-8", "n1-standard-16"], accelerator: Some("nvidia-tesla-t4") },
    GcpGpuOffering { gpu: "T4", count: 2, machine_types: &["n1-standard-8", "n1-standard-16"], accelerator: Some("nvidia-tesla-t4") },
    GcpGpuOffering { gpu: "T4", count: 4, machine_types: &["n1-standard-16", "n1-standard-32"], accelerator: Some("nvidia-tesla-t4") },
    GcpGpuOffering { gpu: "L4", count: 1, machine_types: &["g2-standard-4", "g2-standard-8", "g2-standard-12", "g2-standard-16", "g2-standard-32"], accelerator: None },
    GcpGpuOffering { gpu: "L4", count: 2, machine_types: &["g2-standard-24"], accelerator: None },
    GcpGpuOffering { gpu: "L4", count: 4, machine_types: &["g2-standard-48"], accelerator: None },
    GcpGpuOffering { gpu: "L4", count: 8, machine_types: &["g2-standard-96"], accelerator: None },
    GcpGpuOffering { gpu: "A100", count: 1, machine_types: &["a2-highgpu-1g"], accelerator: None },
    GcpGpuOffering { gpu: "A100", count: 2, machine_types: &["a2-highgpu-2g"], accelerator: None },
    GcpGpuOffering { gpu: "A100", count: 4, machine_types: &["a2-highgpu-4g"], accelerator: None },
    GcpGpuOffering { gpu: "A100", count: 8, machine_types: &["a2-highgpu-8g"], accelerator: None },
    GcpGpuOffering { gpu: "A100-80GB", count: 1, machine_types: &["a2-ultragpu-1g"], accelerator: None },
    GcpGpuOffering { gpu: "A100-80GB", count: 2, machine_types: &["a2-ultragpu-2g"], accelerator: None },
    GcpGpuOffering { gpu: "A100-80GB", count: 4, machine_types: &["a2-ultragpu-4g"], accelerator: None },
    GcpGpuOffering { gpu: "A100-80GB", count: 8, machine_types: &["a2-ultragpu-8g"], accelerator: None },
    GcpGpuOffering { gpu: "H100", count: 1, machine_types: &["a3-highgpu-1g"], accelerator: None },
    GcpGpuOffering { gpu: "H100", count: 2, machine_types: &["a3-highgpu-2g"], accelerator: None },
    GcpGpuOffering { gpu: "H100", count: 4, machine_types: &["a3-highgpu-4g"], accelerator: None },
    GcpGpuOffering { gpu: "H100", count: 8, machine_types: &["a3-highgpu-8g"], accelerator: None },
];

/// Machine families with GPUs built in.
const GPU_FAMILIES: &[&str] = &["g2-", "g4-", "a2-", "a3-", "a4-"];

pub fn is_gpu_machine_type(machine_type: &str) -> bool {
    GPU_FAMILIES.iter().any(|f| machine_type.starts_with(f))
}

/// All machine-type choices satisfying an ask, cheapest first.
pub fn types_for(ask: &ResourceAsk) -> Result<Vec<TypeChoice>, ProviderError> {
    match ask {
        ResourceAsk::InstanceType(machine_type) => Ok(vec![TypeChoice {
            machine_type: machine_type.clone(),
            gpu: is_gpu_machine_type(machine_type),
            accelerator: None,
        }]),
        ResourceAsk::Gpus(gpus) => gpu_types_for(gpus),
    }
}

fn gpu_types_for(gpus: &GpuRequest) -> Result<Vec<TypeChoice>, ProviderError> {
    let offering = OFFERINGS
        .iter()
        .find(|o| o.gpu == gpus.name && o.count == gpus.count)
        .ok_or_else(|| {
            ProviderError::invalid(format!(
                "no GCP machine type known for GPU '{gpus}'; available: {}",
                OFFERINGS
                    .iter()
                    .map(|o| format!("{}:{}", o.gpu, o.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    Ok(offering
        .machine_types
        .iter()
        .map(|t| TypeChoice {
            machine_type: t.to_string(),
            gpu: true,
            accelerator: offering.accelerator.map(|kind| Accelerator {
                kind: kind.to_string(),
                count: gpus.count,
            }),
        })
        .collect())
}

/// `us-central1-a` → `us-central1` (zones are `<region>-<letter>`).
pub fn region_of_zone(zone: &str) -> Option<&str> {
    let (region, suffix) = zone.rsplit_once('-')?;
    (suffix.len() == 1 && suffix.chars().all(|c| c.is_ascii_lowercase()) && region.contains('-'))
        .then_some(region)
}

/// Last path segment of a GCP resource URL (`.../zones/us-central1-a` →
/// `us-central1-a`); identity for bare names.
pub fn basename(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// Offers in preference order: type-major (cheapest type first, tried in
/// every zone before the next size up), then region order, then zone order.
/// `zones_offering(choice)` is the set of zones where the type (and its
/// accelerator) exists; `regions` pairs each region with its zones.
pub fn build_offers(
    choices: &[TypeChoice],
    regions: &[(String, Vec<String>)],
    zones_offering: impl Fn(&TypeChoice) -> BTreeSet<String>,
    zone_pin: Option<&str>,
) -> Vec<Offer> {
    let mut offers = Vec::new();
    for choice in choices {
        let offered = zones_offering(choice);
        for (region, zones) in regions {
            for zone in zones {
                if zone_pin.is_some_and(|pin| pin != zone) || !offered.contains(zone) {
                    continue;
                }
                let mut offer =
                    Offer::new(&choice.machine_type, region).with_zone(Some(zone.clone()));
                offer.detail = serde_json::to_value(choice).expect("TypeChoice serializes");
                offers.push(offer);
            }
        }
    }
    offers
}

/// Decode the `TypeChoice` an offer carries (falls back to the type name for
/// offers persisted by older builds or built by hand).
pub fn choice_of(offer: &Offer) -> TypeChoice {
    serde_json::from_value(offer.detail.clone()).unwrap_or_else(|_| TypeChoice {
        machine_type: offer.instance_type.clone(),
        gpu: is_gpu_machine_type(&offer.instance_type),
        accelerator: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, count: u32) -> ResourceAsk {
        ResourceAsk::Gpus(GpuRequest {
            name: name.to_string(),
            count,
        })
    }

    #[test]
    fn gpu_ladders_resolve_cheapest_first() {
        let l4 = types_for(&gpu("L4", 1)).unwrap();
        assert_eq!(l4[0].machine_type, "g2-standard-4");
        assert!(l4.len() > 1 && l4.iter().all(|c| c.gpu && c.accelerator.is_none()));
        let h100 = types_for(&gpu("H100", 8)).unwrap();
        assert_eq!(h100[0].machine_type, "a3-highgpu-8g");
        assert_eq!(
            types_for(&gpu("A100-80GB", 1)).unwrap()[0].machine_type,
            "a2-ultragpu-1g"
        );
    }

    #[test]
    fn t4_attaches_an_accelerator_with_the_requested_count() {
        let t4 = types_for(&gpu("T4", 2)).unwrap();
        assert_eq!(t4[0].machine_type, "n1-standard-8");
        assert_eq!(
            t4[0].accelerator,
            Some(Accelerator {
                kind: "nvidia-tesla-t4".into(),
                count: 2
            })
        );
    }

    #[test]
    fn unknown_gpu_is_invalid_and_lists_available() {
        let err = types_for(&gpu("B200", 1)).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("available:") && err.msg.contains("L4:1"));
    }

    #[test]
    fn explicit_machine_type_infers_gpu_ness() {
        let c = &types_for(&ResourceAsk::InstanceType("g2-standard-8".into())).unwrap()[0];
        assert!(c.gpu);
        let c = &types_for(&ResourceAsk::InstanceType("e2-medium".into())).unwrap()[0];
        assert!(!c.gpu);
    }

    #[test]
    fn zone_parsing() {
        assert_eq!(region_of_zone("us-central1-a"), Some("us-central1"));
        assert_eq!(region_of_zone("europe-west4-b"), Some("europe-west4"));
        assert_eq!(region_of_zone("us-central1"), None);
        assert_eq!(region_of_zone("bogus"), None);
        assert_eq!(
            basename("https://www.googleapis.com/compute/v1/projects/p/zones/us-east1-c"),
            "us-east1-c"
        );
        assert_eq!(basename("us-east1-c"), "us-east1-c");
    }

    #[test]
    fn offers_are_type_major_zonal_and_filtered_by_offering() {
        let choices = types_for(&gpu("L4", 1)).unwrap()[..2].to_vec();
        let regions = vec![
            (
                "us-central1".to_string(),
                vec!["us-central1-a".into(), "us-central1-b".into()],
            ),
            ("us-east1".to_string(), vec!["us-east1-b".into()]),
        ];
        // g2-standard-4 everywhere except us-central1-b.
        let offering = |c: &TypeChoice| -> BTreeSet<String> {
            let mut zones: BTreeSet<String> = ["us-central1-a", "us-central1-b", "us-east1-b"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            if c.machine_type == "g2-standard-4" {
                zones.remove("us-central1-b");
            }
            zones
        };
        let rendered: Vec<String> = build_offers(&choices, &regions, offering, None)
            .iter()
            .map(|o| o.to_string())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "g2-standard-4@us-central1/us-central1-a",
                "g2-standard-4@us-east1/us-east1-b",
                "g2-standard-8@us-central1/us-central1-a",
                "g2-standard-8@us-central1/us-central1-b",
                "g2-standard-8@us-east1/us-east1-b",
            ]
        );
        let pinned = build_offers(&choices, &regions, offering, Some("us-east1-b"));
        assert_eq!(pinned.len(), 2);
        assert!(pinned
            .iter()
            .all(|o| o.zone.as_deref() == Some("us-east1-b")));
    }

    #[test]
    fn choice_round_trips_through_offer_detail() {
        let choices = types_for(&gpu("T4", 1)).unwrap();
        let regions = vec![("us-west1".to_string(), vec!["us-west1-b".to_string()])];
        let offers = build_offers(
            &choices[..1],
            &regions,
            |_| ["us-west1-b".to_string()].into_iter().collect(),
            None,
        );
        assert_eq!(choice_of(&offers[0]), choices[0]);
        // Hand-built offers fall back to the type name.
        let bare = Offer::new("a2-highgpu-1g", "us-central1");
        assert!(choice_of(&bare).gpu);
    }
}
