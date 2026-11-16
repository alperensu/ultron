//! Oturum yöneticisi: sohbet başına motor oturumlarını, tur sıralamasını ve motor değişimini
//! yönetir. Kalıcı durum (güncel motor, oturumlar, son görülen git HEAD) veritabanındadır;
//! bellekte yalnızca "şu an çalışan tur" kaydı tutulur.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{Config, EngineId};
use crate::db::{Db, RECENT_EVENTS_WINDOW, SessionRow, SessionStatus};
use crate::engine::{Engine, ToolPolicy, TurnOutcome, TurnRequest, agy, run_turn};
use crate::eventlog::EventLog;
use crate::events::{EngineEvent, SwitchReason};
use crate::external;
use crate::handoff::build_handoff_before;
use crate::prompts::{self, Role};
use crate::summarizer::{NullSummarizer, Summarizer};
use crate::workstate::WorkStateService;

/// Devir paketinde aynen verilen son mesaj sayısı.
const HANDOFF_RECENT_MESSAGES: usize = 6;

/// Yerel oturum devam ettirilemediğinde, yeni oturum öncesinde yazılan olay metni.
const RESUME_FAILED_MESSAGE: &str = "yerel oturum devam ettirilemedi, yeni oturum açıldı";

type ActiveTurns = Arc<Mutex<HashMap<String, CancellationToken>>>;

/// Sohbet başına tek aktif tur kilidi; düşünce serbest bırakır.
struct TurnGuard {
    active: ActiveTurns,
    chat_id: String,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.active.lock() {
            map.remove(&self.chat_id);
        }
    }
}

/// Sohbet oturumlarını yöneten ana nesne.
pub struct SessionManager {
    db: Arc<Db>,
    log: Arc<EventLog>,
    cfg: Config,
    engines: HashMap<EngineId, Arc<dyn Engine>>,
    workstate: WorkStateService,
    active: ActiveTurns,
}

