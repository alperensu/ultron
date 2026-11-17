//! Claude Code sürücüsü: `claude -p --output-format stream-json --verbose`.
//!
//! Gerçek akış (claude 2.1.325) ile doğrulanan olay tipleri: `system`/`init`,
//! `system`/`hook_*` (yok sayılır), `assistant` (metin blokları), `rate_limit_event`,
//! `result`. `tool_use` / `tool_result` biçimi Anthropic mesaj biçiminden alınmıştır;
//! canlı bir araç çağrısı yakalanmadan doğrulanmamıştır.

use std::collections::HashMap;
use std::sync::Mutex;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{
    Capabilities, CommandSpec, Engine, RestrictionLevel, ToolPolicy, TurnRequest, is_quota_message,
};
use crate::config::{EngineConfig, EngineId};
use crate::eventlog::{DEFAULT_EXCERPT_CHARS, excerpt};
use crate::events::EngineEvent;
use crate::Paths;

/// Ultron hook'unun her ret gerekçesinin başına eklediği ön ek. `hook_response` olaylarında
/// komut yolu bulunmadığından olayın Ultron'a ait olduğu yalnız bu ön ekle anlaşılır.
pub const HOOK_REASON_PREFIX: &str = "[Ultron] ";

/// Kullanıcı argümanlarıyla değiştirilemeyen, güvenlik kararını etkileyen bayraklar.
const FORBIDDEN_EXTRA_ARGS: [&str; 6] = [
    "--settings",
    "--setting-sources",
    "--bare",
    "--permission-mode",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
];

/// Hook'un araç politikasını öğrenmesi için alt sürece verilen ortam değişkeni değeri.
pub fn tool_policy_env(policy: ToolPolicy) -> &'static str {
    match policy {
        ToolPolicy::Full => "full",
        ToolPolicy::Workspace => "workspace",
        ToolPolicy::ReadOnly => "readonly",
    }
}

/// Yalnız Ultron durum dizininde, güvenli bir sohbet kimliğine bağlı ayar yolu.
pub fn policy_settings_path(paths: &Paths, chat: &str) -> Result<PathBuf> {
    if chat.is_empty()
        || chat.len() > 128
        || !chat
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("Claude hook sohbet kimliği geçersiz");
    }
    Ok(paths.run.join(format!("claude-settings-{chat}.json")))
}

