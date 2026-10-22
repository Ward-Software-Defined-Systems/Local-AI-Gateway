//! System CPU and memory and both backends' process trees via `sysinfo`.
//!
//! A process belongs to a backend when its executable path or `argv[0]` contains one of the path hints,
//! or its name matches. Ollama's name match is exact (`ollama`): a prefix match would count tools named
//! `ollama-*`. LM Studio's inference worker is a plain `node` process found only by its `~/.lmstudio/`
//! path, and its Linux daemon is `llmster`. On Linux `/proc/<pid>/exe` is unreadable for another user's
//! service, so the name and `argv[0]` checks do the work there. `without_tasks` keeps Linux threads from
//! counting as processes. The gateway's own process is never counted.

use gateway_core::types::ProcStats;
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, Process, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System, UpdateKind,
};

/// Directories only: a bare `/usr/local/bin/ollama` would also match a monitor installed there.
pub const OLLAMA_PATH_HINTS: &[&str] = &[
    "/Applications/Ollama.app/",
    "/opt/homebrew/opt/ollama/",
    "/opt/homebrew/Cellar/ollama/",
    "/usr/local/opt/ollama/",
    "/usr/local/Cellar/ollama/",
    "/usr/local/lib/ollama/",
    "/usr/lib/ollama/",
    "/snap/ollama/",
    "/.ollama/",
];

/// Matched exactly against the process name.
pub const OLLAMA_NAME_HINTS: &[&str] = &["ollama"];

/// `/LM Studio.app/` is the macOS app and its helpers; `/.lmstudio/` is everything LM Studio runs from its
/// home (the `node` inference worker, `llmster`, `llama-server`, the `lms` CLI); `/opt/LM-Studio/` the
/// Linux package.
pub const LMS_PATH_HINTS: &[&str] = &["/LM Studio.app/", "/.lmstudio/", "/opt/LM-Studio/"];

/// Substrings of the process name: the Linux app and its helpers, the headless daemon, engine processes.
pub const LMS_NAME_HINTS: &[&str] = &["lm-studio", "llmster", "llama-server", "mlx-llm"];

pub fn matches_ollama(exe: Option<&str>, arg0: Option<&str>, name: &str) -> bool {
    let path_hint = |s: &str| OLLAMA_PATH_HINTS.iter().any(|h| s.contains(h));
    exe.is_some_and(path_hint)
        || arg0.is_some_and(path_hint)
        || OLLAMA_NAME_HINTS
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
}

pub fn matches_lms(exe: Option<&str>, arg0: Option<&str>, name: &str) -> bool {
    let path_hint = |s: &str| LMS_PATH_HINTS.iter().any(|h| s.contains(h));
    exe.is_some_and(path_hint)
        || arg0.is_some_and(path_hint)
        || LMS_NAME_HINTS
            .iter()
            .any(|h| name.to_ascii_lowercase().contains(h))
}

/// What sysinfo reads per process: CPU, memory, the path and `argv[0]` once each, no threads.
pub fn process_refresh() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .without_tasks()
}

/// A `System` configured for the sampler. The first CPU reading is always zero; the sampler primes it.
pub fn new_system() -> System {
    System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::nothing().with_ram())
            .with_processes(process_refresh()),
    )
}

pub fn refresh(sys: &mut System) {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh());
}

/// The sysinfo part of a sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemReading {
    pub system_cpu_pct: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    pub lmstudio: ProcStats,
    pub ollama: ProcStats,
}

