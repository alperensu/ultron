//! Yan etkisiz eylem değerlendirmesi ve kullanıcı politika ayarları.
//!
//! Bu modül komut çalıştırmaz. Shell ayrıştırması güvenlik amaçlı, sınırlı bir
//! normalleştirmedir; desteklenmeyen dinamik sözdizimi onay gerektirir.

mod command;
mod rules;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Paths;

pub use command::normalize_shell;
pub use rules::evaluate;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Shell { command: String, cwd: PathBuf },
    FileWrite { path: PathBuf },
    FileDelete { path: PathBuf },
    Network { url: String },
    ToolCall { tool: String, input: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    Ask {
        reason: String,
        category: RiskCategory,
    },
    Deny {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskCategory {
    Delete,
    GitHistory,
    Deploy,
    Finance,
    Secrets,
    Install,
    SystemConfig,
    Publish,
}

/// Listeler varsayılan olarak boştur; yerleşik kurallar bu ayarlardan bağımsızdır.
/// `allow_list` yalnız tam shell komutlarını eşler ve kesin retleri açamaz.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub finance_paths: Vec<PathBuf>,
    pub deploy_patterns: Vec<String>,
    pub protected_paths: Vec<PathBuf>,
    pub allow_list: Vec<String>,
}

impl PolicyConfig {
    /// `Paths::resolve()` ile çözülen `ULTRON_HOME\policy.toml` konumunu okur.
    /// Eksik dosya varsayılanları verir; bozuk veya okunamayan dosya hata verir.
    pub fn load(paths: &Paths) -> Result<Self> {
        match std::fs::read_to_string(paths.root.join("policy.toml")) {
            Ok(text) => toml::from_str(&text).context("policy.toml çözümlenemedi"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("policy.toml okunamadı"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyContext {
    pub project_root: Option<PathBuf>,
    pub workspace_root: PathBuf,
}
