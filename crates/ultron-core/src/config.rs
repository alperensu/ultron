use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Desteklenen motorlar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineId {
    Claude,
    Codex,
    Agy,
}

/// Tek bir motorun yapılandırması.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub id: EngineId,
    pub enabled: bool,
    pub binary: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl EngineConfig {
    /// Model ve efor kurallarını denetler.
    pub fn validate(&self) -> Result<()> {
        if let Some(model) = &self.model {
            let lower = model.to_lowercase();
            if self.id == EngineId::Codex {
                // Sürüm numarasından bağımsız, aile son ekine göre.
                if lower.ends_with("-sol") || lower.ends_with("-terra") {
                    if let Some(effort) = &self.effort
                        && !matches!(effort.as_str(), "low" | "medium" | "high")
                    {
                        bail!("{model} için efor en fazla high olabilir: {effort}");
                    }
                } else if lower.ends_with("-luna") && self.effort.as_deref() != Some("max") {
                    bail!("{model} yalnız 'max' eforla çalışır");
                }
            }
            if self.id == EngineId::Agy && !lower.starts_with("gemini-") {
                bail!("agy yalnız Gemini modellerini kabul eder: {model}");
            }
        }
        Ok(())
    }
}

/// Beyin (yalnız üst düzey karar veren) modeli mi? `fable`, `astra` veya `opus` içeren
/// adlar beyin modelidir ve işçi olarak çalıştırılamaz.
pub fn is_brain_only(model: &str) -> bool {
    let lower = model.to_lowercase();
    ["fable", "astra", "opus"].iter().any(|k| lower.contains(k))
}

/// Bir modelin işçi (worker) olarak çalıştırılabileceğini denetler.
pub fn validate_worker_model(model: &str) -> Result<()> {
    if is_brain_only(model) {
        bail!("'{model}' beyin modelidir; işçi olarak çalıştırılamaz");
    }
    Ok(())
}

/// Çalışma durumu / proje notu özetleyicisi seçimi (`summarizer` anahtarı).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummarizerMode {
    /// agy etkinse agy; agy kotaya takılırsa aynı çalıştırmanın kalanında claude-haiku.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Yalnız agy (etkin değilse özetleyici yok).
    #[serde(rename = "agy")]
    Agy,
    /// Yalnız Claude Haiku, araçsız.
    #[serde(rename = "claude-haiku")]
    ClaudeHaiku,
    /// Özetleyici yok; yalnız kural tabanlı durum.
    #[serde(rename = "none")]
    Off,
}

/// Ultron genel yapılandırması (`root\config.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engines: Vec<EngineConfig>,
    pub default_engine: EngineId,
    pub context_budget_chars: usize,
    /// Elle girilen sıfırlama hakları (`[reset_rights]`).
    pub reset_rights: ResetRights,
    /// `false` ise Claude kullanım uç noktası hiç çağrılmaz.
    pub claude_live_quota: bool,
    /// Özetleyici seçimi: `auto` (varsayılan), `agy`, `claude-haiku` veya `none`.
    pub summarizer: SummarizerMode,
}

/// Havuz başına kalan sıfırlama hakları; `None` = bilinmiyor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResetRights {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            engines: default_engines(),
            default_engine: EngineId::Claude,
            context_budget_chars: 24000,
            reset_rights: ResetRights::default(),
            claude_live_quota: true,
            summarizer: SummarizerMode::Auto,
        }
    }
}

impl Config {
    /// `root\config.toml` dosyasını okur; yoksa varsayılanları döner.
    pub fn load(paths: &Paths) -> Result<Config> {
        let file = paths.root.join("config.toml");
        let cfg: Config = if file.exists() {
            let text = std::fs::read_to_string(&file)?;
            toml::from_str(&text)?
        } else {
            Config::default()
        };
        for engine in &cfg.engines {
            engine.validate()?;
        }
        Ok(cfg)
    }
}

fn default_engines() -> Vec<EngineConfig> {
    vec![
        EngineConfig {
            id: EngineId::Claude,
            enabled: true,
            binary: "claude".to_string(),
            model: None,
            effort: None,
            extra_args: Vec::new(),
        },
        EngineConfig {
            id: EngineId::Codex,
            enabled: true,
            binary: "codex".to_string(),
            model: Some("gpt-6.1-sol".to_string()),
            effort: Some("medium".to_string()),
            extra_args: Vec::new(),
        },
        EngineConfig {
            id: EngineId::Agy,
            enabled: true,
            binary: "agy".to_string(),
            model: Some("gemini-3.8-flash-low".to_string()),
            effort: None,
            extra_args: Vec::new(),
        },
    ]
}
