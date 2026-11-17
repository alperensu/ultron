//! IPC sunucusu: named pipe üzerinde token doğrulamalı, satır tabanlı JSON protokolü.

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use ultron_core::engine::{Engine, ToolPolicy, TurnOutcome};
use ultron_core::events::{EngineEvent, SwitchReason};
use ultron_core::ipc::{
    self, ChatDetail, MAX_LINE_BYTES, Request, Response, ct_eq, read_frame, sys, write_msg,
};
use ultron_core::summarizer::{AgySummarizer, AutoSummarizer, ClaudeSummarizer, NullSummarizer, Summarizer};
use ultron_core::config::SummarizerMode;
use ultron_core::{Config, Db, EngineId, EventLog, Paths, SessionManager, SessionStatus};

use crate::keepawake::KeepAwake;

#[path = "approvals.rs"]
pub mod approvals;

/// Kimlik doğrulaması (Hello) için tanınan süre.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Arama sonucu üst sınırı.
const MAX_SEARCH_LIMIT: usize = 100;

/// Olay replay'inde tek sorguda okunan en fazla olay sayısı (bellek sınırı).
const REPLAY_PAGE: usize = 500;

type Reader = BufReader<ReadHalf<NamedPipeServer>>;
type Writer = WriteHalf<NamedPipeServer>;

/// Sunucu başlatma ayarları.
pub struct ServerConfig {
    pub paths: Paths,
    /// Pipe adı (üretimde `ipc::pipe_name()`; testlerde benzersiz ad).
    pub pipe_name: String,
    pub cfg: Config,
    pub engines: HashMap<EngineId, Arc<dyn Engine>>,
}

struct State {
    manager: SessionManager,
    db: Arc<Db>,
    log: Arc<EventLog>,
    token: String,
    approver_token: String,
    keep: KeepAwake,
    default_engine: EngineId,
    turns: Mutex<JoinSet<()>>,
    paths: Paths,
    quota: ultron_core::quota::QuotaService,
    approvals: approvals::Approvals,
    /// İçe aktarma/silme işlemleri tek seferde bir tane çalışır.
    import_lock: tokio::sync::Mutex<()>,
}

/// Yalnız testlerde `home` geçersiz kılmasını açar (üretimde her zaman USERPROFILE).
const HOME_OVERRIDE_ENV: &str = "ULTRON_TEST_ALLOW_HOME_OVERRIDE";

fn require_approver(auth: &ConnectionAuth) -> Result<()> {
    if auth.role != ConnectionRole::Approver {
        return Err(anyhow!("yetkisiz: onaylayıcı rolü gerekli"));
    }
    Ok(())
}

// Aynı masaüstünün iki penceresi birbirinin dosya işlemiyle yarışmaz.
use crate::trash_ops::TRASH_LOCK;
static POLICY_LOCK: Mutex<()> = Mutex::new(());

fn read_policy_table(paths: &Paths) -> Result<toml::Table> {
    use std::io::Read;
    use std::os::windows::fs::MetadataExt;
    let path = paths.root.join("policy.toml");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(meta.is_file() && meta.file_attributes() & 0x400 == 0, "policy.toml düzenli bir dosya olmalı");
    const MAX_POLICY_BYTES: u64 = 64 * 1024;
    let mut text = String::new();
    File::open(path)?.take(MAX_POLICY_BYTES + 1).read_to_string(&mut text)?;
    anyhow::ensure!(text.len() as u64 <= MAX_POLICY_BYTES, "policy.toml çok büyük (en çok 64 KiB)");
    text.parse().map_err(|_| anyhow!("policy.toml çözümlenemedi"))
}

fn policy_settings(table: &toml::Table) -> Result<ipc::PolicySettings> {
    let config = toml::Value::Table(table.clone()).try_into()?;
    let retention = match table.get("trash_retention_days") {
        None => 30,
        Some(value) => value.as_integer().and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| anyhow!("geçersiz çöp saklama süresi"))?,
    };
    Ok(ipc::PolicySettings { config, trash_retention_days: retention })
}

fn get_policy_settings(paths: &Paths) -> Result<ipc::PolicySettings> {
    let _guard = POLICY_LOCK.lock().map_err(|_| anyhow!("politika kilidi zehirlendi"))?;
    policy_settings(&read_policy_table(paths)?)
}

fn set_policy_settings(paths: &Paths, settings: ipc::PolicySettings) -> Result<ipc::PolicySettings> {
    use std::io::Write;
    let _guard = POLICY_LOCK.lock().map_err(|_| anyhow!("politika kilidi zehirlendi"))?;
    anyhow::ensure!((1..=3650).contains(&settings.trash_retention_days), "saklama süresi 1–3650 gün olmalı");
    for list in [&settings.config.finance_paths, &settings.config.protected_paths] {
        anyhow::ensure!(list.len() <= 128, "en çok 128 yol girilebilir");
        for path in list {
            let text = path.to_string_lossy();
            reject_remote_path(&text)?;
            anyhow::ensure!(path.is_absolute() && text.len() <= 4096 && !text.chars().any(char::is_control), "geçersiz mutlak politika yolu");
        }
    }
    let mut table = read_policy_table(paths)?;
    let current = policy_settings(&table)?;
    anyhow::ensure!(settings.config.deploy_patterns == current.config.deploy_patterns && settings.config.allow_list == current.config.allow_list,
        "salt okunur kurallar değişmiş; ayarları yeniden yükle");
    let mut config = toml::Value::try_from(&settings.config)?.try_into::<toml::Table>()?;
    // Bilinmeyen alanlar ve arayüzün düzenleyemediği kurallar korunur.
    for name in ["finance_paths", "protected_paths"] {
        table.insert(name.into(), config.remove(name).context("politika alanı eksik")?);
    }
    table.insert("trash_retention_days".into(), i64::from(settings.trash_retention_days).into());
    let text = toml::to_string(&table)?;
    anyhow::ensure!(text.len() <= 64 * 1024, "politika ayarları çok büyük");
    let tmp = paths.root.join(format!("policy.toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, paths.root.join("policy.toml"))?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result?;
    policy_settings(&table)
}

const TRASH_PURGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Ayarlı saklama süresini aşan çöp kayıtlarını siler. Süre 1–3650 dışındaysa (0, bozuk ayar)
/// temizlik kapalı sayılır ve `None` döner; hiçbir şey silinmez.
fn purge_expired_trash(paths: &Paths, now: chrono::DateTime<chrono::Utc>) -> Result<Option<usize>> {
    let days = get_policy_settings(paths)?.trash_retention_days;
    if !(1..=3650).contains(&days) {
        return Ok(None);
    }
    ultron_core::trash::Trash::new(paths).purge_expired(days, now).map(Some)
}

/// Başlangıçta bir kez, sonra her 6 saatte bir süresi dolmuş çöp kayıtlarını temizler.
async fn trash_retention_loop(paths: Paths, cancel: CancellationToken) {
    loop {
        let p = paths.clone();
        match tokio::task::spawn_blocking(move || purge_expired_trash(&p, chrono::Utc::now())).await {
            Ok(Ok(Some(count))) => tracing::info!(count, "süresi dolan çöp kayıtları silindi"),
            Ok(Ok(None)) => tracing::info!("çöp saklama süresi geçersiz veya kapalı; temizlik atlandı"),
            Ok(Err(error)) => tracing::warn!(error = %error, "çöp temizliği başarısız"),
            Err(error) => tracing::warn!(error = %error, "çöp temizliği görevi çöktü"),
        }
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(TRASH_PURGE_INTERVAL) => {}
        }
    }
}

fn purge_trash_entry(paths: &Paths, id: &str) -> Result<()> {
    // `list` UUID, kök/ata, meta, tüm alt yolların reparse durumu ve boyutunu doğrular.
    let trash = ultron_core::trash::Trash::new(paths);
    anyhow::ensure!(trash.list()?.iter().any(|entry| entry.id == id), "çöp kaydı bulunamadı");
    let root = std::fs::canonicalize(paths.root.join("trash"))?;
    let slot = root.join(id);
    anyhow::ensure!(std::fs::canonicalize(&slot)?.parent() == Some(root.as_path()), "çöp kökü dışı silme reddedildi");
    // Original_path hiçbir zaman silme hedefi değildir.
    std::fs::remove_dir_all(slot)?;
    Ok(())
}

#[cfg(test)]
mod desktop_management_tests {
    use super::*;

    #[test]
    fn policy_settings_roundtrip_preserves_other_rules_and_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        std::fs::write(paths.root.join("policy.toml"),
            "deploy_patterns = ['example-deploy']\nallow_list = ['git status']\nfuture_setting = 'keep'\n").unwrap();
        let mut settings = get_policy_settings(&paths).unwrap();
        assert_eq!(settings.trash_retention_days, 30);
        settings.config.finance_paths = vec![tmp.path().join("finance")];
        settings.config.protected_paths = vec![tmp.path().join("protected")];
        settings.trash_retention_days = 45;
        assert_eq!(set_policy_settings(&paths, settings.clone()).unwrap(), settings);
        assert_eq!(get_policy_settings(&paths).unwrap(), settings);
        assert_eq!(read_policy_table(&paths).unwrap()["future_setting"].as_str(), Some("keep"));
        settings.config.allow_list.push("example forbidden edit".into());
        assert!(set_policy_settings(&paths, settings).is_err());
    }

    #[test]
    fn policy_settings_reject_invalid_paths_retention_and_corrupt_file_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let mut settings = get_policy_settings(&paths).unwrap();
        settings.trash_retention_days = 0;
        assert!(set_policy_settings(&paths, settings.clone()).is_err());
        settings.trash_retention_days = 30;
        settings.config.protected_paths.push("relative".into());
        assert!(set_policy_settings(&paths, settings.clone()).is_err());
        assert!(!paths.root.join("policy.toml").exists());
        std::fs::write(paths.root.join("policy.toml"), "[invalid").unwrap();
        settings.config.protected_paths.clear();
        assert!(get_policy_settings(&paths).is_err());
        assert!(set_policy_settings(&paths, settings).is_err());
        assert_eq!(std::fs::read_to_string(paths.root.join("policy.toml")).unwrap(), "[invalid");
    }

    #[test]
    fn expired_trash_is_purged_with_configured_retention_and_disabled_when_invalid() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(home.path().join("ultron"));
        paths.ensure().unwrap();
        let file = home.path().join("old.txt");
        std::fs::write(&file, "x").unwrap();
        let trash = ultron_core::trash::Trash::new(&paths);
        trash.put(&file, None).unwrap();
        let future = chrono::Utc::now() + chrono::Duration::days(40);
        // 60 gün saklama: 40 gün sonra kayıt henüz dolmamış (sahte saat).
        let mut settings = get_policy_settings(&paths).unwrap();
        settings.trash_retention_days = 60;
        set_policy_settings(&paths, settings).unwrap();
        assert_eq!(purge_expired_trash(&paths, future).unwrap(), Some(0));
        assert_eq!(trash.list().unwrap().len(), 1);
        std::fs::write(paths.root.join("policy.toml"), "trash_retention_days = 0
").unwrap();
        assert_eq!(purge_expired_trash(&paths, future).unwrap(), None);
        assert_eq!(trash.list().unwrap().len(), 1);
        std::fs::write(paths.root.join("policy.toml"), "trash_retention_days = 30
").unwrap();
        assert_eq!(purge_expired_trash(&paths, future).unwrap(), Some(1));
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn trash_purge_validates_id_and_deletes_only_selected_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let first = tmp.path().join("first.txt");
        let second = tmp.path().join("second.txt");
        std::fs::write(&first, "synthetic first").unwrap();
        std::fs::write(&second, "synthetic second").unwrap();
        let trash = ultron_core::trash::Trash::new(&paths);
        let a = trash.put(&first, None).unwrap();
        let b = trash.put(&second, None).unwrap();
        assert!(purge_trash_entry(&paths, "..").is_err());
        purge_trash_entry(&paths, &a.id).unwrap();
        assert!(!paths.root.join("trash").join(a.id).exists());
        assert_eq!(trash.list().unwrap(), vec![b.clone()]);
        let restored = trash.restore(&b.id, None).unwrap();
        assert_eq!(std::fs::read_to_string(restored).unwrap(), "synthetic second");
    }
}

