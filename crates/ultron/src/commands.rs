//! `ultron` komutları: istek kurma, servise bağlanma/başlatma ve olay çıktısı.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use ultron_core::ipc::{Client, PolicyArg, Request, Response};
use ultron_core::{EngineId, Paths, StoredEvent};
use ultron_core::engine::claude::HOOK_REASON_PREFIX;
use ultron_core::import::mask_secrets;
use ultron_core::policy::{Action, Decision};

/// Görev Zamanlayıcı görev adı.
const TASK_NAME: &str = r"Ultron\ultrond";

#[derive(Debug, Parser)]
#[command(name = "ultron", version, about = "Ultron terminal istemcisi")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Claude PreToolUse isteğini requester olarak denetler.
    Policy {
        #[command(subcommand)]
        action: PolicyCmd,
    },
    /// Mesaj gönderir ve olayları akıtarak yazdırır.
    Chat {
        /// Mevcut sohbet kimliği; yoksa yeni sohbet açılır.
        #[arg(long)]
        chat: Option<String>,
        #[arg(long, value_enum)]
        engine: Option<EngineArg>,
        /// Çalışma dizini (varsayılan: geçerli dizin).
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "workspace")]
        policy: PolicyOpt,
        /// Gönderilecek mesaj.
        message: String,
    },
    /// Sohbeti başka bir motora geçirir.
    Switch {
        #[arg(long)]
        chat: String,
        #[arg(long, value_enum)]
        to: EngineArg,
    },
    /// Bir sohbetin olaylarını yazdırır.
    Events {
        #[arg(long)]
        chat: String,
        #[arg(long)]
        follow: bool,
    },
    /// Geçmiş mesajlarda arar.
    History {
        #[command(subcommand)]
        action: HistoryCmd,
    },
    /// Proje ve geçmiş notu içe aktarma; sohbet içe aktarmaz.
    Import {
        #[command(subcommand)]
        action: ImportCmd,
    },
    /// Servis durumunu gösterir.
    Status,
    /// ultrond'u oturum açılışında başlatan görevi kurar ya da kaldırır.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum PolicyCmd {
    /// Hook JSON'unu stdin'den alır; her hata güvenli ret döndürür.
    Check,
}

/// Claude hook girdisini doğrular; göreli dosya yollarını hook cwd'sine bağlar.
/// Bilinmeyen araçlar onayla bile açılmaz; yeni araçlar açıkça eşlenmelidir.
pub fn hook_action(input: &Value) -> Result<Action> {
    let field = |object: &Value, key: &str| -> Result<String> {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && !s.contains('\0'))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Hook girdisinde zorunlu alan eksik veya geçersiz"))
    };
    let _session = field(input, "session_id")?;
    if input.get("hook_event_name").and_then(Value::as_str) != Some("PreToolUse") {
        bail!("Hook olay türü PreToolUse değil");
    }
    let cwd = PathBuf::from(field(input, "cwd")?);
    if !cwd.is_absolute() {
        bail!("Hook çalışma dizini mutlak olmalıdır");
    }
    let tool = field(input, "tool_name")?;
    let args = input
        .get("tool_input")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow!("Hook araç girdisi JSON nesnesi olmalıdır"))?;
    let path = |key: &str| -> Result<PathBuf> {
        let path = PathBuf::from(field(args, key)?);
        Ok(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        })
    };
    let string = |key: &str| -> Result<()> {
        if args.get(key).and_then(Value::as_str).is_none() {
            bail!("Hook araç girdisinde zorunlu metin alanı eksik");
        }
        Ok(())
    };
    if tool == "NotebookEdit" && args.get("edit_mode").and_then(Value::as_str) == Some("delete") {
        // Hücre silme içerik kaybıdır; silme onayı akışına girer.
        return Ok(Action::FileDelete {
            path: path("notebook_path")?,
        });
    }
    match tool.as_str() {
        "Bash" => Ok(Action::Shell {
            command: field(args, "command")?,
            cwd,
        }),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            match tool.as_str() {
                "Write" => string("content")?,
                "Edit" => {
                    string("old_string")?;
                    string("new_string")?;
                }
                "NotebookEdit" => string("new_source")?,
                "MultiEdit" => {
                    let edits = args
                        .get("edits")
                        .and_then(Value::as_array)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("MultiEdit düzenlemeleri eksik"))?;
                    if edits.iter().any(|edit| {
                        ["old_string", "new_string"]
                            .iter()
                            .any(|key| edit.get(key).and_then(Value::as_str).is_none())
                    }) {
                        bail!("MultiEdit düzenlemesi geçersiz");
                    }
                }
                _ => unreachable!(),
            }
            Ok(Action::FileWrite {
                path: path(if tool == "NotebookEdit" {
                    "notebook_path"
                } else {
                    "file_path"
                })?,
            })
        }
        // Yan etkisiz iç araçlar açıkça eşlenir; politika motoru yine ToolCall olarak değerlendirir.
        "Task" | "Agent" | "BashOutput" | "KillShell" | "LS" | "NotebookRead" | "ExitPlanMode"
        | "AskUserQuestion" | "Skill" => Ok(Action::ToolCall {
            tool,
            input: args.clone(),
        }),
        "Read" | "Grep" | "Glob" | "WebFetch" | "WebSearch" | "TodoWrite" => {
            let mut args = args.clone();
            match tool.as_str() {
                "Read" => {
                    args["file_path"] =
                        Value::String(path("file_path")?.to_string_lossy().into_owned());
                }
                "Grep" | "Glob" => {
                    field(&args, "pattern")?;
                    // Arama kapsamı verilmemişse daemon'un kendi cwd'sini kullanma.
                    args["path"] = Value::String(
                        if args.get("path").is_some() {
                            path("path")?
                        } else {
                            cwd.clone()
                        }
                        .to_string_lossy()
                        .into_owned(),
                    );
                }
                "WebFetch" => {
                    field(&args, "url")?;
                    field(&args, "prompt")?;
                }
                "WebSearch" => {
                    field(&args, "query")?;
                }
                "TodoWrite" if !args.get("todos").is_some_and(Value::is_array) => {
                    bail!("TodoWrite listesi eksik")
                }
                _ => {}
            }
            Ok(Action::ToolCall { tool, input: args })
        }
        other => {
            let name: String = mask_secrets(other).0.chars().take(64).collect();
            bail!("Bilinmeyen araç güvenlik nedeniyle reddedildi: {name}")
        }
    }
}

