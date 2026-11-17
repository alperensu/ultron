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
    ApprovalRequested {
        approval_id: String,
        category: crate::policy::RiskCategory,
        reason: String,
        summary: String,
    },
    ApprovalResolved {
        approval_id: String,
        status: crate::db::approvals::ApprovalStatus,
        by: crate::db::approvals::ResolvedBy,
    },
    /// Gerçekleşmiş bir eylemin tur sonu denetiminde saptanan politika ihlali.
    /// Ultron'un hata olmayan bilgilendirmesi (ör. onaylı silmenin `ultron trash` yönlendirmesi).
    PolicyNotice {
        message: String,
    },
    PolicyViolation {
        #[serde(serialize_with = "serialize_violation_command", deserialize_with = "deserialize_violation_command")]
        command: String,
        category: crate::policy::RiskCategory,
    },
}

/// Maskeleme kırpmadan önce yapılır; böylece kırpma sır desenini bölemez.
pub fn bounded_violation_command(command: &str) -> String {
    crate::import::mask_secrets(command).0.chars().take(4000).collect()
}

fn serialize_violation_command<S: serde::Serializer>(
    command: &str,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&bounded_violation_command(command))
}

fn deserialize_violation_command<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let command = String::deserialize(deserializer)?;
    Ok(bounded_violation_command(&command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_violation_masks_and_bounds_wire_fields() {
        let secret = ["sentetik", "deger"].join("_");
        let event = EngineEvent::PolicyViolation {
            command: format!("rm x; example --password {secret}; {}", "ğ".repeat(5000)),
            category: crate::policy::RiskCategory::Delete,
        };
        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(wire["type"], "policy_violation");
        assert_eq!(wire["category"], "Delete");
        assert!(!wire.to_string().contains(&secret));
        assert!(wire["command"].as_str().unwrap().chars().count() <= 4000);
        let decoded: EngineEvent = serde_json::from_value(serde_json::json!({
            "type": "policy_violation", "command": format!("token={secret}; {}", "ç".repeat(5000)),
            "category": "Delete"
        })).unwrap();
        let EngineEvent::PolicyViolation { command, .. } = decoded else { panic!("yanlış olay") };
        assert!(!command.contains(&secret));
        assert!(command.chars().count() <= 4000);
    }
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
