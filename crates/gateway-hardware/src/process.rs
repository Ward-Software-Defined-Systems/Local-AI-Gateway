//! System CPU and memory and both backends' process trees via `sysinfo`.
//!
//! A process belongs to a backend on strong evidence first: Ollama's exact name or a path under its
//! install, LM Studio's install paths or its own process names (`llmster`, `lm-studio`). The inference
//! engines are weak evidence, because both servers run them: Ollama serves GGUF models through a
//! `llama-server` child (its MLX models through `ollama runner`) and LM Studio runs `llama-server` and
//! `mlx-llm` workers of its own, so an engine process goes to whichever backend owns its nearest
//! identified ancestor, and an engine with no known ancestor counts as LM Studio, as the terminal monitor
//! did. Ollama's name match is exact (`ollama`): a prefix match would count tools named `ollama-*`. LM
//! Studio's `node` worker is found only by its `~/.lmstudio/` path. On Linux `/proc/<pid>/exe` is
//! unreadable for another user's service, so the name, `argv[0]` and parent checks do the work there.
//! `without_tasks` keeps Linux threads from counting as processes. The gateway's own process is never
//! counted.

use std::collections::HashMap;

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

/// Substrings of the process name that are LM Studio's own: the Linux app and its helpers, the headless
/// daemon.
pub const LMS_NAME_HINTS: &[&str] = &["lm-studio", "llmster"];

/// Substrings of the process name of an inference engine either server may run.
pub const ENGINE_NAME_HINTS: &[&str] = &["llama-server", "mlx-llm"];

/// How far up the parent chain an engine looks for its server.
const MAX_ANCESTORS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    LmStudio,
    Ollama,
}

pub fn matches_ollama(exe: Option<&str>, arg0: Option<&str>, name: &str) -> bool {
    let path_hint = |s: &str| OLLAMA_PATH_HINTS.iter().any(|h| s.contains(h));
    exe.is_some_and(path_hint)
        || arg0.is_some_and(path_hint)
        || OLLAMA_NAME_HINTS
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
}

/// LM Studio by its own paths and names; an engine name alone is not enough (see `is_engine`).
pub fn matches_lms(exe: Option<&str>, arg0: Option<&str>, name: &str) -> bool {
    let path_hint = |s: &str| LMS_PATH_HINTS.iter().any(|h| s.contains(h));
    exe.is_some_and(path_hint)
        || arg0.is_some_and(path_hint)
        || LMS_NAME_HINTS
            .iter()
            .any(|h| name.to_ascii_lowercase().contains(h))
}

/// An inference engine process that both servers spawn.
pub fn is_engine(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ENGINE_NAME_HINTS.iter().any(|h| lower.contains(h))
}

/// Strong evidence only. Ollama is tested first so that its `llama-server` under `/usr/local/lib/ollama/`
/// is its own, whatever LM Studio's hints say.
pub fn identity(exe: Option<&str>, arg0: Option<&str>, name: &str) -> Option<Owner> {
    if matches_ollama(exe, arg0, name) {
        Some(Owner::Ollama)
    } else if matches_lms(exe, arg0, name) {
        Some(Owner::LmStudio)
    } else {
        None
    }
}

/// What the matcher knows about one process; `collect` fills it from sysinfo, tests build it by hand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub exe: Option<String>,
    pub arg0: Option<String>,
    pub name: String,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
}

/// Sums each backend's processes: strong identities first, then the engines by their nearest identified
/// ancestor, an orphaned engine to LM Studio.
pub fn attribute(procs: &[ProcessInfo], own_pid: u32) -> (ProcStats, ProcStats) {
    let by_pid: HashMap<u32, &ProcessInfo> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut owners: HashMap<u32, Owner> = procs
        .iter()
        .filter(|p| p.pid != own_pid)
        .filter_map(|p| identity(p.exe.as_deref(), p.arg0.as_deref(), &p.name).map(|o| (p.pid, o)))
        .collect();
    for p in procs {
        if p.pid == own_pid || owners.contains_key(&p.pid) || !is_engine(&p.name) {
            continue;
        }
        let mut owner = None;
        let mut cursor = p.parent;
        for _ in 0..MAX_ANCESTORS {
            let Some(pid) = cursor else { break };
            if let Some(found) = owners.get(&pid) {
                owner = Some(*found);
                break;
            }
            cursor = by_pid.get(&pid).and_then(|q| q.parent);
        }
        owners.insert(p.pid, owner.unwrap_or(Owner::LmStudio));
    }
    let mut lmstudio = ProcStats::default();
    let mut ollama = ProcStats::default();
    for p in procs {
        let stats = match owners.get(&p.pid) {
            Some(Owner::LmStudio) => &mut lmstudio,
            Some(Owner::Ollama) => &mut ollama,
            None => continue,
        };
        stats.process_count += 1;
        stats.cpu_pct += p.cpu_pct;
        stats.rss_bytes += p.rss_bytes;
    }
    (lmstudio, ollama)
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
    let procs: Vec<ProcessInfo> = sys
        .processes()
        .iter()
        // A thread listed as a process (only when the refresh kind asks for tasks) must not count again.
        .filter(|(_, proc_)| proc_.thread_kind().is_none())
        .map(|(pid, proc_)| info(*pid, proc_))
        .collect();
    let (lmstudio, ollama) = attribute(&procs, own_pid.as_u32());
    SystemReading {
        system_cpu_pct,
        mem_used_bytes: sys.used_memory(),
        mem_total_bytes: sys.total_memory(),
        mem_available_bytes: sys.available_memory(),
        lmstudio,
        ollama,
    }
}