fn reject_remote_path(path: &str) -> Result<()> {
    if ultron_core::import::discovery::is_remote_or_verbatim(path) {
        return Err(anyhow!("UNC ve verbatim biçimli yollar kabul edilmez"));
    }
    Ok(())
}
pub struct ServerHandle {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    pipe_name: String,
    token: String,
    approver_token: String,
    keep: KeepAwake,
}

impl ServerHandle {
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn approver_token(&self) -> &str {
        &self.approver_token
    }

    /// Şu an çalışan tur sayısı (keep-awake tutucuları).
    pub fn running_turns(&self) -> usize {
        self.keep.holders()
    }

    /// Dinlemeyi durdurur, bağlantıları ve çalışan turları sonlandırır, tek örnek kilidini bırakır.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Tek örnek kilidini alır; ikinci örnek paylaşım ihlaliyle reddedilir.
fn acquire_lock(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
        .map_err(|e| {
            anyhow!(
                "başka bir ultrond örneği çalışıyor ({}): {e}",
                path.display()
            )
        })
}

/// Açık `Running` oturumlarından, son olayı `turn_completed` olmayanları (yani tur ortasında
/// kalanları) `Interrupted` yapar ve `Error` olayı yazar. Döndürülen sayı etkilenen oturumdur.
pub fn recover_interrupted(db: &Db, log: &EventLog) -> Result<usize> {
    let mut count = 0;
    for mut session in db.open_sessions()? {
        if session.status != SessionStatus::Running {
            continue;
        }
        let last = db.last_event(&session.chat_id)?;
        if last.as_ref().is_some_and(|e| e.kind == "turn_completed") {
            continue;
        }
        log.append(
            &session.chat_id,
            Some(&session.id),
            EngineEvent::Error {
                message: "servis yeniden başladı".to_string(),
            },
        )?;
        session.status = SessionStatus::Interrupted;
        session.ended_at = Some(chrono::Utc::now());
        db.upsert_session(&session)?;
        count += 1;
    }
    Ok(count)
}

/// Çalışma durumu özetleyicisini `summarizer` yapılandırmasına göre seçer.
/// - `auto`: agy etkinse agy; agy kotaya takılırsa aynı çalıştırmanın kalanında claude-haiku
///   (Claude etkinse). agy etkin değilse özetleyici yok.
/// - `agy` / `claude-haiku`: yalnız o motor (etkin değilse özetleyici yok).
/// - `none`: özetleyici yok.
///
/// Özetleyiciler kullanıcı projelerinden ayrı `run\summarizer` dizininde (salt okunur kipte) çalışır.
pub fn select_summarizer(paths: &Paths, cfg: &Config) -> Result<Box<dyn Summarizer>> {
    let enabled = |id: EngineId| cfg.engines.iter().find(|e| e.id == id && e.enabled);
    let (agy, claude) = (enabled(EngineId::Agy), enabled(EngineId::Claude));
    let dir = || -> Result<std::path::PathBuf> {
        let dir = paths.run.join("summarizer");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("özetleyici dizini oluşturulamadı: {}", dir.display()))?;
        Ok(dir)
    };
    let boxed: Box<dyn Summarizer> = match (cfg.summarizer, agy, claude) {
        (SummarizerMode::Off, _, _) => Box::new(NullSummarizer),
        (SummarizerMode::Agy | SummarizerMode::Auto, Some(agy), claude) => {
            let primary = AgySummarizer::agy(agy.clone(), dir()?);
            match claude {
                Some(claude) if cfg.summarizer == SummarizerMode::Auto => Box::new(AutoSummarizer::new(
                    Box::new(primary),
                    Box::new(ClaudeSummarizer::haiku(claude, dir()?)),
                )),
                _ => Box::new(primary),
            }
        }
        (SummarizerMode::ClaudeHaiku, _, Some(claude)) => {
            Box::new(ClaudeSummarizer::haiku(claude, dir()?))
        }
        _ => Box::new(NullSummarizer),
    };
    Ok(boxed)
}

/// Kuru çalıştırma için özetleyici türü adı (motor çağırmadan, dizin oluşturmadan).
pub fn summarizer_kind(cfg: &Config) -> &'static str {
    let enabled = |id: EngineId| cfg.engines.iter().any(|e| e.id == id && e.enabled);
    match cfg.summarizer {
        SummarizerMode::Off => "null",
        SummarizerMode::Agy | SummarizerMode::Auto if enabled(EngineId::Agy) => "agy",
        SummarizerMode::ClaudeHaiku if enabled(EngineId::Claude) => "claude-haiku",
        _ => "null",
    }
}

