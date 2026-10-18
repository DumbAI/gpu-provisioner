//! Live-availability planning — the Prime Intellect half of planning. A
//! marketplace's stock churns too fast for a static catalog, so we query
//! `GET /availability/gpus` at plan time and turn each offer (one per
//! provider × data center) into an [`Offer`], cheapest first.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ResourceAsk, ResourceRequest};

use super::api::{AvailabilityQuery, GpuOffer};

/// Image preferred when an offer lists it (Prime Intellect's default).
pub const DEFAULT_IMAGE: &str = "ubuntu_22_cuda_12";
/// `stockStatus` value of an offer that cannot be launched right now.
const OUT_OF_STOCK: &str = "Unavailable";

/// Everything `create` needs from the chosen offer, persisted as
/// [`Offer::detail`] (the core never looks inside).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferDetail {
    pub cloud_id: String,
    pub gpu_type: String,
    /// Taken from the offer: SXM offers need it, so a hardcoded `PCIe` would
    /// be wrong for them.
    pub socket: String,
    /// Upstream provider type (`hyperstack`, `lambdalabs`, ...).
    pub provider: String,
    pub gpu_count: u32,
    pub security: Option<String>,
    pub country: Option<String>,
    pub data_center: Option<String>,
    pub image: Option<String>,
}

impl OfferDetail {
    fn from_offer(item: &GpuOffer) -> Self {
        let image = if item.images.iter().any(|i| i == DEFAULT_IMAGE) {
            Some(DEFAULT_IMAGE.to_string())
        } else {
            item.images.first().cloned()
        };
        OfferDetail {
            cloud_id: item.cloud_id.clone(),
            gpu_type: item.gpu_type.clone(),
            socket: item.socket.clone(),
            provider: item.provider.clone(),
            gpu_count: item.gpu_count,
            security: item.security.clone(),
            country: item.country.clone(),
            data_center: item.data_center.clone(),
            image,
        }
    }
}

/// `"<provider>/<cloudId>"` — the instance-type string users can pass back
/// via `--instance-type`.
pub fn instance_type(provider: &str, cloud_id: &str) -> String {
    format!("{provider}/{cloud_id}")
}

/// Split an `--instance-type` ask into `(provider, cloud_id)`; a bare cloud
/// id matches any provider.
pub fn parse_instance_type(s: &str) -> (Option<&str>, &str) {
    match s.split_once('/') {
        Some((provider, cloud_id)) => (Some(provider), cloud_id),
        None => (None, s),
    }
}

/// Normalize a user GPU name to Prime Intellect's spelling
/// (`a100-80gb` → `A100_80GB`).
fn normalize_gpu(name: &str) -> String {
    name.trim().to_uppercase().replace('-', "_")
}

/// Whether offer GPU type `offered` (e.g. `H100_80GB`) satisfies the user's
/// ask `wanted` (e.g. `H100`): exact match, or `wanted` plus a `_<memory>`
/// suffix. Prefix-with-underscore keeps `H200` from matching `GH200_96GB`
/// and `A10` from matching `A100_80GB`.
pub fn gpu_matches(offered: &str, wanted: &str) -> bool {
    let offered = normalize_gpu(offered);
    let wanted = normalize_gpu(wanted);
    offered == wanted || offered.starts_with(&format!("{wanted}_"))
}

/// The availability query for a resource request (server-side filters).
pub fn query_for(request: &ResourceRequest) -> AvailabilityQuery {
    let mut query = AvailabilityQuery {
        regions: request.regions.clone(),
        data_center_id: request.zone.clone(),
        ..Default::default()
    };
    match &request.ask {
        ResourceAsk::Gpus(gpus) => query.gpu_count = Some(gpus.count),
        ResourceAsk::InstanceType(it) => {
            query.cloud_id = Some(parse_instance_type(it).1.to_string());
        }
    }
    query
}