impl SessionManager {
    /// Varsayılan olarak yalnız kural tabanlı çalışma durumu kullanılır
    /// (`NullSummarizer`); özetleyici için [`SessionManager::with_summarizer`].
    pub fn new(
        db: Arc<Db>,
        log: Arc<EventLog>,
        cfg: Config,
        engines: HashMap<EngineId, Arc<dyn Engine>>,
    ) -> SessionManager {
        SessionManager {
            workstate: WorkStateService::new(db.clone(), Box::new(NullSummarizer)),
            db,
            log,
            cfg,
            engines,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Çalışma durumu özetleyicisini değiştirir.
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Box<dyn Summarizer>) -> SessionManager {
        self.workstate = WorkStateService::new(self.db.clone(), summarizer);
        self
    }

    /// Bir tur çalıştırır. Aynı motorda açık oturum varsa native resume; yoksa (sohbetin
    /// önceki olayları varsa) devir paketi + mesaj ile yeni oturum başlatır. Sohbetin güncel
    /// motoru `engine`'den farklıysa önce `EngineSwitched` yazılır (sessiz değişim yok).
    /// Native resume başarısız olursa (`Failed`) oturum kapatılır ve tur bir kez devir
    /// paketiyle yeni oturumda yeniden denenir.
    pub async fn send(
        &self,
        chat_id: &str,
        cwd: &Path,
        engine: EngineId,
        text: &str,
        policy: ToolPolicy,
    ) -> Result<TurnOutcome> {
        check_chat_id(chat_id)?;
        let driver = self
            .engines
            .get(&engine)
            .cloned()
            .ok_or_else(|| anyhow!("motor kayıtlı değil: {engine:?}"))?;
        let (_guard, cancel) = self.begin_turn(chat_id)?;
        self.db.insert_chat(chat_id, None, "", Utc::now())?;

        let project_bound = self
            .db
            .get_chat(chat_id)?
            .is_some_and(|chat| chat.project_id.is_some());
        let mode = prompts::work_mode(project_bound, cwd);
        let project_files = if project_bound {
            ["AGENTS.md", "CLAUDE.md"]
                .into_iter()
                .map(|name| cwd.join(name))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let instructions = prompts::compose(Role::Brain, mode, &project_files);
        let run_dir = crate::paths::Paths::resolve()?.run;
        let driver = prompts::with_instructions(driver, &instructions, &run_dir, chat_id)?;

        // Dış değişiklikler, motor değişim olayı yazılmadan önceki son olay zamanına göre bulunur.
        let had_events = self.db.has_events(chat_id)?;
        let external = match self.db.last_event(chat_id)? {
            Some(last) => {
                let since = last.ts;
                let head = self.db.chat_last_head(chat_id)?;
                // Yalnız `since` anında ya da sonrasında Ultron'un dokunduğu dosyalar dışlanır;
                // önceki turlarda dokunulan dosyalara sonradan yapılan kullanıcı düzenlemesi raporlanır.
                let touched: BTreeSet<String> = self
                    .db
                    .events_tail(chat_id, RECENT_EVENTS_WINDOW)?
                    .iter()
                    .filter(|e| e.ts >= since && e.kind == "file_changed")
                    .filter_map(|e| e.payload.get("path").and_then(|p| p.as_str()))
                    .map(str::to_string)
                    .collect();
                let dir = cwd.to_path_buf();
                blocking(move || external::changes_since(&dir, since, head.as_deref(), &touched))
                    .await??
            }
            None => None,
        };

        match self.db.chat_engine(chat_id)? {
            Some(current) if current != engine => {
                self.switch_inner(chat_id, engine, SwitchReason::User)?;
            }
            None => self.db.set_chat_engine(chat_id, engine)?,
            Some(_) => {}
        }
        if let Some(summary) = &external {
            self.log.append(
                chat_id,
                None,
                EngineEvent::ExternalChanges {
                    summary: summary.clone(),
                },
            )?;
        }

        let resumable = self.db.latest_session(chat_id)?.filter(|s| {
            s.engine == engine
                && s.status != SessionStatus::Closed
                && s.native_session_id.is_some()
        });
        let resume_native_id = resumable
            .as_ref()
            .and_then(|s| s.native_session_id.clone());
        let resumed = resume_native_id.is_some();
        let mut session = resumable.unwrap_or_else(|| new_session(chat_id, engine));
        session.status = SessionStatus::Running;
        session.ended_at = None;
        self.db.upsert_session(&session)?;

        let prompt = if resumed {
            match &external {
                Some(ext) => format!("[SON OTURUMDAN BERİ DEĞİŞENLER]\n{ext}\n\n{text}"),
                None => text.to_string(),
            }
        } else if had_events {
            let pack = build_handoff_before(
                &self.db,
                chat_id,
                None,
                self.cfg.context_budget_chars,
                HANDOFF_RECENT_MESSAGES,
                external.as_deref(),
                Some(cwd),
            )?;
            format!("{}\n\n{text}", pack.text)
        } else {
            text.to_string()
        };

        let user_seq = self
            .log
            .append(
                chat_id,
                Some(&session.id),
                EngineEvent::UserText {
                    text: text.to_string(),
                },
            )?
            .seq;

        let req = TurnRequest {
            chat_id: chat_id.to_string(),
            cwd: cwd.to_path_buf(),
            prompt,
            resume_native_id,
            tool_policy: policy,
        };
        let (mut outcome, mut summary) = self
            .run_attempt(chat_id, &mut session, req, driver.clone(), cancel.clone())
            .await?;

        if resumed && matches!(outcome, TurnOutcome::Failed { .. }) {
            // Yerel oturum bozuk olabilir: kapat, devir paketiyle yeni oturumda bir kez daha dene.
            session.status = SessionStatus::Closed;
            session.ended_at = Some(Utc::now());
            self.db.upsert_session(&session)?;
            session = new_session(chat_id, engine);
            self.db.upsert_session(&session)?;
            self.log.append(
                chat_id,
                Some(&session.id),
                EngineEvent::Error {
                    message: RESUME_FAILED_MESSAGE.to_string(),
                },
            )?;
            // Bu turun kullanıcı mesajı zaten günlükte; pakete girmesin diye kendisinden önceki olaylar alınır.
            let pack = build_handoff_before(
                &self.db,
                chat_id,
                Some(user_seq),
                self.cfg.context_budget_chars,
                HANDOFF_RECENT_MESSAGES,
                external.as_deref(),
                Some(cwd),
            )?;
            let retry = TurnRequest {
                chat_id: chat_id.to_string(),
                cwd: cwd.to_path_buf(),
                prompt: format!("{}\n\n{text}", pack.text),
                resume_native_id: None,
                tool_policy: policy,
            };
            (outcome, summary) = self
                .run_attempt(chat_id, &mut session, retry, driver, cancel)
                .await?;
        }

        let exit_code = match &outcome {
            TurnOutcome::Completed => Some(0),
            TurnOutcome::Failed { exit_code, .. } => *exit_code,
            TurnOutcome::Cancelled | TurnOutcome::QuotaExhausted => None,
        };
        let mut has_error = summary.saw_error;
        if engine == EngineId::Agy
            && let Some(ev) =
                agy::finalize_summary(summary.saw_text, summary.already_failed(), exit_code)
        {
            if let EngineEvent::Error { message } = &ev {
                outcome = TurnOutcome::Failed {
                    exit_code,
                    stderr_excerpt: message.clone(),
                };
            }
            self.log.append(chat_id, Some(&session.id), ev)?;
            has_error = true;
        }

        let status = match &outcome {
            TurnOutcome::Completed => SessionStatus::Running,
            TurnOutcome::Cancelled => SessionStatus::Interrupted,
            TurnOutcome::Failed {
                exit_code,
                stderr_excerpt,
            } => {
                if !has_error {
                    let code = exit_code.map_or_else(|| "?".to_string(), |c| c.to_string());
                    self.log.append(
                        chat_id,
                        Some(&session.id),
                        EngineEvent::Error {
                            message: format!(
                                "motor hata ile bitti (çıkış kodu {code}): {stderr_excerpt}"
                            ),
                        },
                    )?;
                }
                SessionStatus::Failed
            }
            TurnOutcome::QuotaExhausted => {
                if !summary.saw_quota {
                    self.log.append(
                        chat_id,
                        Some(&session.id),
                        EngineEvent::QuotaExhausted {
                            message: "motor kota hatası bildirdi".to_string(),
                            resets_at: None,
                        },
                    )?;
                }
                SessionStatus::WaitingQuota
            }
        };
        session.status = status;
        session.ended_at = matches!(status, SessionStatus::Failed | SessionStatus::Interrupted)
            .then(Utc::now);
        self.db.upsert_session(&session)?;

        let dir = cwd.to_path_buf();
        if let Some(head) = blocking(move || external::git_head(&dir)).await? {
            self.db.set_chat_last_head(chat_id, &head)?;
        }
        // Özetleyici yalnız tamamlanan turda çalışır; diğer sonuçlarda kural tabanlı durum
        // yine güncellenir ve özet eski işaretlenir.
        let refreshed = if outcome == TurnOutcome::Completed {
            self.workstate.on_turn_completed(chat_id).await
        } else {
            self.workstate.refresh_rules(chat_id)
        };
        if let Err(err) = refreshed {
            tracing::warn!(chat_id, error = %err, "çalışma durumu güncellenemedi");
        }
        Ok(outcome)
    }

    /// Sohbeti başka bir motora geçirir: `EngineSwitched` yazar, açık oturumu `Closed` yapar.
    /// Bir sonraki `send` yeni motorda devir paketiyle başlar. Tur çalışırken reddedilir.
    pub async fn switch(&self, chat_id: &str, to: EngineId, reason: SwitchReason) -> Result<()> {
        check_chat_id(chat_id)?;
        let (_guard, _cancel) = self.begin_turn(chat_id)?;
        self.switch_inner(chat_id, to, reason)
    }

    /// Çalışan turu iptal eder (süreç ağacı sonlandırılır; oturum `Interrupted` olur).
    pub async fn cancel(&self, chat_id: &str) -> Result<()> {
        let token = self
            .active
            .lock()
            .map_err(|_| anyhow!("tur kaydı kilidi zehirlendi"))?
            .get(chat_id)
            .cloned()
            .ok_or_else(|| anyhow!("sohbette çalışan tur yok"))?;
        token.cancel();
        Ok(())
    }

    /// Tek bir motor denemesi: olayları günlüğe yazar ve sonucu döndürür. Tur sonunda hata
    /// görülüp `TurnCompleted` gelmediyse sonuç `Failed` sayılır. Olay toplama veya çalıştırma
    /// hatasında oturum `Failed` yapılır ve hata döner.
    async fn run_attempt(
        &self,
        chat_id: &str,
        session: &mut SessionRow,
        req: TurnRequest,
        driver: Arc<dyn Engine>,
        cancel: CancellationToken,
    ) -> Result<(TurnOutcome, TurnSummary)> {
        let (tx, mut rx) = mpsc::channel::<EngineEvent>(64);
        let run = run_turn(driver.as_ref(), req, tx, cancel);
        let consume = async {
            let mut summary = TurnSummary::default();
            while let Some(ev) = rx.recv().await {
                if let EngineEvent::SessionStarted {
                    native_session_id: Some(id),
                } = &ev
                {
                    session.native_session_id = Some(id.clone());
                    self.db.upsert_session(session)?;
                }
                summary.observe(&ev);
                self.log.append(chat_id, Some(&session.id), ev)?;
            }
            Ok::<_, anyhow::Error>(summary)
        };
        let (outcome, summary) = tokio::join!(run, consume);

        let (outcome, summary) = match (outcome, summary) {
            (Ok(outcome), Ok(summary)) => (outcome, summary),
            (outcome, collected) => {
                let err = outcome.err().or(collected.err());
                session.status = SessionStatus::Failed;
                session.ended_at = Some(Utc::now());
                self.db.upsert_session(session)?;
                return Err(err.unwrap_or_else(|| anyhow!("tur beklenmedik şekilde bitti")));
            }
        };
        let outcome = if outcome == TurnOutcome::Completed
            && summary.saw_error
            && !summary.saw_turn_completed
        {
            TurnOutcome::Failed {
                exit_code: Some(0),
                stderr_excerpt: "tur tamamlanmadan hata bildirildi".to_string(),
            }
        } else {
            outcome
        };
        Ok((outcome, summary))
    }

    fn begin_turn(&self, chat_id: &str) -> Result<(TurnGuard, CancellationToken)> {
        let mut map = self
            .active
            .lock()
            .map_err(|_| anyhow!("tur kaydı kilidi zehirlendi"))?;
        if map.contains_key(chat_id) {
            bail!("sohbette çalışan bir tur var");
        }
        let token = CancellationToken::new();
        map.insert(chat_id.to_string(), token.clone());
        Ok((
            TurnGuard {
                active: self.active.clone(),
                chat_id: chat_id.to_string(),
            },
            token,
        ))
    }

    fn switch_inner(&self, chat_id: &str, to: EngineId, reason: SwitchReason) -> Result<()> {
        if !self.engines.contains_key(&to) {
            bail!("motor kayıtlı değil: {to:?}");
        }
        let from = self
            .db
            .chat_engine(chat_id)?
            .ok_or_else(|| anyhow!("sohbette henüz motor yok; önce bir mesaj gönderin"))?;
        if from == to {
            bail!("sohbet zaten {to:?} motorunda");
        }
        let latest = self.db.latest_session(chat_id)?;
        self.log.append(
            chat_id,
            latest.as_ref().map(|s| s.id.as_str()),
            EngineEvent::EngineSwitched { from, to, reason },
        )?;
        if let Some(mut session) = latest
            && session.status != SessionStatus::Closed
        {
            session.status = SessionStatus::Closed;
            session.ended_at = Some(Utc::now());
            self.db.upsert_session(&session)?;
        }
        self.db.set_chat_engine(chat_id, to)?;
        Ok(())
    }
}

fn new_session(chat_id: &str, engine: EngineId) -> SessionRow {
    SessionRow {
        id: Uuid::new_v4().to_string(),
        chat_id: chat_id.to_string(),
        engine,
        native_session_id: None,
        status: SessionStatus::Running,
        started_at: Utc::now(),
        ended_at: None,
    }
}

/// Tur sırasında gelen olaylardan yalnız sonuç kararında gereken bilgiler; olaylar
/// bellekte biriktirilmez (uzun turlarda bellek sınırsız büyümesin diye).
#[derive(Default)]
struct TurnSummary {
    saw_text: bool,
    saw_error: bool,
    saw_quota: bool,
    saw_turn_completed: bool,
}

impl TurnSummary {
    fn observe(&mut self, ev: &EngineEvent) {
        match ev {
            EngineEvent::AssistantText { .. } => self.saw_text = true,
            EngineEvent::Error { .. } => self.saw_error = true,
            EngineEvent::QuotaExhausted { .. } => self.saw_quota = true,
            EngineEvent::TurnCompleted => self.saw_turn_completed = true,
            _ => {}
        }
    }

    fn already_failed(&self) -> bool {
        self.saw_error || self.saw_quota
    }
}

fn check_chat_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("geçersiz sohbet kimliği: {id}");
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("arka plan görevi başarısız: {e}"))
}