/// Sunucuyu başlatır: kilit, token, kurtarma, pipe. Çağıranın tokio çalışma zamanında olması gerekir.
pub async fn start(config: ServerConfig) -> Result<ServerHandle> {
    let ServerConfig {
        paths,
        pipe_name,
        cfg,
        engines,
    } = config;
    paths.ensure()?;
    let lock = acquire_lock(&paths.run.join("ultrond.lock"))?;
    // Kilit alındı: çalışan tur yoktur; önceki çalışmadan kalan hook ayar dosyalarını sil.
    ultron_core::engine::claude::cleanup_policy_settings(&paths);

    let db = Arc::new(Db::open(&paths.db)?);
    let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
    let approvals = approvals::Approvals::new(db.clone(), log.clone(), paths.clone());
    approvals.recover_pending()?;
    let recovered = recover_interrupted(&db, &log)?;
    if recovered > 0 {
        tracing::warn!(recovered, "yarım kalan oturumlar Interrupted yapıldı");
    }

    // İki token da pipe'tan önce yazılır; eksik rol dosyasıyla servis hazır sayılmaz.
    let token = sys::random_token()?;
    sys::write_private_file(&ipc::token_path(&paths), token.as_bytes())?;
    let approver_token = sys::random_token()?;
    sys::write_private_file(&ipc::approver_token_path(&paths), approver_token.as_bytes())?;

    let sid = sys::current_user_sid()?;
    let sd = sys::SecurityDescriptor::owner_only(&sid, "GA")?;
    let mut server = sys::create_server(&pipe_name, true, &sd)
        .with_context(|| format!("pipe oluşturulamadı: {pipe_name}"))?;

    let keep = KeepAwake::new();
    let summarizer = select_summarizer(&paths, &cfg)?;
    let state = Arc::new(State {
        manager: SessionManager::new(db.clone(), log.clone(), cfg.clone(), engines)
            .with_summarizer(summarizer),
        db,
        log,
        token: token.clone(),
        approver_token: approver_token.clone(),
        keep: keep.clone(),
        default_engine: cfg.default_engine,
        turns: Mutex::new(JoinSet::new()),
        paths: paths.clone(),
        quota: ultron_core::quota::QuotaService::from_env(),
        approvals,
        import_lock: tokio::sync::Mutex::new(()),
    });

    let cancel = CancellationToken::new();
    tokio::spawn(trash_retention_loop(paths.clone(), cancel.clone()));
    let loop_cancel = cancel.clone();
    let loop_name = pipe_name.clone();
    let task = tokio::spawn(async move {
        let _lock = lock; // süreç boyunca tutulur
        let mut conns: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                () = loop_cancel.cancelled() => break,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                connected = server.connect() => {
                    if let Err(e) = connected {
                        tracing::warn!(error = %e, "pipe bağlantısı başarısız");
                    }
                    let next = match sys::create_server(&loop_name, false, &sd) {
                        Ok(next) => next,
                        Err(e) => {
                            tracing::error!(error = %e, "yeni pipe örneği oluşturulamadı");
                            break;
                        }
                    };
                    let conn = std::mem::replace(&mut server, next);
                    conns.spawn(handle_conn(conn, state.clone(), loop_cancel.clone()));
                }
            }
        }
        conns.shutdown().await;
        if let Ok(mut turns) = state.turns.lock() {
            turns.abort_all();
        }
    });

    Ok(ServerHandle {
        cancel,
        task,
        pipe_name,
        token,
        approver_token,
        keep,
    })
}

