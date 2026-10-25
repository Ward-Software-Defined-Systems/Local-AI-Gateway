//! Shared foundation for the Local AI Gateway: the vocabulary every crate speaks (`types`), the
//! `config.toml` schema and validation (`config`), where files live on macOS and Linux (`paths`), the
//! in-process event bus (`events`), rolling metrics (`aggregate`), frontier pricing (`pricing`) and the
//! unified listener's routing table (`routing`). The SQLite store (`store`) holds requests, bodies,
//! findings, alerts and hardware samples; `tls` is the gateway's own certificate authority.

pub mod aggregate;
pub mod config;
pub mod events;
pub mod paths;
pub mod pricing;
pub mod routing;
pub mod state;
pub mod store;
pub mod tls;
pub mod types;
