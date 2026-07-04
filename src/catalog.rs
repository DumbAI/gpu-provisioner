//! Tiny static catalog: GPU shorthand → EC2 instance type, plus GPU-family
//! detection (drives AMI selection). The mini analog of SkyPilot's catalog —
//! deliberately small and extensible; users can always pass `--instance-type`.

#[derive(Debug, Clone, Copy)]
pub struct GpuOffering {
    pub gpu: &'static str,
    pub count: u32,
    pub instance_type: &'static str,
}

pub const OFFERINGS: &[GpuOffering] = &[
    GpuOffering {
        gpu: "T4",
        count: 1,
        instance_type: "g4dn.xlarge",
    },
    GpuOffering {
        gpu: "T4",
        count: 4,
        instance_type: "g4dn.12xlarge",
    },
    GpuOffering {
        gpu: "A10G",
        count: 1,
        instance_type: "g5.xlarge",
    },
    GpuOffering {
        gpu: "A10G",
        count: 4,
        instance_type: "g5.12xlarge",
    },
    GpuOffering {
        gpu: "A10G",
        count: 8,
        instance_type: "g5.48xlarge",
    },
    GpuOffering {
        gpu: "L4",
        count: 1,
        instance_type: "g6.xlarge",
    },
    GpuOffering {
        gpu: "L40S",
        count: 1,
        instance_type: "g6e.xlarge",
    },
    GpuOffering {
        gpu: "A100",
        count: 8,
        instance_type: "p4d.24xlarge",
    },
    GpuOffering {
        gpu: "H100",
        count: 8,
        instance_type: "p5.48xlarge",
    },
];

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid --gpus value '{0}': expected NAME or NAME:COUNT (e.g. 'A10G:1')")]
    Malformed(String),
    #[error("no instance type known for '{gpu}:{count}'; available: {available}")]
    NotFound {
        gpu: String,
        count: u32,
        available: String,
    },
}

/// Resolve a GPU shorthand like `"A10G:1"` (count defaults to 1).
pub fn resolve_gpus(gpus: &str) -> Result<&'static GpuOffering, CatalogError> {
    let mut parts = gpus.split(':');
    let name = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CatalogError::Malformed(gpus.to_string()))?
        .to_uppercase();
    let count: u32 = match parts.next() {
        None => 1,
        Some(c) => c
            .parse()
            .map_err(|_| CatalogError::Malformed(gpus.to_string()))?,
    };
    if parts.next().is_some() {
        return Err(CatalogError::Malformed(gpus.to_string()));
    }
    OFFERINGS
        .iter()
        .find(|o| o.gpu == name && o.count == count)
        .ok_or_else(|| CatalogError::NotFound {
            gpu: name,
            count,
            available: OFFERINGS
                .iter()
                .map(|o| format!("{}:{}", o.gpu, o.count))
                .collect::<Vec<_>>()
                .join(", "),
        })
}

/// Whether an instance type carries an NVIDIA GPU (x86) — selects the GPU AMI.
/// (ARM GPU families like g5g are not supported by the x86 GPU AMI.)
pub fn is_gpu_instance_type(instance_type: &str) -> bool {
    let family = instance_type.split('.').next().unwrap_or("");
    matches!(
        family,
        "p2" | "p3"
            | "p3dn"
            | "p4d"
            | "p4de"
            | "p5"
            | "p5e"
            | "p5en"
            | "g3"
            | "g3s"
            | "g4dn"
            | "g4ad"
            | "g5"
            | "g6"
            | "g6e"
            | "gr6"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_shorthand() {
        assert_eq!(resolve_gpus("A10G:1").unwrap().instance_type, "g5.xlarge");
        assert_eq!(resolve_gpus("a10g").unwrap().instance_type, "g5.xlarge");
        assert_eq!(resolve_gpus("T4:4").unwrap().instance_type, "g4dn.12xlarge");
    }

    #[test]
    fn rejects_unknown_and_malformed() {
        assert!(matches!(
            resolve_gpus("B200:1"),
            Err(CatalogError::NotFound { .. })
        ));
        assert!(matches!(
            resolve_gpus("A10G:x"),
            Err(CatalogError::Malformed(_))
        ));
        assert!(matches!(
            resolve_gpus("A10G:1:2"),
            Err(CatalogError::Malformed(_))
        ));
    }

    #[test]
    fn gpu_family_detection() {
        assert!(is_gpu_instance_type("g5.xlarge"));
        assert!(is_gpu_instance_type("p4d.24xlarge"));
        assert!(!is_gpu_instance_type("t3.micro"));
        assert!(!is_gpu_instance_type("m7i.large"));
    }
}
