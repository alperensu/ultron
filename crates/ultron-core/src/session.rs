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
use crate::db::{Db, SessionRow, SessionStatus};
use crate::engine::{Engine, ToolPolicy, TurnOutcome, TurnRequest, agy, run_turn};
use crate::eventlog::EventLog;
use crate::events::{EngineEvent, SwitchReason};
use crate::external;
use crate::handoff::build_handoff;
use crate::summarizer::{NullSummarizer, Summarizer};
use crate::workstate::{WorkState, WorkStateService};

/// Devir paketinde aynen verilen son mesaj sayısı.
const HANDOFF_RECENT_MESSAGES: usize = 6;

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

        // Dış değişiklikler, motor değişim olayı yazılmadan önceki son olay zamanına göre bulunur.
        let last_ts = self
            .db
            .events_for_chat(chat_id, None)?
            .last()
            .map(|e| e.ts);
        let had_events = last_ts.is_some();
        let external = match last_ts {
            Some(since) => {
                let head = self.db.chat_last_head(chat_id)?;
                let touched = self.files_touched(chat_id)?;
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
        let mut session = resumable.unwrap_or_else(|| SessionRow {
            id: Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            engine,
            native_session_id: None,
            status: SessionStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
        });
        session.status = SessionStatus::Running;
        session.ended_at = None;
        self.db.upsert_session(&session)?;

        let prompt = if resume_native_id.is_some() {
            match &external {
                Some(ext) => format!("[SON OTURUMDAN BERİ DEĞİŞENLER]\n{ext}\n\n{text}"),
                None => text.to_string(),
            }
        } else if had_events {
            let pack = build_handoff(
                &self.db,
                chat_id,
                self.cfg.context_budget_chars,
                HANDOFF_RECENT_MESSAGES,
                external.as_deref(),
                Some(cwd),
            )?;
            format!("{}\n\n{text}", pack.text)
        } else {
            text.to_string()
        };

        self.log.append(
            chat_id,
            Some(&session.id),
            EngineEvent::UserText {
                text: text.to_string(),
            },
        )?;

        let req = TurnRequest {
            chat_id: chat_id.to_string(),
            cwd: cwd.to_path_buf(),
            prompt,
            resume_native_id,
            tool_policy: policy,
        };
        let (tx, mut rx) = mpsc::channel::<EngineEvent>(64);
        let run = run_turn(driver.as_ref(), req, tx, cancel);
        let consume = async {
            let mut collected = Vec::new();
            while let Some(ev) = rx.recv().await {
                if let EngineEvent::SessionStarted {
                    native_session_id: Some(id),
                } = &ev
                {
                    session.native_session_id = Some(id.clone());
                    self.db.upsert_session(&session)?;
                }
                self.log.append(chat_id, Some(&session.id), ev.clone())?;
                collected.push(ev);
            }
            Ok::<_, anyhow::Error>(collected)
        };
        let (outcome, collected) = tokio::join!(run, consume);

        let (mut outcome, collected) = match (outcome, collected) {
            (Ok(outcome), Ok(collected)) => (outcome, collected),
            (outcome, collected) => {
                let err = outcome.err().or(collected.err());
                session.status = SessionStatus::Failed;
                session.ended_at = Some(Utc::now());
                self.db.upsert_session(&session)?;
                return Err(err.unwrap_or_else(|| anyhow!("tur beklenmedik şekilde bitti")));
            }
        };

        let exit_code = match &outcome {
            TurnOutcome::Completed => Some(0),
            TurnOutcome::Failed { exit_code, .. } => *exit_code,
            TurnOutcome::Cancelled | TurnOutcome::QuotaExhausted => None,
        };
        let mut has_error = collected
            .iter()
            .any(|e| matches!(e, EngineEvent::Error { .. }));
        if engine == EngineId::Agy
            && let Some(ev) = agy::finalize(&collected, exit_code)
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
                if !collected
                    .iter()
                    .any(|e| matches!(e, EngineEvent::QuotaExhausted { .. }))
                {
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
        if outcome == TurnOutcome::Completed
            && let Err(err) = self.workstate.on_turn_completed(chat_id).await
        {
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

    fn files_touched(&self, chat_id: &str) -> Result<BTreeSet<String>> {
        Ok(match self.db.get_work_state(chat_id)? {
            Some(value) => serde_json::from_value::<WorkState>(value)?.files_touched,
            None => BTreeSet::new(),
        })
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