/// Önceki çalışmadan kalan `claude-settings-*` dosyalarını siler (en iyi çaba).
pub fn cleanup_policy_settings(paths: &Paths) {
    let Ok(entries) = std::fs::read_dir(&paths.run) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("claude-settings-") && (name.ends_with(".json") || name.ends_with(".tmp"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Exec-form hook: kabuk alıntısı/yorumlaması yok; başlatma ve zaman aşımı da ret.
pub fn install_policy_settings(paths: &Paths, chat: &str, ultron: &Path) -> Result<PathBuf> {
    use std::io::Write;
    let path = policy_settings_path(paths, chat)?;
    if !paths.root.is_absolute() || !ultron.is_absolute() || !ultron.is_file() {
        bail!("Claude politika kancası için mutlak durum ve ultron.exe yolu gerekir");
    }
    std::fs::create_dir_all(&paths.run).context("Claude hook dizini oluşturulamadı")?;
    let settings = serde_json::json!({"disableAllHooks": false, "hooks": {"PreToolUse": [{
        "matcher": "*", "hooks": [{"type": "command", "command": ultron,
            "args": ["policy", "check"], "timeout": 320, "onFailure": "block"}]
    }]}});
    let temporary = paths
        .run
        .join(format!("claude-settings-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&settings)?)?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = std::fs::rename(&temporary, &path) {
            // Claude hedef dosyayı o an açıksa Windows yeniden adlandırmayı reddedebilir.
            // İçerik zaten aynıysa mevcut dosya doğrudur.
            let same = std::fs::read(&path).is_ok_and(|old| old == serde_json::to_vec(&settings)
                .unwrap_or_default());
            if !same {
                return Err(error.into());
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("Claude hook ayarları yazılamadı; motor başlatılmadı")?;
    Ok(path)
}

/// Workspace politikasında engellenen komut desenleri.
const WORKSPACE_DENY: &str = "Bash(rm:*),Bash(del:*),Bash(Remove-Item:*),Bash(git push:*)";

/// MCP sunucusu içermeyen satır içi yapılandırma (`--strict-mcp-config` ile birlikte).
const EMPTY_MCP: &str = r#"{"mcpServers":{}}"#;

/// Yerleşik sistem talimatını korur; değişken uzunluklu araç bayraklarından önce ekler.
pub(crate) fn append_prompt_file(spec: &mut CommandSpec, path: &std::path::Path) {
    let position = spec
        .args
        .iter()
        .position(|arg| matches!(arg.as_str(), "--tools" | "--disallowedTools"))
        .unwrap_or(spec.args.len());
    spec.args.splice(
        position..position,
        [
            "--append-system-prompt-file".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    );
}

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
            "--include-hook-events".into(),
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
        // Proje/yerel ayar dosyaları hook ekleyip araç girdisini onaydan sonra
        // değiştiremesin: yalnız kullanıcı ayarları + Ultron'un --settings dosyası.
        args.extend(["--setting-sources".into(), "user".into()]);
        // Yol çözülemezse prepare_command hata vererek başlatmayı durdurur.
        let settings = Paths::resolve()
            .and_then(|paths| policy_settings_path(&paths, &req.chat_id))
            .unwrap_or_default();
        args.extend(["--settings".into(), settings.to_string_lossy().into_owned()]);
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

    fn prepare_command(&self, req: &TurnRequest, cmd: &mut tokio::process::Command) -> Result<()> {
        if self.cfg.extra_args.iter().any(|arg| {
            FORBIDDEN_EXTRA_ARGS
                .iter()
                .any(|flag| arg == flag || arg.strip_prefix(flag).is_some_and(|r| r.starts_with('=')))
        }) {
            bail!("Claude ek argümanları Ultron güvenlik ayarlarını değiştiremez");
        }
        let paths = Paths::resolve()?;
        let ultron = std::env::current_exe()
            .context("Ultron program dizini bulunamadı")?
            .with_file_name("ultron.exe");
        install_policy_settings(&paths, &req.chat_id, &ultron)?;
        cmd.env("ULTRON_CHAT_ID", &req.chat_id);
        cmd.env("ULTRON_TOOL_POLICY", tool_policy_env(req.tool_policy));
        Ok(())
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
                Some("hook_response") => hook_response_events(raw),
                // Başlatma/ilerleme olayları yalnız gürültüdür.
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

fn hook_response_events(raw: &Value) -> Vec<EngineEvent> {
    if raw.get("hook_event").and_then(Value::as_str) != Some("PreToolUse") {
        return Vec::new();
    }
    let output = ["stdout", "output"]
        .into_iter()
        .find_map(|key| serde_json::from_str::<Value>(raw.get(key)?.as_str()?).ok());
    let denied = output.as_ref().is_some_and(|value| {
        value
            .pointer("/hookSpecificOutput/permissionDecision")
            .and_then(Value::as_str)
            == Some("deny")
    });
    let failed = raw
        .get("outcome")
        .and_then(Value::as_str)
        .is_some_and(|s| s != "success")
        || raw
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0);
    // Olayda komut yolu yok; kullanıcının kendi hook'ları da PreToolUse olabilir. Yalnız
    // Ultron'un ön ekli gerekçesi taşıyan kararlar Ultron hatası sayılır.
    let reason = output
        .as_ref()
        .and_then(|value| value.pointer("/hookSpecificOutput/permissionDecisionReason"))
        .and_then(Value::as_str)
        .filter(|s| s.starts_with(HOOK_REASON_PREFIX));
    match reason {
        Some(reason) if denied || failed => vec![EngineEvent::Error {
            // Yalnız maskeli gerekçe günlüğe geçer; ham stdout/stderr aktarılmaz.
            message: crate::import::mask_secrets(reason.trim_start_matches(HOOK_REASON_PREFIX))
                .0,
        }],
        _ => Vec::new(),
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
