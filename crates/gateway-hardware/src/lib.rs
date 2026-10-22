//! Hardware telemetry, one `HardwareSample` per tick.
//!
//! - System CPU and memory, and each backend's process tree (count, CPU, RSS), come from `sysinfo`
//!   with no privilege (`process.rs`).
//! - GPU utilisation and memory come from `ioreg -r -d 1 -c IOAccelerator` on macOS (`ioreg.rs`, no
//!   sudo) and from `nvidia-smi` on Linux (`nvidia_smi.rs`). Each is a short-lived command per tick.
//! - Apple Neural Engine power is optional: `sudo -n /usr/bin/powermetrics` as a long-lived child
//!   (`powermetrics.rs`), which a service can only run with a NOPASSWD sudoers rule.
//!
//! Every parser compiles and is tested on every platform; only `sampler::run` picks a GPU source by
//! platform. Ported from the two monitors' `hardware/` modules, minus the interactive sudo prompt.

pub mod ioreg;
pub mod nvidia_smi;
pub mod powermetrics;
pub mod process;
pub mod sampler;

pub use sampler::{SamplerConfig, run};

/// Decimal units, as the TUIs and Activity Monitor show them.
pub fn format_bytes(b: u64) -> String {
    let f = b as f64;
    if f >= 1.0e9 {
        format!("{:.1} GB", f / 1.0e9)
    } else if f >= 1.0e6 {
        format!("{:.0} MB", f / 1.0e6)
    } else if f >= 1.0e3 {
        format!("{:.0} KB", f / 1.0e3)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "2 KB");
        assert_eq!(format_bytes(1_500_000), "2 MB");
        assert_eq!(format_bytes(38_200_000_000), "38.2 GB");
    }
}
