//! Taşımadan bağımsız kabul, rezervasyon, denetim ve sonuç sınırı.
//! Task 10 IPC'yi bu katmana bağlar; Task 7 kabul edilmiş olay yürütücüsünü sağlar.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinSet;
use ultron_core::config::TelegramConfig;
use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
use ultron_core::db::quota_wait::{QuotaResumeContext, QuotaWaitState, ResumePolicy};
use ultron_core::db::telegram::{TelegramAcceptance, TelegramTurnAcceptance};
use ultron_core::engine::{ToolPolicy, TurnOutcome};
use ultron_core::events::{EngineEvent, QuotaWaitReason, SwitchReason};
use ultron_core::ipc::{ChatInfo, PolicyArg, RouteResult};
use ultron_core::quota::{QuotaReport, QuotaService};
use ultron_core::session::TurnReservation;
use ultron_core::{Config, Db, EngineId, EventLog, Paths, SessionManager, StoredEvent};

use crate::keepawake::KeepAwake;
use crate::server::approvals::{Approvals, MAX_APPROVAL_WAIT_SECS};
use crate::telegram::contracts::*;
use crate::telegram::control::DesktopPresence;

/// Yürütücü kullanıcı olayını tekrar yazamaz; normal send_reserved bu sözleşmeyi
/// karşılamaz. Task 7 adaptörü kabul edilmiş olay/continuation girişini kullanır.
pub enum ExecutionSource {
    AcceptedUser(StoredEvent),
    QuotaResume(Box<QuotaResumeContext>),
}

pub struct ReservedExecution {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub cwd: PathBuf,
    pub engine: EngineId,
    pub policy: ToolPolicy,
    pub reservation: TurnReservation,
    pub source: ExecutionSource,
}

/// Tek zorunlu yürütme adaptörü; üretimde alternatif shell/engine yolu yoktur.
/// Resume adaptörü yürütmeden önce claimed generation'ı atomik running yapar;
/// iptal edilmiş/değişmiş nesli veya belirsiz yan etkiyi çalıştıramaz.
pub trait TurnExecutor: Send + Sync {
    fn execute(&self, turn: ReservedExecution) -> BoxFuture<'_, Result<TurnOutcome>>;
}

/// Task 6 tool-kind/idari işlem filtresini ve atomik ilk karar yolunu sağlar.
/// Local kaynağı yalnız require_approver sonrasında kullanılabilir (Task 10).
pub trait ToolApprovalResolver: Send + Sync {
    fn resolve<'a>(&'a self, origin: TurnOrigin, approval_id: &'a str, approve: bool)
        -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>>;
}

pub struct TurnServiceDependencies {
    pub manager: Arc<SessionManager>,
    pub db: Arc<Db>,
    pub log: Arc<EventLog>,
    pub paths: Paths,
    pub config: Config,
    pub telegram: Arc<RwLock<TelegramConfig>>,
    /// getMe ile doğrulanmış bot kimliği; token'ın kendisi tutulmaz.
    pub bot_id: Option<i64>,
    pub user_home: PathBuf,
    pub executor: Arc<dyn TurnExecutor>,
    pub resolver: Arc<dyn ToolApprovalResolver>,
    pub approvals: Arc<Approvals>,
    pub quota: Arc<QuotaService>,
    pub presence: Arc<DesktopPresence>,
    pub keep: KeepAwake,
}

struct ActiveTurn { logical_turn_id: String, engine: EngineId }

struct Inner {
    deps: TurnServiceDependencies,
    bot_id: RwLock<Option<i64>>,
    /// Şema cwd saklamıyor. Bu süreçte kabul edilmiş bağlam korunur; restart'ta
    /// eski projesiz oturumun dizini doğrulanamazsa yeni bir dizin uydurulmaz.
    contexts: Mutex<HashMap<String, PathBuf>>,
    active: Mutex<HashMap<String, ActiveTurn>>,
    tasks: Mutex<Option<JoinSet<()>>>,
}

#[derive(Clone)]
pub struct TurnService { inner: Arc<Inner> }

struct ActiveLease { inner: Arc<Inner>, chat_id: String }
impl Drop for ActiveLease {
    fn drop(&mut self) {
        if let Ok(mut active) = self.inner.active.lock() { active.remove(&self.chat_id); }
    }
}

impl TurnService {
    pub fn new(deps: TurnServiceDependencies) -> Result<Self, GatewayError> {
        deps.telegram.read().map_err(|_| GatewayError::Unavailable)?.validate().map_err(|_| GatewayError::InvalidInput)?;
        if deps.bot_id.is_some_and(|id| id <= 0) { return Err(GatewayError::InvalidInput); }
        let bot_id = RwLock::new(deps.bot_id);
        Ok(Self { inner: Arc::new(Inner { deps, bot_id, contexts: Mutex::new(HashMap::new()), active: Mutex::new(HashMap::new()), tasks: Mutex::new(Some(JoinSet::new())) }) })
    }

    /// Yalnız eski poller join edildikten ve yeni getMe doğrulandıktan sonra çağrılır.
    pub fn set_bot_identity(&self, bot_id: Option<i64>) -> Result<(), GatewayError> {
        if bot_id.is_some_and(|id| id <= 0) { return Err(GatewayError::InvalidInput); }
        let _acceptance = self.inner.tasks.lock().map_err(|_| GatewayError::Unavailable)?;
        *self.inner.bot_id.write().map_err(|_| GatewayError::Unavailable)? = bot_id;
        Ok(())
    }

    /// IPC requester normal tur başlatabilir; Full yine yeni yerel onay bekler.
    pub fn submit_local_message(&self, mut request: SubmitUserMessage) -> Result<AcceptedTurn, GatewayError> {
        request.origin = TurnOrigin::Local;
        self.submit(request)
    }

    pub async fn resolve_local_approval(&self, role: crate::telegram::control::ControlRole, approval_id: &str, approve: bool) -> Result<ApprovalStatus, GatewayError> {
        if role != crate::telegram::control::ControlRole::Approver { return Err(GatewayError::Unauthorized); }
        self.inner.deps.resolver.resolve(TurnOrigin::Local, approval_id, approve).await
    }

    fn allowed(&self, user_id: i64) -> Result<(), GatewayError> {
        if user_id > 0 && self.inner.deps.telegram.read().map_err(|_| GatewayError::Unavailable)?.allowed_user_ids.contains(&user_id) {
            Ok(())
        } else { Err(GatewayError::Unauthorized) }
    }

    fn origin(&self, origin: &TurnOrigin) -> Result<(), GatewayError> {
        if let TurnOrigin::Telegram { user_id, private_chat_id, update_id } = origin {
            self.allowed(*user_id)?;
            if private_chat_id != user_id || *update_id < 0 { return Err(GatewayError::Unauthorized); }
        }
        Ok(())
    }

