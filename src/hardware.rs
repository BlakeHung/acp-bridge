//! Best-effort hardware backend detection for the local LLM scenario.
//!
//! acp-bridge runs against a user-configured LLM endpoint — Ollama / vLLM /
//! llama.cpp / LM Studio / etc. Picking the right *backend driver* and the
//! right model size depends on what the host machine actually has. This
//! module probes the OS, CPU arch, and GPU(s) at startup and prints a short
//! report to stderr so operators don't have to guess.
//!
//! All probing is **offline** — no network calls, no telemetry. Probes that
//! require external tools (`nvidia-smi`, `rocm-smi`) are skipped silently
//! when the tool is absent.

use std::process::Command;

#[derive(Debug, Clone)]
pub struct HardwareInfo {
    pub os: &'static str,
    pub arch: &'static str,
    pub gpus: Vec<GpuInfo>,
    /// Operator-facing hints — what backend / flags are likely a good fit.
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub name: String,
    /// VRAM in megabytes, when known.
    pub vram_mb: Option<u64>,
    /// Acceleration API available for this GPU on this OS.
    pub accel: Accel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Apple,
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    Metal,
    Cuda,
    Rocm,
    Vulkan,
    None,
}

pub fn detect() -> HardwareInfo {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let mut gpus = Vec::new();

    if os == "macos" && arch == "aarch64" {
        gpus.push(GpuInfo {
            vendor: GpuVendor::Apple,
            name: "Apple Silicon GPU + Neural Engine (unified memory)".into(),
            vram_mb: None,
            accel: Accel::Metal,
        });
    }

    if os == "linux" {
        gpus.extend(detect_nvidia());
        gpus.extend(detect_amd());
    }

    let recommendations = build_recommendations(os, &gpus);

    HardwareInfo {
        os,
        arch,
        gpus,
        recommendations,
    }
}

/// Probe NVIDIA GPUs via `nvidia-smi`.
fn detect_nvidia() -> Vec<GpuInfo> {
    let out = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    parse_nvidia_smi(&String::from_utf8_lossy(&out))
}

fn parse_nvidia_smi(text: &str) -> Vec<GpuInfo> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            // Split from the right: rocm-smi / nvidia-smi product names can
            // legitimately contain commas (e.g. "NVIDIA GeForce RTX 4090,
            // Ada"). The last comma always separates name from VRAM.
            let mut parts = line.rsplitn(2, ',').map(str::trim);
            let vram_mb = parts.next()?.parse::<u64>().ok();
            let name = parts.next()?.to_string();
            Some(GpuInfo {
                vendor: GpuVendor::Nvidia,
                name,
                vram_mb,
                accel: Accel::Cuda,
            })
        })
        .collect()
}

/// Probe AMD GPUs via `rocm-smi` first, falling back to a sysfs scan for the
/// Vulkan-only case (no ROCm installed).
fn detect_amd() -> Vec<GpuInfo> {
    if let Ok(out) = Command::new("rocm-smi")
        .args(["--showproductname", "--showmeminfo", "vram", "--csv"])
        .output()
    {
        if out.status.success() {
            let parsed = parse_rocm_smi(&String::from_utf8_lossy(&out.stdout));
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    scan_sysfs_amd()
}

fn parse_rocm_smi(text: &str) -> Vec<GpuInfo> {
    // rocm-smi CSV header line then card rows. Format varies across versions,
    // so we look for any line that mentions "card" and tries to pull a name +
    // a VRAM total in bytes from the same row.
    let mut gpus = Vec::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if !lower.starts_with("card") {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        // The first column is the card identifier (`card0`, `card1`, …). The
        // earlier filter "skip anything that equals `card`" never matched
        // those identifiers, so the parser was picking `card0` as the GPU
        // name. Take the second column as the product name when present.
        let name = fields
            .get(1)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "AMD GPU".to_string());
        // rocm-smi versions differ on the VRAM unit: older ones emit bytes
        // (e.g. 17163091968), newer ones can emit MB (e.g. 16368). Take the
        // largest parseable number on the row and convert if it looks like
        // bytes (> 100 MB worth).
        let vram_mb = fields
            .iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .max()
            .map(|v| if v > 100_000_000 { v / 1024 / 1024 } else { v });
        gpus.push(GpuInfo {
            vendor: GpuVendor::Amd,
            name,
            vram_mb,
            accel: Accel::Rocm,
        });
    }
    gpus
}

/// Sysfs scan for AMD GPUs when rocm-smi is not installed. Only reports
/// presence + vendor; VRAM is unknown without ROCm or hwmon parsing.
fn scan_sysfs_amd() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    let drm = match std::fs::read_dir("/sys/class/drm") {
        Ok(d) => d,
        Err(_) => return gpus,
    };
    for entry in drm.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.starts_with("card") || n.contains('-') {
            continue;
        }
        let vendor_path = entry.path().join("device").join("vendor");
        let Ok(vendor_raw) = std::fs::read_to_string(&vendor_path) else {
            continue;
        };
        // Some kernel versions write the vendor ID without the `0x` prefix
        // or in upper case. Normalize before comparing.
        let vendor = vendor_raw.trim().to_lowercase();
        if vendor == "0x1002" || vendor == "1002" {
            gpus.push(GpuInfo {
                vendor: GpuVendor::Amd,
                name: format!("AMD GPU ({n}, ROCm not installed — Vulkan only)"),
                vram_mb: None,
                accel: Accel::Vulkan,
            });
        }
    }
    gpus
}

