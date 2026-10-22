//! The DLP hook the pipeline calls. Phase 4 supplies the real engine; until then `NoopScanner` allows
//! everything and finds nothing. The scanner sees the full request body before it is forwarded and the
//! reassembled reply text when the response ends, and hands back the copies to store (redacted).

use gateway_core::types::{ApiFormat, Backend, Direction, Finding};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ScanContext {
    pub request_id: Uuid,
    pub backend: Backend,
    pub api_format: ApiFormat,
    pub model: Option<String>,
    pub is_cloud: bool,
    pub direction: Direction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refuse the request with 403 before it reaches the backend.
    Block {
        rule_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub findings: Vec<Finding>,
    pub decision: Decision,
    /// The body with every finding redacted, for storage; `None` when nothing needed redacting.
    pub redacted: Option<Vec<u8>>,
}

impl ScanOutcome {
    pub fn clean() -> Self {
        Self {
            findings: Vec::new(),
            decision: Decision::Allow,
            redacted: None,
        }
    }
}

pub trait RequestScanner: Send + Sync {
    /// The buffered request body, as JSON when it is JSON.
    fn scan_request(&self, ctx: &ScanContext, body: &[u8]) -> ScanOutcome;
    /// The reply text at the end of the response, and the raw response bytes to redact for storage.
    fn scan_response(&self, ctx: &ScanContext, text: &str, raw: &[u8]) -> ScanOutcome;
}

#[derive(Debug, Default)]
pub struct NoopScanner;

impl RequestScanner for NoopScanner {
    fn scan_request(&self, _ctx: &ScanContext, _body: &[u8]) -> ScanOutcome {
        ScanOutcome::clean()
    }

    fn scan_response(&self, _ctx: &ScanContext, _text: &str, _raw: &[u8]) -> ScanOutcome {
        ScanOutcome::clean()
    }
}
