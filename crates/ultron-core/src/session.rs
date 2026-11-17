//! Oturum yöneticisi: sohbet başına motor oturumlarını, tur sıralamasını ve motor değişimini
//! yönetir. Kalıcı durum (güncel motor, oturumlar, son görülen git HEAD) veritabanındadır;
//! bellekte yalnızca "şu an çalışan tur" kaydı tutulur.

pub mod audit;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{Config, EngineId};
use crate::db::{Db, RECENT_EVENTS_WINDOW, SessionRow, SessionStatus};
use crate::db::approvals::{Approval, ApprovalStatus};
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

/// Sohbet/yönetici kimliğinden bağımsız, aynı fiziksel çalışma alanı için tur kilidi.
/// Zayıf referanslar tamamlanan çalışma alanlarını bellekte tutmaz.
fn workspace_turn_lock(cwd: &Path) -> Result<Arc<tokio::sync::Mutex<()>>> {
    type WorkspaceLocks = Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<WorkspaceLocks> = OnceLock::new();
    let root = std::fs::canonicalize(cwd)?;
    #[cfg(windows)]
    let root = PathBuf::from(root.as_os_str().to_string_lossy().to_lowercase());
    let mut locks = LOCKS.get_or_init(Mutex::default).lock()
        .map_err(|_| anyhow!("çalışma alanı tur kilidi zehirlendi"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(root, Arc::downgrade(&lock));
    Ok(lock)
}

/// Mesaj içeriğini kalıcı onay kuyruğuna yazmadan, kesin özetini üretir.
#[cfg(windows)]
fn message_sha256(text: &str) -> Result<String> {
    use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut digest = [0_u8; 32];
    let length = u32::try_from(text.len()).map_err(|_| anyhow!("yükseltme mesajı çok büyük"))?;
    // SAFETY: salt okunur girdi ve 32 baytlık çıktı çağrı boyunca geçerli; anahtar yoktur.
    let status = unsafe {
        BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0,
            text.as_ptr(), length, digest.as_mut_ptr(), digest.len() as u32)
    };
    if status < 0 {
        bail!("yükseltme mesajının SHA-256 özeti üretilemedi");
    }
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(not(windows))]
fn message_sha256(_text: &str) -> Result<String> {
    bail!("yükseltme mesajının SHA-256 özeti bu platformda desteklenmiyor")
}

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

/// Sohbet için ayrılmış tek aktif tur; düşünce kilit serbest kalır.
pub struct TurnReservation {
    guard: TurnGuard,
    cancel: CancellationToken,
    engine: EngineId,
    turn_id: String,
    elevation: Option<Elevation>,
}

struct Elevation {
    approval_id: String,
    cwd: std::path::PathBuf,
    text: String,
}

impl TurnReservation {
    /// Onay bekleyen tur da normal Cancel komutuyla durdurulabilir.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
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
    audit_reports: broadcast::Sender<audit::AuditReport>,
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
            audit_reports: broadcast::channel(64).0,
        }
    }

    /// Çalışma durumu özetleyicisini değiştirir.
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Box<dyn Summarizer>) -> SessionManager {
        self.workstate = WorkStateService::new(self.db.clone(), summarizer);
        self
    }

    /// Codex/agy denetim sonuçları. Ortak PolicyViolation/bildirim adaptörü bu akışı tüketebilir.
    /// Dinleyici olmasa da ihlaller sohbet günlüğüne maskeli Error olarak kaydedilir.
    pub fn subscribe_audits(&self) -> broadcast::Receiver<audit::AuditReport> {
        self.audit_reports.subscribe()
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
        let reservation = self.reserve_turn(chat_id, engine)?;
        self.send_reserved(reservation, chat_id, cwd, engine, text, policy)
            .await
    }

    /// Motor kayıtlı mı ve sohbette başka tur çalışmıyor mu denetler; turu ayırır.
    /// Hata, kabulden önce istemciye döndürülebilsin diye `send`'den ayrıdır.
    pub fn reserve_turn(&self, chat_id: &str, engine: EngineId) -> Result<TurnReservation> {
        check_chat_id(chat_id)?;
        if !self.engines.contains_key(&engine) {
            return Err(anyhow!("motor kayıtlı değil: {engine:?}"));
        }
        let (guard, cancel) = self.begin_turn(chat_id)?;
        Ok(TurnReservation {
            guard,
            cancel,
            engine,
            turn_id: Uuid::new_v4().to_string(),
            elevation: None,
        })
    }

    /// Motor başlamadan önce, yalnız bu rezervasyona ait Full onayını kuyruğa alır.
    /// Onay burada çözülmez; requester hiçbir zaman onaylayıcı yetkisi kazanmaz.
    pub fn request_elevation(
        &self,
        reservation: &mut TurnReservation,
        cwd: &Path,
        text: &str,
    ) -> Result<Approval> {
        if !Arc::ptr_eq(&reservation.guard.active, &self.active) {
            bail!("tur rezervasyonu başka yöneticiye ait");
        }
        if reservation.elevation.is_some() {
            bail!("bu tur için yükseltme zaten istendi");
        }
        let cwd = std::fs::canonicalize(cwd)?;
        let action = crate::policy::Action::ToolCall {
            tool: "ultron.turn.elevate".into(),
            input: serde_json::json!({
                "turn_id": reservation.turn_id, "cwd": cwd, "policy": "full",
                "text_preview": crate::import::mask_secrets(text).0.chars().take(512).collect::<String>(),
                "text_sha256": message_sha256(text)?,
            }),
        };
        self.db.insert_chat(&reservation.guard.chat_id, None, "", Utc::now())?;
        let approval = self.db.create_approval(
            Some(&reservation.guard.chat_id), reservation.engine,
            crate::policy::RiskCategory::SystemConfig,
            "Yalnız bu tur için Full araç politikası istendi; sandbox kaldırılmaz", &action,
        )?;
        reservation.elevation = Some(Elevation {
            approval_id: approval.id.clone(), cwd, text: text.into(),
        });
        Ok(approval)
    }

    /// [`SessionManager::reserve_turn`] ile ayrılmış turu çalıştırır.
    pub async fn send_reserved(
        &self,
        reservation: TurnReservation,
        chat_id: &str,
        cwd: &Path,
        engine: EngineId,
        text: &str,
        policy: ToolPolicy,
    ) -> Result<TurnOutcome> {
        if reservation.guard.chat_id != chat_id
            || reservation.engine != engine
            || !Arc::ptr_eq(&reservation.guard.active, &self.active)
        {
            bail!("tur rezervasyonu başka sohbet, motor veya yönetici için kullanılamaz");
        }
        if policy == ToolPolicy::Full {
            let elevation = reservation.elevation.as_ref()
                .ok_or_else(|| anyhow!("Full politikası bu tur için kullanıcı onayı gerektirir"))?;
            let approval = self.db.get_approval(&elevation.approval_id)?
                .ok_or_else(|| anyhow!("yükseltme onayı bulunamadı"))?;
            if approval.status != ApprovalStatus::Approved
                || approval.chat_id.as_deref() != Some(chat_id)
                || approval.engine != engine
                || elevation.cwd != std::fs::canonicalize(cwd)?
                || elevation.text != text
            {
                bail!("Full politikası bu tur için geçerli kullanıcı onayı gerektirir");
            }
            // Komut metni kuyrukta maskelidir; yetkiyi sır taşımayan tekil tur kimliği bağlar.
            let recorded: crate::policy::Action = serde_json::from_str(&approval.action_json)?;
            let digest = message_sha256(text)?;
            if !matches!(recorded, crate::policy::Action::ToolCall { tool, input }
                if tool == "ultron.turn.elevate"
                    && input.get("turn_id").and_then(serde_json::Value::as_str) == Some(reservation.turn_id.as_str())
                    && input.get("policy").and_then(serde_json::Value::as_str) == Some("full")
                    && input.get("text_sha256").and_then(serde_json::Value::as_str) == Some(digest.as_str())) {
                bail!("yükseltme eylemi rezervasyonla eşleşmiyor");
            }
        }
        let TurnReservation {
            guard: _guard,
            cancel,
            engine: _,
            turn_id: _,
            elevation: _,
        } = reservation;
        let workspace_lock = workspace_turn_lock(cwd)?;
        let _workspace_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(TurnOutcome::Cancelled),
            guard = workspace_lock.lock_owned() => guard,
        };
        let driver = self
            .engines
            .get(&engine)
            .cloned()
            .ok_or_else(|| anyhow!("motor kayıtlı değil: {engine:?}"))?;
        self.db.insert_chat(chat_id, None, "", Utc::now())?;

        let project_id = self.db.get_chat(chat_id)?.and_then(|chat| chat.project_id);
        let project_bound = project_id.is_some();
        let project_note = match &project_id {
            Some(id) => self.db.get_project_note(id)?.map(|n| n.markdown),
            None => None,
        };
        let mode = prompts::work_mode(project_bound, cwd);
        let project_files = if project_bound {
            ["AGENTS.md", "CLAUDE.md"]
                .into_iter()
                .map(|name| cwd.join(name))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let instructions = if engine == EngineId::Agy {
            // agy komut satırı sınırı: not bütçeye sığmazsa kırpılır veya atılır, tur başarısız olmaz.
            prompts::compose_with_note_budget(
                Role::Brain,
                mode,
                &project_files,
                project_note.as_deref(),
                &crate::engine::agy::instructions_fit,
            )
        } else {
            prompts::compose_with_note(Role::Brain, mode, &project_files, project_note.as_deref())
        };
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

        if resumed && matches!(outcome, TurnOutcome::Failed { .. }) && !summary.saw_effects {
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
        let mut audit = if matches!(session.engine, EngineId::Codex | EngineId::Agy) {
            let dir = req.cwd.clone();
            let paths = crate::Paths::resolve()?;
            let project_bound = self.db.get_chat(chat_id)?.is_some_and(|chat| chat.project_id.is_some());
            match blocking(move || audit::TurnAudit::start(&dir, paths, project_bound)).await? {
                Ok(audit) => Some(audit),
                Err(error) => {
                    session.status = SessionStatus::Failed;
                    session.ended_at = Some(Utc::now());
                    self.db.upsert_session(session)?;
                    self.log.append(chat_id, Some(&session.id), EngineEvent::Error {
                        message: format!("Tur denetimi hazırlanamadı: {}", crate::import::mask_secrets(&error.to_string()).0),
                    })?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let (tx, mut rx) = mpsc::channel::<EngineEvent>(64);
        let run = run_turn(driver.as_ref(), req, tx, cancel);
        let consume = async {
            let mut summary = TurnSummary::default();
            while let Some(mut ev) = rx.recv().await {
                if let EngineEvent::SessionStarted {
                    native_session_id: Some(id),
                } = &ev
                {
                    session.native_session_id = Some(id.clone());
                    self.db.upsert_session(session)?;
                }
                summary.observe(&ev);
                if let Some(audit) = audit.as_mut() {
                    // Codex ToolCall tamamlanan MCP öğesidir; agy çağrı adımı ise
                    // henüz gerçekleşmemiş teklif olabilir. Teklifi ihlal sayma.
                    if session.engine == EngineId::Codex || !matches!(ev, EngineEvent::ToolCall { .. }) {
                        audit.observe(&ev);
                    }
                    ev = audit::mask_event(ev)?;
                }
                self.log.append(chat_id, Some(&session.id), ev)?;
            }
            Ok::<_, anyhow::Error>(summary)
        };
        let (outcome, mut summary) = tokio::join!(run, consume);

        // Başlatma hatası, iptal veya kota da gerçekleşmiş yan etkileri gizlememeli.
        if let Some(audit) = audit {
            let chat = chat_id.to_owned();
            let id = session.id.clone();
            let report = blocking(move || audit.finish(&chat, &id)).await?;
            if (!report.changes.is_empty() || !report.errors.is_empty())
                && let Ok(summary) = summary.as_mut()
            {
                summary.saw_effects = true;
            }
            for (path, change) in &report.changes {
                self.log.append(chat_id, Some(&session.id), EngineEvent::FileChanged {
                    path: crate::import::mask_secrets(&path.to_string_lossy()).0, change: change.clone(),
                })?;
            }
            for violation in &report.violations {
                self.log.append(chat_id, Some(&session.id), EngineEvent::Error {
                    message: format!("Politika ihlali ({:?}): {}", violation.category, violation.command),
                })?;
            }
            for error in &report.errors {
                let event = if error.starts_with("Denetim sınırlı:") {
                    EngineEvent::Unknown { raw: serde_json::json!({
                        "type": "audit_limited", "message": error,
                    }) }
                } else {
                    EngineEvent::Error { message: format!("Tur denetimi tamamlanamadı: {error}") }
                };
                self.log.append(chat_id, Some(&session.id), event)?;
            }
            let _ = self.audit_reports.send(report);
        }

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
    saw_effects: bool,
}

impl TurnSummary {
    fn observe(&mut self, ev: &EngineEvent) {
        match ev {
            EngineEvent::AssistantText { .. } => self.saw_text = true,
            EngineEvent::Error { .. } => self.saw_error = true,
            EngineEvent::QuotaExhausted { .. } => self.saw_quota = true,
            EngineEvent::TurnCompleted => self.saw_turn_completed = true,
            EngineEvent::Command { exit_code: Some(_), .. }
            | EngineEvent::FileChanged { .. }
            | EngineEvent::ToolCall { .. } => self.saw_effects = true,
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

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn elevation_digest_matches_sha256_vector() {
        assert_eq!(message_sha256("abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[tokio::test]
    async fn workspace_audits_wait_for_other_chats_and_release_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let first = workspace_turn_lock(temp.path()).unwrap().lock_owned().await;
        let same = workspace_turn_lock(&temp.path().join(".")).unwrap();
        assert!(same.try_lock().is_err(), "ikinci sohbet aynı alanı eşzamanlı kullanamaz");
        let other = tempfile::tempdir().unwrap();
        assert!(workspace_turn_lock(other.path()).unwrap().try_lock().is_ok());
        drop(first);
        assert!(same.try_lock().is_ok(), "iptal/bitiş kilidi bırakmalı");
    }
}
