//! agy sürücüsü: `agy -p=<istem> --output-format stream-json` (agy 1.5.8).
//!
//! Gerçek yakalamada doğrulanan olay tipleri: `init`, `step_update` (`step_type`:
//! `user_input` / `agent_response`, `text_delta` artımlı metin, `usage`), `result`
//! (`status: "SUCCESS"`). Başarısız sonuç ve kota biçimi canlı yakalanmadı; kota
//! örneği belgelenen ya da olası sınır mesajına göre modellenmiştir.
//!
//! Gerçek yakalamada `-p` değeri olarak istem alınır (`-p` yalnız bayrak değildir);
//! bu yüzden istem argüman olarak `-p=<istem>` biçiminde verilir ve stdin kullanılmaz.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    Capabilities, CommandSpec, Engine, RestrictionLevel, ToolPolicy, TurnRequest, is_quota_message,
};
use crate::config::{EngineConfig, EngineId};
use crate::eventlog::{DEFAULT_EXCERPT_CHARS, excerpt};
use crate::events::EngineEvent;

/// Varsayılan agy modeli (yalnız Gemini modelleri kabul edilir).
pub const DEFAULT_MODEL: &str = "gemini-3.8-flash-low";

/// agy motoru.
pub struct AgyEngine {
    cfg: EngineConfig,
}

/// Kartı korur; Windows sınırında yalnız değişken istemin eski kısmı kırpılır.
pub(crate) fn prepend_instructions(spec: &mut CommandSpec, instructions: &str) {
    if let Some(arg) = spec
        .args
        .iter_mut()
        .rev()
        .find(|arg| arg.starts_with("-p="))
    {
        let prompt = &arg["-p=".len()..];
        let prefix = crate::prompts::prefix(instructions, "");
        let available = MAX_PROMPT_ARG_UTF16.saturating_sub("-p=".len() + utf16_len(&prefix));
        let tail = tail_with_budget(prompt, available);
        *arg = format!("-p={prefix}{tail}");
    }
}

pub(crate) fn instructions_fit(instructions: &str) -> bool {
    // Kırpma işareti ve en son kullanıcı metni için de yer bırakılır.
    utf16_len(&crate::prompts::prefix(instructions, "")) + 1024 < MAX_PROMPT_ARG_UTF16
}

fn tail_with_budget(prompt: &str, budget: usize) -> String {
    if utf16_len(prompt) <= budget {
        return prompt.to_string();
    }
    let mark_len = utf16_len(TRUNCATION_MARK);
    if budget < mark_len {
        return String::new();
    }
    let mut used = mark_len;
    let mut start = prompt.len();
    for (idx, ch) in prompt.char_indices().rev() {
        if used + ch.len_utf16() > budget {
            break;
        }
        used += ch.len_utf16();
        start = idx;
    }
    format!("{TRUNCATION_MARK}{}", &prompt[start..])
}

impl AgyEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self { cfg }
    }

    /// Göreli hata reset'ini deterministik bir saate bağlar; başarı metni taranmaz.
    pub fn parse_line_at(&self, line: &str, now: DateTime<Utc>) -> Vec<EngineEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(raw) => parse_value(&raw, now),
            Err(_) => vec![EngineEvent::Unknown {
                raw: Value::String(line.to_string()),
            }],
        }
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
        let model = DEFAULT_MODEL.to_string();
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
            ToolPolicy::ReadOnly | ToolPolicy::Full => {
                args.extend(["--mode".into(), "plan".into(), "--sandbox".into()]);
            }
            ToolPolicy::Workspace => {
                args.extend(["--mode".into(), "accept-edits".into(), "--sandbox".into()]);
            }
        }
        args.extend(self.cfg.extra_args.iter().cloned());
        // İstem her zaman son argüman; `--dangerously-skip-permissions` asla eklenmez.
        args.push(prompt_arg(&req.prompt));
        CommandSpec {
            program: self.cfg.binary.clone(),
            args,
            cwd: req.cwd.clone(),
            stdin: None,
        }
    }

    fn prepare_command(
        &self,
        req: &TurnRequest,
        _cmd: &mut tokio::process::Command,
    ) -> anyhow::Result<()> {
        if self.cfg.model.as_deref().is_some_and(|model| model != DEFAULT_MODEL) {
            anyhow::bail!("agy yalnız Gemini Flash-low modeliyle çalıştırılabilir");
        }
        // --settings, --mode, --model, --add-dir ve izin atlama seçenekleri kabul edilmez.
        if !self.cfg.extra_args.is_empty() {
            anyhow::bail!("agy ek argümanları koruma politikasını değiştiremez");
        }
        if req.resume_native_id.as_ref().is_some_and(|id| id.starts_with('-')) {
            anyhow::bail!("agy oturum kimliği seçenek olamaz");
        }
        Ok(())
    }

    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        self.parse_line_at(line, Utc::now())
    }
}

