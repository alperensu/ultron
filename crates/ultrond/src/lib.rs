//! `ultrond` servis kütüphanesi: IPC sunucusu ve keep-awake. İkili (`main.rs`) bunu çalıştırır;
//! testler sunucuyu süreç içinde başlatır.

pub mod keepawake;
pub mod server;
pub(crate) mod trash_ops;

use std::collections::HashMap;
use std::sync::Arc;

use ultron_core::engine::{AgyEngine, ClaudeEngine, CodexEngine, Engine};
use ultron_core::{Config, EngineId};

/// Yapılandırmadaki etkin motorlardan gerçek motor sürücülerini kurar.
pub fn build_engines(cfg: &Config) -> HashMap<EngineId, Arc<dyn Engine>> {
    let mut map: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    for engine in cfg.engines.iter().filter(|e| e.enabled) {
        let driver: Arc<dyn Engine> = match engine.id {
            EngineId::Claude => Arc::new(ClaudeEngine::new(engine.clone())),
            EngineId::Codex => Arc::new(CodexEngine::new(engine.clone())),
            EngineId::Agy => Arc::new(AgyEngine::new(engine.clone())),
        };
        map.insert(engine.id, driver);
    }
    map
}