/// Filter (ask, stock, spot, zone pin) and order (cheapest first) raw
/// availability into offers.
pub fn offers_from(items: Vec<GpuOffer>, request: &ResourceRequest) -> Vec<Offer> {
    let mut kept: Vec<GpuOffer> = items
        .into_iter()
        .filter(|item| match &request.ask {
            ResourceAsk::Gpus(gpus) => {
                item.gpu_count == gpus.count && gpu_matches(&item.gpu_type, &gpus.name)
            }
            ResourceAsk::InstanceType(it) => {
                let (provider, cloud_id) = parse_instance_type(it);
                item.cloud_id == cloud_id && provider.is_none_or(|p| p == item.provider)
            }
        })
        .filter(|item| item.stock_status.as_deref() != Some(OUT_OF_STOCK))
        .filter(|item| request.spot || item.is_spot != Some(true))
        .filter(|item| match &request.zone {
            Some(zone) => item.data_center.as_deref() == Some(zone.as_str()),
            None => true,
        })
        .collect();
    // Cheapest first; unpriced offers last. Stable, so API order breaks ties.
    kept.sort_by(|a, b| match (a.prices.on_demand, b.prices.on_demand) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    kept.iter().map(to_offer).collect()
}

fn to_offer(item: &GpuOffer) -> Offer {
    let region = item
        .region
        .clone()
        .or_else(|| item.country.clone())
        .unwrap_or_else(|| "any".to_string());
    Offer {
        instance_type: instance_type(&item.provider, &item.cloud_id),
        region,
        zone: item.data_center.clone(),
        hourly_price: item.prices.on_demand,
        detail: serde_json::to_value(OfferDetail::from_offer(item))
            .expect("OfferDetail serializes"),
    }
}

/// Whether a fresh availability item is still the same, launchable offer.
pub fn still_offered(item: &GpuOffer, detail: &OfferDetail) -> bool {
    item.cloud_id == detail.cloud_id
        && item.provider == detail.provider
        && item.gpu_count == detail.gpu_count
        && item.data_center == detail.data_center
        && item.stock_status.as_deref() != Some(OUT_OF_STOCK)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::api::Prices;
    use super::*;
    use crate::provider::GpuRequest;

    pub(crate) fn item(
        provider: &str,
        cloud_id: &str,
        gpu_type: &str,
        count: u32,
        dc: &str,
        price: Option<f64>,
    ) -> GpuOffer {
        GpuOffer {
            cloud_id: cloud_id.into(),
            gpu_type: gpu_type.into(),
            socket: "SXM5".into(),
            provider: provider.into(),
            gpu_count: count,
            region: Some("united_states".into()),
            data_center: Some(dc.into()),
            country: Some("US".into()),
            stock_status: Some("Available".into()),
            security: Some("secure_cloud".into()),
            prices: Prices { on_demand: price },
            images: vec!["cuda_12_4_pytorch_2_5".into(), DEFAULT_IMAGE.into()],
            is_spot: None,
        }
    }

    fn gpu_request(name: &str, count: u32) -> ResourceRequest {
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

    #[test]
    fn gpu_matching_respects_name_boundaries() {
        assert!(gpu_matches("H100_80GB", "H100"));
        assert!(gpu_matches("A100_80GB", "A100"));
        assert!(gpu_matches("A100_40GB", "A100"));
        assert!(gpu_matches("A100_80GB", "a100-80gb"));
        assert!(gpu_matches("H100_80GB", "H100_80GB"));
        assert!(!gpu_matches("GH200_96GB", "H200"));
        assert!(!gpu_matches("A100_80GB", "A10"));
        assert!(gpu_matches("A10_24GB", "A10"));
        assert!(!gpu_matches("H200_141GB", "H100"));
    }

    #[test]
    fn offers_are_filtered_and_cheapest_first() {
        let mut out_of_stock = item("runpod", "x", "H100_80GB", 1, "dc-x", Some(0.5));
        out_of_stock.stock_status = Some("Unavailable".into());
        let mut spot = item("runpod", "s", "H100_80GB", 1, "dc-s", Some(0.1));
        spot.is_spot = Some(true);
        let items = vec![
            item(
                "hyperstack",
                "n3-H100x1",
                "H100_80GB",
                1,
                "CANADA-1",
                Some(1.9),
            ),
            item(
                "lambdalabs",
                "gpu_1x_h100",
                "H100_80GB",
                1,
                "us-south-2",
                Some(3.29),
            ),
            item("nebius", "unpriced", "H100_80GB", 1, "eu-1", None),
            item("datacrunch", "1H200", "H200_141GB", 1, "FIN-01", Some(1.0)),
            item(
                "hyperstack",
                "n3-H100x2",
                "H100_80GB",
                2,
                "CANADA-1",
                Some(3.8),
            ),
            out_of_stock,
            spot,
        ];
        let offers = offers_from(items, &gpu_request("H100", 1));
        let types: Vec<&str> = offers.iter().map(|o| o.instance_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "hyperstack/n3-H100x1",
                "lambdalabs/gpu_1x_h100",
                "nebius/unpriced"
            ]
        );
        let first = &offers[0];
        assert_eq!(first.region, "united_states");
        assert_eq!(first.zone.as_deref(), Some("CANADA-1"));
        assert_eq!(first.hourly_price, Some(1.9));
        let detail: OfferDetail = serde_json::from_value(first.detail.clone()).unwrap();
        assert_eq!(detail.socket, "SXM5");
        assert_eq!(detail.image.as_deref(), Some(DEFAULT_IMAGE));
        assert_eq!(detail.provider, "hyperstack");
    }

    #[test]
    fn zone_pin_selects_a_data_center() {
        let items = vec![
            item("hyperstack", "a", "H100_80GB", 1, "CANADA-1", Some(1.0)),
            item("hyperstack", "b", "H100_80GB", 1, "US-1", Some(2.0)),
        ];
        let mut request = gpu_request("H100", 1);
        request.zone = Some("US-1".into());
        let offers = offers_from(items, &request);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].zone.as_deref(), Some("US-1"));
    }

    #[test]
    fn instance_type_ask_matches_provider_and_cloud_id() {
        let items = vec![
            item(
                "hyperstack",
                "n3-H100x1",
                "H100_80GB",
                1,
                "CANADA-1",
                Some(1.9),
            ),
            item("runpod", "n3-H100x1", "H100_80GB", 1, "US-1", Some(1.0)),
        ];
        let mut request = gpu_request("H100", 1);
        request.ask = ResourceAsk::InstanceType("hyperstack/n3-H100x1".into());
        let offers = offers_from(items.clone(), &request);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].instance_type, "hyperstack/n3-H100x1");
        // A bare cloud id matches any provider.
        request.ask = ResourceAsk::InstanceType("n3-H100x1".into());
        assert_eq!(offers_from(items, &request).len(), 2);
        assert_eq!(query_for(&request).cloud_id.as_deref(), Some("n3-H100x1"));
    }

    #[test]
    fn falls_back_to_first_image_and_country_region() {
        let mut it = item("p", "c", "L40S_48GB", 1, "dc", Some(1.0));
        it.images = vec!["cuda_12_1_pytorch_2_4".into()];
        it.region = None;
        let offers = offers_from(vec![it], &gpu_request("L40S", 1));
        assert_eq!(offers[0].region, "US");
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.image.as_deref(), Some("cuda_12_1_pytorch_2_4"));
    }

    #[test]
    fn query_passes_regions_count_and_zone() {
        let mut request = gpu_request("H100", 8);
        request.regions = vec!["canada".into()];
        request.zone = Some("CANADA-1".into());
        let q = query_for(&request);
        assert_eq!(q.regions, vec!["canada"]);
        assert_eq!(q.gpu_count, Some(8));
        assert_eq!(q.data_center_id.as_deref(), Some("CANADA-1"));
    }
}
