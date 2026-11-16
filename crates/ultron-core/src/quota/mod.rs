//! Kota okuma ve bütçe modu. Claude canlı uç noktadan, Codex oturum günlüklerinden okunur;
//! Agy için ölçüm yoktur. Kaynak bozuk/yoksa Unknown/Estimated döner, asla çökmez.

pub mod claude;
pub mod codex;
pub mod mode;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use claude::{HttpClient, HttpFuture, ReqwestClient, Token};
pub use mode::{BudgetMode, budget_mode};

use crate::config::Config;

const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaSource {
    Live,
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String,
    pub used_pct: Option<f32>,
    pub resets_at: Option<DateTime<Utc>>,
    pub window_secs: u64,
    pub source: QuotaSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pool {
    Claude,
    Codex,
    Agy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolReport {
    pub pool: Pool,
    pub windows: Vec<QuotaWindow>,
    pub reset_rights: Option<u32>,
    pub mode: BudgetMode,
    /// Canlı okuma yapılamadıysa nedeni (token içermez).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaReport {
    pub pools: Vec<PoolReport>,
    pub fetched_at: DateTime<Utc>,
}

/// Okunan dosya konumları (testlerde geçici dizine yönlendirilir).
#[derive(Debug, Clone)]
pub struct QuotaSources {
    pub claude_credentials: PathBuf,
    pub claude_json: PathBuf,
    pub codex_sessions: PathBuf,
}

impl QuotaSources {
    /// Kullanıcı ev dizininden (`USERPROFILE`/`HOME`, `CODEX_HOME`) türetir.
    pub fn from_env() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        QuotaSources {
            claude_credentials: home.join(".claude").join(".credentials.json"),
            claude_json: home.join(".claude.json"),
            codex_sessions: codex.join("sessions"),
        }
    }
}

type Cached = (Instant, Vec<QuotaWindow>);

pub struct QuotaService {
    sources: QuotaSources,
    http: Arc<dyn HttpClient>,
    cache: Mutex<Option<Cached>>,
}

impl QuotaService {
    pub fn new(sources: QuotaSources, http: Arc<dyn HttpClient>) -> Self {
        QuotaService {
            sources,
            http,
            cache: Mutex::new(None),
        }
    }

    /// Gerçek HTTP istemcisiyle, ortam yollarından.
    pub fn from_env() -> Self {
        Self::new(QuotaSources::from_env(), Arc::new(ReqwestClient::new()))
    }

    async fn claude_windows(
        &self,
        cfg: &Config,
        now: DateTime<Utc>,
    ) -> (Vec<QuotaWindow>, Option<String>) {
        if !cfg.claude_live_quota {
            let w = claude::read_cached_estimate(&self.sources.claude_json, now);
            return (w, Some("canlı kota yapılandırmada kapalı".into()));
        }
        if let Ok(guard) = self.cache.lock()
            && let Some((at, w)) = guard.as_ref()
            && at.elapsed() < CACHE_TTL
        {
            return (w.clone(), None);
        }
        let reason = match claude::read_token(&self.sources.claude_credentials, now) {
            None => "Claude belirteci yok ya da süresi dolmuş".to_string(),
            Some(token) => match self.http.get(claude::USAGE_URL, &token).await {
                Ok(body) => {
                    let w = claude::parse_usage(&body, QuotaSource::Live);
                    if w.is_empty() {
                        "uç nokta yanıtı çözümlenemedi".to_string()
                    } else {
                        if let Ok(mut g) = self.cache.lock() {
                            *g = Some((Instant::now(), w.clone()));
                        }
                        return (w, None);
                    }
                }
                Err(e) => token.redact(&e),
            },
        };
        tracing::warn!(reason = %reason, "Claude canlı kota okunamadı; tahmini yola düşülüyor");
        let w = claude::read_cached_estimate(&self.sources.claude_json, now);
        (w, Some(format!("canlı okuma başarısız ({reason}); tahmini")))
    }

    /// Tüm havuzların raporu.
    pub async fn report(&self, cfg: &Config, now: DateTime<Utc>) -> QuotaReport {
        let (claude_w, note) = self.claude_windows(cfg, now).await;
        let codex_w = codex::read_codex_windows(&self.sources.codex_sessions, now);
        let mk = |pool, windows: Vec<QuotaWindow>, rights, note| PoolReport {
            pool,
            mode: budget_mode(&windows, now),
            windows,
            reset_rights: rights,
            note,
        };
        QuotaReport {
            pools: vec![
                mk(Pool::Claude, claude_w, cfg.reset_rights.claude, note),
                mk(Pool::Codex, codex_w, cfg.reset_rights.codex, None),
                // Agy: ölçüm yok → Unknown/Dengeli.
                mk(
                    Pool::Agy,
                    vec![QuotaWindow {
                        label: "kota".into(),
                        used_pct: None,
                        resets_at: None,
                        window_secs: 0,
                        source: QuotaSource::Unknown,
                    }],
                    None,
                    Some("ölçüm kaynağı yok".into()),
                ),
            ],
            fetched_at: now,
        }
    }
}
