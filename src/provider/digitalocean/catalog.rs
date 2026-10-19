//! Planning against live DigitalOcean data: `GET /v2/sizes` returns every
//! Droplet size with its hourly price, GPU (`gpu_info`), and the regions it
//! can be created in, so one call yields one offer per size × region,
//! cheapest first. Region = DigitalOcean region slug (`tor1`); there are no
//! zones.
//!
//! Without an explicit region filter, only North American regions are
//! considered (New York, San Francisco, Toronto, Atlanta, Richmond):
//! launching across an ocean by default would surprise. `--region`
//! overrides that.
//!
//! The image follows DigitalOcean's GPU guidance: the AI/ML-ready NVIDIA
//! images (`gpu-h100x1-base` for every single-GPU plan, `gpu-h100x8-base`
//! for multi-GPU plans), the ROCm image `gpu-amd-base` for AMD plans, and
//! Ubuntu 24.04 for CPU sizes.

use serde::{Deserialize, Serialize};

use crate::provider::{Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::Size;

/// Region slug prefixes used when the request names no region.
pub const DEFAULT_REGION_PREFIXES: &[&str] = &["nyc", "sfo", "tor", "atl", "ric"];

/// NVIDIA image for single-GPU plans ("use `gpu-h100x1-base` even for
/// single GPU plans using GPUs other than H100s").
pub const IMAGE_NVIDIA_SINGLE: &str = "gpu-h100x1-base";
/// NVIDIA image for multi-GPU plans.
pub const IMAGE_NVIDIA_MULTI: &str = "gpu-h100x8-base";
/// AMD Instinct image (ROCm).
pub const IMAGE_AMD: &str = "gpu-amd-base";
/// CPU sizes.
pub const IMAGE_CPU: &str = "ubuntu-24-04-x64";

/// What a DigitalOcean offer carries from plan to launch (`Offer::detail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    pub size: String,
    pub region: String,
    /// Default image slug for this size (an `--image` override wins).
    pub image: String,
}

/// Uppercase tokens of a GPU name (split on space, `-`, `_`, parentheses).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '(' | ')' | ','))
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .collect()
}

/// Whether the user's GPU shorthand names this size's GPU. Each user token
/// must equal a token of `gpu_info.model` (`nvidia_rtx_4000_ada`) or the
/// size description — or adjacent tokens joined (`RTX4000` ≙ `rtx_4000`,
/// `4000ADA` ≙ `4000_ada`). Whole-token comparison keeps `L40` off `L40S`
/// and `MI300X` off `MI325X`.
pub fn gpu_name_matches(user: &str, size: &Size) -> bool {
    let Some(gpu) = &size.gpu_info else {
        return false;
    };
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    [gpu.model.as_str(), size.description.as_str()]
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

fn gpu_count(size: &Size) -> u32 {
    size.gpu_info.as_ref().map_or(0, |g| g.count)
}

fn matches_ask(ask: &ResourceAsk, size: &Size) -> bool {
    match ask {
        ResourceAsk::InstanceType(wanted) => size.slug.eq_ignore_ascii_case(wanted),
        ResourceAsk::Gpus(gpus) => {
            gpu_count(size) == gpus.count && gpu_name_matches(&gpus.name, size)
        }
    }
}

/// Regions allowed by the request: explicit regions (case-insensitive), a
/// zone pin (DigitalOcean has no zones, so it pins a region), or by default
/// the North American regions.
fn region_allowed(region: &str, request: &ResourceRequest) -> bool {
    if let Some(pin) = &request.zone {
        if !pin.eq_ignore_ascii_case(region) {
            return false;
        }
    }
    if request.regions.is_empty() {
        return request.zone.is_some()
            || DEFAULT_REGION_PREFIXES
                .iter()
                .any(|p| region.starts_with(p));
    }
    request
        .regions
        .iter()
        .any(|r| r.eq_ignore_ascii_case(region))
}

/// The default image for a size (see the module docs).
pub fn default_image(size: &Size) -> &'static str {
    match &size.gpu_info {
        Some(gpu) if gpu.count > 0 => {
            if gpu.model.to_lowercase().starts_with("amd") {
                IMAGE_AMD
            } else if gpu.count == 1 {
                IMAGE_NVIDIA_SINGLE
            } else {
                IMAGE_NVIDIA_MULTI
            }
        }
        _ => IMAGE_CPU,
    }
}