    /// Kapanış yeni kabulü kapatır, turn future'larını ve wake guard'larını bırakır.
    pub async fn shutdown(&self) -> Result<(), GatewayError> {
        let tasks = self.inner.tasks.lock().map_err(|_| GatewayError::Unavailable)?.take();
        if let Some(mut tasks) = tasks { tasks.shutdown().await; }
        Ok(())
    }

    fn submit(&self, mut request: SubmitUserMessage) -> Result<AcceptedTurn, GatewayError> {
        self.origin(&request.origin)?;
        let remote = matches!(request.origin, TurnOrigin::Telegram { .. });
        if request.text.trim().is_empty() || request.text.len() > 1024 * 1024 { return Err(GatewayError::InvalidInput); }
        if remote && (request.elevated || request.policy == PolicyArg::Full) { return Err(GatewayError::Unauthorized); }
        let deps = &self.inner.deps;
        // tasks kilidi kabulden spawn'a kadar tutulur; shutdown araya giremez.
        let mut tasks = self.inner.tasks.lock().map_err(|_| GatewayError::Unavailable)?;
        let tasks = tasks.as_mut().ok_or(GatewayError::Unavailable)?;
        while tasks.try_join_next().is_some() {}
        let bot_id = if let TurnOrigin::Telegram { update_id, .. } = &request.origin {
            let bot_id = self.inner.bot_id.read().map_err(|_| GatewayError::Unavailable)?.ok_or(GatewayError::Unavailable)?;
            if deps.db.telegram_inbox_entry(bot_id, *update_id).map_err(unavailable)?.is_some()
                || deps.db.telegram_next_update_id(bot_id).map_err(unavailable)?.is_some_and(|next| *update_id < next) {
                return Err(GatewayError::StaleGeneration);
            }
            Some(bot_id)
        } else { None };
        if let TurnOrigin::Telegram { user_id, .. } = &request.origin {
            let binding = deps.db.telegram_binding(*user_id).map_err(unavailable)?;
            if request.chat_id.is_none() { request.chat_id = binding; }
        }
        let chat_id = request.chat_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        if chat_id.is_empty() || !chat_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { return Err(GatewayError::InvalidInput); }
        if self.inner.active.lock().map_err(|_| GatewayError::Unavailable)?.contains_key(&chat_id) { return Err(GatewayError::Busy); }
        let existing = deps.db.get_chat(&chat_id).map_err(unavailable)?;
        // Telegram'da mevcut sohbetin proje/motor/çalışma dizini kullanıcı metniyle değişmez.
        if remote && let Some(chat) = &existing {
            request.project_id = chat.project_id.clone();
            request.engine = chat.engine;
            request.cwd = if let Some(cwd) = self.inner.contexts.lock().map_err(|_| GatewayError::Unavailable)?.get(&chat_id) {
                cwd.clone()
            } else if chat.project_id.is_none() && deps.db.latest_session(&chat_id).map_err(unavailable)?.is_some() {
                return Err(GatewayError::NeedsAttention);
            } else { PathBuf::new() };
        }
        let project_id = existing.as_ref().and_then(|chat| chat.project_id.clone()).or(request.project_id.clone());
        if let (Some(chat), Some(requested)) = (&existing, &request.project_id)
            && chat.project_id.as_ref().is_some_and(|bound| bound != requested) { return Err(GatewayError::InvalidInput); }
        let project = project_id.as_ref().map(|id| deps.db.get_project(id).map_err(unavailable)?.ok_or(GatewayError::NotFound)).transpose()?;
        let cwd = if let Some(project) = &project { PathBuf::from(&project.path) }
            else if request.cwd.as_os_str().is_empty() || same_path(&request.cwd, &deps.user_home) {
                let path = deps.paths.root.join("workspaces").join(&chat_id);
                std::fs::create_dir_all(&path).map_err(unavailable)?;
                path
            } else { request.cwd };
        if !cwd.is_absolute() { return Err(GatewayError::InvalidInput); }
        let engine = request.engine.or(deps.db.chat_engine(&chat_id).map_err(unavailable)?).unwrap_or(deps.config.default_engine);
        // Motor yokluğu Busy değildir. SessionManager kayıt/rezervasyonu tek yerde doğrular.
        if !deps.config.engines.iter().any(|e| e.id == engine && e.enabled) { return Err(GatewayError::Unavailable); }
        let mut reservation = deps.manager.reserve_turn(&chat_id, engine).map_err(|_| GatewayError::Busy)?;
        deps.db.insert_chat(&chat_id, None, "", Utc::now()).map_err(unavailable)?;
        if let Some(project) = project { deps.db.bind_chat_project(&chat_id, &project.id).map_err(unavailable)?; }
        let policy = if request.elevated { ToolPolicy::Full } else { request.policy.into() };
        let elevation = if policy == ToolPolicy::Full {
            let approval = deps.manager.request_elevation(&mut reservation, &cwd, &request.text).map_err(unavailable)?;
            deps.approvals.publish_pending().map_err(unavailable)?;
            Some(approval.id)
        } else { None };
        let logical_turn_id = uuid::Uuid::new_v4().to_string();
        let event = StoredEvent { id: uuid::Uuid::new_v4().to_string(), chat_id: chat_id.clone(), session_id: None, seq: 0, ts: Utc::now(), kind: "user_text".into(), payload: serde_json::json!({"type":"user_text", "text":request.text}) };
        let accepted = match request.origin {
            TurnOrigin::Local => deps.db.append_user_event_invalidating_quota(&event).map_err(unavailable)?,
            TurnOrigin::Telegram { user_id, private_chat_id, update_id } => {
                let bot_id = bot_id.ok_or(GatewayError::Unavailable)?;
                match deps.db.accept_telegram_turn(&TelegramTurnAcceptance { bot_id, update_id, user_id, private_chat_id, logical_turn_id: &logical_turn_id, event: &event }).map_err(unavailable)? {
                    TelegramAcceptance::Accepted(event) => event,
                    TelegramAcceptance::Duplicate(_) => return Err(GatewayError::StaleGeneration),
                }
            }
        };
        // Kabul kalıcılaştıktan sonra hazırlık hatası da aynı completion yoluna gider.
        let preparation = (|| {
            if let TurnOrigin::Telegram { update_id, .. } = &request.origin
                && !deps.db.start_telegram_inbox(bot_id.ok_or(GatewayError::Unavailable)?, *update_id).map_err(unavailable)? {
                return Err(GatewayError::NeedsAttention);
            }
            self.inner.contexts.lock().map_err(|_| GatewayError::Unavailable)?.insert(chat_id.clone(), cwd.clone());
            // Task 10 ayrıca EventLog abonelerine kalıcı yayın telafisini bağlar.
            deps.log.write_persisted_transcript(&accepted).map_err(unavailable)?;
            deps.db.touch_chat(&chat_id, accepted.payload["text"].as_str().unwrap_or("")).map_err(unavailable)
        })();
        let user_event_seq = accepted.seq;
        let execution = ReservedExecution { logical_turn_id: logical_turn_id.clone(), chat_id: chat_id.clone(), cwd, engine, policy, reservation, source: ExecutionSource::AcceptedUser(accepted) };
        let completion = match self.spawn(tasks, execution, elevation, remote, preparation) {
            Ok(completion) => completion,
            Err(_) => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(failed_completion(deps, &logical_turn_id, &chat_id, user_event_seq, remote));
                rx
            }
        };
        Ok(AcceptedTurn { logical_turn_id, chat_id, user_event_seq, completion })
    }

    fn spawn(&self, tasks: &mut JoinSet<()>, execution: ReservedExecution, elevation: Option<String>, remote: bool, preparation: Result<(), GatewayError>)
        -> Result<oneshot::Receiver<Result<TurnCompletion, GatewayError>>, GatewayError> {
        let chat_id = execution.chat_id.clone();
        let logical_turn_id = execution.logical_turn_id.clone();
        let start_seq = match &execution.source { ExecutionSource::AcceptedUser(event) => event.seq, ExecutionSource::QuotaResume(context) => context.wait.checkpoint_seq };
        self.inner.active.lock().map_err(|_| GatewayError::Unavailable)?.insert(chat_id.clone(), ActiveTurn { logical_turn_id: logical_turn_id.clone(), engine: execution.engine });
        let lease = ActiveLease { inner: self.inner.clone(), chat_id: chat_id.clone() };
        let inner = self.inner.clone();
        let guard = inner.deps.keep.acquire();
        let (tx, rx) = oneshot::channel();
        tasks.spawn(async move {
            let _lease = lease;
            let _guard = guard;
            let result = async {
                preparation?;
                if let Some(id) = elevation { wait_elevation(&inner.deps, &id, &execution.reservation.cancellation_token()).await.map_err(unavailable)?; }
                let _grant = ultron_core::turn_grant::issue(&chat_id, &execution.cwd);
                let outcome = run_audited(&inner.deps.manager, &inner.deps.log, &chat_id, inner.deps.executor.execute(execution)).await.map_err(unavailable)?;
                let end_seq = inner.deps.db.next_seq(&chat_id).map_err(unavailable)? - 1;
                let mut after = start_seq;
                let mut text = String::new();
                loop {
                    let page = inner.deps.db.events_for_chat_page(&chat_id, Some(after), 500).map_err(unavailable)?;
                    if page.is_empty() { break; }
                    for event in page {
                        if event.seq > end_seq { break; }
                        after = event.seq;
                        match serde_json::from_value(event.payload) {
                            Ok(EngineEvent::AssistantText { text: chunk }) => text.push_str(&chunk),
                            // Araç öncesi ara yanıt final asistan yanıtı değildir.
                            Ok(EngineEvent::ToolCall { .. } | EngineEvent::ToolResult { .. } | EngineEvent::Command { .. }) => text.clear(),
                            _ => {},
                        }
                    }
                    if after >= end_seq { break; }
                }
                // Kota bir attempt sonucudur; aynı mantıksal turun final sınırını kapatmaz.
                if remote && !matches!(&outcome, TurnOutcome::QuotaExhausted)
                    && !inner.deps.db.complete_telegram_turn(&logical_turn_id, end_seq).map_err(unavailable)? {
                    return Err(GatewayError::NeedsAttention);
                }
                Ok(TurnCompletion { logical_turn_id: logical_turn_id.clone(), chat_id: chat_id.clone(), start_seq, end_seq, outcome: match outcome { TurnOutcome::Completed => CompletionOutcome::Completed, TurnOutcome::Cancelled => CompletionOutcome::Cancelled, TurnOutcome::QuotaExhausted => CompletionOutcome::QuotaExhausted, TurnOutcome::Failed { .. } => CompletionOutcome::Failed }, assistant_text: if text.is_empty() { None } else { Some(text) } })
            }.await;
            let result = result.or_else(|_| failed_completion(&inner.deps, &logical_turn_id, &chat_id, start_seq, remote));
            drop(_guard);
            drop(_lease);
            let _ = tx.send(result);
        });
        Ok(rx)
    }

    /// Scheduler hedefi etkin ve fallback_order içinde olmalıdır. Kaynak
    /// oturum/motor bağlamı değişmez; farklı hedefte rezervasyondan önce
    /// EngineSwitched(Quota) ve QuotaHandoff kalıcılaştırılır.
    /// Task 1 taşıma sözleşmesine hedef motor veya yeni bir yetki eklemez.
    pub fn resume_quota_step_with_engine(&self, request: ResumeQuotaStep, engine: EngineId) -> Result<AcceptedTurn, GatewayError> {
        self.resume(request, Some(engine))
    }

    fn resume(&self, request: ResumeQuotaStep, target: Option<EngineId>) -> Result<AcceptedTurn, GatewayError> {
        let deps = &self.inner.deps;
        let mut tasks = self.inner.tasks.lock().map_err(|_| GatewayError::Unavailable)?;
        let tasks = tasks.as_mut().ok_or(GatewayError::Unavailable)?;
        let context = deps.db.quota_resume_context(&request.wait_id, request.generation, Utc::now()).map_err(unavailable)?.ok_or(GatewayError::StaleGeneration)?;
        if context.wait.policy == ResumePolicy::RequiresLocalApproval { return Err(GatewayError::NeedsAttention); }
        let settings = deps.telegram.read().map_err(|_| GatewayError::Unavailable)?;
        if !settings.auto_continue_enabled { return Err(GatewayError::Unavailable); }
        let chat_id = context.wait.chat_id.clone();
        if self.inner.active.lock().map_err(|_| GatewayError::Unavailable)?.contains_key(&chat_id) { return Err(GatewayError::Busy); }
        let engine = target.unwrap_or(context.wait.engine);
        if !deps.config.engines.iter().any(|e| e.id == engine && e.enabled) { return Err(GatewayError::Unavailable); }
        if target.is_some() && !settings.fallback_order.contains(&engine) { return Err(GatewayError::InvalidInput); }
        if engine != context.wait.engine {
            // Sessiz geçiş yoktur; olay yazılamazsa rezervasyon/çalıştırma yapılmaz.
            deps.log.append(&chat_id, Some(&context.wait.source_session_id), EngineEvent::EngineSwitched { from: context.wait.engine, to: engine, reason: SwitchReason::Quota }).map_err(unavailable)?;
            deps.log.append(&chat_id, Some(&context.wait.source_session_id), EngineEvent::QuotaHandoff { logical_turn_id: context.wait.logical_turn_id.clone(), from: context.wait.engine, to: engine, resets_at: context.wait.reset_at, reason: QuotaWaitReason::FallbackSelected }).map_err(unavailable)?;
        }
        let reservation = deps.manager.reserve_turn(&chat_id, engine).map_err(|_| GatewayError::Busy)?;
        let logical_turn_id = context.wait.logical_turn_id.clone();
        let user_event_seq = context.wait.user_event_seq;
        let remote = deps.db.telegram_turn_origin(&logical_turn_id).map_err(unavailable)?.is_some();
        let execution = ReservedExecution { logical_turn_id: logical_turn_id.clone(), chat_id: chat_id.clone(), cwd: context.wait.cwd.clone(), engine, policy: context.wait.policy.as_policy_arg().into(), reservation, source: ExecutionSource::QuotaResume(Box::new(context)) };
        let completion = self.spawn(tasks, execution, None, remote, Ok(()))?;
        Ok(AcceptedTurn { logical_turn_id, chat_id, user_event_seq, completion })
    }
}