/// Claude'un belgelenmiş karar biçimi; hata ayrıntıları veya ham girdi yazılmaz.
///
/// `Allow` yalnız tam yetkili (`Full`) turda `allow` olarak yazılır. Workspace ve salt okunur
/// turlarda karar verilmez (`{}`): Claude'un kendi izin kipi geçerli kalır, böylece hook
/// o kipin kısıtını aşamaz. Ret her zaman ret kalır.
pub fn hook_output(decision: Decision, emit_allow: bool) -> Value {
    let reason = match decision {
        Decision::Allow if emit_allow => {
            return serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse", "permissionDecision": "allow",
                "permissionDecisionReason":
                    format!("{HOOK_REASON_PREFIX}Ultron politikası işlemi onayladı")
            }});
        }
        Decision::Allow => return serde_json::json!({}),
        Decision::Deny { reason } if !reason.trim().is_empty() => mask_secrets(&reason).0,
        Decision::Deny { .. } => "Ultron politikası işlemi reddetti".into(),
        Decision::Ask { .. } => "Onay tamamlanmadı; işlem reddedildi".into(),
    };
    let reason = format!("{HOOK_REASON_PREFIX}{reason}");
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "deny",
            "permissionDecisionReason": reason
        },
        "systemMessage": reason
    })
}

