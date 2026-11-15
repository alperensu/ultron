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
    self, MAX_LINE_BYTES, Request, Response, ct_eq, read_frame, sys, write_msg,
};
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
        let last = db.events_for_chat(&session.chat_id, None)?.pop();
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
    let state = Arc::new(State {
        manager: SessionManager::new(db.clone(), log.clone(), cfg.clone(), engines),
        db,
        log,
        token: token.clone(),
        keep: keep.clone(),
        default_engine: cfg.default_engine,
        turns: Mutex::new(JoinSet::new()),
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
        } => {
            if !Path::new(&cwd).is_absolute() {
                return Err(anyhow!("cwd mutlak bir yol olmalı: {cwd}"));
            }
            let chat_id = chat_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let engine = engine.unwrap_or(st.default_engine);
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
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(Flow::Close),
                        woke = wake.recv() => match woke {
                            Ok(ev) => write_msg(writer, &Response::Event(ev)).await?,
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
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
