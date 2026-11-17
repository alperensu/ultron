//! Codex sürücüsü: `codex exec --json` (uygulama CLI: 0.169.0-alpha.11.1).
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
use std::process::Stdio;
use std::time::{Duration, Instant};

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

/// Full onayı bile işletim sistemi sandbox'ını kaldırmaz.
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
        args.extend(["--json".into(), "--ignore-user-config".into(), "--strict-config".into()]);
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
        // Devam edilen oturumun veya profilin önceki izinlerini devralma.
        args.extend([
            "-c".into(),
            format!("sandbox_mode=\"{sandbox}\""),
            "-c".into(),
            "approval_policy=\"never\"".into(),
        ]);
        args.extend(protection_args(req.tool_policy));
        // Son argüman: istemi stdin'den oku.
        args.push("-".into());
        CommandSpec {
            program: effective_binary(&self.cfg.binary),
            args,
            cwd: req.cwd.clone(),
            stdin: Some(req.prompt.clone()),
        }
    }

    fn prepare_command(
        &self,
        req: &TurnRequest,
        cmd: &mut tokio::process::Command,
    ) -> anyhow::Result<()> {
        validate_extra_args(&self.cfg.extra_args)?;
        if req.resume_native_id.as_ref().is_some_and(|id| id.starts_with('-')) {
            anyhow::bail!("Codex oturum kimliği seçenek olamaz");
        }
        let program = cmd.as_std().get_program().to_owned();
        let help = probe_cli(&program, &req.cwd, &["exec".into(), "--help".into()])?;
        validate_protection_help(&help)?;
        // Boş TOML tablosu alt katmanlardaki sunucuları silmez. Kullanıcı katmanı
        // yüklenmez; proje/sistem katmanlarında görülen her sunucu ayrıca kapatılır.
        // Liste yalnız yapılandırmadır: MCP sunucusu veya araç çalıştırılmaz.
        let servers = probe_cli(&program, &req.cwd, &["mcp".into(), "list".into(), "--json".into()])?;
        let disabled = disabled_mcp_override(&servers)?;
        cmd.args(["-c", &disabled]);
        #[cfg(windows)]
        {
            let mut args = vec!["sandbox".into()];
            args.extend(protection_args(req.tool_policy));
            args.extend(["-c".into(), format!("sandbox_mode=\"{}\"", sandbox_for(req.tool_policy))]);
            args.extend(["--".into(), "cmd.exe".into(), "/d".into(), "/c".into(), "exit 0".into()]);
            // Aynı ikili ve sabitlenmiş arka uçla etkisiz komut. Kurulum/çalıştırma
            // başarısızsa gerçek motor süreci hiçbir zaman başlatılmaz.
            probe_cli(&program, &req.cwd, &args)?;
        }
        Ok(())
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

/// CLI yardımındaki -c TOML geçersiz kılmaları; yeni ve devam turlarında aynıdır.
fn protection_args(policy: ToolPolicy) -> Vec<String> {
    let mut args = Vec::new();
    for value in [
        "windows.sandbox=\"elevated\"".to_owned(),
        "sandbox_workspace_write.writable_roots=[]".into(),
        format!("sandbox_workspace_write.network_access={}", policy == ToolPolicy::Full),
        "sandbox_workspace_write.exclude_tmpdir_env_var=true".into(),
        "sandbox_workspace_write.exclude_slash_tmp=true".into(),
        "mcp_servers={}".into(),
        "orchestrator.mcp.enabled=false".into(),
    ] {
        args.extend(["-c".into(), value]);
    }
    args
}

fn validate_protection_help(help: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        ["--ignore-user-config", "--strict-config"].iter().all(|flag| help.contains(flag)),
        "Codex CLI zorunlu sandbox/MCP korumalarını desteklemiyor; tur başlatılmadı"
    );
    Ok(())
}