/// Windows komut satırı sınırının (32767 karakter) güvenli altında kalan istem üst sınırı,
/// UTF-16 birimi cinsinden. `-p=` öneki ve kırpma işareti bu sınırın içindedir.
const MAX_PROMPT_ARG_UTF16: usize = 30_000;

/// Kırpıldığında istemin başına eklenen işaret.
const TRUNCATION_MARK: &str = "[KIRPILDI: komut satırı sınırı]\n";

/// `-p=<istem>` argümanını kurar. İstem sınırı aşarsa en son kısmı (kullanıcı metni sonda
/// olduğu için) tutulur ve başına kırpma işareti eklenir. Kırpma karakter sınırında yapılır;
/// UTF-16 vekil çifti asla bölünmez.
fn prompt_arg(prompt: &str) -> String {
    let arg = format!("-p={prompt}");
    if utf16_len(&arg) <= MAX_PROMPT_ARG_UTF16 {
        return arg;
    }
    let budget = MAX_PROMPT_ARG_UTF16 - "-p=".len() - utf16_len(TRUNCATION_MARK);
    let mut used = 0;
    let mut start = prompt.len();
    for (idx, ch) in prompt.char_indices().rev() {
        let width = ch.len_utf16();
        if used + width > budget {
            break;
        }
        used += width;
        start = idx;
    }
    format!("-p={TRUNCATION_MARK}{}", &prompt[start..])
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn parse_value(raw: &Value, now: DateTime<Utc>) -> Vec<EngineEvent> {
    match raw.get("event").and_then(Value::as_str).unwrap_or_default() {
        "init" => vec![EngineEvent::SessionStarted {
            native_session_id: str_field(raw, "conversation_id"),
        }],
        "step_update" => match raw.get("step_update") {
            Some(step) => step_events(raw, step),
            None => unknown(raw),
        },
        "result" => match raw.get("result") {
            Some(result) => result_events(result, now),
            None => unknown(raw),
        },
        _ => unknown(raw),
    }
}

fn step_events(raw: &Value, step: &Value) -> Vec<EngineEvent> {
    let step_type = str_or_empty(step, "step_type");
    match step_type.as_str() {
        "user_input" => Vec::new(),
        "agent_response" => {
            let delta = str_or_empty(step, "text_delta");
            // Boşluk tek başına gelen artımlar (ör. satır sonu) da metnin parçasıdır; yalnız boş
            // artım olay üretmez.
            if delta.is_empty() {
                Vec::new()
            } else {
                vec![EngineEvent::AssistantText { text: delta }]
            }
        }
        // Araç adımı: step_type "tool" içerir ya da `tool_info` taşır. Gerçek yakalamada alan
        // adları doğrulanmamıştır; bu yüzden ham girdi/çıktı korunur ve eksik alanlar boş bırakılır.
        other if other.to_lowercase().contains("tool") || step.get("tool_info").is_some() => {
            tool_step_events(step, other)
        }
        _ => unknown(raw),
    }
}

/// Araç adımını çağrı (`ToolCall`) ya da sonuç (`ToolResult`) olarak eşler. Adımda `output`
/// alanı varsa sonuçtur; yoksa çağrıdır.
fn tool_step_events(step: &Value, step_type: &str) -> Vec<EngineEvent> {
    let info = step.get("tool_info").cloned().unwrap_or(Value::Null);
    let tool = str_field(&info, "name")
        .or_else(|| str_field(&info, "tool"))
        .unwrap_or_else(|| step_type.to_string());
    match step.get("output") {
        Some(output) => {
            let text = match output {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let is_error = str_field(step, "state").as_deref() == Some("FAILED")
                || info.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let mut events = vec![EngineEvent::ToolResult {
                tool,
                output_excerpt: excerpt(&text, DEFAULT_EXCERPT_CHARS),
                is_error,
            }];
            // Yalnız açık çıkış kodu taşıyan sonuç çalışmış komut kanıtıdır.
            // Alan biçimi canlı yakalamayla doğrulanmadı; eksik alanlarda tahmin yapılmaz.
            let exit_code = step.get("exit_code").or_else(|| output.get("exit_code"))
                .and_then(Value::as_i64).and_then(|code| i32::try_from(code).ok());
            let command = info.pointer("/input/command").and_then(Value::as_str);
            if let (Some(exit_code), Some(command)) = (exit_code, command) {
                events.push(EngineEvent::Command {
                    command: command.to_owned(), exit_code: Some(exit_code),
                    output_excerpt: excerpt(&text, DEFAULT_EXCERPT_CHARS),
                });
            }
            events
        }
        None => {
            let input = match info.get("input") {
                Some(input) => input.clone(),
                None if !info.is_null() => info,
                None => step.clone(),
            };
            vec![EngineEvent::ToolCall { tool, input }]
        }
    }
}

fn result_events(result: &Value, now: DateTime<Utc>) -> Vec<EngineEvent> {
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
    // agy 1.5.9: hata metni `response` değil `error` alanındadır (`response` boş gelir).
    let mut response = str_or_empty(result, "response");
    if response.is_empty() {
        response = str_or_empty(result, "error");
    }
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
            resets_at: relative_reset(&message, now),
            message,
        }]
    } else {
        vec![EngineEvent::Error { message }]
    }
}

