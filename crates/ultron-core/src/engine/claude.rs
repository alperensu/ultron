//! Claude Code sürücüsü: `claude -p --output-format stream-json --verbose`.
//!
//! Gerçek akış (claude 2.1.325) ile doğrulanan olay tipleri: `system`/`init`,
//! `system`/`hook_*` (yok sayılır), `assistant` (metin blokları), `rate_limit_event`,
//! `result`. `tool_use` / `tool_result` biçimi Anthropic mesaj biçiminden alınmıştır;
//! canlı bir araç çağrısı yakalanmadan doğrulanmamıştır.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{
    Capabilities, CommandSpec, Engine, RestrictionLevel, ToolPolicy, TurnRequest, is_quota_message,
};
use crate::config::{EngineConfig, EngineId};
use crate::eventlog::{DEFAULT_EXCERPT_CHARS, excerpt};
use crate::events::EngineEvent;

/// Workspace politikasında engellenen komut desenleri.
const WORKSPACE_DENY: &str = "Bash(rm:*),Bash(del:*),Bash(Remove-Item:*),Bash(git push:*)";

/// MCP sunucusu içermeyen satır içi yapılandırma (`--strict-mcp-config` ile birlikte).
const EMPTY_MCP: &str = r#"{"mcpServers":{}}"#;

/// Claude Code motoru.
pub struct ClaudeEngine {
    cfg: EngineConfig,
    /// Dosya düzenleyen `tool_use` kimliği → (yol, değişiklik türü). Değişiklik olayı yalnız
    /// eşleşen `tool_result` başarılı geldiğinde üretilir.
    pending_edits: Mutex<HashMap<String, (String, &'static str)>>,
}

impl ClaudeEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            cfg,
            pending_edits: Mutex::new(HashMap::new()),
        }
    }
}

impl Engine for ClaudeEngine {
    fn id(&self) -> EngineId {
        EngineId::Claude
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            resume: true,
            mid_turn_input: true,
            tool_restriction: RestrictionLevel::Partial,
        }
    }

    fn command(&self, req: &TurnRequest) -> CommandSpec {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];
        if let Some(id) = &req.resume_native_id {
            args.extend(["--resume".into(), id.clone()]);
        }
        if let Some(model) = &self.cfg.model {
            args.extend(["--model".into(), model.clone()]);
        }
        args.extend([
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            EMPTY_MCP.into(),
        ]);
        args.extend(self.cfg.extra_args.iter().cloned());
        // Değişken uzunluklu bayraklar (--tools, --disallowedTools) en sona konur.
        match req.tool_policy {
            ToolPolicy::ReadOnly => args.extend([
                "--permission-mode".into(),
                "plan".into(),
                "--tools".into(),
                "Read,Grep,Glob".into(),
            ]),
            ToolPolicy::Workspace => args.extend([
                "--permission-mode".into(),
                "acceptEdits".into(),
                "--disallowedTools".into(),
                WORKSPACE_DENY.into(),
            ]),
            ToolPolicy::Full => {}
        }
        CommandSpec {
            program: self.cfg.binary.clone(),
            args,
            cwd: req.cwd.clone(),
            stdin: Some(req.prompt.clone()),
        }
    }

    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(raw) => self.parse_value(&raw),
            Err(_) => vec![EngineEvent::Unknown {
                raw: Value::String(line.to_string()),
            }],
        }
    }
}

impl ClaudeEngine {
    fn parse_value(&self, raw: &Value) -> Vec<EngineEvent> {
        let kind = raw.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "system" => match raw.get("subtype").and_then(Value::as_str) {
                Some("init") => vec![EngineEvent::SessionStarted {
                    native_session_id: str_field(raw, "session_id"),
                }],
                // Kanca olayları (hook_started / hook_response) yalnız gürültüdür.
                Some(sub) if sub.starts_with("hook_") => Vec::new(),
                _ => unknown(raw),
            },
            "assistant" => assistant_events(self, raw),
            "user" => self.user_events(raw),
            "result" => result_events(raw),
            "rate_limit_event" => rate_limit_events(raw),
            _ => unknown(raw),
        }
    }

    /// Düzenleme isteğini bekleyen değişiklikler arasına kaydeder.
    fn remember_edit(&self, id: &str, path: String, change: &'static str) {
        self.pending_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), (path, change));
    }

    /// Bekleyen düzenlemeyi kimliğine göre çıkarır.
    fn take_edit(&self, id: &str) -> Option<(String, &'static str)> {
        self.pending_edits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    fn user_events(&self, raw: &Value) -> Vec<EngineEvent> {
        let Some(blocks) = raw.pointer("/message/content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            // Sonuç bloğu araç adını taşımaz; kimlik olarak `tool_use_id` kullanılır.
            let id = str_or_empty(block, "tool_use_id");
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            out.push(EngineEvent::ToolResult {
                tool: id.clone(),
                output_excerpt: excerpt(
                    &content_text(block.get("content")),
                    DEFAULT_EXCERPT_CHARS,
                ),
                is_error,
            });
            // Değişiklik yalnız başarılı sonuçta kaydedilir; reddedilen/hatalı araç dosyayı değiştirmez.
            if let Some((path, change)) = self.take_edit(&id).filter(|_| !is_error) {
                out.push(EngineEvent::FileChanged {
                    path,
                    change: change.to_string(),
                });
            }
        }
        out
    }
}