/// Claude'un stdout kod çözümünden bağımsız olması için ASCII'ye kaçışlanmış JSON.
pub fn hook_json(value: &Value) -> String {
    let mut out = String::new();
    for ch in value.to_string().chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn hook_denied(reason: &str) -> Value {
    hook_output(
        Decision::Deny {
            reason: reason.into(),
        },
        false,
    )
}

/// Hook'un onay beklemesi için üst sınır. Claude hook zaman aşımı (320 sn) bundan çok
/// büyüktür; ret her zaman Claude'un hook'u iptal etmesinden önce yazılır.
pub const HOOK_APPROVAL_WAIT_SECS: u32 = 240;

/// Requester bağlantısı: yalnız run/token okunur; servis başlatılmaz, onay çözülmez.
pub async fn check_hook(
    input: Value,
    paths: &Paths,
    pipe: &str,
    chat_id: Option<String>,
    wait_secs: u32,
    emit_allow: bool,
) -> Value {
    let action = match hook_action(&input) {
        Ok(action) => action,
        Err(error) => return hook_denied(&error.to_string()),
    };
    // Karar özgün eylem üzerinden verilir. Servis, onay kaydı ve olay günlüğüne yazmadan
    // önce sırları kendisi maskeler; istemci çıktısı da ham girdiyi hiç içermez.
    if chat_id.as_deref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    }) {
        return hook_denied("Ultron sohbet kimliği geçersiz; işlem reddedildi");
    }
    let wait_secs = wait_secs.min(HOOK_APPROVAL_WAIT_SECS);
    let request = async {
        let token = std::fs::read_to_string(ultron_core::ipc::token_path(paths))?;
        let mut client = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Client::connect(pipe, token.trim()),
        )
        .await??;
        client
            .send(&Request::PolicyCheck {
                action,
                chat_id,
                engine: EngineId::Claude,
                wait_secs,
            })
            .await?;
        match client.next().await? {
            Some(Response::Ok { data }) => serde_json::from_value::<Decision>(
                data.get("decision").cloned().unwrap_or(Value::Null),
            )
            .map_err(anyhow::Error::from),
            _ => bail!("Politika servisi geçerli karar vermedi"),
        }
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(wait_secs) + 10),
        request,
    )
    .await
    {
        Ok(Ok(decision)) => hook_output(decision, emit_allow),
        Ok(Err(_)) => hook_denied(
            "Ultron politika servisine ulaşılamadı veya geçersiz yanıt alındı; işlem reddedildi",
        ),
        Err(_) => hook_denied("Ultron onayı zaman aşımına uğradı; işlem reddedildi"),
    }
}

async fn policy_hook_stdin() -> Value {
    // Ayrık okuyucu: stdin EOF gelmezse Tokio'nun bloklayan stdin iş parçacığını
    // kapatmasını beklemeyiz. Boyut ve okuma süresi de sınırlıdır.
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let result = std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let input = match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok((Ok(_), bytes))) if bytes.len() <= 1024 * 1024 => {
            serde_json::from_slice::<Value>(&bytes).ok()
        }
        _ => None,
    };
    let Some(input) = input else {
        return hook_denied("Hook girdisi okunamadı, geçersiz veya sınırı aştı; işlem reddedildi");
    };
    let (Ok(paths), Ok(pipe)) = (Paths::resolve(), ultron_core::ipc::pipe_name()) else {
        return hook_denied("Ultron servis yapılandırması bulunamadı; işlem reddedildi");
    };
    let chat = match std::env::var("ULTRON_CHAT_ID") {
        Ok(id) => Some(id),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return hook_denied("Ultron sohbet kimliği okunamadı; işlem reddedildi"),
    };
    let emit_allow = std::env::var("ULTRON_TOOL_POLICY").as_deref() == Ok("full");
    check_hook(input, &paths, &pipe, chat, HOOK_APPROVAL_WAIT_SECS, emit_allow).await
}

/// Hook sürecinin beklenmedik bir panikte de güvenle kapanması: Claude yalnız çıkış kodu 2'yi
/// (veya geçerli ret JSON'unu) engelleme sayar; panik başka kodla çıkarsa araç çalışabilirdi.
pub fn install_policy_panic_guard() {
    std::panic::set_hook(Box::new(|_| {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "{HOOK_REASON_PREFIX}Politika kancası beklenmedik bir hata verdi; işlem reddedildi"
        );
        std::process::exit(2);
    }));
}

/// `ultron policy check`: kararı stdout'a yazar. Yazma başarısızsa (kapalı boru) çıkış 2.
async fn policy_check_main() -> Result<()> {
    use std::io::Write;
    install_policy_panic_guard();
    let line = hook_json(&policy_hook_stdin().await);
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        let _ = writeln!(
            std::io::stderr(),
            "{HOOK_REASON_PREFIX}Karar yazılamadı; işlem reddedildi"
        );
        std::process::exit(2);
    }
    Ok(())
}