/// Hata metni sabittir; sağlayıcı hatası, araç çıktısı veya sır taşınmaz. Transcript
/// arızası SQLite final sınırını engelleyemez; yayın telafisi kalıcı seq'den yapılır.
fn failed_completion(deps: &TurnServiceDependencies, logical_turn_id: &str, chat_id: &str, start_seq: i64, remote: bool) -> Result<TurnCompletion, GatewayError> {
    let event = StoredEvent { id: uuid::Uuid::new_v4().to_string(), chat_id: chat_id.into(), session_id: None, seq: 0, ts: Utc::now(), kind: "error".into(), payload: serde_json::json!({"type": "error", "message": "Tur uygulama katmanında tamamlanamadı"}) };
    let event = deps.db.append_event(&event).map_err(unavailable)?.ok_or(GatewayError::Unavailable)?;
    let _ = deps.log.write_persisted_transcript(&event);
    if remote && !deps.db.complete_telegram_turn(logical_turn_id, event.seq).map_err(unavailable)? { return Err(GatewayError::NeedsAttention); }
    Ok(TurnCompletion { logical_turn_id: logical_turn_id.into(), chat_id: chat_id.into(), start_seq, end_seq: event.seq, outcome: CompletionOutcome::Failed, assistant_text: None })
}

fn unavailable(_: impl std::fmt::Display) -> GatewayError { GatewayError::Unavailable }
fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    !right.as_os_str().is_empty() && left.to_string_lossy().replace('/', "\\").trim_end_matches('\\').eq_ignore_ascii_case(right.to_string_lossy().replace('/', "\\").trim_end_matches('\\'))
}

