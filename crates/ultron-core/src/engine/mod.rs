//! Motor arayüzü: CLI motorlarının ortak sözleşmesi.

pub mod claude;
pub mod codex;
pub mod fake;
pub mod runner;

use std::path::PathBuf;

use crate::config::EngineId;
use crate::events::EngineEvent;

pub use claude::ClaudeEngine;
pub use codex::CodexEngine;
pub use fake::FakeEngine;
pub use runner::{TurnOutcome, run_turn};

/// Hata metninin kota/kullanım sınırı olup olmadığına bakar (büyük/küçük harf duyarsız).
/// Yalnız hata ve sonuç olaylarında çağrılır.
pub(crate) fn is_quota_message(text: &str) -> bool {
    const INDICATORS: [&str; 7] = [
        "usage limit",
        "rate limit",
        "rate_limit",
        "quota",
        "resource_exhausted",
        "too many requests",
        "429",
    ];
    let lower = text.to_lowercase();
    INDICATORS.iter().any(|needle| lower.contains(needle))
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
    fn parse_line(&self, line: &str) -> Vec<EngineEvent>;
}
