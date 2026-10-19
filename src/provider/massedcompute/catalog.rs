//! Planning for Massed Compute: live inventory → ordered offers, plus the
//! default image choice.
//!
//! `GET /gpu-inventory` lists every product (`gpu_1x_a6000`, described as
//! `1x RTX A6000`) with its hourly price, the regions where it currently has
//! capacity, and a capacity count. Each product with capacity becomes one
//! offer:
//!
//! - `instance_type` = the product name (`gpu_1x_a6000`);
//! - `region` = `any`: the launch API takes no placement (`regionName` must be
//!   `any`), so Massed Compute picks the data center;
//! - ordered cheapest first.
//!
//! `--region` keeps products with capacity in at least one of the named
//! regions (`us-central-3`, …), but cannot pin placement. Massed Compute's
//! data centers are in the US, so no default region filter is needed.

use serde::{Deserialize, Serialize};

use crate::provider::{GpuRequest, Offer, ProviderError, ResourceAsk, ResourceRequest};

use super::api::{Image, InventoryEntry};

/// Region recorded on offers: the launch API only accepts `any`.
pub const ANY_REGION: &str = "any";

/// Launch details carried in `Offer::detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferDetail {
    /// Product name for `productName`.
    pub product: String,
    pub gpu_count: u32,
}

/// The ask resolved into what the inventory is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// GPU shorthand (`A6000:1`), matched against product descriptions.
    Shorthand(GpuRequest),
    /// An exact product name (`gpu_1x_a6000`), CPU products included.
    Exact(String),
}

impl Ask {
    pub fn from_request(ask: &ResourceAsk) -> Ask {
        match ask {
            ResourceAsk::Gpus(gpus) => Ask::Shorthand(gpus.clone()),
            ResourceAsk::InstanceType(name) => Ask::Exact(name.clone()),
        }
    }

    /// Whether the GPU model matches, ignoring the count.
    fn names(&self, entry: &InventoryEntry) -> bool {
        match self {
            Ask::Shorthand(gpus) => gpu_name_matches(&gpus.name, entry),
            Ask::Exact(name) => entry.instance_type.name.eq_ignore_ascii_case(name),
        }
    }

    fn count_ok(&self, entry: &InventoryEntry) -> bool {
        match self {
            Ask::Shorthand(gpus) => gpu_count(entry) == Some(gpus.count),
            Ask::Exact(_) => true,
        }
    }
}

/// Uppercase tokens of a product name/description (split on space, `-`,
/// `_`), without the `gpu` prefix and the `<n>x` count token.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_uppercase)
        .filter(|t| t != "GPU" && !is_count_token(t))
        .collect()
}

/// `1X`, `8X` — the GPU-count token of a product name or description.
fn is_count_token(token: &str) -> bool {
    token
        .strip_suffix('X')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// GPUs per instance, from `gpu_<n>x_…` or a leading `<n>x` in the
/// description; `None` for CPU products.
pub fn gpu_count(entry: &InventoryEntry) -> Option<u32> {
    let parse = |text: &str| {
        text.split(|c: char| c.is_whitespace() || c == '_')
            .find(|t| is_count_token(&t.to_uppercase()))
            .and_then(|t| t[..t.len() - 1].parse().ok())
    };
    let name = &entry.instance_type.name;
    if name.to_lowercase().starts_with("gpu") {
        parse(name).or_else(|| parse(&entry.instance_type.description))
    } else {
        None
    }
}

/// Whether the user's GPU shorthand names this product's GPU. Each user token
/// must equal a token of the product name or description, or two adjacent
/// tokens joined (`RTXA6000` ≙ `RTX A6000`). Whole-token comparison keeps
/// `A10` off `A100` and `L40` off `L40S`.
pub fn gpu_name_matches(user: &str, entry: &InventoryEntry) -> bool {
    let wanted = tokens(user);
    if wanted.is_empty() {
        return false;
    }
    [
        entry.instance_type.description.as_str(),
        entry.instance_type.name.as_str(),
    ]
    .iter()
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

/// Build ordered offers from the inventory.
pub fn offers_from(
    inventory: &[InventoryEntry],
    request: &ResourceRequest,
) -> Result<Vec<Offer>, ProviderError> {
    let ask = Ask::from_request(&request.ask);
    let mut matched_any = false;
    let mut ranked: Vec<(u64, Offer)> = Vec::new();

    for entry in inventory.iter().filter(|e| ask.names(e)) {
        matched_any = true;
        if !ask.count_ok(entry) {
            continue;
        }
        let regions = &entry.regions_with_capacity_available;
        if entry.capacity_available == 0 && regions.is_empty() {
            continue;
        }
        if !request.regions.is_empty()
            && !regions.iter().any(|r| {
                request
                    .regions
                    .iter()
                    .any(|want| want.eq_ignore_ascii_case(&r.name))
            })
        {
            continue;
        }
        let cents = entry.instance_type.price_cents_per_hour.unwrap_or(0);
        let detail = OfferDetail {
            product: entry.instance_type.name.clone(),
            gpu_count: gpu_count(entry).unwrap_or(0),
        };
        let mut offer = Offer::new(&entry.instance_type.name, ANY_REGION);
        offer.hourly_price = (cents > 0).then_some(cents as f64 / 100.0);
        offer.detail = serde_json::to_value(&detail).expect("offer detail serializes");
        ranked.push((cents, offer));
    }

    if !matched_any {
        let mut known: Vec<&str> = inventory
            .iter()
            .map(|e| e.instance_type.description.as_str())
            .filter(|d| !d.is_empty())
            .collect();
        known.sort_unstable();
        known.dedup();
        known.truncate(8);
        return Err(ProviderError::invalid(format!(
            "no Massed Compute product matches {}; products include: {}",
            match &ask {
                Ask::Shorthand(g) => format!("'{g}'"),
                Ask::Exact(name) => format!("instance type '{name}'"),
            },
            known.join(", ")
        )));
    }

    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.instance_type.cmp(&b.1.instance_type))
    });
    Ok(ranked.into_iter().map(|(_, offer)| offer).collect())
}