impl GatewayApplication for TurnService {
    fn submit_user_message(&self, request: SubmitUserMessage) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> {
        Box::pin(async move {
            if !matches!(request.origin, TurnOrigin::Telegram { .. }) { return Err(GatewayError::Unauthorized); }
            self.submit(request)
        })
    }
    fn resume_quota_step(&self, request: ResumeQuotaStep) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> { Box::pin(async move { self.resume(request, None) }) }
    fn resolve_tool_approval<'a>(&'a self, origin: TurnOrigin, approval_id: &'a str, approve: bool) -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>> {
        Box::pin(async move {
            if !matches!(origin, TurnOrigin::Telegram { .. }) { return Err(GatewayError::Unauthorized); }
            self.origin(&origin)?;
            self.inner.deps.resolver.resolve(origin, approval_id, approve).await
        })
    }
    fn status_snapshot(&self) -> BoxFuture<'_, Result<GatewayStatusSnapshot, GatewayError>> {
        Box::pin(async {
            let active_turns = self.inner.active.lock().map_err(|_| GatewayError::Unavailable)?.iter().map(|(chat_id, turn)| ActiveTurnSnapshot { logical_turn_id: turn.logical_turn_id.clone(), chat_id: chat_id.clone(), engine: turn.engine }).collect();
            let pending_approvals = self.inner.deps.db.list_approvals(Some(ApprovalStatus::Pending)).map_err(unavailable)?;
            let settings = self.inner.deps.telegram.read().map_err(|_| GatewayError::Unavailable)?.clone();
            let now = Utc::now();
            let quota_waits = self.inner.deps.db.pending_quota_waits().map_err(unavailable)?.into_iter().map(|wait| QuotaWaitSnapshot { logical_turn_id: wait.logical_turn_id, chat_id: wait.chat_id, engine: wait.engine, resets_at: wait.reset_at, reason:
                if wait.state == QuotaWaitState::NeedsAttention { QuotaWaitReason::NeedsAttention }
                else if wait.policy == ResumePolicy::RequiresLocalApproval { QuotaWaitReason::LocalApprovalRequired }
                else if !settings.auto_continue_enabled { QuotaWaitReason::Disabled }
                else if let Some(reset) = wait.reset_at {
                    if reset <= now { QuotaWaitReason::ResetReached }
                    else if reset > now + chrono::Duration::minutes(i64::from(settings.auto_continue_wait_minutes)) { QuotaWaitReason::ResetBeyondWait }
                    else { QuotaWaitReason::ResetScheduled }
                } else { QuotaWaitReason::ResetUnknown }
            }).collect();
            Ok(GatewayStatusSnapshot { active_turns, pending_approvals, quota_waits, away: self.inner.deps.presence.is_away() })
        })
    }
    fn list_chats(&self) -> BoxFuture<'_, Result<Vec<ChatInfo>, GatewayError>> {
        Box::pin(async {
            let mut chats = self.inner.deps.db.list_chats(None, false).map_err(unavailable)?;
            chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
            chats.truncate(10);
            Ok(chats)
        })
    }
    fn route<'a>(&'a self, text: &'a str, last_active_project: Option<&'a str>) -> BoxFuture<'a, Result<RouteResult, GatewayError>> {
        Box::pin(async move { ultron_core::routing::route(&self.inner.deps.db, text, last_active_project).map_err(unavailable) })
    }
    fn quota_report(&self) -> BoxFuture<'_, Result<QuotaReport, GatewayError>> {
        Box::pin(async { let cfg = Config::load(&self.inner.deps.paths).map_err(unavailable)?; Ok(self.inner.deps.quota.report(&cfg, Utc::now()).await) })
    }
    fn get_binding(&self, user_id: i64) -> BoxFuture<'_, Result<Option<String>, GatewayError>> {
        Box::pin(async move { self.allowed(user_id)?; self.inner.deps.db.telegram_binding(user_id).map_err(unavailable) })
    }
    fn set_binding<'a>(&'a self, user_id: i64, chat_id: Option<&'a str>) -> BoxFuture<'a, Result<(), GatewayError>> {
        Box::pin(async move {
            self.allowed(user_id)?;
            if let Some(id) = chat_id {
                let chat = self.inner.deps.db.get_chat(id).map_err(unavailable)?.ok_or(GatewayError::NotFound)?;
                if chat.archived { return Err(GatewayError::NotFound); }
            }
            self.inner.deps.db.set_telegram_binding(user_id, chat_id).map_err(unavailable)
        })
    }
}