enum Flow {
    Continue,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionRole {
    Requester,
    Approver,
}

struct ConnectionAuth {
    role: ConnectionRole,
    requester_id: String,
}

async fn send_error(writer: &mut Writer, message: impl Into<String>) {
    let _ = write_msg(
        writer,
        &Response::Error {
            message: message.into(),
        },
    )
    .await;
}

async fn handle_conn(stream: NamedPipeServer, st: Arc<State>, cancel: CancellationToken) {
    let (r, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    // Kimlik doğrulama: ilk satır Hello olmalı.
    let first = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader, MAX_LINE_BYTES)).await;
    let line = match first {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return,
        Ok(Err(e)) => {
            send_error(&mut writer, e.to_string()).await;
            return;
        }
        Err(_) => {
            send_error(&mut writer, "kimlik doğrulama zaman aşımı").await;
            return;
        }
    };
    let role = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Hello { token }) => {
            // İki sır da sabit zamanlı karşılaştırılır; rolü istemci etiketi belirlemez.
            let requester = ct_eq(token.as_bytes(), st.token.as_bytes());
            let approver = ct_eq(token.as_bytes(), st.approver_token.as_bytes());
            if approver {
                ConnectionRole::Approver
            } else if requester {
                ConnectionRole::Requester
            } else {
                send_error(&mut writer, "kimlik doğrulaması başarısız").await;
                return;
            }
        }
        _ => {
            send_error(&mut writer, "kimlik doğrulaması başarısız").await;
            return;
        }
    };
    let auth = ConnectionAuth {
        role,
        requester_id: uuid::Uuid::new_v4().to_string(),
    };
    {
        if write_msg(
                &mut writer,
                &Response::Ok {
                    data: json!({"hello": true, "version": ultron_core::VERSION,
                        "role": if role == ConnectionRole::Approver { "approver" } else { "requester" }}),
                },
            )
            .await
            .is_err()
            {
                return;
            }
    }

    loop {
        let line = tokio::select! {
            () = cancel.cancelled() => return,
            frame = read_frame(&mut reader, MAX_LINE_BYTES) => match frame {
                Ok(Some(line)) => line,
                Ok(None) => return,
                Err(ipc::FrameError::Io(_)) => return,
                Err(e) => {
                    send_error(&mut writer, e.to_string()).await;
                    return;
                }
            },
        };
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                send_error(&mut writer, format!("geçersiz istek: {e}")).await;
                continue;
            }
        };
        match dispatch(&st, &auth, request, &mut reader, &mut writer, &cancel).await {
            Ok(Flow::Continue) => {}
            Ok(Flow::Close) => return,
            Err(e) => {
                if write_msg(
                    &mut writer,
                    &Response::Error {
                        message: format!("{e:#}"),
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn ok(writer: &mut Writer, data: Value) -> Result<()> {
    write_msg(writer, &Response::Ok { data }).await
}

/// `config.toml` içindeki `[reset_rights]` tablosunu günceller (diğer anahtarlar korunur,
/// yorumlar korunmaz); geçici dosya + yeniden adlandırma ile yazılır.
fn set_reset_rights(paths: &Paths, pool: &str, count: u32) -> Result<()> {
    if !matches!(pool, "claude" | "codex") {
        return Err(anyhow!("geçersiz havuz: {pool} (claude|codex)"));
    }
    // Daemon düzeyinde seri hale getirme; okuma yazmadan hemen önce yapılır ki dışarıdan
    // yapılan düzenlemeler (ör. `claude_live_quota`) ezilmesin.
    static CONFIG_LOCK: Mutex<()> = Mutex::new(());
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| anyhow!("config kilidi zehirlendi"))?;
    let file = paths.root.join("config.toml");
    let mut table: toml::Table = match std::fs::read_to_string(&file) {
        Ok(text) => text.parse().map_err(|_| anyhow!("config.toml çözümlenemedi"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e.into()),
    };
    let rights = table
        .entry("reset_rights")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml: reset_rights tablo olmalı"))?;
    rights.insert(pool.to_string(), toml::Value::Integer(i64::from(count)));
    let tmp = paths
        .root
        .join(format!("config.toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, toml::to_string(&table)?)?;
    if let Err(e) = std::fs::rename(&tmp, &file) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod config_error_tests {
    use super::*;

    #[test]
    fn malformed_config_errors_do_not_expose_source_or_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let source = "[telegram]\nallowed_user_ids = [101,,202]\n";
        for (name, result) in [
            ("config.toml", true),
            ("policy.toml", false),
        ] {
            let file = paths.root.join(name);
            std::fs::write(&file, source).unwrap();
            let error = if result {
                set_reset_rights(&paths, "claude", 1).unwrap_err()
            } else {
                get_policy_settings(&paths).unwrap_err()
            };
            let message = format!("{error:#}");
            assert_eq!(message, format!("{name} çözümlenemedi"));
            for private in ["101", "202", "allowed_user_ids", "[telegram]"] {
                assert!(!message.contains(private), "{message}");
            }
            assert_eq!(error.chain().count(), 1);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
        }
    }
}

/// Yol, kullanıcının ev dizini mi (büyük/küçük harf ve sembolik bağdan bağımsız)?
fn is_user_home(path: &Path) -> bool {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else {
        return false;
    };
    let norm = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(path) == norm(Path::new(&home))
}

const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

/// Full tur için yalnız onaylayıcı bağlantısının verdiği, hâlâ geçerli karar kabul edilir.
async fn wait_elevation(st: &State, id: &str, cancel: &CancellationToken) -> Result<()> {
    use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
    let deadline = tokio::time::sleep(Duration::from_secs(u64::from(approvals::MAX_APPROVAL_WAIT_SECS)));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = st.approvals.resolve(id, false, ResolvedBy::Timeout);
                return Err(anyhow!("yükseltme bekleyen tur iptal edildi"));
            }
            () = &mut deadline => {
                let _ = st.approvals.resolve(id, false, ResolvedBy::Timeout);
                return Err(anyhow!("yükseltme onay süresi doldu"));
            }
            _ = tick.tick() => {
                let row = st.db.get_approval(id)?.ok_or_else(|| anyhow!("yükseltme onayı bulunamadı"))?;
                match row.status {
                    ApprovalStatus::Pending => {}
                    ApprovalStatus::Approved if row.resolved_by == Some(ResolvedBy::Approver) => {
                        // Karar DB'ye yazılıp döküm/yayın başarısız olmuşsa tur başlamaz.
                        if st.db.approval_outbox()?.is_empty() {
                            return Ok(());
                        }
                    }
                    _ => return Err(anyhow!("Full tur için onaylayıcı onayı verilmedi")),
                }
            }
        }
    }
}

/// Denetim raporu normal olay günlüğünden geçer: SQLite, döküm ve tüm aboneler.
fn persist_audit(log: &EventLog, report: ultron_core::session::audit::AuditReport) -> Result<()> {
    for violation in report.violations {
        log.append(
            &report.chat_id,
            Some(&report.session_id),
            EngineEvent::PolicyViolation {
                command: violation.command,
                category: violation.category,
            },
        )?;
    }
    Ok(())
}

/// Abonelik turdan önce kurulur; son rapor kalıcılaşmadan Send tamamlanmaz.
async fn send_audited(
    st: &State,
    reservation: ultron_core::session::TurnReservation,
    chat_id: &str,
    cwd: &Path,
    engine: EngineId,
    text: &str,
    policy: ToolPolicy,
) -> Result<TurnOutcome> {
    let mut audits = st.manager.subscribe_audits();
    let turn = st.manager.send_reserved(reservation, chat_id, cwd, engine, text, policy);
    tokio::pin!(turn);
    let mut audit_error = None;
    let result = loop {
        tokio::select! {
            biased;
            report = audits.recv(), if audit_error.is_none() => {
                match report {
                    Ok(report) if report.chat_id == chat_id => {
                        if let Err(error) = persist_audit(&st.log, report) {
                            audit_error = Some(error);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => audit_error = Some(anyhow!("tur denetimi akışı kayıpsız alınamadı")),
                }
            }
            result = &mut turn => break result,
        }
    };
    // Tur future'ı aynı poll içinde rapor üretip tamamlanmış olabilir.
    while audit_error.is_none() {
        match audits.try_recv() {
            Ok(report) if report.chat_id == chat_id => {
                if let Err(error) = persist_audit(&st.log, report) {
                    audit_error = Some(error);
                }
            }
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(_) => audit_error = Some(anyhow!("tur denetimi akışı kayıpsız alınamadı")),
        }
    }
    match audit_error {
        Some(error) => Err(error.context("politika ihlalleri kalıcılaştırılamadı")),
        None => result,
    }
}

/// IPC'den gelen katalog yolunu sınırlı okur: düzenli dosya, `.md`/`.txt`, en çok 1 MiB.
async fn read_catalog_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let ext_ok = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("txt"));
    if !ext_ok {
        return Err(anyhow!("proje kataloğu .md veya .txt olmalı"));
    }
    let file = tokio::fs::File::open(path)
        .await
        .context("proje kataloğu okunamadı")?;
    let meta = file.metadata().await.context("proje kataloğu okunamadı")?;
    if !meta.is_file() {
        return Err(anyhow!("proje kataloğu düzenli bir dosya olmalı"));
    }
    if meta.len() > MAX_CATALOG_BYTES {
        return Err(anyhow!("proje kataloğu çok büyük (en çok 1 MiB)"));
    }
    // Dosya okuma sırasında büyüyebilir: okuma da sınırlanır.
    let mut buf = Vec::new();
    file.take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut buf)
        .await
        .context("proje kataloğu okunamadı")?;
    if buf.len() as u64 > MAX_CATALOG_BYTES {
        return Err(anyhow!("proje kataloğu çok büyük (en çok 1 MiB)"));
    }
    String::from_utf8(buf).map_err(|_| anyhow!("proje kataloğu UTF-8 olmalı"))
}

async fn dispatch(
    st: &Arc<State>,
    auth: &ConnectionAuth,
    request: Request,
    reader: &mut Reader,
    writer: &mut Writer,
    cancel: &CancellationToken,
) -> Result<Flow> {
    match request {
        Request::TelegramStatus
        | Request::TelegramSetToken { .. }
        | Request::TelegramTest
        | Request::GetTelegramSettings
        | Request::SetTelegramSettings { .. }
        | Request::DesktopHeartbeat { .. } => {
            send_error(writer, "Telegram ağ geçidi henüz bağlanmadı").await;
            Ok(Flow::Continue)
        }
        Request::TrashList => {
            require_approver(auth)?;
            let entries = {
                let _guard = TRASH_LOCK.lock().map_err(|_| anyhow!("çöp kilidi zehirlendi"))?;
                ultron_core::trash::Trash::new(&st.paths).list()?
            };
            ok(writer, json!({"entries": entries})).await?;
            Ok(Flow::Continue)
        }
        Request::TrashPut { paths, token } => {
            // Sohbet ve oturum dizini istemciden değil, tur belirtecinden çözülür.
            let grant = ultron_core::turn_grant::resolve(&token)
                .ok_or_else(|| anyhow!("geçersiz veya süresi dolmuş tur yetkisi"))?;
            let targets: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
            let entries =
                crate::trash_ops::put_inside(&st.paths, &grant.chat_id, &grant.cwd, &targets)?;
            ok(writer, json!({"entries": entries})).await?;
            Ok(Flow::Continue)
        }
        Request::TrashRestore { id } => {
            require_approver(auth)?;
            let path = {
                let _guard = TRASH_LOCK.lock().map_err(|_| anyhow!("çöp kilidi zehirlendi"))?;
                ultron_core::trash::Trash::new(&st.paths).restore(&id, None)?
            };
            ok(writer, json!({"path": path})).await?;
            Ok(Flow::Continue)
        }
        Request::TrashPurge { id } => {
            require_approver(auth)?;
            {
                let _guard = TRASH_LOCK.lock().map_err(|_| anyhow!("çöp kilidi zehirlendi"))?;
                purge_trash_entry(&st.paths, &id)?;
            }
            ok(writer, json!({"id": id})).await?;
            Ok(Flow::Continue)
        }
        Request::GetPolicyConfig => {
            require_approver(auth)?;
            ok(writer, serde_json::to_value(get_policy_settings(&st.paths)?)?).await?;
            Ok(Flow::Continue)
        }
        Request::SetPolicyConfig { settings } => {
            require_approver(auth)?;
            ok(writer, serde_json::to_value(set_policy_settings(&st.paths, settings)?)?).await?;
            Ok(Flow::Continue)
        }
        Request::PolicyCheck {
            action,
            chat_id,
            engine,
            wait_secs,
        } => {
            let decision = st
                .approvals
                .check_owned(action, chat_id, engine, wait_secs, Some(&auth.requester_id))
                .await?;
            ok(writer, json!({"decision": decision})).await?;
            Ok(Flow::Continue)
        }
        Request::ListApprovals {
            status,
            limit,
            cursor,
        } => {
            let owner =
                (auth.role == ConnectionRole::Requester).then_some(auth.requester_id.as_str());
            ok(
                writer,
                serde_json::to_value(st.db.list_approvals_page(
                    status,
                    owner,
                    limit,
                    cursor.as_deref(),
                )?)?,
            )
            .await?;
            Ok(Flow::Continue)
        }
        Request::ResolveApproval { id, approve, by } => {
            use ultron_core::db::approvals::ResolvedBy;
            require_approver(auth)?;
            if !matches!(by.as_str(), "desktop" | "telegram") {
                return Err(anyhow!("geçersiz onay etiketi (desktop|telegram)"));
            }
            // Eski `by` yalnız uyumlu bir etikettir; kalıcı kimlik onaylayıcı bağlantısıdır.
            let approval = st.approvals.resolve(&id, approve, ResolvedBy::Approver)?;
            ok(writer, json!({"approval": approval})).await?;
            Ok(Flow::Continue)
        }
        Request::Hello { .. } => {
            send_error(writer, "Hello yalnız bağlantının ilk mesajı olabilir").await;
            Ok(Flow::Close)
        }
        Request::Status => {
            ok(
                writer,
                json!({
                    "version": ultron_core::VERSION,
                    "pid": std::process::id(),
                    "running_turns": st.keep.holders(),
                    "default_engine": st.default_engine,
                }),
            )
            .await?;
            Ok(Flow::Continue)
        }
        Request::ListProjects => {
            ok(writer, json!({"projects": st.db.list_projects()?})).await?;
            Ok(Flow::Continue)
        }
        Request::ListChats {
            project_id,
            include_archived,
        } => {
            let chats = st.db.list_chats(project_id.as_deref(), include_archived)?;
            ok(writer, json!({"chats": chats})).await?;
            Ok(Flow::Continue)
        }
        Request::GetChat { chat_id } => {
            let chat = st
                .db
                .get_chat(&chat_id)?
                .ok_or_else(|| anyhow!("sohbet bulunamadı: {chat_id}"))?;
            let detail = ChatDetail {
                work_state: st.db.get_work_state(&chat_id)?,
                last_engine: st.db.latest_session(&chat_id)?.map(|s| s.engine),
                chat,
            };
            ok(writer, json!({"chat": detail})).await?;
            Ok(Flow::Continue)
        }
        Request::UpdateChat {
            chat_id,
            title,
            pinned,
            archived,
            project_id,
        } => {
            let title = match title.as_deref().map(str::trim) {
                Some("") => return Err(anyhow!("başlık boş olamaz")),
                Some(t) => Some(t),
                None => None,
            };
            if let Some(Some(pid)) = &project_id
                && st.db.get_project(pid)?.is_none()
            {
                return Err(anyhow!("proje bulunamadı: {pid}"));
            }
            let project_arg = project_id.as_ref().map(Option::as_deref);
            if !st
                .db
                .update_chat(&chat_id, title, pinned, archived, project_arg)?
            {
                return Err(anyhow!("sohbet bulunamadı: {chat_id}"));
            }
            let chat = st.db.get_chat(&chat_id)?;
            ok(writer, json!({"chat": chat})).await?;
            Ok(Flow::Continue)
        }
        Request::GetQuota => {
            // Her istekte taze okunur: [reset_rights] ve claude_live_quota elle/arayüzden değişebilir.
            let cfg = ultron_core::Config::load(&st.paths)?;
            let report = st.quota.report(&cfg, chrono::Utc::now()).await;
            ok(writer, json!({"quota": report})).await?;
            Ok(Flow::Continue)
        }
        Request::SetResetRights { pool, count } => {
            set_reset_rights(&st.paths, &pool, count)?;
            ok(writer, json!({"pool": pool, "count": count})).await?;
            Ok(Flow::Continue)
        }
        Request::ImportCatalog { path } => {
            let md = read_catalog_file(Path::new(&path)).await?;
            let report = ultron_core::projects::import_catalog(&st.db, &md)?;
            ok(writer, serde_json::to_value(report)?).await?;
            Ok(Flow::Continue)
        }
        Request::ImportClaudeHistory { root } => {
            require_approver(auth)?;
            let _guard = st
                .import_lock
                .try_lock()
                .map_err(|_| anyhow!("başka bir içe aktarma/silme işlemi çalışıyor"))?;
            let root = match root {
                Some(r) => {
                    reject_remote_path(&r)?;
                    std::path::PathBuf::from(r)
                }
                None => std::env::var_os("USERPROFILE")
                    .map(|h| std::path::PathBuf::from(h).join(".claude").join("projects"))
                    .ok_or_else(|| anyhow!("USERPROFILE tanımlı değil"))?,
            };
            let db = st.db.clone();
            let report = tokio::task::spawn_blocking(move || {
                let mut state = ultron_core::import::ImportState::load(&db)?;
                ultron_core::import::import_claude_history(&db, &root, &mut state)
            })
            .await??;
            ok(writer, json!({"report": report})).await?;
            Ok(Flow::Continue)
        }
        Request::ImportProjects { dry_run, home } => {
            require_approver(auth)?;
            let _guard = st
                .import_lock
                .try_lock()
                .map_err(|_| anyhow!("başka bir içe aktarma/silme işlemi çalışıyor"))?;
            // `home` yalnız test ortamında dikkate alınır; üretimde daima USERPROFILE.
            let override_ok = std::env::var_os(HOME_OVERRIDE_ENV).is_some_and(|v| v == "1");
            let home = match home.filter(|_| override_ok) {
                Some(h) => {
                    reject_remote_path(&h)?;
                    std::path::PathBuf::from(h)
                }
                None => std::env::var_os("USERPROFILE")
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| anyhow!("USERPROFILE tanımlı değil"))?,
            };
            reject_remote_path(&home.to_string_lossy())?;
            let cfg = ultron_core::Config::load(&st.paths)?;
            let filters = ultron_core::import::discovery::Filters::system(&home);
            // Dry-run motor çağırmaz ve dizin oluşturmaz.
            let summarizer: Box<dyn Summarizer> = if dry_run {
                Box::new(NullSummarizer)
            } else {
                select_summarizer(&st.paths, &cfg)?
            };
            let mut report = ultron_core::import::import_projects(
                &st.db,
                summarizer.as_ref(),
                &home,
                &filters,
                dry_run,
            )
            .await?;
            // Gerçek çalıştırmada tür, yedek geçiş olduysa içe aktarma sonundaki özetleyiciden gelir.
            report.summarizer = if dry_run {
                summarizer_kind(&cfg).to_string()
            } else {
                report.summarizer.clone()
            };
            ok(writer, json!({"report": report})).await?;
            Ok(Flow::Continue)
        }
        Request::RemoveImportedChats { dry_run } => {
            require_approver(auth)?;
            let _guard = st
                .import_lock
                .try_lock()
                .map_err(|_| anyhow!("başka bir içe aktarma/silme işlemi çalışıyor"))?;
            let db = st.db.clone();
            let db_path = st.paths.db.clone();
            let report = tokio::task::spawn_blocking(move || {
                ultron_core::import::remove_imported_chats(&db, &db_path, dry_run)
            })
            .await??;
            ok(writer, json!({"report": report})).await?;
            Ok(Flow::Continue)
        }
        Request::Route { text } => {
            let active = st.db.last_active_project()?;
            let result = ultron_core::routing::route(&st.db, &text, active.as_deref())?;
            ok(writer, serde_json::to_value(result)?).await?;
            Ok(Flow::Continue)
        }
        Request::Switch { chat_id, to } => {
            st.manager.switch(&chat_id, to, SwitchReason::User).await?;
            ok(writer, json!({"chat_id": chat_id})).await?;
            Ok(Flow::Continue)
        }
        Request::Cancel { chat_id } => {
            st.manager.cancel(&chat_id).await?;
            ok(writer, json!({"chat_id": chat_id})).await?;
            Ok(Flow::Continue)
        }
        Request::HistorySearch {
            chat_id,
            query,
            limit,
        } => {
            let hits = st.db.search_messages(
                chat_id.as_deref(),
                &query,
                limit.clamp(1, MAX_SEARCH_LIMIT),
            )?;
            ok(writer, json!({"hits": hits})).await?;
            Ok(Flow::Continue)
        }
        Request::Send {
            chat_id,
            cwd,
            engine,
            text,
            policy,
            elevated,
            project_id,
        } => {
            let chat_id = chat_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            if chat_id.is_empty()
                || !chat_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(anyhow!("geçersiz sohbet kimliği: {chat_id}"));
            }
            // Projeye bağlı gönderimde çalışma dizini projenin yoludur.
            let project = match &project_id {
                Some(pid) => Some(
                    st.db
                        .get_project(pid)?
                        .ok_or_else(|| anyhow!("proje bulunamadı: {pid}"))?,
                ),
                None => None,
            };
            let cwd = match &project {
                Some(p) => p.path.clone(),
                // Projesiz sohbet kullanıcının ev dizininde çalışmaz: sohbete özel çalışma alanı.
                None if cwd.trim().is_empty() || is_user_home(Path::new(&cwd)) => {
                    let dir = st.paths.root.join("workspaces").join(&chat_id);
                    std::fs::create_dir_all(&dir).with_context(|| {
                        format!("çalışma alanı oluşturulamadı: {}", dir.display())
                    })?;
                    dir.to_string_lossy().into_owned()
                }
                None => cwd,
            };
            if !Path::new(&cwd).is_absolute() {
                return Err(anyhow!("cwd mutlak bir yol olmalı: {cwd}"));
            }
            // Kabulden önce: motor kayıtlı mı ve sohbette tur çalışıyor mu. Hata burada dönerse
            // istemci kabul yanıtı görmez; tur kilidi kabul öncesi ayrılır.
            let pre_engine = match engine {
                Some(engine) => engine,
                None => st.db.chat_engine(&chat_id)?.unwrap_or(st.default_engine),
            };
            let mut reservation = st.manager.reserve_turn(&chat_id, pre_engine)?;
            let policy = if elevated { ToolPolicy::Full } else { policy.into() };
            st.db.insert_chat(&chat_id, None, "", chrono::Utc::now())?;
            if let Some(project) = &project {
                let bound = st.db.get_chat(&chat_id)?.and_then(|c| c.project_id);
                if bound.as_ref().is_some_and(|b| *b != project.id) {
                    return Err(anyhow!("sohbet başka bir projeye bağlı: {chat_id}"));
                }
                st.db.bind_chat_project(&chat_id, &project.id)?;
            }
            st.db.touch_chat(&chat_id, &text)?;
            // Motor verilmezse mevcut sohbetin motoru kullanılır (sessiz motor değişimi olmaz).
            let engine = pre_engine;
            let mut last_seq = st.db.next_seq(&chat_id)? - 1;
            let mut wake = st.log.subscribe();
            let mut approval_wake = st.approvals.subscribe_global();
            let elevation = if policy == ToolPolicy::Full {
                let approval = st.manager.request_elevation(&mut reservation, Path::new(&cwd), &text)?;
                st.approvals.publish_pending()?;
                Some(approval)
            } else {
                None
            };
            let guard = st.keep.acquire();
            let (done_tx, mut done_rx) = oneshot::channel();
            {
                let task_state = st.clone();
                let task_chat = chat_id.clone();
                let mut turns = st
                    .turns
                    .lock()
                    .map_err(|_| anyhow!("tur kaydı kilidi zehirlendi"))?;
                while turns.try_join_next().is_some() {}
                turns.spawn(async move {
                    let result = async {
                        if let Some(approval) = elevation {
                            wait_elevation(&task_state, &approval.id, &reservation.cancellation_token()).await?;
                        }
                        // Tur süresince geçerli çöp kutusu yetkisi (tur bitince düşer).
                        let _grant = ultron_core::turn_grant::issue(&task_chat, Path::new(&cwd));
                        send_audited(
                            &task_state,
                            reservation,
                            &task_chat,
                            Path::new(&cwd),
                            engine,
                            &text,
                            policy,
                        )
                        .await
                    }.await;
                    // Kabulden sonraki hata yalnız bu bağlantıya gitmesin: sohbet günlüğüne yazılır,
                    // böylece her abone görür.
                    if let Err(e) = &result
                        && let Err(log_err) = task_state.log.append(
                            &task_chat,
                            None,
                            EngineEvent::Error {
                                message: format!("{e:#}"),
                            },
                        )
                    {
                        tracing::error!(error = %log_err, "tur hatası günlüğe yazılamadı");
                    }
                    drop(guard);
                    let _ = done_tx.send(result);
                });
            }
            ok(writer, json!({"chat_id": chat_id})).await?;
            loop {
                flush_events(st, &chat_id, &mut last_seq, writer).await?;
                tokio::select! {
                    () = cancel.cancelled() => return Ok(Flow::Close),
                    result = &mut done_rx => {
                        flush_events(st, &chat_id, &mut last_seq, writer).await?;
                        match result {
                            Ok(Ok(outcome)) => {
                                ok(writer, json!({"done": true, "outcome": outcome_name(&outcome)})).await?;
                            }
                            Ok(Err(e)) => {
                                send_error(writer, format!("{e:#}")).await;
                            }
                            Err(_) => {
                                send_error(writer, "tur sonlandırıldı").await;
                            }
                        }
                        return Ok(Flow::Continue);
                    }
                    _ = approval_wake.recv() => {}
                    woke = wake.recv() => {
                        if matches!(woke, Err(broadcast::error::RecvError::Closed)) {
                            return Ok(Flow::Close);
                        }
                    }
                    extra = reader.fill_buf() => {
                        // İstemci gitti ya da akış sırasında istek gönderdi: bağlantı kapanır,
                        // tur arka planda sürer.
                        if extra.is_ok_and(|b| !b.is_empty()) {
                            send_error(writer, "akış sırasında yeni istek beklenmiyor").await;
                        }
                        return Ok(Flow::Close);
                    }
                }
            }
        }
        Request::Events {
            chat_id,
            follow,
            after_seq,
            limit,
        } => match chat_id {
            Some(chat) => {
                let mut approval_wake = st.approvals.subscribe_global();
                let mut last_seq = after_seq.unwrap_or(0);
                if let (false, Some(limit)) = (follow, limit) {
                    let page = st
                        .db
                        .events_for_chat_page(&chat, Some(last_seq), limit as usize)?;
                    for ev in page {
                        write_msg(writer, &Response::Event(ev)).await?;
                    }
                    ok(writer, json!({"done": true})).await?;
                    return Ok(Flow::Continue);
                }
                let mut wake = if follow {
                    subscribe_and_ack(st, writer, Some(&chat)).await?
                } else {
                    st.log.subscribe()
                };
                loop {
                    flush_events(st, &chat, &mut last_seq, writer).await?;
                    if !follow {
                        ok(writer, json!({"done": true})).await?;
                        return Ok(Flow::Continue);
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(Flow::Close),
                        _ = approval_wake.recv() => {}
                        woke = wake.recv() => {
                            if matches!(woke, Err(broadcast::error::RecvError::Closed)) {
                                return Ok(Flow::Close);
                            }
                        }
                        extra = reader.fill_buf() => {
                            if extra.is_ok_and(|b| !b.is_empty()) {
                                send_error(writer, "akış sırasında yeni istek beklenmiyor").await;
                            }
                            return Ok(Flow::Close);
                        }
                    }
                }
            }
            None if follow => {
                let mut approval_wake = st.approvals.subscribe_global();
                let mut wake = subscribe_and_ack(st, writer, None).await?;
                // Sohbet başına son iletilen sıra. Yayın kanalı taşarsa (Lagged) kaçan olaylar
                // bu imleçlerden itibaren veritabanından tamamlanır.
                let mut cursors: HashMap<String, i64> = HashMap::new();
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(Flow::Close),
                        event = approval_wake.recv() => match event {
                            Ok(ev) => write_msg(writer, &Response::Event(ev)).await?,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // İstemci ListApprovals ile kalıcı durumdan yeniden eşitlenir.
                                return Err(anyhow!("onay akışı geride kaldı; onay listesini yeniden okuyun"));
                            }
                            Err(broadcast::error::RecvError::Closed) => return Ok(Flow::Close),
                        },
                        woke = wake.recv() => match woke {
                            Ok(ev) => {
                                let seen = cursors.get(&ev.chat_id).copied().unwrap_or(0);
                                if ev.seq > seen {
                                    cursors.insert(ev.chat_id.clone(), ev.seq);
                                    write_msg(writer, &Response::Event(ev)).await?;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                for (chat, cursor) in cursors.iter_mut() {
                                    flush_events(st, chat, cursor, writer).await?;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => return Ok(Flow::Close),
                        },
                        extra = reader.fill_buf() => {
                            if extra.is_ok_and(|b| !b.is_empty()) {
                                send_error(writer, "akış sırasında yeni istek beklenmiyor").await;
                            }
                            return Ok(Flow::Close);
                        }
                    }
                }
            }
            None => Err(anyhow!("follow olmadan chat_id gerekli")),
        },
    }
}

