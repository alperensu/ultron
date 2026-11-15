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
            if lower.contains("astra") {
                bail!("'astra' adlı model desteklenmiyor: {model}");
            }
            if self.id == EngineId::Codex {
                match lower.as_str() {
                    "gpt-6.1-sol" | "gpt-6.1-terra" => {
                        if let Some(effort) = &self.effort
                            && !matches!(effort.as_str(), "low" | "medium" | "high")
                        {
                            bail!("{model} için efor en fazla high olabilir: {effort}");
                        }
                    }
                    "gpt-6-luna" if self.effort.as_deref() != Some("max") => {
                        bail!("gpt-6-luna yalnız 'max' eforla çalışır");
                    }
                    _ => {}
                }
            }
            if self.id == EngineId::Agy && !lower.starts_with("gemini-") {
                bail!("agy yalnız Gemini modellerini kabul eder: {model}");
            }
        }
        Ok(())
    }
}

/// Ultron genel yapılandırması (`root\config.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engines: Vec<EngineConfig>,
    pub default_engine: EngineId,
    pub context_budget_chars: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            engines: default_engines(),
            default_engine: EngineId::Claude,
            context_budget_chars: 24000,
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
