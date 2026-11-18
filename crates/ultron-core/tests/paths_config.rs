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
fn effort_ceiling_matches_family_suffix_any_version() {
    for model in ["gpt-5.6-sol", "gpt-7-terra"] {
        assert!(engine(EngineId::Codex, Some(model), Some("max")).validate().is_err());
        assert!(engine(EngineId::Codex, Some(model), Some("high")).validate().is_ok());
    }
    assert!(engine(EngineId::Codex, Some("gpt-7.2-luna"), Some("medium")).validate().is_err());
    assert!(engine(EngineId::Codex, Some("gpt-7.2-luna"), Some("max")).validate().is_ok());
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

#[test]
fn summarizer_key_defaults_to_auto_and_parses_all_values() {
    use ultron_core::config::SummarizerMode;
    assert_eq!(Config::default().summarizer, SummarizerMode::Auto);
    assert_eq!(toml::from_str::<Config>("").unwrap().summarizer, SummarizerMode::Auto);
    for (text, mode) in [
        ("auto", SummarizerMode::Auto),
        ("agy", SummarizerMode::Agy),
        ("claude-haiku", SummarizerMode::ClaudeHaiku),
        ("none", SummarizerMode::Off),
    ] {
        let cfg: Config = toml::from_str(&format!("summarizer = \"{text}\"")).unwrap();
        assert_eq!(cfg.summarizer, mode, "{text}");
    }
}

#[test]
fn summarizer_key_rejects_unknown_value() {
    assert!(toml::from_str::<Config>("summarizer = \"gemini\"").is_err());
    assert!(toml::from_str::<Config>("summarizer = \"Claude-Haiku\"").is_err());
}

fn load_config_text(text: &str) -> anyhow::Result<Config> {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.toml"), text).unwrap();
    Config::load(&Paths::from_root(tmp.path().to_path_buf()))
}

#[test]
fn telegram_defaults_accept_missing_empty_and_old_config() {
    use ultron_core::config::{NotifyMode, SummarizerMode, TelegramConfig};
    for text in ["", "[telegram]", "[telegram]\nallowed_user_ids = []"] {
        let cfg = load_config_text(text).unwrap();
        assert_eq!(cfg.telegram, TelegramConfig::default());
        assert_eq!(cfg.telegram.notify, NotifyMode::Always);
        assert!(cfg.telegram.auto_continue_enabled);
        assert_eq!(cfg.telegram.auto_continue_wait_minutes, 45);
        assert!(cfg.telegram.fallback_order.is_empty());
        assert_eq!(cfg.engines, Config::default().engines);
    }
    let old = r#"
default_engine = "codex"
context_budget_chars = 17000
claude_live_quota = false
summarizer = "none"
[[engines]]
id = "codex"
enabled = false
binary = "synthetic-engine"
model = "gpt-6.1-sol"
effort = "high"
extra_args = ["--synthetic"]
[reset_rights]
claude = 2
codex = 3
"#;
    let before = load_config_text(old).unwrap();
    let after = load_config_text(&format!("{old}\n[telegram]\nnotify = \"away\"\n")).unwrap();
    assert_eq!(before.engines, after.engines);
    assert_eq!(before.reset_rights, after.reset_rights);
    assert_eq!(after.reset_rights.claude, Some(2));
    assert_eq!(after.reset_rights.codex, Some(3));
    assert_eq!(after.summarizer, SummarizerMode::Off);
    assert!(!after.claude_live_quota);
    assert_eq!(after.default_engine, EngineId::Codex);
    assert_eq!(after.context_budget_chars, 17000);
    assert_eq!(after.telegram.notify, NotifyMode::Away);
    assert_eq!(load_config_text(&toml::to_string(&after).unwrap()).unwrap(), after);
}

#[test]
fn telegram_accepts_positive_ids_and_inclusive_wait_bounds() {
    for minutes in [1, 45, 1440] {
        let text = format!(r#"[telegram]
allowed_user_ids = [101, 202]
notify = "away"
auto_continue_enabled = false
auto_continue_wait_minutes = {minutes}
fallback_order = ["codex", "agy", "claude"]
"#);
        let cfg = load_config_text(&text).unwrap();
        assert_eq!(cfg.telegram.allowed_user_ids, vec![101, 202]);
        assert_eq!(cfg.telegram.auto_continue_wait_minutes, minutes);
        assert!(!cfg.telegram.auto_continue_enabled);
        assert_eq!(cfg.telegram.fallback_order, vec![EngineId::Codex, EngineId::Agy, EngineId::Claude]);
        assert!(!format!("{cfg:?}").contains("101"));
        assert!(!format!("{:?}", cfg.telegram).contains("202"));
    }
}

#[test]
fn telegram_invalid_config_is_rejected_without_ids_in_error_chain() {
    for entry in [
        "allowed_user_ids = [0]",
        "allowed_user_ids = [-101]",
        "allowed_user_ids = [101, 101]",
        "allowed_user_ids = [9223372036854775808]",
        "allowed_user_ids = [\"synthetic-private-id\"]",
        "allowed_user_ids = [101.5]",
        "fallback_order = [\"codex\", \"codex\"]",
        "fallback_order = [\"unknown-engine\"]",
        "notify = \"unknown-notify\"",
        "auto_continue_wait_minutes = 0",
        "auto_continue_wait_minutes = 1441",
        "auto_continue_wait_minutes = -1",
    ] {
        let error = load_config_text(&format!("[telegram]\n{entry}")).unwrap_err();
        let debug = format!("{error:?} {error:#}");
        for private in ["101", "9223372036854775808", "synthetic-private-id"] {
            assert!(!debug.contains(private), "hata kimlik içermemeli");
        }
    }
}

#[test]
fn telegram_allowlist_is_bounded() {
    use ultron_core::config::MAX_TELEGRAM_ALLOWED_USERS;
    let mut cfg = Config::default();
    cfg.telegram.allowed_user_ids = (1..=MAX_TELEGRAM_ALLOWED_USERS as i64).collect();
    assert!(cfg.telegram.validate().is_ok());
    cfg.telegram.allowed_user_ids.push(MAX_TELEGRAM_ALLOWED_USERS as i64 + 1);
    assert!(cfg.telegram.validate().is_err());
    assert!(load_config_text(&toml::to_string(&cfg).unwrap()).is_err());
}

// ---------------------------------------------------------------------------------------------
// Aşama 5a: [memory] yapılandırması ve kasa yolu çözümlemesi
// ---------------------------------------------------------------------------------------------

#[test]
fn ultron_home_never_resolves_to_the_real_profile_vault() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: ENV_LOCK ile bu dosyadaki diğer env kullanan testlerden ayrıştırılmıştır.
    unsafe { std::env::set_var("ULTRON_HOME", tmp.path()) };
    let paths = Paths::resolve().unwrap();
    unsafe { std::env::remove_var("ULTRON_HOME") };

    assert_eq!(paths.default_vault, tmp.path().join("vault"));
    let vault = ultron_core::config::MemoryConfig::default().resolve_vault(&paths).unwrap();
    assert!(vault.starts_with(tmp.path()), "{vault:?}");
    for var in ["USERPROFILE", "HOME"] {
        if let Some(profile) = std::env::var_os(var) {
            assert!(!vault.starts_with(&profile) || tmp.path().starts_with(&profile), "gerçek profil kasası seçildi");
            assert!(!vault.ends_with("harness-brain"));
        }
    }
    // Dizin yalnız `ensure` ile durum dizinleri oluşturulur; kasa kendiliğinden açılmaz.
    paths.ensure().unwrap();
    assert!(!paths.default_vault.exists());
}

#[test]
fn production_resolution_uses_harness_brain_under_the_profile() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path().join("profil");
    let local = tmp.path().join("yerel");
    let old_home = std::env::var_os("ULTRON_HOME");
    let old_profile = std::env::var_os("USERPROFILE");
    let old_local = std::env::var_os("LOCALAPPDATA");
    // SAFETY: ENV_LOCK altında; değerler test sonunda geri yüklenir. Dizinler oluşturulmaz.
    unsafe {
        std::env::remove_var("ULTRON_HOME");
        std::env::set_var("USERPROFILE", &profile);
        std::env::set_var("LOCALAPPDATA", &local);
    }
    let paths = Paths::resolve();
    unsafe {
        match old_home { Some(v) => std::env::set_var("ULTRON_HOME", v), None => std::env::remove_var("ULTRON_HOME") }
        match old_profile { Some(v) => std::env::set_var("USERPROFILE", v), None => std::env::remove_var("USERPROFILE") }
        match old_local { Some(v) => std::env::set_var("LOCALAPPDATA", v), None => std::env::remove_var("LOCALAPPDATA") }
    }
    let paths = paths.unwrap();
    assert_eq!(paths.root, local.join("ultron"));
    assert_eq!(paths.default_vault, profile.join("harness-brain"));
    assert!(!profile.exists() && !local.exists(), "çözümleme dosya sistemine dokunmamalı");
}

