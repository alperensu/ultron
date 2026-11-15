use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::EngineId;

/// Motor çıktısından normalize edilmiş olay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    SessionStarted {
        native_session_id: Option<String>,
    },
    AssistantText {
        text: String,
    },
    UserText {
        text: String,
    },
    ToolCall {
        tool: String,
        input: Value,
    },
    ToolResult {
        tool: String,
        output_excerpt: String,
        is_error: bool,
    },
    Command {
        command: String,
        exit_code: Option<i32>,
        output_excerpt: String,
    },
    FileChanged {
        path: String,
        change: String,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
    },
    QuotaExhausted {
        message: String,
        resets_at: Option<DateTime<Utc>>,
    },
    TurnCompleted,
    Error {
        message: String,
    },
    /// Tanınmayan olay tipi; ham JSON korunur.
    Unknown {
        raw: Value,
    },
    EngineSwitched {
        from: EngineId,
        to: EngineId,
        reason: SwitchReason,
    },
    ExternalChanges {
        summary: String,
    },
}

/// Motor değişiminin nedeni.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    User,
    Quota,
    Error,
}

impl EngineEvent {
    /// Tam metin aramasına girecek metin (yalnız mesajlar).
    pub fn searchable_text(&self) -> Option<&str> {
        match self {
            EngineEvent::AssistantText { text } | EngineEvent::UserText { text } => Some(text),
            _ => None,
        }
    }
}
