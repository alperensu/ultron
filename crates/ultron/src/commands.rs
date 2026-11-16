//! `ultron` komutları: istek kurma, servise bağlanma/başlatma ve olay çıktısı.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use ultron_core::ipc::{Client, PolicyArg, Request, Response};
use ultron_core::{EngineId, Paths, StoredEvent};

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
    /// Servis durumunu gösterir.
    Status,
    /// ultrond'u oturum açılışında başlatan görevi kurar ya da kaldırır.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
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
