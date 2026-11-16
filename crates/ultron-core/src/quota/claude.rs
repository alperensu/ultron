//! Claude canlı kota: OAuth kullanım uç noktası. Token yalnızca bellekte tutulur,
//! `Debug` çıktısında maskelenir ve hiçbir hata metnine/log'a girmez.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{QuotaSource, QuotaWindow};

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA_HEADER: &str = "oauth-2025-04-20";
pub const FIVE_HOUR_SECS: u64 = 5 * 3600;
pub const SEVEN_DAY_SECS: u64 = 7 * 24 * 3600;

/// OAuth erişim belirteci; `Debug` maskeli, serileştirilemez.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Token(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Metinde geçen belirteci siler (hata mesajı güvenliği).
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_string()
        } else {
            text.replace(&self.0, "[gizli]")
        }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

pub type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// Enjekte edilebilir HTTP istemcisi: Bearer ile GET, gövde metni ya da hata metni döner.
pub trait HttpClient: Send + Sync {
    fn get<'a>(&'a self, url: &'a str, bearer: &'a Token) -> HttpFuture<'a>;
}

/// Gerçek istemci (reqwest + rustls, 5 sn zaman aşımı, yönlendirme yok).
pub struct ReqwestClient {
    inner: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("ultron-quota/1")
            .build()
            .unwrap_or_default();
        ReqwestClient { inner }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for ReqwestClient {
    fn get<'a>(&'a self, url: &'a str, bearer: &'a Token) -> HttpFuture<'a> {
        Box::pin(async move {
            let resp = self
                .inner
                .get(url)
                .bearer_auth(bearer.expose())
                .header("anthropic-beta", BETA_HEADER)
                .send()
                .await
                .map_err(|e| bearer.redact(&format!("istek başarısız: {}", e.without_url())))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            resp.text()
                .await
                .map_err(|e| bearer.redact(&format!("yanıt okunamadı: {}", e.without_url())))
        })
    }
}

/// `credentials.json` → erişim belirteci. Süresi geçmişse ya da yoksa `None`.
pub fn read_token(credentials: &Path, now: DateTime<Utc>) -> Option<Token> {
    let text = std::fs::read_to_string(credentials).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let oauth = v.get("claudeAiOauth")?;
    if let Some(exp_ms) = oauth.get("expiresAt").and_then(Value::as_f64)
        && exp_ms / 1000.0 < now.timestamp() as f64
    {
        return None;
    }
    let token = oauth.get("accessToken")?.as_str()?;
    (!token.is_empty()).then(|| Token::new(token))
}

/// Uç nokta yanıtını (`five_hour`/`seven_day`: `utilization`, `resets_at`) pencerelere çevirir.
pub fn parse_usage(body: &str, source: QuotaSource) -> Vec<QuotaWindow> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    parse_usage_value(&v, source)
}

pub(crate) fn parse_usage_value(v: &Value, source: QuotaSource) -> Vec<QuotaWindow> {
    let mut out = Vec::new();
    for (key, label, secs) in [
        ("five_hour", "5 saat", FIVE_HOUR_SECS),
        ("seven_day", "7 gün", SEVEN_DAY_SECS),
    ] {
        let Some(w) = v.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(used) = w.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = w
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        out.push(QuotaWindow {
            label: label.to_string(),
            used_pct: Some(used.clamp(0.0, 100.0) as f32),
            resets_at,
            window_secs: secs,
            source,
        });
    }
    out
}

/// Yerel önbellek (`~/.claude.json` → `cachedUsageUtilization.utilization`): tahmini yol.
pub fn read_cached_estimate(claude_json: &Path, now: DateTime<Utc>) -> Vec<QuotaWindow> {
    let Ok(text) = std::fs::read_to_string(claude_json) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(util) = v
        .get("cachedUsageUtilization")
        .and_then(|c| c.get("utilization"))
    else {
        return Vec::new();
    };
    parse_usage_value(util, QuotaSource::Estimated)
        .into_iter()
        .map(|mut w| {
            // Süresi dolmuş pencerenin yüzdesi geçersizdir.
            if w.resets_at.is_some_and(|r| r <= now) {
                w.used_pct = None;
                w.resets_at = None;
                w.source = QuotaSource::Unknown;
            }
            w
        })
        .collect()
}