/// Resolve the image: `--image` as a numeric id or an exact image name, else
/// the default — the newest plain Ubuntu image (desktop and app-specific
/// stacks are skipped).
pub fn choose_image(images: &[Image], wanted: Option<&str>) -> Result<i64, ProviderError> {
    if let Some(wanted) = wanted {
        return images
            .iter()
            .find(|i| {
                i.vm_image_id.to_string() == wanted || i.vm_image_name.eq_ignore_ascii_case(wanted)
            })
            .map(|i| i.vm_image_id)
            .ok_or_else(|| {
                ProviderError::invalid(format!(
                    "no Massed Compute image '{wanted}'; available: {}",
                    image_list(images)
                ))
            });
    }
    images
        .iter()
        .filter(|i| {
            let text = format!("{} {}", i.vm_image_name, i.vm_image_description).to_lowercase();
            text.contains("ubuntu") && !text.contains("desktop")
        })
        .max_by(|a, b| {
            ubuntu_version(&a.vm_image_name)
                .cmp(&ubuntu_version(&b.vm_image_name))
                // Prefer the base image over app stacks of the same release.
                .then_with(|| base_rank(a).cmp(&base_rank(b)))
                .then_with(|| b.vm_image_id.cmp(&a.vm_image_id))
        })
        .map(|i| i.vm_image_id)
        .ok_or_else(|| {
            ProviderError::unavailable(format!(
                "no plain Ubuntu image found; pass --image with one of: {}",
                image_list(images)
            ))
        })
}

/// `(major, minor)` of the first `NN.NN` in an image name, e.g. `24.04`.
fn ubuntu_version(name: &str) -> (u32, u32) {
    name.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|t| {
            let (major, minor) = t.split_once('.')?;
            Some((major.parse().ok()?, minor.parse().ok()?))
        })
        .unwrap_or((0, 0))
}

fn base_rank(image: &Image) -> u8 {
    let text = format!("{} {}", image.vm_image_name, image.vm_image_description).to_lowercase();
    u8::from(
        ["base", "minimal", "server", "clean"]
            .iter()
            .any(|k| text.contains(k)),
    )
}

