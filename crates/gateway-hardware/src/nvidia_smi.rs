//! Linux GPU telemetry: one `nvidia-smi --query-gpu=index,utilization.gpu,memory.used,memory.total
//! --format=csv,noheader,nounits` per tick, no privilege. Utilisation is the busiest GPU's, memory is summed
//! across GPUs. A field reported as `[N/A]` stays `None` without dropping the line; machines without the
//! NVIDIA driver fail to spawn it and show n/a. Ported from the monitors' `nvidia_smi.rs`, single-shot
//! instead of loop mode.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

const TIMEOUT: Duration = Duration::from_secs(2);
/// nvidia-smi reports memory in MiB.
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
struct GpuLine {
    util_pct: Option<f32>,
    mem_used_bytes: Option<u64>,
    mem_total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuReading {
    pub util_pct: Option<f32>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub gpu_count: usize,
}

/// Parses `index, utilization.gpu, memory.used, memory.total`; anything else is skipped.
fn parse_line(line: &str) -> Option<(u32, GpuLine)> {
    let mut fields = line.split(',').map(str::trim);
    let index = fields.next()?.parse::<u32>().ok()?;
    let util = fields.next()?;
    let used = fields.next()?;
    let total = fields.next();
    if fields.next().is_some() {
        return None;
    }
    Some((
        index,
        GpuLine {
            util_pct: util.parse().ok(),
            mem_used_bytes: used.parse::<u64>().ok().map(|mib| mib.saturating_mul(MIB)),
            mem_total_bytes: total
                .and_then(|t| t.parse::<u64>().ok())
                .map(|mib| mib.saturating_mul(MIB)),
        },
    ))
}

pub fn parse(output: &str) -> Option<GpuReading> {
    let mut gpus: BTreeMap<u32, GpuLine> = BTreeMap::new();
    for line in output.lines() {
        if let Some((index, reading)) = parse_line(line) {
            gpus.insert(index, reading);
        }
    }
    if gpus.is_empty() {
        return None;
    }
    Some(GpuReading {
        util_pct: gpus.values().filter_map(|g| g.util_pct).reduce(f32::max),
        mem_used_bytes: gpus
            .values()
            .filter_map(|g| g.mem_used_bytes)
            .reduce(|a, b| a.saturating_add(b)),
        mem_total_bytes: gpus
            .values()
            .filter_map(|g| g.mem_total_bytes)
            .reduce(|a, b| a.saturating_add(b)),
        gpu_count: gpus.len(),
    })
}

pub async fn sample() -> Option<GpuReading> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=index,utilization.gpu,memory.used,memory.total",
        "--format=csv,noheader,nounits",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            parse(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(Ok(output)) => {
            debug!(status = %output.status, "nvidia-smi failed");
            None
        }
        Ok(Err(err)) => {
            debug!(error = %err, "nvidia-smi could not run");
            None
        }
        Err(_) => {
            debug!("nvidia-smi timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_variants() {
        assert_eq!(
            parse_line("0, 26, 55, 16376"),
            Some((
                0,
                GpuLine {
                    util_pct: Some(26.0),
                    mem_used_bytes: Some(55 * MIB),
                    mem_total_bytes: Some(16376 * MIB),
                }
            ))
        );
        assert_eq!(parse_line("1, [N/A], 1024, 8192").unwrap().1.util_pct, None);
        assert_eq!(
            parse_line("0, 26, 55").unwrap().1.mem_total_bytes,
            None,
            "three-field output still parses"
        );
        assert_eq!(parse_line("No devices were found"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("0, 26, 55, 16376, extra"), None);
    }

    #[test]
    fn readings_take_max_util_and_summed_memory() {
        let reading =
            parse("0, 10, 1000, 8000\n1, 40, 2000, 8000\nNVIDIA-SMI has failed\n").unwrap();
        assert_eq!(reading.util_pct, Some(40.0));
        assert_eq!(reading.mem_used_bytes, Some(3000 * MIB));
        assert_eq!(reading.mem_total_bytes, Some(16000 * MIB));
        assert_eq!(reading.gpu_count, 2);
        assert_eq!(parse("No devices were found\n"), None);
        let na = parse("0, [N/A], [N/A], [N/A]\n").unwrap();
        assert_eq!((na.util_pct, na.mem_used_bytes), (None, None));
    }

    /// `cargo test -p gateway-hardware live_nvidia -- --ignored --nocapture` (Linux with the driver).
    #[tokio::test]
    #[ignore]
    async fn live_nvidia_smi_sample() {
        println!("{:?}", sample().await);
    }
}
