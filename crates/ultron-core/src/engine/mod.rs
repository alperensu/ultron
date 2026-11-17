//! Motor arayüzü: CLI motorlarının ortak sözleşmesi.

pub mod agy;
pub mod claude;
pub mod codex;
pub mod fake;
pub mod runner;

use std::path::PathBuf;

use crate::config::EngineId;
use crate::events::EngineEvent;

pub use agy::AgyEngine;
pub use claude::ClaudeEngine;
pub use codex::CodexEngine;
pub use fake::FakeEngine;
pub use runner::{TurnOutcome, run_turn};

/// Hata metninin kota/kullanım sınırı olup olmadığına bakar (büyük/küçük harf duyarsız).
/// Yalnız hata ve sonuç olaylarında çağrılır. Metin kelimelere ayrılır; `429` yalnız tam
/// kelime olarak eşleşir (`14290` ve `req_ab429cd` eşleşmez).
pub(crate) fn is_quota_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let pair = |a: &str, b: &str| words.windows(2).any(|w| w[0] == a && w[1] == b);
    if pair("usage", "limit") || pair("rate", "limit") || pair("resource", "exhausted") {
        return true;
    }
    if words.windows(3).any(|w| w == ["too", "many", "requests"]) {
        return true;
    }
    if words.contains(&"429") {
        return true;
    }
    // "quota" yalnız limit/exceed/exhaust kelimeleriyle yan yana geçtiğinde kota hatasıdır.
    words.iter().enumerate().any(|(i, w)| {
        *w == "quota"
            && words[i.saturating_sub(2)..(i + 3).min(words.len())]
                .iter()
                .any(|n| n.starts_with("exceed") || n.starts_with("limit") || n.starts_with("exhaust"))
    })
}

/// Motorun araç/izin kısıtı düzeyi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    ReadOnly,
    Workspace,
    Full,
}

/// Bir motor turu isteği.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub chat_id: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub resume_native_id: Option<String>,
    pub tool_policy: ToolPolicy,
}

/// Motorun araç kısıtlamasını ne kadar uygulayabildiği.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestrictionLevel {
    Unknown,
    Partial,
    Full,
}

/// Motor yetenekleri.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub streaming: bool,
    pub resume: bool,
    pub mid_turn_input: bool,
    pub tool_restriction: RestrictionLevel,
}

/// Çalıştırılacak alt süreç tarifi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
}

/// Bir CLI motoru: komutu kurar, çıktı satırlarını olaya çevirir.
pub trait Engine: Send + Sync {
    fn id(&self) -> EngineId;
    fn capabilities(&self) -> Capabilities;
    fn command(&self, req: &TurnRequest) -> CommandSpec;
    /// Başlatmadan önce oturuma özel dosya/ortam hazırlığı; hata süreci başlatmaz.
    fn prepare_command(
        &self,
        _req: &TurnRequest,
        _cmd: &mut tokio::process::Command,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn parse_line(&self, line: &str) -> Vec<EngineEvent>;
}

#[cfg(test)]
mod tests {
    use super::is_quota_message;

    #[test]
    fn quota_detection_ignores_numbers_containing_429() {
        assert!(!is_quota_message("tokens 14290"));
        assert!(!is_quota_message("req_ab429cd"));
        assert!(!is_quota_message("id 4290 / 1429"));
    }

    #[test]
    fn quota_detection_matches_http_429() {
        assert!(is_quota_message("HTTP 429 Too Many Requests"));
        assert!(is_quota_message("status=429"));
        assert!(is_quota_message("You have reached your usage limit."));
        assert!(is_quota_message("rate_limit_error"));
        assert!(is_quota_message("RESOURCE_EXHAUSTED"));
        assert!(is_quota_message("quota exceeded for this project"));
        assert!(is_quota_message("daily quota exhausted"));
    }

    #[test]
    fn quota_word_alone_is_not_quota() {
        assert!(!is_quota_message("quota"));
        assert!(!is_quota_message("the quota helper module failed to compile"));
        assert!(!is_quota_message("rate limiting was added to the parser"));
        assert!(!is_quota_message("accurate limit values"));
    }
}