/// Entegrasyon testlerinde aboneliği ve onayı ayrı ayrı bekleten bariyer.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub struct SubscribeGate {
    pub root: std::path::PathBuf,
    /// Sunucu kapıya ulaşınca bildirilir.
    pub reached: tokio::sync::Notify,
    /// Abonelik, bir izin verilene dek başlamaz.
    pub release: tokio::sync::Semaphore,
    pub subscribed: tokio::sync::Notify,
    /// Abonelik kurulduktan sonra onayı ayrıca bekletir.
    pub acknowledge: tokio::sync::Semaphore,
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub static SUBSCRIBE_GATE: Mutex<Option<Arc<SubscribeGate>>> = Mutex::new(None);

/// `follow` akışları için tek abonelik noktası: (varsa test kapısını bekler,) abone olur ve
/// aboneliğin kurulduğunu bildiren ilk kareyi (`{"subscribed":true,...}`) yazar.
async fn subscribe_and_ack(
    st: &State,
    writer: &mut Writer,
    chat_id: Option<&str>,
) -> Result<broadcast::Receiver<ultron_core::db::StoredEvent>> {
    #[cfg(feature = "test-hooks")]
    let gate = SUBSCRIBE_GATE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|g| g.root == st.paths.root);
    #[cfg(feature = "test-hooks")]
    if let Some(gate) = &gate {
        gate.reached.notify_one();
        gate.release.acquire().await?.forget();
    }
    // Günlük yalnız eklenir: aboneliğin iki yanında aynı başlar okunursa bu başlar
    // subscribe() anında da geçerlidir. Ayrı Db sorgularının arasına yazım girebilir;
    // bu durumda alıcıyı bırakıp yeniden dene. Sürekli yazımda yanlış bir onay verme.
    let mut snapshot = None;
    for _ in 0..8 {
        let before = subscription_heads(st, chat_id)?;
        let wake = st.log.subscribe();
        let after = subscription_heads(st, chat_id)?;
        if before == after {
            snapshot = Some((wake, after));
            break;
        }
        drop(wake);
        tokio::task::yield_now().await;
    }
    let (wake, head_seq) = snapshot.context("abonelik başları sabitlenemedi; yeniden bağlanın")?;
    #[cfg(feature = "test-hooks")]
    if let Some(gate) = &gate {
        gate.subscribed.notify_one();
        gate.acknowledge.acquire().await?.forget();
    }
    ok(writer, json!({"subscribed": true, "head_seq": head_seq})).await?;
    Ok(wake)
}

