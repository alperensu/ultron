//! Codex sürücüsü: `codex exec --json` (codex-cli 0.154.0).
//!
//! Gerçek yakalamada doğrulanan olay tipleri: `thread.started`, `turn.started`,
//! `item.completed` (`item.type == "error"`), `error`, `turn.failed`.
//! `agent_message`, `command_execution`, `file_change`, `turn.completed` biçimleri
//! Codex'in belgelenmiş JSONL çıktısına göre yazılmıştır; canlı bir başarılı tur
//! yakalanmadan doğrulanmamıştır.
//!
//! `codex exec resume` alt komutu `-s` ve `-C` kabul etmez; devamda sandbox
//! `-c sandbox_mode=...` ile, çalışma dizini ise süreç `cwd` ile verilir.

use serde_json::Value;

use super::{
    Capabilities, CommandSpec, Engine, RestrictionLevel, ToolPolicy, TurnRequest, is_quota_message,
};
use crate::config::{EngineConfig, EngineId};
use crate::eventlog::{DEFAULT_EXCERPT_CHARS, excerpt};
use crate::events::EngineEvent;

/// Codex motoru.
pub struct CodexEngine {
    cfg: EngineConfig,
}

impl CodexEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self { cfg }
    }
}

/// Politikaya karşılık gelen sandbox kipi. `danger-full-access` Aşama 1'de kullanılmaz.
fn sandbox_for(policy: ToolPolicy) -> &'static str {
    match policy {
        ToolPolicy::ReadOnly => "read-only",
        ToolPolicy::Workspace | ToolPolicy::Full => "workspace-write",
    }
}

impl Engine for CodexEngine {
    fn id(&self) -> EngineId {
        EngineId::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            resume: true,
            mid_turn_input: false,
            tool_restriction: RestrictionLevel::Partial,
        }
    }

    fn command(&self, req: &TurnRequest) -> CommandSpec {
        let sandbox = sandbox_for(req.tool_policy);
        let mut args: Vec<String> = vec!["exec".into()];
        if let Some(id) = &req.resume_native_id {
            args.extend(["resume".into(), id.clone()]);
        }
        args.push("--json".into());
        if let Some(model) = &self.cfg.model {
            args.extend(["-m".into(), model.clone()]);
        }
        if let Some(effort) = &self.cfg.effort {
            args.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
        }
        args.push("--skip-git-repo-check".into());
        match &req.resume_native_id {
            Some(_) => args.extend(["-c".into(), format!("sandbox_mode=\"{sandbox}\"")]),
            None => args.extend([
                "-C".into(),
                req.cwd.to_string_lossy().into_owned(),
                "-s".into(),
                sandbox.into(),
            ]),
        }
        // Son argüman: istemi stdin'den oku.
        args.push("-".into());
        CommandSpec {
            program: self.cfg.binary.clone(),
            args,
            cwd: req.cwd.clone(),
            stdin: Some(req.prompt.clone()),
        }
    }

    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(raw) => parse_value(&raw),
            Err(_) => vec![EngineEvent::Unknown {
                raw: Value::String(line.to_string()),
            }],
        }
    }
}

fn parse_value(raw: &Value) -> Vec<EngineEvent> {
    match raw.get("type").and_then(Value::as_str).unwrap_or_default() {
        "thread.started" => vec![EngineEvent::SessionStarted {
            native_session_id: str_field(raw, "thread_id"),
        }],
        "turn.started" | "item.started" => Vec::new(),
        "item.completed" => match raw.get("item") {
            Some(item) => item_events(raw, item),
            None => unknown(raw),
        },
        "turn.completed" => {
            let usage = raw.get("usage");
            vec![
                EngineEvent::Usage {
                    input_tokens: usage.and_then(|u| u64_field(u, "input_tokens")),
                    output_tokens: usage.and_then(|u| u64_field(u, "output_tokens")),
                    cached_tokens: usage.and_then(|u| u64_field(u, "cached_input_tokens")),
                },
                EngineEvent::TurnCompleted,
            ]
        }
        "turn.failed" => failure(
            str_at(raw, "/error/message").unwrap_or_else(|| "turn failed".to_string()),
        ),
        "error" => failure(str_field(raw, "message").unwrap_or_else(|| "error".to_string())),
        _ => unknown(raw),
    }
}

fn item_events(raw: &Value, item: &Value) -> Vec<EngineEvent> {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "agent_message" => {
            let text = str_or_empty(item, "text");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![EngineEvent::AssistantText { text }]
            }
        }
        "command_execution" => vec![EngineEvent::Command {
            command: str_or_empty(item, "command"),
            exit_code: item
                .get("exit_code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok()),
            output_excerpt: excerpt(
                &str_or_empty(item, "aggregated_output"),
                DEFAULT_EXCERPT_CHARS,
            ),
        }],
        "file_change" => item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .map(|c| EngineEvent::FileChanged {
                        path: str_or_empty(c, "path"),
                        change: str_or_empty(c, "kind"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "mcp_tool_call" => vec![EngineEvent::ToolCall {
            tool: format!(
                "{}.{}",
                str_or_empty(item, "server"),
                str_or_empty(item, "tool")
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }],
        "error" => failure(str_or_empty(item, "message")),
        // Akıl yürütme özeti gürültüdür; diğer tipler ham olarak korunur.
        "reasoning" => Vec::new(),
        _ => unknown(raw),
    }
}

fn failure(message: String) -> Vec<EngineEvent> {
    if is_quota_message(&message) {
        vec![EngineEvent::QuotaExhausted {
            message,
            resets_at: None,
        }]
    } else {
        vec![EngineEvent::Error { message }]
    }
}

fn unknown(raw: &Value) -> Vec<EngineEvent> {
    vec![EngineEvent::Unknown { raw: raw.clone() }]
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn str_or_empty(v: &Value, key: &str) -> String {
    str_field(v, key).unwrap_or_default()
}

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer).and_then(Value::as_str).map(str::to_string)
}

fn u64_field(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