fn build_recommendations(os: &str, gpus: &[GpuInfo]) -> Vec<String> {
    let mut hints = Vec::new();

    let nv_count = gpus
        .iter()
        .filter(|g| g.vendor == GpuVendor::Nvidia)
        .count();
    let nv_vram_mb: u64 = gpus
        .iter()
        .filter(|g| g.vendor == GpuVendor::Nvidia)
        .filter_map(|g| g.vram_mb)
        .sum();

    if os == "macos" && gpus.iter().any(|g| g.vendor == GpuVendor::Apple) {
        hints.push(
            "Apple Silicon: Ollama uses Metal automatically; for >7B models consider MLX backends."
                .into(),
        );
    }

    if nv_count >= 2 {
        hints.push(format!(
            "Multi-GPU NVIDIA ({nv_count}× cards, {} GB total VRAM): vLLM with --tensor-parallel-size {nv_count}, or Ollama with OLLAMA_SCHED_SPREAD=1.",
            nv_vram_mb / 1024
        ));
    } else if nv_count == 1 {
        hints.push(format!(
            "Single NVIDIA GPU ({} GB VRAM): llama.cpp with --n-gpu-layers high, or Ollama with default CUDA.",
            nv_vram_mb / 1024
        ));
    }

    if gpus.iter().any(|g| g.accel == Accel::Rocm) {
        hints.push(
            "AMD GPU with ROCm: llama.cpp ROCm build, or Ollama (ROCm support via HSA_OVERRIDE_GFX_VERSION when needed)."
                .into(),
        );
    } else if gpus.iter().any(|g| g.accel == Accel::Vulkan) {
        hints.push(
            "AMD GPU without ROCm: llama.cpp Vulkan backend is usually the cleanest path.".into(),
        );
    }

    if gpus.is_empty() {
        hints.push(
            "No GPU detected — acp-bridge will still work against any CPU-served LLM (llama.cpp, Ollama CPU)."
                .into(),
        );
    }

    hints
}

