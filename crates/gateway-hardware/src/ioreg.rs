//! macOS GPU telemetry without privilege: `ioreg -r -d 1 -c IOAccelerator` prints each accelerator with a
//! `"PerformanceStatistics" = {...}` dictionary holding `Device Utilization %`, `Renderer Utilization %`,
//! `Tiler Utilization %`, `In use system memory` and `Alloc system memory` (unified memory, bytes). With
//! several accelerators the utilisation is the busiest one's and the memory is summed. The parser is pure;
//! the fixture is a verbatim capture from an M4 Max.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuReading {
    pub device_pct: Option<f32>,
    pub renderer_pct: Option<f32>,
    pub tiler_pct: Option<f32>,
    pub mem_in_use_bytes: Option<u64>,
    pub mem_alloc_bytes: Option<u64>,
    /// `"model"` of the accelerator, e.g. `Apple M4 Max`.
    pub model: Option<String>,
}

/// One accelerator's statistics dictionary.
#[derive(Debug, Clone, Default, PartialEq)]
struct Accelerator {
    device_pct: Option<f32>,
    renderer_pct: Option<f32>,
    tiler_pct: Option<f32>,
    mem_in_use: Option<u64>,
    mem_alloc: Option<u64>,
}

pub fn parse(output: &str) -> Option<GpuReading> {
    let mut accelerators = Vec::new();
    let mut model = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"PerformanceStatistics\" = {")
            && let Some(dict) = rest.strip_suffix('}')
        {
            accelerators.push(parse_dict(dict));
        } else if let Some(rest) = line.strip_prefix("\"model\" = \"")
            && let Some(name) = rest.strip_suffix('"')
            && model.is_none()
        {
            model = Some(name.to_string());
        }
    }
    if accelerators.is_empty() {
        return None;
    }
    let max = |pick: fn(&Accelerator) -> Option<f32>| {
        accelerators.iter().filter_map(pick).reduce(f32::max)
    };
    let sum = |pick: fn(&Accelerator) -> Option<u64>| {
        accelerators
            .iter()
            .filter_map(pick)
            .reduce(|a, b| a.saturating_add(b))
    };
    Some(GpuReading {
        device_pct: max(|a| a.device_pct),
        renderer_pct: max(|a| a.renderer_pct),
        tiler_pct: max(|a| a.tiler_pct),
        mem_in_use_bytes: sum(|a| a.mem_in_use),
        mem_alloc_bytes: sum(|a| a.mem_alloc),
        model,
    })
}

/// `"key"=value,"key"=value,...`; keys hold spaces and parentheses but never commas or quotes.
fn parse_dict(dict: &str) -> Accelerator {
    let mut acc = Accelerator::default();
    for pair in dict.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        let value = value.trim();
        match key {
            "Device Utilization %" => acc.device_pct = value.parse().ok(),
            "Renderer Utilization %" => acc.renderer_pct = value.parse().ok(),
            "Tiler Utilization %" => acc.tiler_pct = value.parse().ok(),
            "In use system memory" => acc.mem_in_use = value.parse().ok(),
            "Alloc system memory" => acc.mem_alloc = value.parse().ok(),
            _ => {}
        }
    }
    acc
}

/// Runs `ioreg` once. `None` when the command is missing, times out, or prints no accelerator.
pub async fn sample() -> Option<GpuReading> {
    let mut cmd = Command::new("ioreg");
    cmd.args(["-r", "-d", "1", "-c", "IOAccelerator"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            parse(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(Ok(output)) => {
            debug!(status = %output.status, "ioreg failed");
            None
        }
        Ok(Err(err)) => {
            debug!(error = %err, "ioreg could not run");
            None
        }
        Err(_) => {
            debug!("ioreg timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/hardware/ioreg-ioaccelerator-m4max.txt");

    #[test]
    fn the_m4_max_fixture_parses() {
        let reading = parse(FIXTURE).expect("one accelerator");
        assert_eq!(reading.model.as_deref(), Some("Apple M4 Max"));
        assert!(reading.device_pct.is_some());
        assert!(reading.renderer_pct.is_some());
        assert!(reading.tiler_pct.is_some());
        assert!(reading.mem_in_use_bytes.unwrap() > 0);
        assert!(reading.mem_alloc_bytes.unwrap() >= reading.mem_in_use_bytes.unwrap());
    }

    #[test]
    fn several_accelerators_take_the_max_utilisation_and_summed_memory() {
        let output = "+-o A\n    \"PerformanceStatistics\" = {\"Device Utilization %\"=10,\"In use system memory\"=100,\"Alloc system memory\"=200}\n\
                      +-o B\n    \"PerformanceStatistics\" = {\"Device Utilization %\"=40,\"Renderer Utilization %\"=7,\"In use system memory\"=50}\n";
        let reading = parse(output).unwrap();
        assert_eq!(reading.device_pct, Some(40.0));
        assert_eq!(reading.renderer_pct, Some(7.0));
        assert_eq!(reading.tiler_pct, None);
        assert_eq!(reading.mem_in_use_bytes, Some(150));
        assert_eq!(reading.mem_alloc_bytes, Some(200));
        assert_eq!(reading.model, None);
    }

    #[test]
    fn output_without_an_accelerator_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("+-o IOService  <class IOService>\n"), None);
    }

    /// `cargo test -p gateway-hardware live_ioreg -- --ignored --nocapture` (macOS only).
    #[tokio::test]
    #[ignore]
    async fn live_ioreg_sample() {
        let reading = sample().await;
        println!("{reading:?}");
        if cfg!(target_os = "macos") {
            assert!(reading.is_some());
        }
    }
}