#[test]
fn explicit_temp_vault_path_is_resolved_and_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("kok"));
    let other = tempfile::tempdir().unwrap();
    let cfg = ultron_core::config::MemoryConfig {
        vault_path: Some(other.path().join("kasa").to_string_lossy().into_owned()),
        ..Default::default()
    };
    assert!(cfg.validate().is_ok());
    assert_eq!(cfg.resolve_vault(&paths).unwrap(), other.path().join("kasa"));
    assert!(!other.path().join("kasa").exists(), "çözümleme dizin oluşturmamalı");

    for bad in ["", "kasa", "./kasa", "..\\kasa", "C:\\a\\..\\b", "\\\\sunucu\\paylasim\\k", "//sunucu/p/k", "\\\\?\\C:\\k", "C:\\k\0x"] {
        let cfg = ultron_core::config::MemoryConfig { vault_path: Some(bad.to_string()), ..Default::default() };
        assert!(cfg.validate().is_err(), "kabul edilmemeliydi: {bad:?}");
    }
    // Kasa Ultron durum dizininin içinde veya onu içeren yerde olamaz.
    for inside in [paths.root.join("state").join("kasa"), paths.run.clone(), paths.transcripts.join("x"), paths.root.clone(),
                   paths.root.parent().unwrap().to_path_buf()] {
        let cfg = ultron_core::config::MemoryConfig { vault_path: Some(inside.to_string_lossy().into_owned()), ..Default::default() };
        assert!(cfg.resolve_vault(&paths).is_err(), "{inside:?}");
    }
}