/// server::send_audited ile aynı kayıpsız denetim davranışı. Task 10 eski kopyayı kaldırır.
pub async fn run_audited<F: Future<Output = Result<TurnOutcome>>>(manager: &SessionManager, log: &EventLog, chat_id: &str, turn: F) -> Result<TurnOutcome> {
    let mut audits = manager.subscribe_audits();
    tokio::pin!(turn);
    let mut audit_error = None;
    let result = loop {
        tokio::select! {
            biased;
            report = audits.recv(), if audit_error.is_none() => {
                match report {
                    Ok(report) if report.chat_id == chat_id => {
                        if let Err(error) = persist_audit(log, report) { audit_error = Some(error); }
                    }
                    Ok(_) => {},
                    Err(_) => audit_error = Some(anyhow!("tur denetimi akışı kayıpsız alınamadı")),
                }
            }
            result = &mut turn => break result,
        }
    };
    while audit_error.is_none() {
        match audits.try_recv() {
            Ok(report) if report.chat_id == chat_id => {
                if let Err(error) = persist_audit(log, report) { audit_error = Some(error); }
            }
            Ok(_) => {},
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(_) => audit_error = Some(anyhow!("tur denetimi akışı kayıpsız alınamadı")),
        }
    }
    match audit_error { Some(error) => Err(error.context("politika ihlalleri kalıcılaştırılamadı")), None => result }
}

pub fn persist_audit(log: &EventLog, report: ultron_core::session::audit::AuditReport) -> Result<()> {
    for violation in report.violations {
        log.append(&report.chat_id, Some(&report.session_id), EngineEvent::PolicyViolation { command: violation.command, category: violation.category })?;
    }
    Ok(())
}