fn disabled_mcp_override(list: &str) -> anyhow::Result<String> {
    let list: Value = serde_json::from_str(list)
        .map_err(|_| anyhow::anyhow!("Codex MCP yapılandırması doğrulanamadı; tur başlatılmadı"))?;
    let servers = list.as_array()
        .ok_or_else(|| anyhow::anyhow!("Codex MCP listesi doğrulanamadı; tur başlatılmadı"))?;
    let mut disabled = toml::map::Map::new();
    for server in servers {
        let name = server.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex MCP sunucusu doğrulanamadı; tur başlatılmadı"))?;
        // Kullanıcı katmanı yokken de geçerli bir kayıt gerekir. Aynı taşıma
        // türünde etkisiz hedef kullan; komut, URL veya sırları argv'ye kopyalama.
        let (field, inert_target) = match server.pointer("/transport/type").and_then(Value::as_str) {
            Some("stdio") => ("command", "ultron-disabled-mcp"),
            Some("streamable_http") => ("url", "https://disabled.invalid/mcp"),
            _ => anyhow::bail!("Codex MCP taşıması doğrulanamadı; tur başlatılmadı"),
        };
        disabled.insert(name.to_owned(), toml::Value::Table(toml::map::Map::from_iter([
            ("enabled".into(), toml::Value::Boolean(false)),
            (field.into(), toml::Value::String(inert_target.into())),
        ])));
    }
    Ok(format!("mcp_servers={}", toml::Value::Table(disabled)))
}

/// Yardım/liste/sandbox ön kontrolü kota harcamaz; süre ve çıktı sınırı vardır.
/// Hata çıktısı yapılandırma/sır içerebileceğinden kullanıcıya kopyalanmaz.
fn probe_cli(program: &std::ffi::OsStr, cwd: &Path, args: &[String]) -> anyhow::Result<String> {
    use std::io::Read;
    const LIMIT: u64 = 1024 * 1024;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|_| anyhow::anyhow!(
        "Codex sandbox/MCP koruması doğrulanamadı; tur başlatılmadı"))?;
    let stdout = child.stdout.take().expect("ön kontrol stdout borusu");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(LIMIT + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().ok().and_then(Result::ok);
    anyhow::ensure!(status.is_some_and(|status| status.success()),
        "Codex sandbox/MCP koruması uygulanamadı; tur başlatılmadı");
    let bytes = bytes.filter(|bytes| bytes.len() as u64 <= LIMIT)
        .ok_or_else(|| anyhow::anyhow!("Codex koruma ön kontrolü çıktı sınırını aştı; tur başlatılmadı"))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Codex koruma ön kontrolü doğrulanamadı; tur başlatılmadı"))
}

/// Yapılandırma, profil ve ek kök seçenekleri sandbox sözleşmesini değiştirebilir.
/// Ek argümanlar yalnız sunumu etkileyen seçeneklerle sınırlıdır.
fn validate_extra_args(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ephemeral" => {}
            "--color" if args.next().is_some_and(|v| matches!(v.as_str(), "auto" | "always" | "never")) => {}
            "--color=auto" | "--color=always" | "--color=never" => {}
            _ => anyhow::bail!("Codex ek argümanı güvenli seçenek listesinde değil"),
        }
    }
    Ok(())
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

    #[test]
    fn mcp_override_disables_every_inherited_server_without_copying_secrets() {
        let override_arg = disabled_mcp_override(r#"[{"name":"files.external","enabled":true,"transport":{"type":"stdio","command":"private-command","env":{"TOKEN":"sentetik"}}},{"name":"other","enabled":false,"transport":{"type":"streamable_http","url":"https://private.invalid"}}]"#).unwrap();
        let config = override_arg.parse::<toml::Table>().unwrap();
        assert_eq!(config["mcp_servers"]["files.external"]["enabled"].as_bool(), Some(false));
        assert_eq!(config["mcp_servers"]["other"]["enabled"].as_bool(), Some(false));
        assert_eq!(config["mcp_servers"]["files.external"]["command"].as_str(), Some("ultron-disabled-mcp"));
        assert_eq!(config["mcp_servers"]["other"]["url"].as_str(), Some("https://disabled.invalid/mcp"));
        assert!(!override_arg.contains("sentetik"));
        assert!(!override_arg.contains("private"));
        assert!(disabled_mcp_override(r#"[{"unexpected":true}]"#).is_err());
        assert!(disabled_mcp_override("{}").is_err());
        assert!(disabled_mcp_override("invalid").is_err());
    }

    #[test]
    fn missing_cli_protection_fails_closed_in_turkish() {
        assert!(validate_protection_help("old CLI").unwrap_err().to_string().contains("tur başlatılmadı"));
        assert!(validate_protection_help("--ignore-user-config --strict-config").is_ok());
        let temp = tempfile::tempdir().unwrap();
        let error = probe_cli(temp.path().join("missing-codex.exe").as_os_str(), temp.path(), &[]).unwrap_err();
        assert!(error.to_string().contains("tur başlatılmadı"));
    }

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