/// Kalıcı max(seq); genel akışta {chat_id: seq}, tek sohbette sayı. Onay beklenirken
/// yazılan olaylar bu görüntüyü değiştirmez, yalnız canlı kuyruğa girer.
fn subscription_heads(st: &State, chat_id: Option<&str>) -> Result<Value> {
    if let Some(chat) = chat_id {
        Ok(json!(st.db.last_event(chat)?.map_or(0, |ev| ev.seq)))
    } else {
        let mut heads = serde_json::Map::new();
        for chat in st.db.list_chats(None, true)? {
            let seq = st.db.last_event(&chat.id)?.map_or(0, |ev| ev.seq);
            heads.insert(chat.id, json!(seq));
        }
        Ok(Value::Object(heads))
    }
}

/// `last_seq`'ten sonraki olayları veritabanından okuyup yazar (kaçırma yok, tekrar yok).
async fn flush_events(
    st: &State,
    chat_id: &str,
    last_seq: &mut i64,
    writer: &mut Writer,
) -> Result<()> {
    loop {
        let page = st
            .db
            .events_for_chat_page(chat_id, Some(*last_seq), REPLAY_PAGE)?;
        let full = page.len() == REPLAY_PAGE;
        for ev in page {
            *last_seq = ev.seq;
            write_msg(writer, &Response::Event(ev)).await?;
        }
        if !full {
            return Ok(());
        }
    }
}