async fn wait_elevation(deps: &TurnServiceDependencies, id: &str, cancel: &tokio_util::sync::CancellationToken) -> Result<()> {
    let deadline = tokio::time::sleep(Duration::from_secs(u64::from(MAX_APPROVAL_WAIT_SECS)));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => { let _ = deps.approvals.resolve(id, false, ResolvedBy::Timeout); return Err(anyhow!("yükseltme bekleyen tur iptal edildi")); }
            () = &mut deadline => { let _ = deps.approvals.resolve(id, false, ResolvedBy::Timeout); return Err(anyhow!("yükseltme onay süresi doldu")); }
            _ = tick.tick() => {
                let row = deps.db.get_approval(id)?.ok_or_else(|| anyhow!("yükseltme onayı bulunamadı"))?;
                match row.status {
                    ApprovalStatus::Pending => {},
                    ApprovalStatus::Approved if row.resolved_by == Some(ResolvedBy::Approver) => { if deps.db.approval_outbox()?.is_empty() { return Ok(()); } }
                    _ => return Err(anyhow!("Full tur için onaylayıcı onayı verilmedi")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use ultron_core::config::SummarizerMode;
    use ultron_core::engine::{Engine, FakeEngine};
    use ultron_core::ipc::ProjectInfo;
    use ultron_core::quota::{HttpClient, HttpFuture, QuotaSources, Token};
    use crate::telegram::control::SystemClock;

    struct NoHttp;
    impl HttpClient for NoHttp {
        fn get<'a>(&'a self, _: &'a str, _: &'a Token) -> HttpFuture<'a> { panic!("test ağ erişimi yapmaz") }
    }
    struct Resolver;
    impl ToolApprovalResolver for Resolver {
        fn resolve<'a>(&'a self, _: TurnOrigin, _: &'a str, _: bool) -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>> {
            Box::pin(async { Err(GatewayError::Unauthorized) })
        }
    }

    struct FakeExecutor {
        log: Arc<EventLog>,
        db: Arc<Db>,
        blocked: AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        calls: AtomicUsize,
        quota: AtomicBool,
        fail: AtomicBool,
        seen: Mutex<Vec<(String, PathBuf, i64)>>,
        resumes: Mutex<Vec<ObservedResume>>,
    }
    struct ObservedResume { source_engine: EngineId, engine: EngineId, source_session_id: String, checkpoint_seq: i64 }
    impl TurnExecutor for FakeExecutor {
        fn execute(&self, turn: ReservedExecution) -> BoxFuture<'_, Result<TurnOutcome>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut running_wait = None;
                let event = match &turn.source {
                    ExecutionSource::AcceptedUser(event) => event,
                    ExecutionSource::QuotaResume(context) => {
                        self.resumes.lock().unwrap().push(ObservedResume { source_engine: context.wait.engine, engine: turn.engine, source_session_id: context.wait.source_session_id.clone(), checkpoint_seq: context.checkpoint.seq });
                        running_wait = Some(self.db.advance_quota_wait(&context.wait.id, context.wait.generation, QuotaWaitState::Running, None, Utc::now())?.ok_or_else(|| anyhow!("eski test nesli"))?);
                        &context.user_event
                    }
                };
                self.seen.lock().unwrap().push((turn.logical_turn_id.clone(), turn.cwd.clone(), event.seq));
                assert!(ultron_core::turn_grant::live_tokens().iter().any(|s| !s.is_empty()));
                self.started.notify_one();
                if self.blocked.load(Ordering::SeqCst) { self.release.notified().await; }
                if self.fail.load(Ordering::SeqCst) { return Err(anyhow!("synthetic-private-executor-detail")); }
                self.log.append(&turn.chat_id, None, EngineEvent::AssistantText { text: "Araçtan önceki açıklama".into() })?;
                self.log.append(&turn.chat_id, None, EngineEvent::ToolResult { tool: "synthetic".into(), output_excerpt: "özel araç çıktısı".into(), is_error: false })?;
                self.log.append(&turn.chat_id, None, EngineEvent::AssistantText { text: "Yalnız bu turun yanıtı".into() })?;
                if self.quota.load(Ordering::SeqCst) {
                    self.log.append(&turn.chat_id, None, EngineEvent::QuotaExhausted { message: "Sentetik kota sınırı".into(), resets_at: None })?;
                    Ok(TurnOutcome::QuotaExhausted)
                } else {
                    self.log.append(&turn.chat_id, None, EngineEvent::TurnCompleted)?;
                    if let Some(wait) = running_wait {
                        self.db.advance_quota_wait(&wait.id, wait.generation, QuotaWaitState::Completed, None, Utc::now())?.ok_or_else(|| anyhow!("eski test nesli"))?;
                    }
                    Ok(TurnOutcome::Completed)
                }
            })
        }
    }

    struct Env { _tmp: tempfile::TempDir, service: TurnService, db: Arc<Db>, executor: Arc<FakeExecutor>, project: PathBuf }
    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let cfg = Config { summarizer: SummarizerMode::Off, claude_live_quota: false, ..Config::default() };
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
        let fixture = tmp.path().join("empty.jsonl");
        std::fs::write(&fixture, "").unwrap();
        let engines: HashMap<EngineId, Arc<dyn Engine>> = cfg.engines.iter().filter(|e| e.enabled)
            .map(|e| (e.id, Arc::new(FakeEngine::from_fixture(&fixture)) as Arc<dyn Engine>)).collect();
        let manager = Arc::new(SessionManager::new(db.clone(), log.clone(), cfg.clone(), engines));
        let executor = Arc::new(FakeExecutor { log: log.clone(), db: db.clone(), blocked: AtomicBool::new(false), started: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(), calls: AtomicUsize::new(0), quota: AtomicBool::new(false), fail: AtomicBool::new(false), seen: Mutex::new(Vec::new()), resumes: Mutex::new(Vec::new()) });
        let project = tmp.path().join("synthetic-project");
        std::fs::create_dir(&project).unwrap();
        db.upsert_project(&ProjectInfo { id: "p1".into(), name: "Sentetik proje".into(), path: project.to_string_lossy().into_owned(), aliases: vec![], description: None, last_active_at: None }).unwrap();
        let deps = TurnServiceDependencies { manager, db: db.clone(), log: log.clone(), paths: paths.clone(), config: cfg, telegram: Arc::new(RwLock::new(TelegramConfig { allowed_user_ids: vec![101, 102], ..Default::default() })), bot_id: Some(999), user_home: tmp.path().join("synthetic-home"), executor: executor.clone(), resolver: Arc::new(Resolver), approvals: Arc::new(Approvals::new(db.clone(), log, paths)), quota: Arc::new(QuotaService::new(QuotaSources::from_home(tmp.path()), Arc::new(NoHttp))), presence: Arc::new(DesktopPresence::new(Arc::new(SystemClock))), keep: KeepAwake::new() };
        Env { _tmp: tmp, service: TurnService::new(deps).unwrap(), db, executor, project }
    }

    fn request(update_id: i64) -> SubmitUserMessage {
        SubmitUserMessage { chat_id: None, project_id: None, cwd: PathBuf::new(), engine: None, text: "Sentetik kullanıcı hedefi".into(), policy: PolicyArg::Workspace, elevated: false, origin: TurnOrigin::Telegram { user_id: 101, private_chat_id: 101, update_id } }
    }

    fn assert_failed_boundary(env: &Env, accepted: &AcceptedTurn, result: &TurnCompletion) {
        use ultron_core::db::telegram::{DeliveryState, InboxState};
        assert_eq!(result.logical_turn_id, accepted.logical_turn_id);
        assert_eq!(result.chat_id, accepted.chat_id);
        assert_eq!(result.start_seq, accepted.user_event_seq);
        assert_eq!(result.outcome, CompletionOutcome::Failed);
        assert!(result.assistant_text.is_none());
        let events = env.db.events_for_chat(&accepted.chat_id, None).unwrap();
        let error = events.iter().find(|event| event.seq == result.end_seq).unwrap();
        assert_eq!(error.kind, "error");
        assert_eq!(error.payload["message"], "Tur uygulama katmanında tamamlanamadı");
        assert!(!serde_json::to_string(&events.iter().map(|event| &event.payload).collect::<Vec<_>>()).unwrap().contains("synthetic-private-executor-detail"));
        assert_eq!(events.iter().filter(|event| event.kind == "user_text").count(), 1);
        let origin = env.db.telegram_turn_origin(&accepted.logical_turn_id).unwrap().unwrap();
        assert_eq!(origin.end_seq, Some(result.end_seq));
        assert_eq!(origin.delivery_state, DeliveryState::Ready);
        assert_eq!(env.db.telegram_inbox_entry(999, 1).unwrap().unwrap().state, InboxState::Completed);
        assert!(env.db.finish_telegram_delivery(&accepted.logical_turn_id, true).unwrap());
        assert_eq!(env.db.telegram_turn_origin(&accepted.logical_turn_id).unwrap().unwrap().delivery_state, DeliveryState::Delivered);
        assert_eq!(env.service.inner.deps.keep.holders(), 0);
        assert!(env.service.inner.deps.manager.reserve_turn(&accepted.chat_id, EngineId::Claude).is_ok());
    }

    #[tokio::test]
    async fn transcript_failure_after_acceptance_delivers_persisted_failed_completion() {
        let env = env();
        let mut submission = request(1);
        submission.chat_id = Some("transcript-failure".into());
        // Dizin aynı dosya yolunu kapatır; hem kabul hem Error transcript'i başarısızdır.
        std::fs::create_dir(env.service.inner.deps.paths.transcripts.join("transcript-failure.jsonl")).unwrap();
        let mut accepted = env.service.submit_user_message(submission).await.unwrap();
        let result = (&mut accepted.completion).await.unwrap().unwrap();
        assert_failed_boundary(&env, &accepted, &result);
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(env.service.submit_user_message(request(1)).await, Err(GatewayError::StaleGeneration)));
        assert!(env.service.status_snapshot().await.unwrap().active_turns.is_empty());
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn executor_error_closes_final_boundary_at_safe_error_seq() {
        let env = env();
        env.executor.fail.store(true, Ordering::SeqCst);
        let mut accepted = env.service.submit_user_message(request(1)).await.unwrap();
        let result = (&mut accepted.completion).await.unwrap().unwrap();
        assert_failed_boundary(&env, &accepted, &result);
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(env.service.submit_user_message(request(1)).await, Err(GatewayError::StaleGeneration)));
        env.executor.fail.store(false, Ordering::SeqCst);
        let next = env.service.submit_user_message(request(2)).await.unwrap();
        assert_eq!(next.completion.await.unwrap().unwrap().outcome, CompletionOutcome::Completed);
        env.service.shutdown().await.unwrap();
    }

    async fn claimed_quota(env: &Env) -> (ResumeQuotaStep, String, i64) {
        env.executor.quota.store(true, Ordering::SeqCst);
        let accepted = env.service.submit_user_message(request(1)).await.unwrap();
        let result = accepted.completion.await.unwrap().unwrap();
        let db = &env.db;
        db.upsert_session(&ultron_core::db::SessionRow { id: "quota-source".into(), chat_id: accepted.chat_id.clone(), engine: EngineId::Claude, native_session_id: Some("synthetic-native".into()), status: ultron_core::SessionStatus::WaitingQuota, started_at: Utc::now(), ended_at: None }).unwrap();
        db.set_chat_engine(&accepted.chat_id, EngineId::Claude).unwrap();
        let wait = db.create_quota_wait(&ultron_core::db::quota_wait::NewQuotaWait { id: "wait".into(), logical_turn_id: accepted.logical_turn_id, chat_id: accepted.chat_id.clone(), source_session_id: "quota-source".into(), engine: EngineId::Claude, user_event_seq: accepted.user_event_seq, checkpoint_seq: result.end_seq, cwd: env.executor.seen.lock().unwrap()[0].1.clone(), policy: ResumePolicy::Workspace, reset_at: Some(Utc::now() + chrono::Duration::hours(2)), next_attempt_at: None }, Utc::now()).unwrap();
        let claimed = db.claim_quota_wait(&wait.id, wait.generation, Utc::now()).unwrap().unwrap();
        env.executor.quota.store(false, Ordering::SeqCst);
        (ResumeQuotaStep { wait_id: claimed.id, generation: claimed.generation }, accepted.chat_id, result.end_seq)
    }

    #[tokio::test]
    async fn quota_fallback_executes_target_preserving_source_and_records_handoff() {
        let env = env();
        let (request, chat_id, checkpoint) = claimed_quota(&env).await;
        env.service.inner.deps.telegram.write().unwrap().fallback_order = vec![EngineId::Codex];
        let accepted = env.service.resume_quota_step_with_engine(request, EngineId::Codex).unwrap();
        let result = accepted.completion.await.unwrap().unwrap();
        assert_eq!(result.outcome, CompletionOutcome::Completed);
        assert_eq!(result.start_seq, checkpoint);
        {
            let seen = env.executor.resumes.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].source_engine, EngineId::Claude);
            assert_eq!(seen[0].engine, EngineId::Codex);
            assert_eq!(seen[0].source_session_id, "quota-source");
            assert_eq!(seen[0].checkpoint_seq, checkpoint);
        }
        let events = env.db.events_for_chat(&chat_id, Some(checkpoint)).unwrap();
        assert!(matches!(serde_json::from_value::<EngineEvent>(events[0].payload.clone()).unwrap(), EngineEvent::EngineSwitched { from: EngineId::Claude, to: EngineId::Codex, reason: SwitchReason::Quota }));
        assert!(matches!(serde_json::from_value::<EngineEvent>(events[1].payload.clone()).unwrap(), EngineEvent::QuotaHandoff { logical_turn_id, from: EngineId::Claude, to: EngineId::Codex, reason: QuotaWaitReason::FallbackSelected, .. } if logical_turn_id == result.logical_turn_id));
        assert_eq!(env.db.events_for_chat(&chat_id, None).unwrap().iter().filter(|event| event.kind == "user_text").count(), 1);
        assert_eq!(env.db.quota_wait("wait").unwrap().unwrap().engine, EngineId::Claude);
        assert_eq!(env.db.telegram_turn_origin(&result.logical_turn_id).unwrap().unwrap().end_seq, Some(result.end_seq));
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quota_default_resume_keeps_source_engine_without_switch() {
        let env = env();
        let (request, chat_id, checkpoint) = claimed_quota(&env).await;
        let accepted = env.service.resume_quota_step(request).await.unwrap();
        accepted.completion.await.unwrap().unwrap();
        assert_eq!(env.executor.resumes.lock().unwrap()[0].engine, EngineId::Claude);
        assert!(env.db.events_for_chat(&chat_id, Some(checkpoint)).unwrap().iter().all(|event| event.kind != "engine_switched" && event.kind != "quota_handoff"));
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quota_fallback_rejects_unconfigured_disabled_and_stale_targets() {
        for disabled in [false, true] {
            let mut env = env();
            if disabled {
                Arc::get_mut(&mut env.service.inner).unwrap().deps.config.engines.iter_mut().find(|engine| engine.id == EngineId::Codex).unwrap().enabled = false;
                env.service.inner.deps.telegram.write().unwrap().fallback_order = vec![EngineId::Codex];
            }
            let (request, chat_id, checkpoint) = claimed_quota(&env).await;
            assert!(matches!(env.service.resume_quota_step_with_engine(ResumeQuotaStep { wait_id: request.wait_id.clone(), generation: request.generation }, EngineId::Claude), Err(GatewayError::InvalidInput)));
            let result = env.service.resume_quota_step_with_engine(request, EngineId::Codex);
            assert!(matches!((disabled, result), (true, Err(GatewayError::Unavailable)) | (false, Err(GatewayError::InvalidInput))));
            assert_eq!(env.executor.calls.load(Ordering::SeqCst), 1);
            assert!(env.db.events_for_chat(&chat_id, Some(checkpoint)).unwrap().is_empty());
            env.db.cancel_quota_waits(&chat_id, Utc::now()).unwrap();
            env.service.inner.deps.telegram.write().unwrap().fallback_order = vec![EngineId::Codex];
            assert!(matches!(env.service.resume_quota_step_with_engine(ResumeQuotaStep { wait_id: "wait".into(), generation: 1 }, EngineId::Codex), Err(GatewayError::StaleGeneration)));
            env.service.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn quota_switch_is_persisted_before_reservation_and_log_failure_stops_execution() {
        for fail_transcript in [false, true] {
            let env = env();
            let (request, chat_id, checkpoint) = claimed_quota(&env).await;
            env.service.inner.deps.telegram.write().unwrap().fallback_order = vec![EngineId::Codex];
            let path = env.service.inner.deps.paths.transcripts.join(format!("{chat_id}.jsonl"));
            if fail_transcript {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            let reservation = env.service.inner.deps.manager.reserve_turn(&chat_id, EngineId::Claude).unwrap();
            let result = env.service.resume_quota_step_with_engine(request, EngineId::Codex);
            assert!(matches!((fail_transcript, result), (true, Err(GatewayError::Unavailable)) | (false, Err(GatewayError::Busy))));
            let events = env.db.events_for_chat(&chat_id, Some(checkpoint)).unwrap();
            assert_eq!(events[0].kind, "engine_switched");
            assert_eq!(env.executor.calls.load(Ordering::SeqCst), 1);
            drop(reservation);
            assert!(env.service.inner.deps.manager.reserve_turn(&chat_id, EngineId::Codex).is_ok());
            env.service.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn binding_new_existing_chat_project_cwd_and_logical_result() {
        let env = env();
        let mut first = request(1);
        first.project_id = Some("p1".into());
        let accepted = env.service.submit_user_message(first).await.unwrap();
        let id = accepted.logical_turn_id.clone();
        let chat_id = accepted.chat_id.clone();
        let result = accepted.completion.await.unwrap().unwrap();
        assert_eq!(result.logical_turn_id, id);
        assert_eq!(result.chat_id, chat_id);
        assert_eq!(result.start_seq, accepted.user_event_seq);
        assert_eq!(result.assistant_text.as_deref(), Some("Yalnız bu turun yanıtı"));
        assert_eq!(env.service.get_binding(101).await.unwrap().as_deref(), Some(chat_id.as_str()));
        assert_eq!(env.service.get_binding(102).await.unwrap(), None);
        assert_eq!(env.executor.seen.lock().unwrap()[0].1, env.project);
        let mut second = request(2);
        second.project_id = Some("ignored".into());
        second.cwd = PathBuf::from("ignored-relative-path");
        let second = env.service.submit_user_message(second).await.unwrap();
        assert_eq!(second.chat_id, chat_id);
        second.completion.await.unwrap().unwrap();
        assert_eq!(env.executor.seen.lock().unwrap()[1].1, env.project);
        let origin = env.db.telegram_turn_origin(&id).unwrap().unwrap();
        assert_eq!(origin.user_id, 101);
        assert_eq!(origin.private_chat_id, 101);
        let events = env.db.events_for_chat(&chat_id, None).unwrap();
        assert_eq!(events.iter().filter(|event| event.kind == "user_text").count(), 2);
        assert!(matches!(env.service.submit_user_message(request(2)).await, Err(GatewayError::StaleGeneration)));
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 2);
        env.service.set_binding(101, None).await.unwrap();
        let third = env.service.submit_user_message(request(3)).await.unwrap();
        assert_ne!(third.chat_id, chat_id);
        third.completion.await.unwrap().unwrap();
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn running_turn_is_rejected_before_inbox_acceptance() {
        let env = env();
        env.executor.blocked.store(true, Ordering::SeqCst);
        let first = env.service.submit_user_message(request(10)).await.unwrap();
        env.executor.started.notified().await;
        assert!(matches!(env.service.submit_user_message(request(11)).await, Err(GatewayError::Busy)));
        assert!(env.db.telegram_inbox_entry(999, 11).unwrap().is_none());
        let status = env.service.status_snapshot().await.unwrap();
        assert_eq!(status.active_turns.len(), 1);
        assert_eq!(status.active_turns[0].logical_turn_id, first.logical_turn_id);
        env.executor.release.notify_one();
        first.completion.await.unwrap().unwrap();
        assert!(env.service.status_snapshot().await.unwrap().active_turns.is_empty());
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn origin_full_and_binding_checks_fail_closed() {
        let env = env();
        let mut invalid = request(1);
        invalid.origin = TurnOrigin::Telegram { user_id: 101, private_chat_id: 102, update_id: 1 };
        assert!(matches!(env.service.submit_user_message(invalid).await, Err(GatewayError::Unauthorized)));
        let mut full = request(1);
        full.policy = PolicyArg::Full;
        assert!(matches!(env.service.submit_user_message(full).await, Err(GatewayError::Unauthorized)));
        assert_eq!(env.service.set_binding(333, None).await, Err(GatewayError::Unauthorized));
        assert_eq!(env.service.set_binding(101, Some("missing")).await, Err(GatewayError::NotFound));
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 0);
        env.service.shutdown().await.unwrap();
        assert!(matches!(env.service.submit_user_message(request(1)).await, Err(GatewayError::Unavailable)));
    }

    #[tokio::test]
    async fn shutdown_releases_reservation_and_keepawake() {
        let env = env();
        env.executor.blocked.store(true, Ordering::SeqCst);
        let accepted = env.service.submit_user_message(request(1)).await.unwrap();
        env.executor.started.notified().await;
        assert_eq!(env.service.inner.deps.keep.holders(), 1);
        env.service.shutdown().await.unwrap();
        assert!(accepted.completion.await.is_err());
        assert_eq!(env.service.inner.deps.keep.holders(), 0);
        assert!(env.service.inner.deps.manager.reserve_turn(&accepted.chat_id, EngineId::Claude).is_ok());
        assert!(env.service.status_snapshot().await.unwrap().active_turns.is_empty());
    }

    #[tokio::test]
    async fn local_home_uses_scratch_and_local_origin_is_not_remote_capability() {
        let env = env();
        let mut local = request(1);
        local.origin = TurnOrigin::Local;
        local.cwd = env.service.inner.deps.user_home.clone();
        let accepted = env.service.submit_local_message(local).unwrap();
        accepted.completion.await.unwrap().unwrap();
        assert_eq!(env.executor.seen.lock().unwrap()[0].1, env.service.inner.deps.paths.root.join("workspaces").join(&accepted.chat_id));
        let mut local = request(2);
        local.origin = TurnOrigin::Local;
        assert!(matches!(env.service.submit_user_message(local).await, Err(GatewayError::Unauthorized)));
        assert_eq!(env.service.resolve_local_approval(crate::telegram::control::ControlRole::Requester, "missing", true).await, Err(GatewayError::Unauthorized));
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_engine_is_rejected_before_acceptance() {
        let mut env = env();
        Arc::get_mut(&mut env.service.inner).unwrap().deps.config.engines.retain(|engine| engine.id != EngineId::Agy);
        let mut request = request(1);
        request.engine = Some(EngineId::Agy);
        assert!(matches!(env.service.submit_user_message(request).await, Err(GatewayError::Unavailable)));
        assert!(env.db.telegram_inbox_entry(999, 1).unwrap().is_none());
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bot_rotation_separates_update_offsets() {
        let env = env();
        let first = env.service.submit_user_message(request(1)).await.unwrap();
        first.completion.await.unwrap().unwrap();
        env.service.set_bot_identity(Some(1000)).unwrap();
        let second = env.service.submit_user_message(request(1)).await.unwrap();
        second.completion.await.unwrap().unwrap();
        assert!(env.db.telegram_inbox_entry(999, 1).unwrap().is_some());
        assert!(env.db.telegram_inbox_entry(1000, 1).unwrap().is_some());
        assert_eq!(env.executor.calls.load(Ordering::SeqCst), 2);
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quota_attempt_does_not_close_logical_turn_delivery_boundary() {
        let env = env();
        env.executor.quota.store(true, Ordering::SeqCst);
        let accepted = env.service.submit_user_message(request(1)).await.unwrap();
        let result = accepted.completion.await.unwrap().unwrap();
        assert_eq!(result.outcome, CompletionOutcome::QuotaExhausted);
        let origin = env.db.telegram_turn_origin(&accepted.logical_turn_id).unwrap().unwrap();
        assert_eq!(origin.end_seq, None);
        assert_eq!(origin.delivery_state, ultron_core::db::telegram::DeliveryState::Pending);
        assert_eq!(env.db.telegram_inbox_entry(999, 1).unwrap().unwrap().state, ultron_core::db::telegram::InboxState::Running);
        env.service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn existing_local_cwd_is_preserved_and_unknown_restart_cwd_fails_closed() {
        let env = env();
        let mut local = request(1);
        local.cwd = env.project.clone();
        let first = env.service.submit_local_message(local).unwrap();
        first.completion.await.unwrap().unwrap();
        env.service.set_binding(101, Some(&first.chat_id)).await.unwrap();
        let second = env.service.submit_user_message(request(2)).await.unwrap();
        second.completion.await.unwrap().unwrap();
        assert_eq!(env.executor.seen.lock().unwrap()[1].1, env.project);
        env.service.inner.contexts.lock().unwrap().clear();
        env.db.upsert_session(&ultron_core::db::SessionRow { id: "synthetic-session".into(), chat_id: first.chat_id, engine: EngineId::Claude, native_session_id: None, status: ultron_core::SessionStatus::Closed, started_at: Utc::now(), ended_at: Some(Utc::now()) }).unwrap();
        assert!(matches!(env.service.submit_user_message(request(3)).await, Err(GatewayError::NeedsAttention)));
        assert!(env.db.telegram_inbox_entry(999, 3).unwrap().is_none());
        env.service.shutdown().await.unwrap();
    }
}