fn assistant_events(engine: &ClaudeEngine, raw: &Value) -> Vec<EngineEvent> {
    let Some(blocks) = raw.pointer("/message/content").and_then(Value::as_array) else {
        return unknown(raw);
    };
    let mut out = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = str_or_empty(block, "text");
                if !text.is_empty() {
                    out.push(EngineEvent::AssistantText { text });
                }
            }
            Some("tool_use") => {
                let tool = str_or_empty(block, "name");
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                if let Some(change) = file_edit_change(&tool) {
                    let path = str_field(&input, "file_path")
                        .or_else(|| str_field(&input, "notebook_path"));
                    if let (Some(path), Some(id)) = (path, str_field(block, "id")) {
                        engine.remember_edit(&id, path, change);
                    }
                }
                out.push(EngineEvent::ToolCall { tool, input });
            }
            Some("thinking" | "redacted_thinking") => {}
            _ => out.push(EngineEvent::Unknown { raw: block.clone() }),
        }
    }
    // Üst düzey `error` alanı (ör. "rate_limit") başarısız mesajı işaretler.
    if let Some(code) = raw.get("error").and_then(Value::as_str) {
        if code == "rate_limit" || is_quota_message(code) {
            out.push(EngineEvent::QuotaExhausted {
                message: code.to_string(),
                resets_at: None,
            });
        } else {
            out.push(EngineEvent::Error {
                message: code.to_string(),
            });
        }
    }
    out
}

/// Dosya düzenleyen araçlar ve bulunan değişiklik türü. Diğer araçlar `None` döner.
fn file_edit_change(tool: &str) -> Option<&'static str> {
    match tool {
        "Write" => Some("write"),
        "Edit" | "MultiEdit" | "NotebookEdit" => Some("edit"),
        _ => None,
    }
}

fn result_events(raw: &Value) -> Vec<EngineEvent> {
    let subtype = str_or_empty(raw, "subtype");
    let is_error = raw.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    if is_error || subtype != "success" {
        let text = str_or_empty(raw, "result");
        let message = if text.is_empty() { subtype } else { text };
        if is_quota_message(&message) {
            return vec![EngineEvent::QuotaExhausted {
                resets_at: reset_from(&message),
                message,
            }];
        }
        return vec![EngineEvent::Error { message }];
    }
    let usage = raw.get("usage");
    vec![
        EngineEvent::Usage {
            input_tokens: usage.and_then(|u| u64_field(u, "input_tokens")),
            output_tokens: usage.and_then(|u| u64_field(u, "output_tokens")),
            cached_tokens: usage.and_then(|u| u64_field(u, "cache_read_input_tokens")),
        },
        EngineEvent::TurnCompleted,
    ]
}

fn rate_limit_events(raw: &Value) -> Vec<EngineEvent> {
    let Some(info) = raw.get("rate_limit_info") else {
        return Vec::new();
    };
    // Yalnız "rejected" engellenmiş istek demektir; "allowed" ve "allowed_warning" değildir.
    if info.get("status").and_then(Value::as_str) != Some("rejected") {
        return Vec::new();
    }
    let resets_at = info
        .get("resetsAt")
        .and_then(Value::as_i64)
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
    vec![EngineEvent::QuotaExhausted {
        message: "rate limit rejected".to_string(),
        resets_at,
    }]
}

/// "...usage limit reached|<unix saniye>" biçimindeki sıfırlanma zamanını okur.
fn reset_from(message: &str) -> Option<DateTime<Utc>> {
    let tail = message.rsplit_once('|')?.1.trim();
    let secs: i64 = tail.parse().ok()?;
    Utc.timestamp_opt(secs, 0).single()
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
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

fn u64_field(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