impl HardwareInfo {
    /// Format the detection report as multi-line text for stderr/log emission.
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("Platform: {} {}", self.os, self.arch)];
        if self.gpus.is_empty() {
            lines.push("GPU: none detected".into());
        } else {
            for g in &self.gpus {
                let vram = g
                    .vram_mb
                    .map(|m| format!(", {} MB VRAM", m))
                    .unwrap_or_default();
                lines.push(format!("GPU: {} ({:?}{vram})", g.name, g.accel));
            }
        }
        for r in &self.recommendations {
            lines.push(format!("Hint: {r}"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_two_gpus() {
        let sample = "NVIDIA GeForce RTX 3090 Ti, 24576\nNVIDIA GeForce RTX 3090 Ti, 24576\n";
        let gpus = parse_nvidia_smi(sample);
        assert_eq!(gpus.len(), 2);
        assert!(gpus[0].name.contains("3090 Ti"));
        assert_eq!(gpus[0].vram_mb, Some(24576));
        assert_eq!(gpus[0].accel, Accel::Cuda);
    }

    #[test]
    fn parses_nvidia_smi_ignores_blank_lines() {
        let sample = "\nNVIDIA H100, 81920\n\n";
        let gpus = parse_nvidia_smi(sample);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vram_mb, Some(81920));
    }

    #[test]
    fn parses_nvidia_smi_handles_missing_vram() {
        let sample = "Garbage line without comma\n";
        let gpus = parse_nvidia_smi(sample);
        assert!(gpus.is_empty());
    }

    #[test]
    fn parses_nvidia_smi_preserves_commas_in_name() {
        // rsplitn keeps the trailing VRAM column even when the product name
        // contains commas. With splitn the name would be truncated and the
        // remainder would fail to parse as u64.
        let sample = "NVIDIA GeForce RTX 4090, Ada, 24576\n";
        let gpus = parse_nvidia_smi(sample);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4090, Ada");
        assert_eq!(gpus[0].vram_mb, Some(24576));
    }

    #[test]
    fn parses_rocm_smi_takes_product_name_not_card_id() {
        let sample =
            "card0, AMD Radeon RX 6800 XT, 17163091968\ncard1, AMD Radeon Pro W6800, 34326183936\n";
        let gpus = parse_rocm_smi(sample);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "AMD Radeon RX 6800 XT");
        assert_eq!(gpus[1].name, "AMD Radeon Pro W6800");
        assert_eq!(gpus[0].accel, Accel::Rocm);
        // VRAM bytes → MB conversion: 17163091968 / 1024 / 1024 = 16368
        assert_eq!(gpus[0].vram_mb, Some(16368));
    }

    #[test]
    fn parses_rocm_smi_falls_back_when_name_missing() {
        let sample = "card0,,17163091968\n";
        let gpus = parse_rocm_smi(sample);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "AMD GPU");
    }

    #[test]
    fn parses_rocm_smi_accepts_mb_unit_when_bytes_not_emitted() {
        // Some newer rocm-smi builds emit VRAM in MB rather than bytes. The
        // parser should keep the value as-is instead of returning None.
        let sample = "card0, AMD Radeon RX 7900 XTX, 24576\n";
        let gpus = parse_rocm_smi(sample);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vram_mb, Some(24576));
    }

    #[test]
    fn recommendations_for_dual_nvidia() {
        let gpus = vec![
            GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: "RTX 3090 Ti".into(),
                vram_mb: Some(24576),
                accel: Accel::Cuda,
            },
            GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: "RTX 3090 Ti".into(),
                vram_mb: Some(24576),
                accel: Accel::Cuda,
            },
        ];
        let hints = build_recommendations("linux", &gpus);
        assert!(hints.iter().any(|h| h.contains("tensor-parallel-size 2")));
        assert!(hints.iter().any(|h| h.contains("48 GB")));
    }

    #[test]
    fn recommendations_for_apple_silicon() {
        let gpus = vec![GpuInfo {
            vendor: GpuVendor::Apple,
            name: "Apple Silicon".into(),
            vram_mb: None,
            accel: Accel::Metal,
        }];
        let hints = build_recommendations("macos", &gpus);
        assert!(hints.iter().any(|h| h.contains("Metal")));
    }

    #[test]
    fn recommendations_for_no_gpu_still_useful() {
        let hints = build_recommendations("linux", &[]);
        assert!(hints.iter().any(|h| h.contains("CPU-served")));
    }

    #[test]
    fn report_lines_includes_platform_and_hints() {
        let hw = HardwareInfo {
            os: "linux",
            arch: "x86_64",
            gpus: vec![GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: "RTX 3090 Ti".into(),
                vram_mb: Some(24576),
                accel: Accel::Cuda,
            }],
            recommendations: vec!["Test hint".into()],
        };
        let lines = hw.report_lines();
        assert!(lines[0].contains("linux"));
        assert!(lines.iter().any(|l| l.contains("3090 Ti")));
        assert!(lines.iter().any(|l| l.contains("Test hint")));
    }
}
