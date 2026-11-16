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

use std::path::{Path, PathBuf};

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

/// Yerleşik Codex talimatlarına dokunmadan kullanıcı isteminin önüne ekler.
pub(crate) fn prepend_instructions(spec: &mut CommandSpec, instructions: &str) {
    let prompt = spec.stdin.as_deref().unwrap_or_default();
    spec.stdin = Some(crate::prompts::prefix(instructions, prompt));
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
        args.extend(self.cfg.extra_args.iter().cloned());
        // Son argüman: istemi stdin'den oku.
        args.push("-".into());
        CommandSpec {
            program: effective_binary(&self.cfg.binary),
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

/// Yapılandırmada varsayılan `codex` adı duruyorsa güncel ikiliyi otomatik bulur;
/// açık yol verilmişse dokunmaz.
fn effective_binary(binary: &str) -> String {
    if binary != "codex" {
        return binary.to_string();
    }
    let env = |k: &str| std::env::var(k).ok();
    let config_text = std::env::var_os("USERPROFILE")
        .map(|h| PathBuf::from(h).join(".codex").join("config.toml"))
        .and_then(|p| std::fs::read_to_string(p).ok());
    let root = Path::new(r"C:\Program Files\WindowsApps");
    // Dizin listesi reddedilebilir; hata olursa aday yok sayılır.
    let candidates: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    resolve_codex_binary(&env, config_text.as_deref(), &candidates, &|| None)
        .to_string_lossy()
        .into_owned()
}

/// Appx klasör adından (`OpenAI.Codex_<sürüm>_<mimari>__<id>`) sürüm sayılarını çıkarır.
fn appx_version(dir: &Path) -> Option<Vec<u64>> {
    let name = dir.file_name()?.to_str()?;
    let rest = name.strip_prefix("OpenAI.Codex_")?;
    let ver = rest.split('_').next()?;
    ver.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Codex ikilisi çözümü (saf çekirdek). Sıra: (a) `ULTRON_CODEX_BIN`, (b) `config.toml`
/// içindeki `mcp_servers.node_repl.env.CODEX_CLI_PATH`, (c) en yeni `OpenAI.Codex_*` appx
/// klasörünün `app/resources/codex.exe` dosyası, (d) PATH araması; hiçbiri yoksa `codex`.
/// Yalnız var olan dosyalar kabul edilir (a hariç: açık istek aynen kullanılır).
pub fn resolve_codex_binary(
    env: &dyn Fn(&str) -> Option<String>,
    config_toml_text: Option<&str>,
    candidate_dirs: &[PathBuf],
    path_lookup: &dyn Fn() -> Option<PathBuf>,
) -> PathBuf {
    if let Some(v) = env("ULTRON_CODEX_BIN").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(v);
    }
    if let Some(p) = config_toml_text
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| {
            t.get("mcp_servers")?
                .get("node_repl")?
                .get("env")?
                .get("CODEX_CLI_PATH")?
                .as_str()
                .map(PathBuf::from)
        })
        .filter(|p| p.is_file())
    {
        return p;
    }
    let mut found: Vec<(Vec<u64>, PathBuf)> = candidate_dirs
        .iter()
        .filter_map(|d| {
            let exe = d.join("app").join("resources").join("codex.exe");
            Some((appx_version(d)?, exe)).filter(|(_, e)| e.is_file())
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some((_, exe)) = found.pop() {
        return exe;
    }
    path_lookup().unwrap_or_else(|| PathBuf::from("codex"))
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
        // Öğe düzeyindeki hata tur sonunu belirlemez (ör. yeniden deneme); kota yalnız
        // turn.failed / üst düzey error olayında değerlendirilir.
        "error" => vec![EngineEvent::Error {
            message: str_or_empty(item, "message"),
        }],
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

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn make_appx(root: &Path, name: &str) -> PathBuf {
        let d = root.join(name);
        let res = d.join("app").join("resources");
        std::fs::create_dir_all(&res).unwrap();
        std::fs::write(res.join("codex.exe"), b"x").unwrap();
        d
    }

    #[test]
    fn explicit_env_wins() {
        let env = |k: &str| (k == "ULTRON_CODEX_BIN").then(|| "X:/sentetik/codex.exe".to_string());
        let got = resolve_codex_binary(&env, None, &[], &|| Some(PathBuf::from("p")));
        assert_eq!(got, PathBuf::from("X:/sentetik/codex.exe"));
    }

    #[test]
    fn config_toml_cli_path_used() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("codex.exe");
        std::fs::write(&exe, b"x").unwrap();
        let text = format!(
            "[mcp_servers.node_repl.env]
CODEX_CLI_PATH = {:?}
",
            exe.to_string_lossy()
        );
        let got = resolve_codex_binary(&|_| None, Some(&text), &[], &|| None);
        assert_eq!(got, exe);
    }

    #[test]
    fn newest_appx_version_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let a = make_appx(tmp.path(), "OpenAI.Codex_26.9.0.0_x64__abc");
        let b = make_appx(tmp.path(), "OpenAI.Codex_26.10.1.0_x64__abc");
        let missing = tmp.path().join("OpenAI.Codex_27.0.0.0_x64__abc");
        let got = resolve_codex_binary(&|_| None, None, &[a, missing, b.clone()], &|| None);
        assert_eq!(got, b.join("app").join("resources").join("codex.exe"));
    }

    #[test]
    fn falls_back_to_path() {
        let got = resolve_codex_binary(&|_| None, Some("bozuk ["), &[], &|| {
            Some(PathBuf::from("Z:/sentetik/bin/codex.exe"))
        });
        assert_eq!(got, PathBuf::from("Z:/sentetik/bin/codex.exe"));
        assert_eq!(
            resolve_codex_binary(&|_| None, None, &[], &|| None),
            PathBuf::from("codex")
        );
    }
}