fn image_list(images: &[Image]) -> String {
    images
        .iter()
        .take(12)
        .map(|i| format!("{} ({})", i.vm_image_id, i.vm_image_name))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::api::{InstanceType, Region, Specs};
    use super::*;

    fn entry(name: &str, description: &str, cents: u64, regions: &[&str]) -> InventoryEntry {
        InventoryEntry {
            instance_type: InstanceType {
                name: name.into(),
                description: description.into(),
                price_cents_per_hour: Some(cents),
                specs: Specs::default(),
            },
            regions_with_capacity_available: regions
                .iter()
                .map(|r| Region {
                    name: (*r).into(),
                    description: String::new(),
                })
                .collect(),
            capacity_available: regions.len() as u32,
        }
    }

    fn inventory() -> Vec<InventoryEntry> {
        vec![
            entry("gpu_1x_a6000", "1x RTX A6000", 57, &["us-central-3"]),
            entry("gpu_2x_a6000", "2x RTX A6000", 114, &["us-central-3"]),
            entry("gpu_1x_a5000", "1x RTX A5000", 44, &["us-central-2"]),
            entry("gpu_1x_l40s", "1x L40S", 97, &["us-central-3"]),
            entry("gpu_1x_l40", "1x L40", 99, &[]),
            entry("gpu_1x_a100_80gb", "1x A100 80GB", 135, &["us-central-1"]),
            entry("gpu_1x_a10", "1x A10", 40, &["us-central-1"]),
            entry("cpu_32", "32 vCPU", 20, &["us-central-1"]),
        ]
    }

    fn request(ask: ResourceAsk, regions: &[&str]) -> ResourceRequest {
        ResourceRequest {
            ask,
            regions: regions.iter().map(|r| r.to_string()).collect(),
            zone: None,
            spot: false,
        }
    }

    fn gpus(name: &str, count: u32) -> ResourceAsk {
        ResourceAsk::Gpus(GpuRequest {
            name: name.into(),
            count,
        })
    }

    #[test]
    fn gpu_matching_respects_whole_tokens() {
        let inv = inventory();
        let names = |user: &str| -> Vec<String> {
            inv.iter()
                .filter(|e| gpu_name_matches(user, e))
                .map(|e| e.instance_type.name.clone())
                .collect()
        };
        assert_eq!(names("A6000"), ["gpu_1x_a6000", "gpu_2x_a6000"]);
        assert_eq!(names("RTXA6000"), ["gpu_1x_a6000", "gpu_2x_a6000"]);
        assert_eq!(names("L40"), ["gpu_1x_l40"], "L40 must not match L40S");
        assert_eq!(names("L40S"), ["gpu_1x_l40s"]);
        assert_eq!(names("A10"), ["gpu_1x_a10"], "A10 must not match A100");
        assert_eq!(names("A100-80GB"), ["gpu_1x_a100_80gb"]);
        assert!(names("H100").is_empty());
    }

    #[test]
    fn gpu_count_comes_from_the_product_name() {
        let inv = inventory();
        assert_eq!(gpu_count(&inv[0]), Some(1));
        assert_eq!(gpu_count(&inv[1]), Some(2));
        assert_eq!(gpu_count(&inv[7]), None, "CPU product");
    }

    #[test]
    fn offers_are_in_stock_count_matched_and_cheapest_first() {
        let offers = offers_from(&inventory(), &request(gpus("A6000", 1), &[])).unwrap();
        let names: Vec<&str> = offers.iter().map(|o| o.instance_type.as_str()).collect();
        assert_eq!(names, ["gpu_1x_a6000"]);
        assert_eq!(offers[0].region, ANY_REGION);
        assert_eq!(offers[0].hourly_price, Some(0.57));
        let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
        assert_eq!(detail.gpu_count, 1);

        // L40 has no capacity anywhere: it matches but yields no offer.
        assert!(offers_from(&inventory(), &request(gpus("L40", 1), &[]))
            .unwrap()
            .is_empty());

        // Two products with the same GPU: cheapest first.
        let mut inv = inventory();
        inv.push(entry(
            "gpu_1x_a6000_alt",
            "1x RTX A6000",
            50,
            &["us-central-1"],
        ));
        let offers = offers_from(&inv, &request(gpus("A6000", 1), &[])).unwrap();
        assert_eq!(offers[0].instance_type, "gpu_1x_a6000_alt");
    }

    #[test]
    fn region_filter_keeps_products_with_capacity_there() {
        let inv = inventory();
        let here = offers_from(&inv, &request(gpus("A6000", 1), &["US-CENTRAL-3"])).unwrap();
        assert_eq!(here.len(), 1);
        let elsewhere = offers_from(&inv, &request(gpus("A6000", 1), &["us-east-9"])).unwrap();
        assert!(elsewhere.is_empty());
    }

    #[test]
    fn exact_product_names_include_cpu_products() {
        let offers = offers_from(
            &inventory(),
            &request(ResourceAsk::InstanceType("cpu_32".into()), &[]),
        )
        .unwrap();
        assert_eq!(offers.len(), 1);
    }

    #[test]
    fn unknown_gpu_is_invalid_with_known_products() {
        let err = offers_from(&inventory(), &request(gpus("H100", 1), &[])).unwrap_err();
        assert_eq!(err.kind, crate::provider::ErrorKind::InvalidRequest);
        assert!(err.msg.contains("RTX A6000"), "{}", err.msg);
    }

    fn image(id: i64, name: &str, description: &str) -> Image {
        Image {
            vm_image_id: id,
            vm_image_name: name.into(),
            vm_image_description: description.into(),
        }
    }

    #[test]
    fn default_image_is_newest_plain_ubuntu() {
        let images = vec![
            image(3, "Ubuntu 22.04", "Base image"),
            image(7, "Art", "AI tools for artists"),
            image(9, "Ubuntu 24.04 Desktop", "Desktop"),
            image(11, "Ubuntu 24.04", "Base image"),
            image(12, "Ubuntu 24.04 ComfyUI", "App stack"),
        ];
        assert_eq!(choose_image(&images, None).unwrap(), 11);
        assert_eq!(choose_image(&images, Some("7")).unwrap(), 7);
        assert_eq!(choose_image(&images, Some("art")).unwrap(), 7);
        assert_eq!(
            choose_image(&images, Some("nope")).unwrap_err().kind,
            crate::provider::ErrorKind::InvalidRequest
        );
        let no_ubuntu = vec![image(7, "Art", "AI tools")];
        assert_eq!(
            choose_image(&no_ubuntu, None).unwrap_err().kind,
            crate::provider::ErrorKind::Unavailable
        );
    }
}
