//! The sampler task: every `interval` it refreshes sysinfo on a blocking thread, reads the platform's
//! GPU source, folds in any powermetrics readings, and publishes one `HardwareSample`: on the event bus for
//! the dashboard, to the store for the charts, and into `latest` for `GET /api/hardware/current`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gateway_core::config::{GpuSource, PowermetricsMode};
use gateway_core::events::{EventSender, GatewayEvent, publish};
use gateway_core::store::StoreHandle;
use gateway_core::types::HardwareSample;
use tokio::sync::{RwLock, watch};
use tracing::{info, warn};

use crate::{ioreg, nvidia_smi, powermetrics, process};

#[derive(Debug, Clone)]
pub struct SamplerConfig {
    pub interval: Duration,
    pub gpu_source: GpuSource,
    pub powermetrics: PowermetricsMode,
}

/// The source actually used after resolving `auto` for this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGpuSource {
    Ioreg,
    NvidiaSmi,
    Disabled,
}

pub fn resolve_gpu_source(configured: GpuSource) -> ResolvedGpuSource {
    match configured {
        GpuSource::Ioreg => ResolvedGpuSource::Ioreg,
        GpuSource::NvidiaSmi => ResolvedGpuSource::NvidiaSmi,
        GpuSource::Disabled => ResolvedGpuSource::Disabled,
        GpuSource::Auto => {
            if cfg!(target_os = "macos") {
                ResolvedGpuSource::Ioreg
            } else if cfg!(target_os = "linux") {
                ResolvedGpuSource::NvidiaSmi
            } else {
                ResolvedGpuSource::Disabled
            }
        }
    }
}

pub type Latest = Arc<RwLock<Option<HardwareSample>>>;

pub async fn run(
    cfg: SamplerConfig,
    events: EventSender,
    store: Option<StoreHandle>,
    latest: Latest,
    mut shutdown: watch::Receiver<bool>,
) {
    let gpu_source = resolve_gpu_source(cfg.gpu_source);
    let interval = cfg.interval.max(Duration::from_secs(1));
    info!(?gpu_source, powermetrics = ?cfg.powermetrics, interval_s = interval.as_secs(), "hardware sampler started");

    let telemetry = match cfg.powermetrics {
        PowermetricsMode::Sudo if cfg!(target_os = "macos") => powermetrics::spawn(interval),
        PowermetricsMode::Sudo => {
            warn!("hardware.powermetrics = \"sudo\" only applies to macOS; ignored");
            None
        }
        PowermetricsMode::Off => None,
    };
    let own_pid = sysinfo::get_current_pid().ok();
    let mut sys = Some(process::new_system());
    // sysinfo needs two CPU samples spaced apart; the first reading is always zero.
    if let Some(s) = sys.as_mut() {
        process::refresh(s);
    }
    let mut gpu_failures = 0u32;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        let Some(mut system) = sys.take() else { break };
        let (system, reading) = match tokio::task::spawn_blocking(move || {
            process::refresh(&mut system);
            let reading = own_pid
                .map(|pid| process::collect(&system, pid))
                .unwrap_or_default();
            (system, reading)
        })
        .await
        {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %err, "hardware sampling task failed; sampler stopping");
                break;
            }
        };
        sys = Some(system);

        let mut sample = HardwareSample {
            at: Utc::now(),
            system_cpu_pct: reading.system_cpu_pct,
            mem_used_bytes: reading.mem_used_bytes,
            mem_total_bytes: reading.mem_total_bytes,
            mem_available_bytes: reading.mem_available_bytes,
            lmstudio: reading.lmstudio,
            ollama: reading.ollama,
            ..Default::default()
        };
        match gpu_source {
            ResolvedGpuSource::Ioreg => match ioreg::sample().await {
                Some(gpu) => {
                    sample.gpu_util_pct = gpu.device_pct;
                    sample.gpu_renderer_pct = gpu.renderer_pct;
                    sample.gpu_tiler_pct = gpu.tiler_pct;
                    sample.gpu_mem_used_bytes = gpu.mem_in_use_bytes;
                    sample.gpu_mem_alloc_bytes = gpu.mem_alloc_bytes;
                    gpu_failures = 0;
                }
                None => note_gpu_failure(&mut gpu_failures, "ioreg"),
            },
            ResolvedGpuSource::NvidiaSmi => match nvidia_smi::sample().await {
                Some(gpu) => {
                    sample.gpu_util_pct = gpu.util_pct;
                    sample.gpu_mem_used_bytes = gpu.mem_used_bytes;
                    sample.gpu_mem_alloc_bytes = gpu.mem_total_bytes;
                    gpu_failures = 0;
                }
                None => note_gpu_failure(&mut gpu_failures, "nvidia-smi"),
            },
            ResolvedGpuSource::Disabled => {}
        }
        if let Some(handle) = &telemetry {
            let readings = *handle.readings.lock().await;
            sample.ane_power_mw = readings.ane_power_mw;
            if sample.gpu_util_pct.is_none() {
                sample.gpu_util_pct = readings.gpu_util_pct;
            }
        }

        *latest.write().await = Some(sample.clone());
        if let Some(store) = &store
            && let Err(err) = store.persist_hardware(sample.clone()).await
        {
            warn!(error = %format!("{err:#}"), "could not persist hardware sample");
        }
        publish(&events, GatewayEvent::Hardware { sample });
    }

    if let Some(handle) = telemetry {
        handle.terminate().await;
    }
    info!("hardware sampler stopped");
}

/// Warn once when a GPU source stops answering, not every two seconds.
fn note_gpu_failure(failures: &mut u32, program: &str) {
    *failures += 1;
    if *failures == 1 {
        warn!("{program} gave no GPU reading; GPU figures show n/a until it answers again");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_resolves_per_platform_and_explicit_sources_stay() {
        let auto = resolve_gpu_source(GpuSource::Auto);
        if cfg!(target_os = "macos") {
            assert_eq!(auto, ResolvedGpuSource::Ioreg);
        } else if cfg!(target_os = "linux") {
            assert_eq!(auto, ResolvedGpuSource::NvidiaSmi);
        }
        assert_eq!(
            resolve_gpu_source(GpuSource::NvidiaSmi),
            ResolvedGpuSource::NvidiaSmi
        );
        assert_eq!(
            resolve_gpu_source(GpuSource::Disabled),
            ResolvedGpuSource::Disabled
        );
    }

    /// Runs the sampler with the GPU source off: a real sysinfo sample arrives within a few seconds and the
    /// GPU fields stay `None`.
    #[tokio::test]
    async fn the_sampler_emits_a_sample_without_a_gpu_source() {
        let (events, mut rx) = gateway_core::events::channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let latest: Latest = Arc::default();
        let task = tokio::spawn(run(
            SamplerConfig {
                interval: Duration::from_secs(1),
                gpu_source: GpuSource::Disabled,
                powermetrics: PowermetricsMode::Off,
            },
            events,
            None,
            latest.clone(),
            shutdown_rx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a sample")
            .unwrap();
        match &*event {
            GatewayEvent::Hardware { sample } => {
                assert!(sample.mem_total_bytes > 0);
                assert_eq!(sample.gpu_util_pct, None);
                assert_eq!(sample.ane_power_mw, None);
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(latest.read().await.is_some());
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("sampler stops")
            .unwrap();
    }
}
