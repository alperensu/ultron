use std::sync::Mutex;

use ultron_core::config::{is_brain_only, validate_worker_model};
use ultron_core::{Config, EngineConfig, EngineId, Paths};

/// Ortam değişkeni (ULTRON_HOME) kullanan testleri sıraya koyar.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn paths_respect_ultron_home() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: ENV_LOCK ile bu test dışında ortam değişkeni okunmaz/yazılmaz.
    unsafe { std::env::set_var("ULTRON_HOME", tmp.path()) };
    let paths = Paths::resolve().unwrap();
    unsafe { std::env::remove_var("ULTRON_HOME") };

    assert_eq!(paths.root, tmp.path());
    assert_eq!(paths.db, tmp.path().join("state").join("ultron.db"));
    paths.ensure().unwrap();
    assert!(paths.transcripts.is_dir());
    assert!(paths.run.is_dir());
}

#[test]
fn config_defaults_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().to_path_buf());
    let cfg = Config::load(&paths).unwrap();

    let codex = cfg.engines.iter().find(|e| e.id == EngineId::Codex).unwrap();
    assert_eq!(codex.model.as_deref(), Some("gpt-6.1-sol"));
    let agy = cfg.engines.iter().find(|e| e.id == EngineId::Agy).unwrap();
    assert_eq!(agy.model.as_deref(), Some("gemini-3.8-flash-low"));
    assert_eq!(cfg.context_budget_chars, 24000);
}

#[test]
fn astra_allowed_in_config_but_not_as_worker() {
    // Yapılandırmada astra artık reddedilmez (beyin modeli olarak tanımlanabilir).
    let cfg = engine(EngineId::Codex, Some("gpt-6.1-astra"), Some("medium"));
    assert!(cfg.validate().is_ok());
    // Ama işçi olarak çalıştırılamaz.
    assert!(validate_worker_model("gpt-6.1-astra").is_err());
    assert!(validate_worker_model("GPT-6.1-ASTRA").is_err());
}

#[test]
fn opus_and_fable_are_brain_only() {
    assert!(is_brain_only("claude-opus-4"));
    assert!(is_brain_only("Claude-OPUS-x"));
    assert!(is_brain_only("fable-1"));
    assert!(is_brain_only("gpt-6.1-astra"));
    assert!(validate_worker_model("claude-opus-4").is_err());
    assert!(validate_worker_model("fable-1").is_err());
}

#[test]
fn sonnet_haiku_luna_are_worker_eligible() {
    for model in ["claude-sonnet-4", "claude-haiku-5-5", "gpt-6-luna", "gpt-6.1-sol", "gpt-6.1-terra"] {
        assert!(!is_brain_only(model), "{model}");
        assert!(validate_worker_model(model).is_ok(), "{model}");
    }
}

#[test]
fn validate_rejects_effort_above_high_for_sol() {
    let cfg = engine(EngineId::Codex, Some("gpt-6.1-sol"), Some("max"));
    assert!(cfg.validate().is_err());
    let cfg = engine(EngineId::Codex, Some("gpt-6.1-sol"), Some("high"));
    assert!(cfg.validate().is_ok());
    let cfg = engine(EngineId::Codex, Some("gpt-6-luna"), Some("medium"));
    assert!(cfg.validate().is_err());
    let cfg = engine(EngineId::Codex, Some("gpt-6-luna"), Some("max"));
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_rejects_non_gemini_agy() {
    let cfg = engine(EngineId::Agy, Some("claude-sonnet"), None);
    assert!(cfg.validate().is_err());
    let cfg = engine(EngineId::Agy, Some("gemini-3.8-flash-low"), None);
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_allows_astra_in_extra_args() {
    let mut cfg = engine(EngineId::Claude, None, None);
    cfg.extra_args = vec!["--model".into(), "claude-astra-1".into()];
    assert!(cfg.validate().is_ok());
    cfg.extra_args = vec!["--verbose".into()];
    assert!(cfg.validate().is_ok());
}

fn engine(id: EngineId, model: Option<&str>, effort: Option<&str>) -> EngineConfig {
    EngineConfig {
        id,
        enabled: true,
        binary: "fake".to_string(),
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
        extra_args: Vec::new(),
    }
}
