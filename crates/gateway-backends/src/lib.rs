//! Backend clients and pollers. LM Studio is read through its native `GET /api/v0/models` (every
//! downloaded model with its load state); Ollama through `GET /api/version`, `/api/tags` (installed
//! models, cloud ones included) and `/api/ps` (resident models), merged into one list. Both produce the
//! shared `ModelInfo` and `BackendSnapshot` shapes. The poller runs one backend on an interval, keeps the
//! last known model list when the backend is unreachable, and publishes changes on the event bus. Ported
//! from LMS-Monitor's and Ollama-Monitor's `api.rs`.

pub mod lmstudio;
pub mod ollama;
pub mod poller;
pub mod version;

pub use poller::{PollerConfig, run};

/// The innermost cause of a reqwest error, e.g. `Connection refused (os error 61)`, instead of the
/// whole chain. Header-sized.
pub fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut current = err;
    while let Some(next) = current.source() {
        current = next;
    }
    current.to_string()
}

/// `<path>: <root cause>` for the status line.
pub fn describe(path: &str, err: &reqwest::Error) -> String {
    format!("{path}: {}", root_cause(err))
}