#[test]
fn memory_config_defaults_toml_and_bounds() {
    use ultron_core::config::MemoryConfig;
    let defaults = MemoryConfig::default();
    assert_eq!(defaults.vault_path, None);
    assert_eq!(defaults.retrieval_max_tokens, 2000);
    assert!(defaults.validate().is_ok());

    // Eski config (memory bölümü yok) ve v13 öncesi dosyalar olduğu gibi yüklenir.
    let cfg = load_config_text("default_engine = \"codex\"\n").unwrap();
    assert_eq!(cfg.memory, MemoryConfig::default());
    assert_eq!(cfg.default_engine, EngineId::Codex);

    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("kasa").to_string_lossy().replace('\\', "/");
    let cfg = load_config_text(&format!("[memory]\nvault_path = \"{vault}\"\nretrieval_max_tokens = 1500\nscan_interval_secs = 30\n")).unwrap();
    assert_eq!(cfg.memory.retrieval_max_tokens, 1500);
    assert_eq!(cfg.memory.scan_interval_secs, 30);
    assert_eq!(cfg.memory.retrieval_max_items, 8, "belirtilmeyen anahtar varsayılan kalır");
    let roundtrip: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(roundtrip.memory, cfg.memory);

    for (key, value) in [
        ("retrieval_max_tokens", 199), ("retrieval_max_tokens", 8001), ("retrieval_max_chars", 499),
        ("retrieval_max_items", 0), ("retrieval_max_items", 21), ("scan_interval_secs", 9),
        ("max_suggestions_per_turn", 0), ("job_queue_limit", 1025), ("job_max_attempts", 11), ("max_vault_items", 99),
    ] {
        assert!(load_config_text(&format!("[memory]\n{key} = {value}\n")).is_err(), "{key}={value}");
    }
    assert!(load_config_text("[memory]\nvault_path = \"goreli/kasa\"\n").is_err());
}

#[test]
fn unknown_memory_keys_are_rejected_with_a_clear_error() {
    // Yazım hatası kasa yolunun sessizce varsayılana düşmesine yol açmamalı.
    let err = load_config_text("[memory]\nvault_pth = \"C:/kasa\"\n").unwrap_err().to_string();
    assert_eq!(err, "config.toml [memory] bölümü geçersiz (bilinmeyen anahtar veya değer)");
    let err = load_config_text("[memory]\nretrieval_max_tokens = \"çok\"\n").unwrap_err().to_string();
    assert!(err.contains("[memory]"), "{err}");
    // Memory dışı bozuk config eski, sızıntısız mesajı korur.
    let err = load_config_text("[telegram]\nallowed_user_ids = [101,,202]\n").unwrap_err().to_string();
    assert_eq!(err, "config.toml çözümlenemedi");
}