/// Build ordered offers from the size catalog.
pub fn offers_from(sizes: &[Size], request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
    let matching: Vec<&Size> = sizes
        .iter()
        .filter(|s| matches_ask(&request.ask, s))
        .collect();
    if matching.is_empty() {
        let mut known: Vec<String> = sizes
            .iter()
            .filter(|s| gpu_count(s) > 0)
            .map(|s| s.slug.clone())
            .collect();
        known.sort();
        known.truncate(12);
        let ask = match &request.ask {
            ResourceAsk::InstanceType(t) => format!("size '{t}'"),
            ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
        };
        return Err(ProviderError::invalid(format!(
            "no DigitalOcean Droplet size matches {ask}; GPU sizes on this account include: {}",
            if known.is_empty() {
                "none (GPU Droplets may need to be enabled for the account)".to_string()
            } else {
                known.join(", ")
            }
        )));
    }
    let mut offers = Vec::new();
    for size in matching.into_iter().filter(|s| s.available) {
        for region in size.regions.iter().filter(|r| region_allowed(r, request)) {
            let detail = OfferDetail {
                size: size.slug.clone(),
                region: region.clone(),
                image: default_image(size).to_string(),
            };
            let mut offer = Offer::new(&size.slug, region);
            offer.hourly_price = (size.price_hourly > 0.0).then_some(size.price_hourly);
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

/// Whether `size` can still be created in `region` per a fresh catalog.
pub fn has_capacity(sizes: &[Size], size: &str, region: &str) -> bool {
    sizes
        .iter()
        .any(|s| s.slug == size && s.available && s.regions.iter().any(|r| r == region))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::GpuRequest;

    fn size(slug: &str, model: Option<&str>, count: u32, hourly: f64, regions: &[&str]) -> Size {
        let mut v = serde_json::json!({
            "slug": slug, "memory": 245760, "vcpus": 20, "disk": 720, "transfer": 15,
            "price_monthly": hourly * 672.0, "price_hourly": hourly,
            "regions": regions, "available": true, "description": "GPU Droplet",
            "disk_info": [{"type": "local", "size": {"amount": 720, "unit": "gib"}}],
        });
        if let Some(model) = model {
            v["gpu_info"] = serde_json::json!({
                "count": count, "model": model, "vram": {"amount": 80, "unit": "gib"}
            });
        }
        serde_json::from_value(v).unwrap()
    }

    fn catalog() -> Vec<Size> {
        vec![
            size(
                "gpu-h100x1-80gb",
                Some("nvidia_h100"),
                1,
                3.39,
                &["nyc2", "tor1", "ams3"],
            ),
            size("gpu-h100x8-640gb", Some("nvidia_h100"), 8, 23.92, &["nyc2"]),
            size("gpu-l40sx1-48gb", Some("nvidia_l40s"), 1, 1.57, &["tor1"]),
            size(
                "gpu-4000adax1-20gb",
                Some("nvidia_rtx_4000_ada"),
                1,
                0.76,
                &["tor1", "atl1"],
            ),
            size("gpu-mi300x1-192gb", Some("amd_mi300x"), 1, 1.99, &["atl1"]),
            size("s-2vcpu-4gb", None, 0, 0.03571, &["nyc1", "fra1"]),
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

    fn by_slug<'a>(sizes: &'a [Size], slug: &str) -> &'a Size {
        sizes.iter().find(|s| s.slug == slug).unwrap()
    }

    #[test]
    fn gpu_matching_uses_whole_tokens() {
        let sizes = catalog();
        let s = |slug| by_slug(&sizes, slug);
        assert!(gpu_name_matches("H100", s("gpu-h100x1-80gb")));
        assert!(gpu_name_matches("L40S", s("gpu-l40sx1-48gb")));
        assert!(!gpu_name_matches("L40", s("gpu-l40sx1-48gb")));
        assert!(gpu_name_matches("RTX4000", s("gpu-4000adax1-20gb")));
        assert!(gpu_name_matches("RTX4000ADA", s("gpu-4000adax1-20gb")));
        assert!(gpu_name_matches("RTX-4000-Ada", s("gpu-4000adax1-20gb")));
        assert!(gpu_name_matches("MI300X", s("gpu-mi300x1-192gb")));
        assert!(!gpu_name_matches("MI300X", s("gpu-h100x1-80gb")));
        assert!(!gpu_name_matches("H100", s("s-2vcpu-4gb")));
    }

    #[test]
    fn offers_per_region_cheapest_first_north_america_by_default() {
        let offers = offers_from(&catalog(), &gpus("H100", 1)).unwrap();
        assert_eq!(
            names(&offers),
            ["gpu-h100x1-80gb@nyc2", "gpu-h100x1-80gb@tor1"]
        );
        assert_eq!(offers[0].hourly_price, Some(3.39));

        let mut request = gpus("H100", 1);
        request.regions = vec!["AMS3".into()];
        let offers = offers_from(&catalog(), &request).unwrap();
        assert_eq!(names(&offers), ["gpu-h100x1-80gb@ams3"]);

        let eight = offers_from(&catalog(), &gpus("H100", 8)).unwrap();
        assert_eq!(names(&eight), ["gpu-h100x8-640gb@nyc2"]);
    }

    #[test]
    fn images_follow_digitalocean_gpu_guidance() {
        let sizes = catalog();
        let image = |slug| default_image(by_slug(&sizes, slug));
        assert_eq!(image("gpu-h100x1-80gb"), IMAGE_NVIDIA_SINGLE);
        assert_eq!(image("gpu-l40sx1-48gb"), IMAGE_NVIDIA_SINGLE);
        assert_eq!(image("gpu-h100x8-640gb"), IMAGE_NVIDIA_MULTI);
        assert_eq!(image("gpu-mi300x1-192gb"), IMAGE_AMD);
        assert_eq!(image("s-2vcpu-4gb"), IMAGE_CPU);

        let offers = offers_from(&catalog(), &gpus("MI300X", 1)).unwrap();
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(
            detail,
            OfferDetail {
                size: "gpu-mi300x1-192gb".into(),
                region: "atl1".into(),
                image: IMAGE_AMD.into(),
            }
        );
    }

    #[test]
    fn unavailable_sizes_zone_pins_and_size_asks() {
        let mut sizes = catalog();
        sizes[2].available = false;
        assert!(offers_from(&sizes, &gpus("L40S", 1)).unwrap().is_empty());

        let mut request = gpus("RTX4000", 1);
        request.zone = Some("atl1".into());
        let offers = offers_from(&catalog(), &request).unwrap();
        assert_eq!(names(&offers), ["gpu-4000adax1-20gb@atl1"]);

        let request = ResourceRequest {
            ask: ResourceAsk::InstanceType("s-2vcpu-4gb".into()),
            regions: Vec::new(),
            zone: None,
            spot: false,
        };
        assert_eq!(
            names(&offers_from(&catalog(), &request).unwrap()),
            ["s-2vcpu-4gb@nyc1"]
        );
    }

    #[test]
    fn unknown_gpu_is_an_invalid_request_listing_known_sizes() {
        let err = offers_from(&catalog(), &gpus("B200", 1)).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("gpu-h100x1-80gb"), "{}", err.msg);
        let err = offers_from(&catalog()[5..], &gpus("H100", 1)).unwrap_err();
        assert!(err.msg.contains("may need to be enabled"), "{}", err.msg);
    }

    #[test]
    fn capacity_recheck() {
        let mut sizes = catalog();
        assert!(has_capacity(&sizes, "gpu-h100x1-80gb", "tor1"));
        assert!(!has_capacity(&sizes, "gpu-h100x1-80gb", "atl1"));
        sizes[0].available = false;
        assert!(!has_capacity(&sizes, "gpu-h100x1-80gb", "tor1"));
    }
}
