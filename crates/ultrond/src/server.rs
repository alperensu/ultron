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
    keep: KeepAwake,
    default_engine: EngineId,
    turns: Mutex<JoinSet<()>>,
    paths: Paths,
    quota: ultron_core::quota::QuotaService,
}

/// Çalışan sunucunun tutamacı.
pub struct ServerHandle {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    pipe_name: String,
    token: String,
    keep: KeepAwake,
}

impl ServerHandle {
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    pub fn token(&self) -> &str {
        &self.token
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
        .map_err(|e| anyhow!("başka bir ultrond örneği çalışıyor ({}): {e}", path.display()))
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
    match cfg.engines.iter().find(|e| e.id == EngineId::Agy && e.enabled) {
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

    let db = Arc::new(Db::open(&paths.db)?);
    let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
    let recovered = recover_interrupted(&db, &log)?;
    if recovered > 0 {
        tracing::warn!(recovered, "yarım kalan oturumlar Interrupted yapıldı");
    }

    // Token pipe'tan önce yazılır; istemci pipe'ı görünce token'ı okuyabilir.
    let token = sys::random_token()?;
    sys::write_private_file(&ipc::token_path(&paths), token.as_bytes())?;

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
        keep: keep.clone(),
        default_engine: cfg.default_engine,
        turns: Mutex::new(JoinSet::new()),
        paths: paths.clone(),
        quota: ultron_core::quota::QuotaService::from_env(),
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
        keep,
    })
}

enum Flow {
    Continue,
    Close,
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
    match serde_json::from_str::<Request>(&line) {
        Ok(Request::Hello { token }) if ct_eq(token.as_bytes(), st.token.as_bytes()) => {
            if write_msg(
                &mut writer,
                &Response::Ok {
                    data: json!({"hello": true, "version": ultron_core::VERSION}),
                },
            )
            .await
            .is_err()
            {
                return;
            }
        }
        _ => {
            send_error(&mut writer, "kimlik doğrulaması başarısız").await;
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
        match dispatch(&st, request, &mut reader, &mut writer, &cancel).await {
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

/// Sonraki görevlerde gerçeklenecek istekler için dürüst hata (sahte veri yok).
/// `config.toml` içindeki `[reset_rights]` tablosunu günceller (diğer anahtarlar korunur,
/// yorumlar korunmaz); geçici dosya + yeniden adlandırma ile yazılır.
fn set_reset_rights(paths: &Paths, pool: &str, count: u32) -> Result<()> {
    if !matches!(pool, "claude" | "codex") {
        return Err(anyhow!("geçersiz havuz: {pool} (claude|codex)"));
    }
    let file = paths.root.join("config.toml");
    let mut table: toml::Table = if file.exists() {
        std::fs::read_to_string(&file)?.parse()?
    } else {
        toml::Table::new()
    };
    let rights = table
        .entry("reset_rights")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml: reset_rights tablo olmalı"))?;
    rights.insert(pool.to_string(), toml::Value::Integer(i64::from(count)));
    let tmp = paths.root.join("config.toml.tmp");
    std::fs::write(&tmp, toml::to_string(&table)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

async fn not_implemented(writer: &mut Writer, task: u8) -> Result<Flow> {
    send_error(writer, format!("henüz uygulanmadı (Aşama 2 Task {task})")).await;
    Ok(Flow::Continue)
}

async fn dispatch(
    st: &Arc<State>,
    request: Request,
    reader: &mut Reader,
    writer: &mut Writer,
    cancel: &CancellationToken,
) -> Result<Flow> {
    match request {
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
        Request::ImportCatalog { .. } => not_implemented(writer, 3).await,
        Request::ImportClaudeHistory { .. } => not_implemented(writer, 4).await,
        Request::Route { .. } => not_implemented(writer, 3).await,
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
            let hits = st
                .db
                .search_messages(chat_id.as_deref(), &query, limit.clamp(1, MAX_SEARCH_LIMIT))?;
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
            let cwd = project.as_ref().map_or(cwd, |p| p.path.clone());
            if !Path::new(&cwd).is_absolute() {
                return Err(anyhow!("cwd mutlak bir yol olmalı: {cwd}"));
            }
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
            let engine = match engine {
                Some(engine) => engine,
                None => st.db.chat_engine(&chat_id)?.unwrap_or(st.default_engine),
            };
            let mut last_seq = st.db.next_seq(&chat_id)? - 1;
            let mut wake = st.log.subscribe();
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
                        .send(
                            &task_chat,
                            Path::new(&cwd),
                            engine,
                            &text,
                            policy.into(),
                        )
                        .await;
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
        } => match chat_id {
            Some(chat) => {
                let mut last_seq = after_seq.unwrap_or(0);
                let mut wake = st.log.subscribe();
                loop {
                    flush_events(st, &chat, &mut last_seq, writer).await?;
                    if !follow {
                        ok(writer, json!({"done": true})).await?;
                        return Ok(Flow::Continue);
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(Flow::Close),
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
                let mut wake = st.log.subscribe();
                // Sohbet başına son iletilen sıra. Yayın kanalı taşarsa (Lagged) kaçan olaylar
                // bu imleçlerden itibaren veritabanından tamamlanır.
                let mut cursors: HashMap<String, i64> = HashMap::new();
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(Flow::Close),
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
