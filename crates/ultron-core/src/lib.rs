//! Ultron çekirdek kütüphanesi.

pub mod config;
pub mod db;
pub mod paths;

pub use config::{Config, EngineConfig, EngineId};
pub use db::{Db, SearchHit, SessionRow, SessionStatus, StoredEvent};
pub use paths::Paths;

/// Uygulama adı; tek kaynak.
pub const APP_NAME: &str = "ultron";

/// Paket sürümü (workspace'ten).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
