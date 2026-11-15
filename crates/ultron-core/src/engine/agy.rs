//! agy sürücüsü: `agy -p=<istem> --output-format stream-json` (agy 1.5.8).
//!
//! Gerçek yakalamada doğrulanan olay tipleri: `init`, `step_update` (`step_type`:
//! `user_input` / `agent_response`, `text_delta` artımlı metin, `usage`), `result`
//! (`status: "SUCCESS"`). Başarısız sonuç ve kota biçimi canlı yakalanmadı; kota
//! örneği belgelenen ya da olası sınır mesajına göre modellenmiştir.
//!
//! Gerçek yakalamada `-p` değeri olarak istem alınır (`-p` yalnız bayrak değildir);
//! bu yüzden istem argüman olarak `-p=<istem>` biçiminde verilir ve stdin kullanılmaz.

use serde_json::Value;

use super::{
    Capabilities, CommandSpec, Engine, RestrictionLevel, ToolPolicy, TurnRequest, is_quota_message,
};
use crate::config::{EngineConfig, EngineId};
use crate::events::EngineEvent;

/// Varsayılan agy modeli (yalnız Gemini modelleri kabul edilir).
pub const DEFAULT_MODEL: &str = "gemini-3.8-flash-low";

/// agy motoru.
pub struct AgyEngine {
    cfg: EngineConfig,
}

impl AgyEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self { cfg }
    }
}

impl Engine for AgyEngine {
    fn id(&self) -> EngineId {
        EngineId::Agy
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            resume: true,
            mid_turn_input: false,
            tool_restriction: RestrictionLevel::Unknown,
        }
    }

    fn command(&self, req: &TurnRequest) -> CommandSpec {
        let model = self
            .cfg
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let mut args: Vec<String> = vec![
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            model,
        ];
        if let Some(id) = &req.resume_native_id {
            args.extend(["--conversation".into(), id.clone()]);
        }
        match req.tool_policy {
            ToolPolicy::ReadOnly => args.extend(["--mode".into(), "plan".into()]),
            ToolPolicy::Workspace => {
                args.extend(["--mode".into(), "accept-edits".into(), "--sandbox".into()]);
            }
            ToolPolicy::Full => args.extend(["--mode".into(), "accept-edits".into()]),
        }
        // İstem her zaman son argüman; `--dangerously-skip-permissions` asla eklenmez.
        args.push(format!("-p={}", req.prompt));
        CommandSpec {
            program: self.cfg.binary.clone(),
            args,
            cwd: req.cwd.clone(),
            stdin: None,
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
    match raw.get("event").and_then(Value::as_str).unwrap_or_default() {
        "init" => vec![EngineEvent::SessionStarted {
            native_session_id: str_field(raw, "conversation_id"),
        }],
        "step_update" => match raw.get("step_update") {
            Some(step) => step_events(raw, step),
            None => unknown(raw),
        },
        "result" => match raw.get("result") {
            Some(result) => result_events(result),
            None => unknown(raw),
        },
        _ => unknown(raw),
    }
}

fn step_events(raw: &Value, step: &Value) -> Vec<EngineEvent> {
    match step.get("step_type").and_then(Value::as_str).unwrap_or_default() {
        "user_input" => Vec::new(),
        "agent_response" => {
            let delta = str_or_empty(step, "text_delta");
            // Boşluk tek başına gelen artımlar olay üretmez; metin aynen korunur.
            if delta.trim().is_empty() {
                Vec::new()
            } else {
                vec![EngineEvent::AssistantText { text: delta }]
            }
        }
        _ => unknown(raw),
    }
}

fn result_events(result: &Value) -> Vec<EngineEvent> {
    let status = str_or_empty(result, "status");
    if status == "SUCCESS" {
        let usage = result.get("usage");
        return vec![
            EngineEvent::Usage {
                input_tokens: usage.and_then(|u| u64_field(u, "input_tokens")),
                output_tokens: usage.and_then(|u| u64_field(u, "output_tokens")),
                cached_tokens: usage.and_then(|u| u64_field(u, "cache_read_tokens")),
            },
            EngineEvent::TurnCompleted,
        ];
    }
    let response = str_or_empty(result, "response");
    let message = if response.is_empty() {
        if status.is_empty() {
            "agy başarısız sonuç döndü".to_string()
        } else {
            status
        }
    } else {
        response
    };
    if is_quota_message(&message) {
        vec![EngineEvent::QuotaExhausted {
            message,
            resets_at: None,
        }]
    } else {
        vec![EngineEvent::Error { message }]
    }
}

/// Tur sonunda çağrılır: çıkış kodu 0 ama hiç `AssistantText` yoksa, lane.py'deki
/// "başarılı göründü ama çıktı yazılmadı" durumudur ve `Error` üretilir.
/// Çıkış kodu sıfır değilse ya da tur zaten hata/kota olayı ürettiyse `None` döner.
pub fn finalize(events: &[EngineEvent], exit_code: Option<i32>) -> Option<EngineEvent> {
    if exit_code != Some(0) {
        return None;
    }
    let already_failed = events
        .iter()
        .any(|e| matches!(e, EngineEvent::Error { .. } | EngineEvent::QuotaExhausted { .. }));
    if already_failed {
        return None;
    }
    let has_output = events
        .iter()
        .any(|e| matches!(e, EngineEvent::AssistantText { .. }));
    if has_output {
        None
    } else {
        Some(EngineEvent::Error {
            message: "agy çıkış kodu 0 ile bitti ama çıktı üretilmedi".to_string(),
        })
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
