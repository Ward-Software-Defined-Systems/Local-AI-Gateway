//! The DLP engine: built-in and custom secret and PII rules, a known-secret matcher, redaction of stored
//! copies, and policy resolution (record, alert or block, with a stricter default for cloud-routed
//! requests). Pure code with no I/O, so every rule has a millisecond test. The proxy calls `Engine` through
//! its `RequestScanner` trait; the dashboard's test box calls `Engine::scan_text` directly.

pub mod engine;
pub mod entropy;
pub mod known_secrets;
pub mod policy;
pub mod redact;
pub mod rules;
pub mod validators;

pub use engine::{Engine, Match};
pub use policy::{EffectivePolicy, Policy};
pub use rules::{Rule, RuleSet, builtin_rules};