fn outcome_name(outcome: &TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::QuotaExhausted => "quota_exhausted",
        TurnOutcome::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ultron_core::engine::FakeEngine;
    use ultron_core::ipc::{PolicyArg, ProjectInfo, RouteResult};

    const STARTED: &str = r#"{"type":"session_started","native_session_id":"nat-1"}"#;
    const REPLY: &str = r#"{"type":"assistant_text","text":"sentetik yanıt"}"#;
    const DONE: &str = r#"{"type":"turn_completed"}"#;

    #[tokio::test]
    async fn elevation_wait_accepts_only_delivered_approver_decisions() {
        use ultron_core::db::approvals::ResolvedBy;
        use ultron_core::policy::{Action, RiskCategory};
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
        db.insert_chat("elevation", None, "sentetik", chrono::Utc::now()).unwrap();
        let st = State {
            manager: SessionManager::new(db.clone(), log.clone(), Config::default(), HashMap::new()),
            approvals: approvals::Approvals::new(db.clone(), log.clone(), paths.clone()),
            db: db.clone(), log, token: String::new(), approver_token: String::new(),
            keep: KeepAwake::new(), default_engine: EngineId::Codex,
            turns: Mutex::new(JoinSet::new()), paths: paths.clone(),
            quota: ultron_core::quota::QuotaService::from_env(), import_lock: tokio::sync::Mutex::new(()),
        };
        let create = || db.create_approval(Some("elevation"), EngineId::Codex,
            RiskCategory::SystemConfig, "sentetik", &Action::ToolCall {
                tool: "ultron.turn.elevate".into(), input: json!({"policy": "full"}),
            }).unwrap();
        let row = create();
        st.approvals.resolve(&row.id, true, ResolvedBy::Desktop).unwrap();
        assert!(wait_elevation(&st, &row.id, &CancellationToken::new()).await.is_err());
        let row = create();
        st.approvals.resolve(&row.id, true, ResolvedBy::Approver).unwrap();
        wait_elevation(&st, &row.id, &CancellationToken::new()).await.unwrap();
        let row = create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(wait_elevation(&st, &row.id, &cancel).await.is_err());
        assert_eq!(db.get_approval(&row.id).unwrap().unwrap().status,
            ultron_core::db::approvals::ApprovalStatus::Denied);
        let row = create();
        let transcript = paths.transcripts.join("elevation.jsonl");
        std::fs::remove_file(&transcript).unwrap();
        std::fs::create_dir(&transcript).unwrap();
        assert!(st.approvals.resolve(&row.id, true, ResolvedBy::Approver).is_err());
        // DB'de approved olsa da teslim hatası Full çalıştırma iznine dönüşmez.
        assert!(tokio::time::timeout(Duration::from_millis(80),
            wait_elevation(&st, &row.id, &CancellationToken::new())).await.is_err());
    }

    #[tokio::test]
    async fn policy_violation_is_persisted_and_broadcast() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("audit", None, "sentetik", chrono::Utc::now()).unwrap();
        db.upsert_session(&ultron_core::db::SessionRow {
            id: "s1".into(), chat_id: "audit".into(), engine: EngineId::Codex,
            native_session_id: None, status: SessionStatus::Closed,
            started_at: chrono::Utc::now(), ended_at: None,
        }).unwrap();
        let log = EventLog::new(db.clone(), paths.clone());
        let mut live = log.subscribe();
        let secret = ["sentetik", "sifre"].join("_");
        persist_audit(&log, ultron_core::session::audit::AuditReport {
            chat_id: "audit".into(), session_id: "s1".into(),
            violations: vec![ultron_core::session::audit::PolicyViolation {
                command: format!("rm x; example --password {secret}; {}", "ğ".repeat(5000)),
                category: ultron_core::policy::RiskCategory::Delete,
            }], changes: Vec::new(), errors: Vec::new(),
        }).unwrap();
        let stored = db.events_for_chat("audit", None).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].kind, "policy_violation");
        assert_eq!(stored[0].session_id.as_deref(), Some("s1"));
        assert_eq!(live.try_recv().unwrap(), stored[0]);
        assert!(!serde_json::to_string(&stored).unwrap().contains(&secret));
        assert!(stored[0].payload["command"].as_str().unwrap().chars().count() <= 4000);
        let transcript = std::fs::read_to_string(paths.transcripts.join("audit.jsonl")).unwrap();
        assert!(transcript.contains("policy_violation"));
        assert!(!transcript.contains(&secret));
        assert!(matches!(live.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }

    fn paths_in(dir: &Path) -> Paths {
        let paths = Paths::from_root(dir.join("ultron"));
        paths.ensure().unwrap();
        paths
    }

    fn abs_dir(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("proje");
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn unique_pipe() -> String {
        format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4())
    }

    async fn start_with(paths: &Paths, pipe: &str, lines: &[&str]) -> ServerHandle {
        // Birim testlerinde yalnız sahte CLI çalıştırılır.
        static FAKE: std::sync::Once = std::sync::Once::new();
        FAKE.call_once(|| {
            let name = format!("ultron-fake-cli{}", std::env::consts::EXE_SUFFIX);
            let exe = std::env::current_exe().unwrap();
            let fake = exe
                .ancestors()
                .skip(1)
                .take(6)
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
                .expect("önce ultron-fake-cli derlenmeli");
            // SAFETY: tüm testler yalnız aynı sahte CLI yolunu bir kez yazar.
            unsafe { std::env::set_var("ULTRON_FAKE_CLI", fake) };
        });
        let fixture = paths.root.join("routing-fixture.jsonl");
        std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
        let mut engines: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
        engines.insert(
            EngineId::Claude,
            Arc::new(FakeEngine::from_fixture(&fixture)),
        );
        start(ServerConfig {
            paths: paths.clone(),
            pipe_name: pipe.into(),
            // Gerçek agy özetleyicisi testlere sızmasın (ağ çağrısı, 2-10 sn dalgalanma).
            cfg: config_with_agy(false),
            engines,
        })
        .await
        .unwrap()
    }

    async fn connect(h: &ServerHandle) -> ipc::Client {
        ipc::Client::connect(h.pipe_name(), h.token())
            .await
            .unwrap()
    }

    async fn next(client: &mut ipc::Client) -> Option<Response> {
        tokio::time::timeout(Duration::from_secs(20), client.next())
            .await
            .unwrap()
            .unwrap()
    }

    async fn response(h: &ServerHandle, request: Request) -> Response {
        let mut client = connect(h).await;
        client.send(&request).await.unwrap();
        next(&mut client).await.unwrap()
    }

    async fn data(h: &ServerHandle, request: Request) -> Value {
        match response(h, request).await {
            Response::Ok { data } => data,
            other => panic!("başarı yanıtı bekleniyordu: {other:?}"),
        }
    }

    fn seed(db: &Db, id: &str, name: &str, path: &Path, active: Option<&str>) {
        db.upsert_project(&ProjectInfo {
            id: id.into(),
            name: name.into(),
            path: path.to_string_lossy().into_owned(),
            aliases: Vec::new(),
            description: None,
            last_active_at: active.map(str::to_string),
        })
        .unwrap();
    }

    #[tokio::test]
    async fn route_uses_projects_activity_without_any_chats() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let db = Db::open(&paths.db).unwrap();
        let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
        let empty: RouteResult = serde_json::from_value(
            data(
                &h,
                Request::Route {
                    text: "devam".into(),
                },
            )
            .await,
        )
        .unwrap();
        assert_eq!(empty.project_id, None);
        seed(
            &db,
            "eski",
            "Ödeme",
            &tmp.path().join("eski"),
            Some("2026-11-16T10:00:00+03:00"),
        );
        seed(
            &db,
            "yeni",
            "Atölye",
            &tmp.path().join("yeni"),
            Some("2026-11-16T08:00:00Z"),
        );
        seed(
            &db,
            "etkin-degil",
            "İletişim",
            &tmp.path().join("diger"),
            None,
        );
        assert!(db.list_chats(None, true).unwrap().is_empty());
        let fallback: RouteResult = serde_json::from_value(
            data(
                &h,
                Request::Route {
                    text: "devam".into(),
                },
            )
            .await,
        )
        .unwrap();
        assert_eq!(fallback.project_id.as_deref(), Some("yeni"));
        assert_eq!(fallback.confidence, 0.5);
        let explicit: RouteResult = serde_json::from_value(
            data(
                &h,
                Request::Route {
                    text: "ÖDEME".into(),
                },
            )
            .await,
        )
        .unwrap();
        assert_eq!(explicit.project_id.as_deref(), Some("eski"));
        assert_eq!(explicit.confidence, 0.9);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn import_catalog_reads_file_upserts_and_reports_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
        let file = tmp.path().join("catalog.md");
        std::fs::write(
            &file,
            include_str!("../../ultron-core/tests/fixtures/catalog/projects.md"),
        )
        .unwrap();
        let request = Request::ImportCatalog {
            path: file.to_string_lossy().into_owned(),
        };
        let first = data(&h, request.clone()).await;
        assert_eq!(first, json!({"imported": 3, "updated": 0}));
        let before = data(&h, Request::ListProjects).await;
        let second = data(&h, request).await;
        assert_eq!(second, json!({"imported": 0, "updated": 3}));
        assert_eq!(data(&h, Request::ListProjects).await, before);
        let missing = Request::ImportCatalog {
            path: tmp.path().join("missing.md").to_string_lossy().into_owned(),
        };
        assert!(matches!(
            response(&h, missing).await,
            Response::Error { .. }
        ));
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            response(
                &h,
                Request::ImportCatalog {
                    path: file.to_string_lossy().into_owned()
                }
            )
            .await,
            Response::Error { .. }
        ));
        assert_eq!(data(&h, Request::ListProjects).await, before);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn projectless_send_uses_scratch_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
        let home = std::env::var("USERPROFILE").expect("USERPROFILE tanımlı olmalı");
        for (chat, cwd) in [("kapsam-1", home), ("kapsam-2", String::new())] {
            let mut client = connect(&h).await;
            client
                .send(&Request::Send {
                    chat_id: Some(chat.into()),
                    cwd,
                    engine: Some(EngineId::Claude),
                    text: "merhaba".into(),
                    policy: PolicyArg::ReadOnly,
                    elevated: false,
                    project_id: None,
                })
                .await
                .unwrap();
            loop {
                match next(&mut client).await.unwrap() {
                    Response::Ok { data } if data.get("done").is_some() => break,
                    Response::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
            assert!(
                paths.root.join("workspaces").join(chat).is_dir(),
                "{chat} için çalışma alanı oluşmalı"
            );
        }
        // Ev dizini dışındaki açık bir cwd olduğu gibi kullanılır (çalışma alanı açılmaz).
        let own = abs_dir(tmp.path());
        let mut client = connect(&h).await;
        client
            .send(&Request::Send {
                chat_id: Some("kapsam-3".into()),
                cwd: own.to_string_lossy().into_owned(),
                engine: Some(EngineId::Claude),
                text: "merhaba".into(),
                policy: PolicyArg::ReadOnly,
                elevated: false,
                project_id: None,
            })
            .await
            .unwrap();
        loop {
            match next(&mut client).await.unwrap() {
                Response::Ok { data } if data.get("done").is_some() => break,
                Response::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert!(!paths.root.join("workspaces").join("kapsam-3").exists());
        h.shutdown().await;
    }

    #[tokio::test]
    async fn import_catalog_rejects_large_or_non_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
        let path_req = |p: &Path| Request::ImportCatalog {
            path: p.to_string_lossy().into_owned(),
        };
        // Çok büyük dosya.
        let big = tmp.path().join("big.md");
        std::fs::write(&big, vec![b'a'; MAX_CATALOG_BYTES as usize + 1]).unwrap();
        let Response::Error { message } = response(&h, path_req(&big)).await else {
            panic!("büyük dosya reddedilmeliydi");
        };
        assert!(message.contains("büyük"), "{message}");
        // Düzenli dosya değil (dizin, .md uzantılı).
        let dir = tmp.path().join("dizin.md");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            response(&h, path_req(&dir)).await,
            Response::Error { .. }
        ));
        // Yanlış uzantı.
        let wrong = tmp.path().join("katalog.json");
        std::fs::write(&wrong, "x").unwrap();
        let Response::Error { message } = response(&h, path_req(&wrong)).await else {
            panic!("uzantı reddedilmeliydi");
        };
        assert!(message.contains(".md"), "{message}");
        h.shutdown().await;
    }

    #[tokio::test]
    async fn send_binding_updates_project_activity_used_by_route() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let cwd = abs_dir(tmp.path());
        let db = Db::open(&paths.db).unwrap();
        seed(&db, "p1", "Ödeme", &cwd, None);
        let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
        let mut client = connect(&h).await;
        client
            .send(&Request::Send {
                chat_id: Some("bound".into()),
                cwd: cwd.to_string_lossy().into_owned(),
                engine: Some(EngineId::Claude),
                text: "merhaba".into(),
                policy: PolicyArg::ReadOnly,
                elevated: false,
                project_id: Some("p1".into()),
            })
            .await
            .unwrap();
        loop {
            match next(&mut client).await.expect("akış erken kapandı") {
                Response::Ok { data } if data.get("done").is_some() => break,
                Response::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert!(
            db.get_project("p1")
                .unwrap()
                .unwrap()
                .last_active_at
                .is_some()
        );
        let routed: RouteResult = serde_json::from_value(
            data(
                &h,
                Request::Route {
                    text: "devam".into(),
                },
            )
            .await,
        )
        .unwrap();
        assert_eq!(routed.project_id.as_deref(), Some("p1"));
        assert_eq!(routed.confidence, 0.5);
        h.shutdown().await;
    }

    fn config_with_agy(enabled: bool) -> Config {
        let mut cfg = Config::default();
        for engine in cfg.engines.iter_mut().filter(|e| e.id == EngineId::Agy) {
            engine.enabled = enabled;
        }
        cfg
    }

    #[test]
    fn agy_summarizer_is_selected_only_when_agy_is_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();

        let enabled = select_summarizer(&paths, &config_with_agy(true)).unwrap();
        assert_eq!(enabled.kind(), "agy");
        assert!(paths.run.join("summarizer").is_dir());

        let disabled = select_summarizer(&paths, &config_with_agy(false)).unwrap();
        assert_eq!(disabled.kind(), "null");
    }

    #[test]
    fn summarizer_mode_selects_engine_and_reports_kind() {
        use ultron_core::config::SummarizerMode;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let with_mode = |mode: SummarizerMode, agy: bool| {
            let mut cfg = config_with_agy(agy);
            cfg.summarizer = mode;
            cfg
        };
        // auto: agy birincil; yedek (claude-haiku) iç içe kurulur, tür agy'dir.
        let auto = with_mode(SummarizerMode::Auto, true);
        assert_eq!(select_summarizer(&paths, &auto).unwrap().kind(), "agy");
        assert_eq!(summarizer_kind(&auto), "agy");
        // auto + agy kapalı: özetleyici yok (Claude'a sessizce geçilmez).
        let auto_off = with_mode(SummarizerMode::Auto, false);
        assert_eq!(select_summarizer(&paths, &auto_off).unwrap().kind(), "null");
        assert_eq!(summarizer_kind(&auto_off), "null");
        let haiku = with_mode(SummarizerMode::ClaudeHaiku, false);
        assert_eq!(select_summarizer(&paths, &haiku).unwrap().kind(), "claude-haiku");
        assert_eq!(summarizer_kind(&haiku), "claude-haiku");
        let agy_only = with_mode(SummarizerMode::Agy, true);
        assert_eq!(select_summarizer(&paths, &agy_only).unwrap().kind(), "agy");
        let off = with_mode(SummarizerMode::Off, true);
        assert_eq!(select_summarizer(&paths, &off).unwrap().kind(), "null");
        assert_eq!(summarizer_kind(&off), "null");
    }
}
