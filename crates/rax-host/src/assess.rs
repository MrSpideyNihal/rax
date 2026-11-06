//! Turns probe results into a readiness verdict.
//!
//! Pure logic with no OS calls, so every rule is unit tested. The thresholds
//! are engineering judgement, recorded in `docs/FEASIBILITY.md`. They are not
//! measured limits.

use serde::Serialize;

use crate::host::HostInfo;
use crate::vulkan::{self, Device, DeviceKind};
use crate::whp::Capabilities;

const MIN_RAM_MIB: u64 = 8 * 1024;
const COMFORTABLE_RAM_MIB: u64 = 16 * 1024;
const LOW_AVAILABLE_RAM_MIB: u64 = 4 * 1024;
const MIN_LOGICAL_CPUS: usize = 4;
const ZERO_COPY_EXTENSIONS: [&str; 2] = [
    "VK_KHR_external_memory_win32",
    "VK_KHR_external_semaphore_win32",
];

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A requirement is met.
    Ok,
    /// RAX can run, at reduced quality.
    Warning,
    /// RAX cannot run until this is fixed.
    Blocker,
}

/// One readiness observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Severity.
    pub severity: Severity,
    /// Stable identifier for scripts.
    pub id: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

impl Finding {
    fn new(severity: Severity, id: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            id,
            message: message.into(),
        }
    }
}

/// Inputs to [`assess`]. An error string means that probe failed.
#[derive(Debug, Clone, Copy)]
pub struct Probes<'a> {
    /// Host facts.
    pub host: &'a HostInfo,
    /// Hypervisor capabilities, or why they could not be read.
    pub hypervisor: Result<&'a Capabilities, &'a str>,
    /// Vulkan devices, or why they could not be listed.
    pub vulkan: Result<&'a [Device], &'a str>,
}

/// Evaluates every readiness rule.
#[must_use]
pub fn assess(probes: Probes<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    assess_hypervisor(probes.hypervisor, &mut findings);
    assess_memory(probes.host, &mut findings);
    assess_cpu(probes.host, &mut findings);
    assess_graphics(probes.vulkan, &mut findings);
    findings
}

/// The most serious severity present, or `Ok` for no findings.
#[must_use]
pub fn overall(findings: &[Finding]) -> Severity {
    findings
        .iter()
        .map(|finding| finding.severity)
        .max()
        .unwrap_or(Severity::Ok)
}

fn assess_hypervisor(hypervisor: Result<&Capabilities, &str>, findings: &mut Vec<Finding>) {
    findings.push(match hypervisor {
        Err(reason) => Finding::new(
            Severity::Blocker,
            "whp.unavailable",
            format!("Windows Hypervisor Platform is unavailable: {reason}"),
        ),
        Ok(capabilities) if !capabilities.hypervisor_present => Finding::new(
            Severity::Blocker,
            "whp.no_hypervisor",
            "WHP is installed but no hypervisor is running. Enable virtualization in \
             firmware and the \"Windows Hypervisor Platform\" Windows feature, then reboot.",
        ),
        Ok(capabilities) => Finding::new(
            Severity::Ok,
            "whp.ready",
            format!(
                "Hypervisor is usable (vendor: {}, features: {:#x})",
                capabilities
                    .processor_vendor
                    .as_deref()
                    .unwrap_or("unknown"),
                capabilities.features
            ),
        ),
    });
}

fn assess_memory(host: &HostInfo, findings: &mut Vec<Finding>) {
    let total = host.total_ram_mib;
    findings.push(if total < MIN_RAM_MIB {
        Finding::new(
            Severity::Blocker,
            "ram.too_low",
            format!("{total} MiB of RAM is below the {MIN_RAM_MIB} MiB minimum"),
        )
    } else if total < COMFORTABLE_RAM_MIB {
        Finding::new(
            Severity::Warning,
            "ram.tight",
            format!("{total} MiB of RAM: the guest budget must stay small"),
        )
    } else {
        Finding::new(Severity::Ok, "ram.ok", format!("{total} MiB of RAM"))
    });
    if host.available_ram_mib < LOW_AVAILABLE_RAM_MIB {
        findings.push(Finding::new(
            Severity::Warning,
            "ram.pressure",
            format!(
                "Only {} MiB available now: benchmarks taken in this state are not comparable",
                host.available_ram_mib
            ),
        ));
    }
}

fn assess_cpu(host: &HostInfo, findings: &mut Vec<Finding>) {
    let cpus = host.logical_cpus;
    findings.push(if cpus < MIN_LOGICAL_CPUS {
        Finding::new(
            Severity::Warning,
            "cpu.few",
            format!("{cpus} logical CPUs: game, render and host threads will contend"),
        )
    } else {
        Finding::new(Severity::Ok, "cpu.ok", format!("{cpus} logical CPUs"))
    });
}