/// Yalnız açık `Resets in 119h`, `44m`, `30s`, `1h 2m 3s` biçimleri.
/// 14 günün üzerindeki, sırasız/tekrarlı veya belirsiz süreler tahmin edilmez.
fn relative_reset(message: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = message.to_ascii_lowercase();
    let (prefix, duration) = lower.split_once("resets in ")?;
    if prefix.chars().last().is_some_and(char::is_alphanumeric)
        || duration.contains("resets in ")
    {
        return None;
    }
    let mut rest = duration.trim().strip_suffix('.').unwrap_or(duration.trim()).trim();
    let mut seconds = 0_i64;
    let mut previous = 3601;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 { return None; }
        let amount: i64 = rest[..digits].parse().ok()?;
        rest = rest[digits..].trim_start();
        let unit = match rest.as_bytes().first()? {
            b'h' => 3600,
            b'm' => 60,
            b's' => 1,
            _ => return None,
        };
        if unit >= previous { return None; }
        previous = unit;
        seconds = seconds.checked_add(amount.checked_mul(unit)?)?;
        if seconds > 14 * 24 * 3600 { return None; }
        rest = rest[1..].trim_start();
    }
    if seconds == 0 { return None; }
    now.checked_add_signed(chrono::Duration::seconds(seconds))
}

/// Tur sonunda çağrılır: çıkış kodu 0 ama hiç `AssistantText` yoksa, lane.py'deki
/// "başarılı göründü ama çıktı yazılmadı" durumudur ve `Error` üretilir.
/// Çıkış kodu sıfır değilse ya da tur zaten hata/kota olayı ürettiyse `None` döner.
pub fn finalize(events: &[EngineEvent], exit_code: Option<i32>) -> Option<EngineEvent> {
    let already_failed = events
        .iter()
        .any(|e| matches!(e, EngineEvent::Error { .. } | EngineEvent::QuotaExhausted { .. }));
    let saw_text = events
        .iter()
        .any(|e| matches!(e, EngineEvent::AssistantText { .. }));
    finalize_summary(saw_text, already_failed, exit_code)
}

/// `finalize`'ın olay listesi tutmayan biçimi: tur boyunca yalnız "metin geldi mi" ve
/// "hata/kota oldu mu" bilgisi yeterlidir.
pub fn finalize_summary(
    saw_text: bool,
    already_failed: bool,
    exit_code: Option<i32>,
) -> Option<EngineEvent> {
    if exit_code != Some(0) || already_failed || saw_text {
        return None;
    }
    Some(EngineEvent::Error {
        message: "agy çıkış kodu 0 ile bitti ama çıktı üretilmedi".to_string(),
    })
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