fn info(pid: Pid, proc_: &Process) -> ProcessInfo {
    ProcessInfo {
        pid: pid.as_u32(),
        parent: proc_.parent().map(|p| p.as_u32()),
        exe: proc_.exe().map(|p| p.to_string_lossy().into_owned()),
        arg0: proc_
            .cmd()
            .first()
            .map(|a| a.to_string_lossy().into_owned()),
        name: proc_.name().to_string_lossy().into_owned(),
        cpu_pct: proc_.cpu_usage(),
        rss_bytes: proc_.memory(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(
        pid: u32,
        parent: Option<u32>,
        exe: Option<&str>,
        arg0: Option<&str>,
        name: &str,
        cpu_pct: f32,
        rss_bytes: u64,
    ) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent,
            exe: exe.map(str::to_string),
            arg0: arg0.map(str::to_string),
            name: name.to_string(),
            cpu_pct,
            rss_bytes,
        }
    }

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
    fn lms_processes_match_by_path_argv0_or_own_name_and_engines_are_weak() {
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
        assert!(
            !matches_lms(None, None, "llama-server"),
            "an engine name alone names no backend"
        );
        assert!(is_engine("llama-server"));
        assert!(is_engine("mlx-llm"));
        assert!(!is_engine("ollama"));
        assert_eq!(
            identity(
                Some("/Applications/Ollama.app/Contents/Resources/llama-server"),
                None,
                "llama-server"
            ),
            Some(Owner::Ollama),
            "Ollama's bundled engine on macOS is Ollama's"
        );
        assert_eq!(identity(None, None, "llama-server"), None);
    }

    /// The Linux layout: `ollama serve` as a service user (unreadable exe), its `llama-server` child with
    /// the install path in `argv[0]`, and a second child whose `argv[0]` is bare, so only the parent chain
    /// can place it. Both are Ollama's; LM Studio gets nothing.
    #[test]
    fn ollamas_llama_server_counts_for_ollama_by_path_and_by_parent() {
        let procs = vec![
            proc(
                1,
                None,
                Some("/sbin/init"),
                Some("/sbin/init"),
                "systemd",
                0.0,
                1,
            ),
            proc(
                100,
                Some(1),
                None,
                Some("/usr/local/bin/ollama"),
                "ollama",
                2.0,
                100,
            ),
            proc(
                101,
                Some(100),
                None,
                Some("/usr/local/lib/ollama/llama-server"),
                "llama-server",
                250.0,
                8_000,
            ),
            proc(
                102,
                Some(100),
                None,
                Some("llama-server"),
                "llama-server",
                50.0,
                1_000,
            ),
            proc(
                7,
                Some(1),
                Some("/usr/bin/node"),
                Some("node"),
                "node",
                5.0,
                50,
            ),
        ];
        let (lms, ollama) = attribute(&procs, 999);
        assert_eq!(ollama.process_count, 3);
        assert_eq!(ollama.cpu_pct, 302.0);
        assert_eq!(ollama.rss_bytes, 9_100);
        assert_eq!(
            lms,
            ProcStats::default(),
            "nothing of Ollama's is LM Studio's"
        );
    }

    /// LM Studio's workers: `llmster` with a bare `llama-server` child, found through the chain, and a
    /// `node` worker found by its path. A `llama-server` nobody owns stays LM Studio's, as before. The
    /// gateway's own process is never counted.
    #[test]
    fn lm_studio_workers_count_for_lm_studio_and_orphans_keep_the_old_default() {
        let procs = vec![
            proc(1, None, None, None, "systemd", 0.0, 0),
            proc(
                200,
                Some(1),
                Some("/home/u/.lmstudio/llmster/0.0.25-1/llmster"),
                None,
                "llmster",
                1.0,
                10,
            ),
            proc(
                201,
                Some(200),
                None,
                Some("llama-server"),
                "llama-server",
                80.0,
                4_000,
            ),
            proc(
                202,
                Some(200),
                Some("/home/u/.lmstudio/.internal/utils/node"),
                None,
                "node",
                3.0,
                300,
            ),
            proc(
                300,
                Some(1),
                None,
                Some("./llama-server"),
                "llama-server",
                10.0,
                500,
            ),
            proc(999, Some(1), None, None, "local-ai-gateway", 1.0, 20),
        ];
        let (lms, ollama) = attribute(&procs, 999);
        assert_eq!(ollama, ProcStats::default());
        assert_eq!(lms.process_count, 4, "llmster, two workers and the orphan");
        assert_eq!(lms.cpu_pct, 94.0);
        assert_eq!(lms.rss_bytes, 4_810);
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
