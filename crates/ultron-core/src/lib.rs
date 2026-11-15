//! Ultron çekirdek kütüphanesi.

pub mod config;
pub mod db;
pub mod engine;
pub mod eventlog;
pub mod events;
pub mod handoff;
pub mod paths;
pub mod summarizer;
pub mod workstate;

pub use config::{Config, EngineConfig, EngineId};
pub use db::{Db, SearchHit, SessionRow, SessionStatus, StoredEvent};
pub use eventlog::EventLog;
pub use events::{EngineEvent, SwitchReason};
pub use paths::Paths;

/// Uygulama adı; tek kaynak.
pub const APP_NAME: &str = "ultron";

/// Paket sürümü (workspace'ten).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