pub fn collect(sys: &System, own_pid: Pid) -> SystemReading {
    let cpu_count = sys.cpus().len().max(1) as f32;
    let system_cpu_pct = sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / cpu_count;
    let mut reading = SystemReading {
        system_cpu_pct,
        mem_used_bytes: sys.used_memory(),
        mem_total_bytes: sys.total_memory(),
        mem_available_bytes: sys.available_memory(),
        ..Default::default()
    };
    for (pid, proc_) in sys.processes() {
        // A thread listed as a process (only when the refresh kind asks for tasks) must not count again.
        if *pid == own_pid || proc_.thread_kind().is_some() {
            continue;
        }
        let (exe, arg0, name) = identity(proc_);
        let stats = if matches_lms(exe.as_deref(), arg0.as_deref(), &name) {
            &mut reading.lmstudio
        } else if matches_ollama(exe.as_deref(), arg0.as_deref(), &name) {
            &mut reading.ollama
        } else {
            continue;
        };
        stats.process_count += 1;
        stats.cpu_pct += proc_.cpu_usage();
        stats.rss_bytes += proc_.memory();
    }
    reading
}

fn identity(proc_: &Process) -> (Option<String>, Option<String>, String) {
    (
        proc_.exe().map(|p| p.to_string_lossy().into_owned()),
        proc_
            .cmd()
            .first()
            .map(|a| a.to_string_lossy().into_owned()),
        proc_.name().to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_processes_match_by_path_argv0_or_exact_name() {
        assert!(matches_ollama(
            Some("/Applications/Ollama.app/Contents/Resources/ollama"),
            None,
            "ollama"
        ));
        assert!(matches_ollama(
            None,
            Some("/usr/local/lib/ollama/runners/cuda/ollama_llama_server"),
            "ollama_llama_server"
        ));
        assert!(
            matches_ollama(None, None, "ollama"),
            "a systemd service with an unreadable exe"
        );
        assert!(
            !matches_ollama(
                Some("/usr/local/bin/ollama-monitor"),
                None,
                "ollama-monitor"
            ),
            "exact name, not prefix"
        );
        assert!(!matches_ollama(
            Some("/Users/w/projects/Local-AI-Gateway/target/debug/local-ai-gateway"),
            None,
            "local-ai-gateway"
        ));
    }

    #[test]
    fn lms_processes_match_by_path_argv0_or_name() {
        assert!(matches_lms(
            Some("/Applications/LM Studio.app/Contents/MacOS/LM Studio"),
            None,
            "LM Studio"
        ));
        assert!(matches_lms(
            Some("/home/u/.lmstudio/.internal/utils/node"),
            None,
            "node"
        ));
        assert!(matches_lms(
            Some("/home/u/.lmstudio/llmster/0.0.25-1/llmster"),
            None,
            "llmster"
        ));
        assert!(matches_lms(
            Some(
                "/home/u/.lmstudio/extensions/backends/llama.cpp-linux-x86_64-nvidia-cuda12-avx2-2.51.0/llama-server"
            ),
            None,
            "llama-server"
        ));
        assert!(matches_lms(
            None,
            Some("/home/svc/.lmstudio/.internal/utils/node"),
            "node"
        ));
        assert!(matches_lms(None, None, "llmster"));
        assert!(matches_lms(
            Some("/tmp/.mount_LM-Stu6HlfEK/lm-studio"),
            None,
            "lm-studio"
        ));
        assert!(matches_lms(
            Some("/opt/LM-Studio/lm-studio"),
            None,
            "lm-studio"
        ));
        assert!(!matches_lms(
            Some("/home/u/LMS-Monitor/target/release/lmstudio-monitor"),
            None,
            "lmstudio-monitor"
        ));
        assert!(!matches_lms(Some("/usr/bin/node"), Some("node"), "node"));
        assert!(!matches_lms(Some("/usr/local/bin/ollama"), None, "ollama"));
    }

    /// A real sysinfo pass never panics and reports memory, whatever is running.
    #[test]
    fn collect_reports_memory_on_this_machine() {
        let mut sys = new_system();
        refresh(&mut sys);
        let reading = collect(&sys, sysinfo::get_current_pid().unwrap());
        assert!(reading.mem_total_bytes > 0);
        assert!(reading.mem_used_bytes <= reading.mem_total_bytes);
        assert!(reading.mem_available_bytes <= reading.mem_total_bytes);
    }
}