fn assess_graphics(devices: Result<&[Device], &str>, findings: &mut Vec<Finding>) {
    let devices = match devices {
        Err(reason) => {
            findings.push(Finding::new(
                Severity::Blocker,
                "gpu.no_loader",
                format!("Vulkan is unavailable: {reason}"),
            ));
            return;
        }
        Ok(devices) => devices,
    };
    let Some(best) = vulkan::preferred(devices) else {
        findings.push(Finding::new(
            Severity::Blocker,
            "gpu.no_device",
            "No Vulkan device found. RAX never falls back to software rendering for gaming.",
        ));
        return;
    };
    findings.push(match best.kind {
        DeviceKind::Other => Finding::new(
            Severity::Blocker,
            "gpu.not_hardware",
            format!("Best Vulkan device is not a hardware GPU: {}", best.name),
        ),
        DeviceKind::Integrated => Finding::new(
            Severity::Warning,
            "gpu.integrated_only",
            format!("Only an integrated GPU is available: {}", best.name),
        ),
        DeviceKind::Discrete => Finding::new(
            Severity::Ok,
            "gpu.discrete",
            format!(
                "Preferred GPU: {} ({} MiB local, Vulkan {})",
                best.name, best.device_local_mib, best.api_version
            ),
        ),
    });
    if devices.len() > 1 {
        findings.push(Finding::new(
            Severity::Warning,
            "gpu.multiple",
            format!(
                "{} GPUs present: the renderer must select {} explicitly, and the display \
                 path may add a cross-adapter copy",
                devices.len(),
                best.name
            ),
        ));
    }
    let missing: Vec<&str> = ZERO_COPY_EXTENSIONS
        .into_iter()
        .filter(|name| !best.has_extension(name))
        .collect();
    if !missing.is_empty() {
        findings.push(Finding::new(
            Severity::Warning,
            "gpu.no_zero_copy",
            format!(
                "{} lacks {}: presentation may need copies",
                best.name,
                missing.join(", ")
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(total_ram_mib: u64, available_ram_mib: u64, logical_cpus: usize) -> HostInfo {
        HostInfo {
            cpu: "test".to_owned(),
            logical_cpus,
            total_ram_mib,
            available_ram_mib,
            windows_version: "10.0.26300".to_owned(),
        }
    }

    fn capabilities(hypervisor_present: bool) -> Capabilities {
        Capabilities {
            hypervisor_present,
            features: 0,
            processor_vendor: None,
        }
    }

    fn gpu(kind: DeviceKind) -> Device {
        Device {
            name: "gpu".to_owned(),
            kind,
            api_version: "1.4.0".to_owned(),
            vendor_id: 0,
            device_local_mib: 4096,
            extensions: ZERO_COPY_EXTENSIONS.map(str::to_owned).to_vec(),
        }
    }

    fn ids(host: &HostInfo, caps: &Capabilities, devices: &[Device]) -> Vec<&'static str> {
        assess(Probes {
            host,
            hypervisor: Ok(caps),
            vulkan: Ok(devices),
        })
        .into_iter()
        .map(|finding| finding.id)
        .collect()
    }

    #[test]
    fn a_capable_machine_is_entirely_ok() {
        let findings = assess(Probes {
            host: &host(32 * 1024, 16 * 1024, 16),
            hypervisor: Ok(&capabilities(true)),
            vulkan: Ok(&[gpu(DeviceKind::Discrete)]),
        });
        assert_eq!(overall(&findings), Severity::Ok);
    }

    #[test]
    fn a_missing_hypervisor_blocks() {
        let found = ids(
            &host(32768, 16384, 16),
            &capabilities(false),
            &[gpu(DeviceKind::Discrete)],
        );
        assert!(found.contains(&"whp.no_hypervisor"));
    }

    #[test]
    fn an_unloadable_whp_blocks_with_the_reason() {
        let findings = assess(Probes {
            host: &host(32768, 16384, 16),
            hypervisor: Err("dll missing"),
            vulkan: Ok(&[gpu(DeviceKind::Discrete)]),
        });
        assert_eq!(overall(&findings), Severity::Blocker);
        assert!(findings[0].message.contains("dll missing"));
    }

    #[test]
    fn memory_thresholds_are_inclusive_at_the_lower_bound() {
        let d = [gpu(DeviceKind::Discrete)];
        let c = capabilities(true);
        assert!(ids(&host(8191, 8000, 8), &c, &d).contains(&"ram.too_low"));
        assert!(ids(&host(8192, 8000, 8), &c, &d).contains(&"ram.tight"));
        assert!(ids(&host(16384, 8000, 8), &c, &d).contains(&"ram.ok"));
    }

    #[test]
    fn low_available_memory_warns_about_benchmarks() {
        let found = ids(
            &host(32768, 1024, 8),
            &capabilities(true),
            &[gpu(DeviceKind::Discrete)],
        );
        assert!(found.contains(&"ram.pressure"));
    }

    #[test]
    fn software_only_vulkan_blocks() {
        let findings = assess(Probes {
            host: &host(32768, 16384, 8),
            hypervisor: Ok(&capabilities(true)),
            vulkan: Ok(&[gpu(DeviceKind::Other)]),
        });
        assert_eq!(overall(&findings), Severity::Blocker);
    }

    #[test]
    fn no_vulkan_devices_blocks() {
        let found = ids(&host(32768, 16384, 8), &capabilities(true), &[]);
        assert!(found.contains(&"gpu.no_device"));
    }

    #[test]
    fn hybrid_graphics_warns() {
        let devices = [gpu(DeviceKind::Integrated), gpu(DeviceKind::Discrete)];
        let found = ids(&host(32768, 16384, 8), &capabilities(true), &devices);
        assert!(found.contains(&"gpu.multiple"));
        assert!(found.contains(&"gpu.discrete"));
    }

    #[test]
    fn missing_zero_copy_extensions_warn() {
        let mut device = gpu(DeviceKind::Discrete);
        device.extensions.clear();
        let found = ids(&host(32768, 16384, 8), &capabilities(true), &[device]);
        assert!(found.contains(&"gpu.no_zero_copy"));
    }

    #[test]
    fn overall_of_nothing_is_ok() {
        assert_eq!(overall(&[]), Severity::Ok);
    }
}
