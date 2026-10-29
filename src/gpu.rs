//! What accelerator this machine has, and which PyTorch build suits it.

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gpu {
    Nvidia {
        name: String,
        driver: String,
        /// Highest CUDA version the driver supports, e.g. (12, 4).
        cuda: Option<(u32, u32)>,
        vram_mb: Option<u32>,
        compute_capability: Option<String>,
    },
    Amd {
        name: String,
    },
    /// Apple Silicon: Metal Performance Shaders through PyTorch's default wheels.
    Apple,
    None,
}

/// CUDA versions PyTorch publishes wheel indexes for, newest first. One line
/// to update when a new one appears; `--cuda <ver>` always overrides.
pub const PYTORCH_CUDA: &[(u32, u32)] = &[(12, 8), (12, 6), (12, 4), (12, 1), (11, 8)];

pub fn detect() -> Gpu {
    if let Some(gpu) = nvidia() {
        return gpu;
    }
    if let Some(gpu) = amd() {
        return gpu;
    }
    if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
        return Gpu::Apple;
    }
    Gpu::None
}

fn nvidia() -> Option<Gpu> {
    let query = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total,compute_cap",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let line = String::from_utf8_lossy(&query.stdout)
        .lines()
        .next()?
        .to_owned();
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    if fields.len() < 2 {
        return None;
    }
    // The supported CUDA version is only in the human-readable banner.
    let banner = Command::new("nvidia-smi")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let cuda = banner
        .split("CUDA Version:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(parse_major_minor);
    Some(Gpu::Nvidia {
        name: fields[0].to_owned(),
        driver: fields[1].to_owned(),
        cuda,
        vram_mb: fields.get(2).and_then(|v| v.parse().ok()),
        compute_capability: fields.get(3).map(|s| s.to_string()),
    })
}

fn amd() -> Option<Gpu> {
    let out = Command::new("rocm-smi")
        .args(["--showproductname", "--csv"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let name = text
        .lines()
        .find(|l| l.starts_with("card"))
        .and_then(|l| l.split(',').nth(1))
        .unwrap_or("AMD GPU")
        .trim()
        .to_owned();
    Some(Gpu::Amd { name })
}

pub fn parse_major_minor(s: &str) -> Option<(u32, u32)> {
    let mut it = s.trim().split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// The newest PyTorch CUDA index the driver can run, as `cu124`.
pub fn cuda_tag_for(driver_cuda: (u32, u32)) -> Option<String> {
    PYTORCH_CUDA
        .iter()
        .find(|v| **v <= driver_cuda)
        .map(|(a, b)| format!("cu{a}{b}"))
}

/// Which PyTorch index suits this machine: `cu124`, `rocm6.2`, `cpu`, or
/// `None` for PyPI's default wheels (macOS, or an unknown setup).
pub fn pytorch_variant(gpu: &Gpu) -> Option<String> {
    match gpu {
        Gpu::Nvidia { cuda: Some(c), .. } => cuda_tag_for(*c),
        Gpu::Nvidia { cuda: None, .. } => None,
        Gpu::Amd { .. } if cfg!(target_os = "linux") => Some("rocm6.2".to_owned()),
        Gpu::Amd { .. } | Gpu::Apple => None,
        // PyPI's Linux torch wheels pull 2+ GB of CUDA libraries a CPU box never uses.
        Gpu::None if cfg!(target_os = "linux") => Some("cpu".to_owned()),
        Gpu::None => None,
    }
}

/// Picks the profile that fits this machine from `profiles` (names like
/// `cu124`, `rocm6.2`, `cpu`): the newest CUDA profile the driver supports,
/// else `cpu`, else nothing.
pub fn profile_for<'a>(gpu: &Gpu, profiles: &'a [String]) -> Option<&'a String> {
    let max_cuda = match gpu {
        Gpu::Nvidia { cuda: Some(c), .. } => Some(*c),
        _ => None,
    };
    let mut best: Option<(&String, (u32, u32))> = None;
    for p in profiles {
        if let Some(v) = p.strip_prefix("cu").and_then(cuda_from_tag) {
            if max_cuda.is_some_and(|m| v <= m) && best.is_none_or(|(_, b)| v > b) {
                best = Some((p, v));
            }
        }
    }
    if let Some((p, _)) = best {
        return Some(p);
    }
    if matches!(gpu, Gpu::Amd { .. }) {
        if let Some(p) = profiles.iter().find(|p| p.starts_with("rocm")) {
            return Some(p);
        }
    }
    profiles.iter().find(|p| p.as_str() == "cpu")
}

/// `124` → (12, 4); `118` → (11, 8).
fn cuda_from_tag(digits: &str) -> Option<(u32, u32)> {
    if digits.len() < 3 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (major, minor) = digits.split_at(2);
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// One line for `doctor`.
pub fn describe(gpu: &Gpu) -> String {
    match gpu {
        Gpu::Nvidia {
            name,
            driver,
            cuda,
            vram_mb,
            compute_capability,
        } => format!(
            "NVIDIA {name}, driver {driver}{}{}{}",
            cuda.map(|(a, b)| format!(", CUDA {a}.{b}"))
                .unwrap_or_default(),
            vram_mb
                .map(|v| format!(", {} GB", v / 1024))
                .unwrap_or_default(),
            compute_capability
                .as_ref()
                .map(|c| format!(", compute {c}"))
                .unwrap_or_default()
        ),
        Gpu::Amd { name } => format!("AMD {name} (ROCm)"),
        Gpu::Apple => "Apple Silicon (MPS)".to_owned(),
        Gpu::None => "no GPU detected".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_drivers_to_indexes_and_profiles() {
        assert_eq!(cuda_tag_for((12, 9)).as_deref(), Some("cu128"));
        assert_eq!(cuda_tag_for((12, 4)).as_deref(), Some("cu124"));
        assert_eq!(cuda_tag_for((12, 3)).as_deref(), Some("cu121"));
        assert_eq!(cuda_tag_for((11, 0)), None);
        assert_eq!(parse_major_minor("12.4"), Some((12, 4)));
        let profiles: Vec<String> = ["cpu", "cu118", "cu124", "cu128"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let nv = Gpu::Nvidia {
            name: "x".into(),
            driver: "550".into(),
            cuda: Some((12, 6)),
            vram_mb: None,
            compute_capability: None,
        };
        assert_eq!(profile_for(&nv, &profiles).unwrap(), "cu124");
        assert_eq!(profile_for(&Gpu::None, &profiles).unwrap(), "cpu");
        assert_eq!(profile_for(&Gpu::Apple, &["cu124".to_string()]), None);
        assert_eq!(pytorch_variant(&nv).as_deref(), Some("cu126"));
        assert_eq!(pytorch_variant(&Gpu::Apple), None);
    }
}