#[derive(Debug, Subcommand)]
pub enum HistoryCmd {
    Search {
        query: String,
        #[arg(long)]
        chat: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum ImportCmd {
    /// Claude/Codex oturumlarından proje klasörlerini bulur, projeleri oluşturur ve
    /// "Şimdiye kadar yapılanlar" notunu yazar.
    Projects {
        /// Yalnız listeler: motor çağrılmaz, hiçbir şey yazılmaz.
        #[arg(long)]
        dry_run: bool,
    },
    /// Eski içe aktarıcının oluşturduğu sohbetleri (yedek alarak) siler.
    RemoveChats {
        /// Yalnız sayar: yedek ve silme yapılmaz.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum ServiceCmd {
    /// Görev Zamanlayıcı görevini oluşturur (`schtasks /Create`).
    Install,
    /// Görevi siler (`schtasks /Delete`).
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EngineArg {
    Claude,
    Codex,
    Agy,
}

impl From<EngineArg> for EngineId {
    fn from(e: EngineArg) -> EngineId {
        match e {
            EngineArg::Claude => EngineId::Claude,
            EngineArg::Codex => EngineId::Codex,
            EngineArg::Agy => EngineId::Agy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PolicyOpt {
    Readonly,
    Workspace,
    Full,
}

impl From<PolicyOpt> for PolicyArg {
    fn from(p: PolicyOpt) -> PolicyArg {
        match p {
            PolicyOpt::Readonly => PolicyArg::ReadOnly,
            PolicyOpt::Workspace => PolicyArg::Workspace,
            PolicyOpt::Full => PolicyArg::Full,
        }
    }
}

/// `schtasks /Create` argümanları (saf fonksiyon; çalıştırmaz).
pub fn schtasks_install_args(ultrond: &Path) -> Vec<String> {
    vec![
        "/Create".into(),
        "/SC".into(),
        "ONLOGON".into(),
        "/RL".into(),
        "LIMITED".into(),
        "/TN".into(),
        TASK_NAME.into(),
        "/TR".into(),
        format!("\"{}\"", ultrond.display()),
        "/F".into(),
    ]
}

/// `schtasks /Delete` argümanları.
pub fn schtasks_uninstall_args() -> Vec<String> {
    vec![
        "/Delete".into(),
        "/TN".into(),
        TASK_NAME.into(),
        "/F".into(),
    ]
}

/// `ultrond.exe`'nin yolu: `ultron.exe`'nin yanında.
pub fn ultrond_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("çalışan ikili bulunamadı")?;
    ultron_core::client::locate_ultrond(&exe)
}

/// Bir olayı tek satırlık çıktıya çevirir. `verbose` kapalıyken (sohbet akışı) kullanıcı
/// metni ve gürültülü olaylar atlanır.
pub fn render_event(ev: &StoredEvent, verbose: bool) -> Option<String> {
    let p = &ev.payload;
    let s = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match ev.kind.as_str() {
        "assistant_text" => Some(s("text")),
        "user_text" => verbose.then(|| format!("[kullanıcı] {}", s("text"))),
        "command" => {
            let code = p
                .get("exit_code")
                .and_then(Value::as_i64)
                .map_or(String::new(), |c| format!(" (çıkış {c})"));
            Some(format!("$ {}{code}", one_line(&s("command"))))
        }
        "tool_call" => Some(format!(
            "[araç] {} {}",
            s("tool"),
            clip(&one_line(&p.get("input").map_or(String::new(), Value::to_string)), 100)
        )),
        "tool_result" => (verbose || p.get("is_error") == Some(&Value::Bool(true))).then(|| {
            let tag = if p.get("is_error") == Some(&Value::Bool(true)) {
                " HATA"
            } else {
                ""
            };
            format!("[araç sonucu{tag}] {}", s("tool"))
        }),
        "file_changed" => Some(format!("[dosya] {} {}", s("change"), s("path"))),
        "engine_switched" => Some(format!(
            "!! MOTOR DEĞİŞTİ: {} -> {} ({})",
            s("from"),
            s("to"),
            s("reason")
        )),
        "external_changes" => Some(format!(
            "[dış değişiklik] {}",
            clip(&one_line(&s("summary")), 160)
        )),
        "quota_exhausted" => Some(format!("!! KOTA DOLDU: {}", s("message"))),
        "error" => Some(format!("HATA: {}", s("message"))),
        other => verbose.then(|| format!("[{other}]")),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Ortak çekirdek istemcisine bağlanır.
pub async fn connect_or_start() -> Result<Client> {
    let paths = Paths::resolve()?;
    let exe = std::env::current_exe().context("çalışan ikili bulunamadı")?;
    let daemon = ultron_core::client::locate_ultrond(&exe)
        .unwrap_or_else(|_| exe.with_file_name("ultrond.exe"));
    ultron_core::client::connect_or_start(&paths, &daemon).await
}

/// Yanıtları, bitiş (`done`) gelene ya da bağlantı kapanana dek yazdırır.
async fn stream(client: &mut Client, verbose: bool) -> Result<Option<Value>> {
    loop {
        match client.next().await? {
            None => return Ok(None),
            Some(Response::Event(ev)) => {
                if let Some(line) = render_event(&ev, verbose) {
                    println!("{line}");
                }
            }
            Some(Response::Ok { data }) => {
                // Follow abonelik onayı çıktı ya da bitiş değildir.
                if data.get("subscribed") == Some(&Value::Bool(true)) {
                    continue;
                }
                if data.get("done") == Some(&Value::Bool(true)) {
                    return Ok(Some(data));
                }
                if let Some(chat) = data.get("chat_id").and_then(Value::as_str) {
                    eprintln!("sohbet: {chat}");
                }
            }
            Some(Response::Error { message }) => bail!("{message}"),
        }
    }
}

/// Komutu çalıştırır.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Policy {
            action: PolicyCmd::Check,
        } => policy_check_main().await,
        Cmd::Service { action } => service(action),
        Cmd::Chat {
            chat,
            engine,
            cwd,
            policy,
            message,
        } => {
            let cwd = match cwd {
                Some(dir) => dir,
                None => std::env::current_dir()?,
            };
            let cwd = std::path::absolute(&cwd)?;
            let mut client = connect_or_start().await?;
            client
                .send(&Request::Send {
                    chat_id: chat,
                    cwd: cwd.to_string_lossy().into_owned(),
                    engine: engine.map(EngineId::from),
                    text: message,
                    policy: policy.into(),
                    elevated: false,
                    project_id: None,
                })
                .await?;
            let done = stream(&mut client, false).await?;
            match done.as_ref().and_then(|d| d.get("outcome")).and_then(Value::as_str) {
                Some("completed" | "cancelled") => Ok(()),
                Some(other) => Err(anyhow!("tur sonucu: {other}")),
                None => Err(anyhow!("servis bağlantıyı turun bitişinden önce kapattı")),
            }
        }
        Cmd::Switch { chat, to } => {
            let mut client = connect_or_start().await?;
            let to_id = EngineId::from(to);
            client
                .send(&Request::Switch {
                    chat_id: chat.clone(),
                    to: to_id,
                })
                .await?;
            match client.next().await? {
                Some(Response::Ok { .. }) => {
                    println!("!! MOTOR DEĞİŞTİ: {chat} -> {to_id:?}");
                    Ok(())
                }
                Some(Response::Error { message }) => bail!("{message}"),
                _ => bail!("servis beklenmeyen yanıt verdi"),
            }
        }
        Cmd::Events { chat, follow } => {
            let mut client = connect_or_start().await?;
            client
                .send(&Request::Events {
                    chat_id: Some(chat),
                    follow,
                    after_seq: None,
                    limit: None,
                })
                .await?;
            stream(&mut client, true).await?;
            Ok(())
        }
        Cmd::History {
            action: HistoryCmd::Search { query, chat, limit },
        } => {
            let mut client = connect_or_start().await?;
            client
                .send(&Request::HistorySearch {
                    chat_id: chat,
                    query,
                    limit,
                })
                .await?;
            match client.next().await? {
                Some(Response::Ok { data }) => {
                    let hits = data["hits"].as_array().cloned().unwrap_or_default();
                    if hits.is_empty() {
                        println!("sonuç yok");
                    }
                    for hit in hits {
                        println!(
                            "[{}] {}",
                            hit["chat_id"].as_str().unwrap_or("?"),
                            clip(&one_line(hit["text"].as_str().unwrap_or("")), 200)
                        );
                    }
                    Ok(())
                }
                Some(Response::Error { message }) => bail!("{message}"),
                _ => bail!("servis beklenmeyen yanıt verdi"),
            }
        }
        Cmd::Import { action } => {
            // Yönetim komutları onaylayıcı sırrıyla bağlanır; motorlar bu sırra erişemez.
            let mut client = connect_approver().await?;
            let request = match action {
                ImportCmd::Projects { dry_run } => Request::ImportProjects {
                    dry_run,
                    home: None,
                },
                ImportCmd::RemoveChats { dry_run } => Request::RemoveImportedChats { dry_run },
            };
            client.send(&request).await?;
            match client.next().await? {
                Some(Response::Ok { data }) => {
                    let report = &data["report"];
                    match action {
                        ImportCmd::Projects { .. } => print!("{}", render_project_report(report)),
                        ImportCmd::RemoveChats { .. } => print!("{}", render_remove_report(report)),
                    }
                    Ok(())
                }
                Some(Response::Error { message }) => bail!("{message}"),
                _ => bail!("servis beklenmeyen yanıt verdi"),
            }
        }
        Cmd::Status => {
            let mut client = connect_or_start().await?;
            client.send(&Request::Status).await?;
            match client.next().await? {
                Some(Response::Ok { data }) => {
                    println!(
                        "ultrond çalışıyor: pid {}, sürüm {}, çalışan tur {}, varsayılan motor {}",
                        data["pid"],
                        data["version"].as_str().unwrap_or("?"),
                        data["running_turns"],
                        data["default_engine"].as_str().unwrap_or("?"),
                    );
                    Ok(())
                }
                Some(Response::Error { message }) => bail!("{message}"),
                _ => bail!("servis beklenmeyen yanıt verdi"),
            }
        }
    }
}

/// Onaylayıcı sırrıyla bağlanır (servis yoksa başlatılır).
async fn connect_approver() -> Result<Client> {
    let _ready = connect_or_start().await?;
    let paths = Paths::resolve()?;
    let secret = std::fs::read_to_string(ultron_core::ipc::approver_token_path(&paths))
        .context("onaylayıcı sırrı okunamadı")?;
    Client::connect(&ultron_core::ipc::pipe_name()?, secret.trim())
        .await
        .context("onaylayıcı bağlantısı kurulamadı")
}

/// `import projects` tablosu: yol, Claude/Codex/işçi oturum sayısı, not güncellendi mi.
pub fn render_project_report(report: &Value) -> String {
    let yes = |v: &Value| if v.as_bool() == Some(true) { "evet" } else { "hayır" };
    let mut out = format!(
        "{:<60} {:>7} {:>6} {:>5} {:<6} {}\n",
        "PROJE YOLU", "CLAUDE", "CODEX", "ISCI", "NOT", "DURUM"
    );
    for row in report["rows"].as_array().cloned().unwrap_or_default() {
        out.push_str(&format!(
            "{:<60} {:>7} {:>6} {:>5} {:<6} {}\n",
            clip(row["path"].as_str().unwrap_or("?"), 60),
            row["claude_sessions"],
            row["codex_sessions"],
            row["worker_sessions"].as_u64().unwrap_or(0),
            yes(&row["note_updated"]),
            row["note_status"].as_str().unwrap_or(""),
        ));
    }
    let dry = report["dry_run"].as_bool() == Some(true);
    out.push_str(&format!(
        "{} proje{}, {} yeni proje, {} not {}, {} eşleşmeyen oturum. Özetleyici: {}.\n",
        report["rows"].as_array().map_or(0, Vec::len),
        if dry { " (dry-run: hiçbir şey yazılmadı, motor çağrılmadı)" } else { "" },
        report["created"],
        report["notes_updated"],
        if dry { "güncellenecek" } else { "güncellendi" },
        report["unmatched_sessions"].as_u64().unwrap_or(0),
        report["summarizer"].as_str().unwrap_or("?"),
    ));
    out
}

/// `import remove-chats` çıktısı.
pub fn render_remove_report(report: &Value) -> String {
    let dry = report["dry_run"].as_bool() == Some(true);
    let mut out = format!(
        "{} içe aktarılmış sohbet, {} olay {}.
",
        report["chats"],
        report["events"],
        if dry { "silinecek (dry-run: hiçbir şey yazılmadı)" } else { "silindi" },
    );
    if let Some(backup) = report["backup"].as_str() {
        out.push_str(&format!("Yedek: {backup}
"));
    }
    out
}

/// `service install|uninstall`: yalnız açıkça çağrıldığında `schtasks` çalıştırır.
fn service(action: ServiceCmd) -> Result<()> {
    let args = match action {
        ServiceCmd::Install => schtasks_install_args(&ultrond_path()?),
        ServiceCmd::Uninstall => schtasks_uninstall_args(),
    };
    let status = Process::new("schtasks")
        .args(&args)
        .status()
        .context("schtasks çalıştırılamadı")?;
    if !status.success() {
        bail!("schtasks başarısız oldu ({status})");
    }
    Ok(())
}
