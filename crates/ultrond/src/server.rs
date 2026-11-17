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
use ultron_core::engine::{Engine, TurnOutcome};
use ultron_core::events::{EngineEvent, SwitchReason};
use ultron_core::ipc::{
    self, ChatDetail, MAX_LINE_BYTES, Request, Response, ct_eq, read_frame, sys, write_msg,
};
use ultron_core::summarizer::{AgySummarizer, NullSummarizer, Summarizer};
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
}

/// Çalışan sunucunun tutamacı.
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

/// Çalışma durumu özetleyicisini yapılandırmaya göre seçer. agy etkinse agy özetleyicisi
/// kurulur; değilse yalnız kural tabanlı durum kalır. Özetleyici, kullanıcı projelerinden
/// ayrı `run\summarizer` dizininde (salt okunur kipte) çalışır.
pub fn select_summarizer(paths: &Paths, cfg: &Config) -> Result<Box<dyn Summarizer>> {
    match cfg
        .engines
        .iter()
        .find(|e| e.id == EngineId::Agy && e.enabled)
    {
        Some(agy) => {
            let dir = paths.run.join("summarizer");
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("özetleyici dizini oluşturulamadı: {}", dir.display()))?;
            Ok(Box::new(AgySummarizer::agy(agy.clone(), dir)))
        }
        None => Ok(Box::new(NullSummarizer)),
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
    });

    let cancel = CancellationToken::new();
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
        Ok(text) => text.parse()?,
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
            if auth.role != ConnectionRole::Approver {
                return Err(anyhow!("yetkisiz: onaylayıcı rolü gerekli"));
            }
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
            let root = match root {
                Some(r) => std::path::PathBuf::from(r),
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
            let reservation = st.manager.reserve_turn(&chat_id, pre_engine)?;
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
                    let result = task_state
                        .manager
                        .send_reserved(
                            reservation,
                            &task_chat,
                            Path::new(&cwd),
                            engine,
                            &text,
                            policy.into(),
                        )
                        .await;
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
}
