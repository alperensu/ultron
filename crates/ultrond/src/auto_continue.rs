//! Kalıcı kota otomatik devam durum makinesi ve zamanlayıcısı (Task 8).
//!
//! Gerçeğin kaynağı SQLite `quota_waits` kaydıdır; bellekteki zamanlayıcılar yalnız onun
//! bir gölgesidir. Her uyanışta kayıt yeniden okunur ve nesil (generation) CAS ile
//! sahiplenilir; bu yüzden timer tekrarında, saat atlamasında veya iki zamanlayıcı
//! yarışında aynı iş iki kez çalışmaz.
//!
//! Geçiş tablosu (DB durumları; zamanlayıcı yalnız `waiting` için kurulur):
//!
//! | Durum | Olay | Sonraki |
//! | --- | --- | --- |
//! | (yok) | kota tükendi, reset bilinen ve pencere içinde | `waiting` + timer (reset + tolerans) |
//! | (yok) | reset geçmişte | `waiting` + hemen tek deneme |
//! | (yok) | reset yok/pencere dışı, uygun fallback var | `waiting` + hemen fallback devri |
//! | (yok) | reset yok/pencere dışı, fallback yok/tükenmiş | `waiting`, timer ve keep-awake yok |
//! | (yok) | Full/yerel onay gerektiren iş veya bozuk kaynak | `needs_attention` |
//! | `waiting` | timer uyandı, nesil/ayar/plan hâlâ geçerli, claim kazanıldı | `claimed` → `running` |
//! | `waiting` | nesil değişti (Cancel/yeni mesaj/switch) | timer düşer, çalışmaz |
//! | `running` | tur tamamlandı | `completed` |
//! | `running` | yine kota | `waiting` (üstel backoff, motor sırası ilerler) veya sınır aşılırsa `needs_attention` |
//! | `running` | başarısız/belirsiz | `needs_attention` |
//! | `claimed`/`running` | çökme | restart'ta `needs_attention` (DB), körlemesine tekrar yok |
//!
//! Yetki: devam yalnız `ResumeQuotaStep { wait_id, generation }` taşır. Eski onay,
//! `TurnGrant` veya Full yükseltmesi taşınmaz; yeni riskli araç adımları normal
//! `Approvals::check_owned` yolunda bekler ve süre dolunca reddedilir.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration as StdDuration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use ultron_core::config::TelegramConfig;
use ultron_core::db::quota_wait::{
    NewQuotaWait, QuotaWait, QuotaWaitProgress, QuotaWaitSchedule, QuotaWaitState, ResumePolicy,
};
use ultron_core::events::{EngineEvent, QuotaWaitReason};
use ultron_core::ipc::TelegramSettings;
use ultron_core::quota::{Pool, QuotaReport, QuotaService, QuotaSource};
use ultron_core::{Config, Db, EngineId, EventLog, Paths, StoredEvent};

use crate::keepawake::{KeepAwake, KeepAwakeGuard, KeepAwakeReason};
use crate::telegram::contracts::{
    AcceptedTurn, BoxFuture, Clock, CompletionOutcome, GatewayApplication, GatewayError,
    ResumeQuotaStep,
};
use crate::turn_service::TurnService;

/// Reset anından sonra eklenen sabit tolerans; sağlayıcı saat farkını karşılar.
const RESET_GRACE_SECS: i64 = 5;
/// Reset toleransına eklenen kayıt kimliğinden türetilen 0..5 sn sapma.
const MAX_JITTER_SECS: u64 = 5;
/// Uyku parçası: her parçadan sonra UTC saat yeniden okunur (saat atlamasına dayanıklı).
const MAX_SLEEP_CHUNK: StdDuration = StdDuration::from_secs(30);
const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_MAX_SECS: i64 = 900;
/// Bir iş en çok bu kadar claim yapılır; sonra `needs_attention`.
pub const MAX_ATTEMPTS: i64 = 6;

/// Aynı adımı sürdüren yürütücü. Üretimde `TurnService`.
pub trait QuotaResumer: Send + Sync {
    /// `engine = None` kaynak motorda sürdürür, `Some` fallback devridir.
    fn resume<'a>(
        &'a self,
        step: ResumeQuotaStep,
        engine: Option<EngineId>,
    ) -> BoxFuture<'a, Result<AcceptedTurn, GatewayError>>;
}

impl QuotaResumer for TurnService {
    fn resume<'a>(
        &'a self,
        step: ResumeQuotaStep,
        engine: Option<EngineId>,
    ) -> BoxFuture<'a, Result<AcceptedTurn, GatewayError>> {
        Box::pin(async move {
            match engine {
                Some(engine) => self.resume_quota_step_with_engine(step, engine),
                None => GatewayApplication::resume_quota_step(self, step).await,
            }
        })
    }
}

/// Attempt reset'i yoksa motorun dolmuş kota pencerelerinden en geç açılış saatini verir.
pub trait ResetProbe: Send + Sync {
    fn blocking_reset<'a>(
        &'a self,
        engine: EngineId,
        now: DateTime<Utc>,
    ) -> BoxFuture<'a, Option<DateTime<Utc>>>;
}

/// Pencere bilgisi kullanılmaz; reset bilinmiyor sayılır.
pub struct NoResetProbe;

impl ResetProbe for NoResetProbe {
    fn blocking_reset<'a>(
        &'a self,
        _engine: EngineId,
        _now: DateTime<Utc>,
    ) -> BoxFuture<'a, Option<DateTime<Utc>>> {
        Box::pin(async { None })
    }
}

/// `/limit` ile aynı `QuotaService` kaynağı. Reset hakkı tüketilmez.
pub struct QuotaServiceProbe {
    service: Arc<QuotaService>,
    paths: Paths,
}

impl QuotaServiceProbe {
    pub fn new(service: Arc<QuotaService>, paths: Paths) -> Self {
        Self { service, paths }
    }
}

impl ResetProbe for QuotaServiceProbe {
    fn blocking_reset<'a>(
        &'a self,
        engine: EngineId,
        now: DateTime<Utc>,
    ) -> BoxFuture<'a, Option<DateTime<Utc>>> {
        Box::pin(async move {
            let cfg = Config::load(&self.paths).ok()?;
            let report = self.service.report(&cfg, now).await;
            blocking_reset_from_report(&report, engine, now)
        })
    }
}

/// Aynı motorun dolmuş (>= %100) ve ölçümü bilinen pencerelerinden hepsinin açılacağı en
/// geç saat. Herhangi bir bloklayıcı pencerenin reset'i bilinmiyorsa kesin saat yoktur.
/// Agy ölçümü Unknown olduğundan hiçbir zaman saat üretmez.
pub fn blocking_reset_from_report(
    report: &QuotaReport,
    engine: EngineId,
    _now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let pool = match engine {
        EngineId::Claude => Pool::Claude,
        EngineId::Codex => Pool::Codex,
        EngineId::Agy => return None,
    };
    let mut latest = None;
    let mut blocking = 0;
    for window in report
        .pools
        .iter()
        .filter(|p| p.pool == pool)
        .flat_map(|p| p.windows.iter())
        .filter(|w| w.source != QuotaSource::Unknown && w.used_pct.is_some_and(|p| p >= 100.0))
    {
        blocking += 1;
        let reset = window.resets_at?;
        latest = Some(latest.map_or(reset, |known: DateTime<Utc>| known.max(reset)));
    }
    if blocking == 0 { None } else { latest }
}

/// Reset'i `quota_exhausted` olayından okur (bozuksa `None`; saat uydurulmaz).
pub fn reset_from_quota_event(event: &StoredEvent) -> Option<DateTime<Utc>> {
    match serde_json::from_value::<EngineEvent>(event.payload.clone()) {
        Ok(EngineEvent::QuotaExhausted { resets_at, .. }) => resets_at,
        _ => None,
    }
}

/// Ayar anlık görüntüsü; kilit `await` boyunca tutulmaz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub enabled: bool,
    pub window: Duration,
    pub fallback_order: Vec<EngineId>,
    /// Yapılandırmada etkin ve kayıtlı motorlar.
    pub eligible: Vec<EngineId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Kaynak motor, aynı yerel oturum.
    SameEngine,
    /// `fallback_order[index]` motoruna devir.
    Fallback { index: i64, engine: EngineId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// `at` (geçmişse hemen) tek deneme yap.
    Run {
        at: DateTime<Utc>,
        target: Target,
        reason: QuotaWaitReason,
    },
    /// Zamanlayıcı yok; `/durum` bu nedeni gösterir.
    Hold { reason: QuotaWaitReason },
}

fn jitter_secs(id: &str) -> i64 {
    let hash = id
        .bytes()
        .fold(0_u64, |acc, b| acc.wrapping_mul(31).wrapping_add(u64::from(b)));
    i64::try_from(hash % MAX_JITTER_SECS).unwrap_or(0)
}

/// `n`. denemeden sonraki bekleme: 60 sn, 120, 240, 480, 900 (üst sınır).
pub fn backoff(attempts: i64) -> Duration {
    let exp = u32::try_from((attempts - 1).clamp(0, 4)).unwrap_or(4);
    Duration::seconds((BACKOFF_BASE_SECS << exp).min(BACKOFF_MAX_SECS))
}

/// Saf karar işlevi: saat/ayar/kayıt -> plan. `now < reset <= now + pencere` (45:00 dahil)
/// zamanlayıcıdır; geçmiş reset hemen tek denemedir; aksi halde fallback, yoksa bekleme.
pub fn decide(wait: &QuotaWait, snap: &PolicySnapshot, now: DateTime<Utc>) -> Plan {
    decide_scheduled(wait, snap, now, &QuotaWaitSchedule::default())
}

fn decide_scheduled(
    wait: &QuotaWait,
    snap: &PolicySnapshot,
    now: DateTime<Utc>,
    schedule: &QuotaWaitSchedule,
) -> Plan {
    if wait.state != QuotaWaitState::Waiting {
        return Plan::Hold {
            reason: QuotaWaitReason::NeedsAttention,
        };
    }
    if wait.policy == ResumePolicy::RequiresLocalApproval {
        return Plan::Hold {
            reason: QuotaWaitReason::LocalApprovalRequired,
        };
    }
    if !snap.enabled {
        return Plan::Hold {
            reason: QuotaWaitReason::Disabled,
        };
    }
    let floor = wait.next_attempt_at;
    if let Some(reset) = wait.reset_at
        && reset <= now + snap.window
        && snap.eligible.contains(&wait.engine)
    {
        let past = reset <= now;
        let base = if let Some(deadline) = schedule.reset_deadline.filter(|_| schedule.planned_reset_at == wait.reset_at) {
            deadline
        } else if past {
            now
        } else {
            reset + Duration::seconds(RESET_GRACE_SECS + jitter_secs(&wait.id))
        };
        let (at, reason) = match floor {
            Some(f) if f > base => (f, QuotaWaitReason::RetryBackoff),
            _ if past => (base, QuotaWaitReason::ResetReached),
            _ => (base, QuotaWaitReason::ResetScheduled),
        };
        return Plan::Run {
            at,
            target: Target::SameEngine,
            reason,
        };
    }
    let start = usize::try_from(wait.next_engine_index.max(0)).unwrap_or(usize::MAX);
    let candidate = schedule
        .fallback_order.as_ref().unwrap_or(&snap.fallback_order)
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, e)| **e != wait.engine && snap.eligible.contains(e) && snap.fallback_order.contains(e));
    if let Some((index, engine)) = candidate {
        let at = floor.map_or(now, |f| f.max(now));
        return Plan::Run {
            at,
            target: Target::Fallback {
                index: i64::try_from(index).unwrap_or(i64::MAX),
                engine: *engine,
            },
            reason: QuotaWaitReason::FallbackSelected,
        };
    }
    let reason = if !snap.fallback_order.is_empty() {
        QuotaWaitReason::FallbackExhausted
    } else if wait.reset_at.is_some() {
        QuotaWaitReason::ResetBeyondWait
    } else {
        QuotaWaitReason::ResetUnknown
    };
    Plan::Hold { reason }
}

/// Tükenen bir tur denemesinin kalıcı kaydı için gerekenler. Metin/araç girdisi taşınmaz;
/// hedef `user_event_seq` olay referansından yüklenir.
pub struct QuotaExhaustedAttempt {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub source_session_id: String,
    pub engine: EngineId,
    pub user_event_seq: i64,
    /// `SessionAttemptResult::end_seq`.
    pub checkpoint_seq: i64,
    pub cwd: std::path::PathBuf,
    pub policy: ResumePolicy,
    /// `reset_from_quota_event(&attempt.quota_event)`; yoksa kota penceresine bakılır.
    pub reset_at: Option<DateTime<Utc>>,
}

/// Bellekteki zamanlayıcının/izlenen beklemenin bildirim için gereken özeti.
#[derive(Clone)]
struct Tracked {
    id: String,
    chat_id: String,
    session_id: String,
    logical_turn_id: String,
    engine: EngineId,
    reset_at: Option<DateTime<Utc>>,
    generation: i64,
}

struct TimerEntry {
    identity: Arc<()>,
    generation: i64,
    at: DateTime<Utc>,
    abort: AbortHandle,
}

pub struct AutoContinueDeps {
    pub db: Arc<Db>,
    pub log: Arc<EventLog>,
    pub resumer: Arc<dyn QuotaResumer>,
    /// `TurnService` ile paylaşılan canlı ayar.
    pub settings: Arc<RwLock<TelegramConfig>>,
    /// `TelegramControl::subscribe_settings()`; ayar değişince yeniden planlanır.
    pub settings_updates: Option<watch::Receiver<TelegramSettings>>,
    /// Config'te etkin motorlar.
    pub eligible_engines: Vec<EngineId>,
    pub clock: Arc<dyn Clock>,
    pub keep: KeepAwake,
    pub probe: Arc<dyn ResetProbe>,
}

struct Inner {
    db: Arc<Db>,
    log: Arc<EventLog>,
    resumer: Arc<dyn QuotaResumer>,
    settings: Arc<RwLock<TelegramConfig>>,
    eligible: Vec<EngineId>,
    clock: Arc<dyn Clock>,
    keep: KeepAwake,
    probe: Arc<dyn ResetProbe>,
    updates: Mutex<Option<watch::Receiver<TelegramSettings>>>,
    timers: Mutex<HashMap<String, TimerEntry>>,
    tracked: Mutex<HashMap<String, Tracked>>,
    tasks: Mutex<Option<JoinSet<()>>>,
    started: AtomicBool,
    cancel: CancellationToken,
    #[cfg(test)]
    timer_barrier: Mutex<Option<Arc<TimerBarrier>>>,
}

#[cfg(test)]
struct TimerBarrier {
    ready: std::sync::Barrier,
    release: std::sync::Barrier,
}

enum ResumeStart {
    Accepted(AcceptedTurn),
    Disabled,
}

/// Başlatma raporu (restart uzlaştırması).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartReport {
    /// Restart'ta `needs_attention`a alınan claimed/running/bozuk kayıt sayısı.
    pub needs_attention: usize,
    /// Kurulan timer sayısı.
    pub timers: usize,
    /// Zamanlayıcısız (bilinmeyen reset, devre dışı vb.) bekleyen iş sayısı.
    pub holding: usize,
}

#[derive(Clone)]
pub struct AutoContinue {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl AutoContinue {
    pub fn new(deps: AutoContinueDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: deps.db,
                log: deps.log,
                resumer: deps.resumer,
                settings: deps.settings,
                eligible: deps.eligible_engines,
                clock: deps.clock,
                keep: deps.keep,
                probe: deps.probe,
                updates: Mutex::new(deps.settings_updates),
                timers: Mutex::new(HashMap::new()),
                tracked: Mutex::new(HashMap::new()),
                tasks: Mutex::new(Some(JoinSet::new())),
                started: AtomicBool::new(false),
                cancel: CancellationToken::new(),
                #[cfg(test)]
                timer_barrier: Mutex::new(None),
            }),
        }
    }

    /// Restart uzlaştırması ve ayar izleyicisi. Tek sefer çağrılır. Çağıran önce eski
    /// onayları expire etmeli ve yarım running turları interrupted yapmalıdır.
    /// Claimed/running kalan iş körlemesine tekrar başlamaz: `needs_attention`.
    pub fn start(&self) -> Result<StartReport> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            bail!("otomatik devam zaten başlatıldı");
        }
        let now = self.inner.clock.now_utc();
        let before: HashMap<String, QuotaWaitState> = self
            .inner
            .db
            .pending_quota_waits()?
            .into_iter()
            .map(|w| (w.id, w.state))
            .collect();
        self.inner.db.recover_quota_waits(now)?;
        let after = self.inner.db.pending_quota_waits()?;
        let mut report = StartReport {
            needs_attention: 0,
            timers: 0,
            holding: 0,
        };
        for wait in &after {
            match wait.state {
                QuotaWaitState::NeedsAttention
                    if before
                        .get(&wait.id)
                        .is_some_and(|s| *s != QuotaWaitState::NeedsAttention) =>
                {
                    report.needs_attention += 1;
                    self.emit_stopped(&tracked_of(wait), QuotaWaitReason::NeedsAttention);
                }
                QuotaWaitState::Waiting => match self.schedule(wait) {
                    Ok(Plan::Run { .. }) => report.timers += 1,
                    Ok(Plan::Hold { .. }) => report.holding += 1,
                    Err(error) => tracing::warn!(%error, "kota beklemesi kurulamadı"),
                },
                _ => {}
            }
        }
        self.spawn_settings_watcher()?;
        Ok(report)
    }

    /// Kapanış: tüm timer/izleyici görevleri iptal edilir (guard'lar bırakılır).
    /// Kalıcı kayıtlar korunur; sonraki açılış `start` ile devralır.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let tasks = lock(&self.inner.tasks).take();
        if let Some(mut tasks) = tasks {
            tasks.shutdown().await;
        }
        lock(&self.inner.timers).clear();
        lock(&self.inner.tracked).clear();
    }

    /// Etkin timer sayısı (her biri bir keep-awake guard taşır). `Status` alanı içindir;
    /// çalışan tur sayısı değildir.
    pub fn timer_count(&self) -> usize {
        lock(&self.inner.timers).len()
    }

    /// Normal bir turun sonucu `QuotaExhausted` olduğunda çağrılır. Aynı mantıksal tur
    /// için bekleyen kayıt varsa onu döndürür (devam denemesinin yeniden tükenmesi
    /// bu yoldan değil, zamanlayıcının kendi uzlaştırmasıyla işlenir).
    pub async fn register_exhausted(&self, attempt: QuotaExhaustedAttempt) -> Result<QuotaWait> {
        let reset_at = self.resolve_reset(&attempt).await;
        self.register_resolved(attempt, reset_at).await
    }

    /// Reset saatini (varsa attempt'ten, yoksa probe ile) çözer. Canlı HTTP yapabilir;
    /// çağıran global kilit tutmadan çağırmalıdır.
    pub async fn resolve_reset(&self, attempt: &QuotaExhaustedAttempt) -> Option<DateTime<Utc>> {
        match attempt.reset_at {
            Some(reset) => Some(reset),
            None => self.inner.probe.blocking_reset(attempt.engine, self.inner.clock.now_utc()).await,
        }
    }

    /// `register_exhausted`un probe yapmayan hali: `reset_at` önceden `resolve_reset` ile çözülmüştür.
    pub async fn register_resolved(&self, attempt: QuotaExhaustedAttempt, reset_at: Option<DateTime<Utc>>) -> Result<QuotaWait> {
        if let Some(existing) = self
            .inner
            .db
            .pending_quota_waits()?
            .into_iter()
            .find(|w| w.logical_turn_id == attempt.logical_turn_id)
        {
            return Ok(existing);
        }
        let now = self.inner.clock.now_utc();
        let wait = self.inner.db.create_quota_wait(
            &NewQuotaWait {
                id: uuid::Uuid::new_v4().simple().to_string(),
                logical_turn_id: attempt.logical_turn_id,
                chat_id: attempt.chat_id,
                source_session_id: attempt.source_session_id,
                engine: attempt.engine,
                user_event_seq: attempt.user_event_seq,
                checkpoint_seq: attempt.checkpoint_seq,
                cwd: attempt.cwd,
                policy: attempt.policy,
                reset_at,
                next_attempt_at: None,
            },
            now,
        )?;
        if wait.state != QuotaWaitState::Waiting {
            self.emit_stopped(&tracked_of(&wait), QuotaWaitReason::NeedsAttention);
            return Ok(wait);
        }
        let snap = self.snapshot()?;
        let reason = match decide(&wait, &snap, now) {
            Plan::Run { reason, .. } | Plan::Hold { reason } => reason,
        };
        // Olay yazılamıyorsa iş sessizce başlamaz: kayıt dikkat bekler.
        if let Err(error) = self.append(&tracked_of(&wait), |t| EngineEvent::QuotaWaitStarted {
            logical_turn_id: t.logical_turn_id.clone(),
            engine: t.engine,
            resets_at: t.reset_at,
            reason,
        }) {
            let _ = self.inner.db.advance_quota_wait(
                &wait.id,
                wait.generation,
                QuotaWaitState::NeedsAttention,
                None,
                now,
            );
            return Err(error.context("kota bekleme olayı yazılamadı"));
        }
        self.schedule(&wait)?;
        self.inner
            .db
            .quota_wait(&wait.id)?
            .ok_or_else(|| anyhow!("bekleme kaydı yok"))
    }

    /// Cancel yolu: sohbetin bekleyen devamlarını kalıcı iptal eder, timer'ları düşürür.
    pub fn cancel_chat(&self, chat_id: &str) -> Result<usize> {
        let count = self
            .inner
            .db
            .cancel_quota_waits(chat_id, self.inner.clock.now_utc())?;
        self.settle(Some(chat_id), QuotaWaitReason::Cancelled);
        Ok(count)
    }

    /// Yeni kullanıcı mesajı (`append_user_event_invalidating_quota`) veya motor switch'i
    /// DB'de nesli zaten geçersiz kıldıktan sonra çağrılır: timer'lar düşer ve
    /// `QuotaWaitStopped(Superseded)` yazılır. Geç kalan timer nesil CAS'inde zaten elenir.
    pub fn on_chat_superseded(&self, chat_id: &str) {
        self.settle(Some(chat_id), QuotaWaitReason::Superseded);
    }

    /// Ayar değişince veya dış bir değişiklik şüphesinde: DB'den yeniden hizala.
    pub fn reconcile(&self) -> Result<()> {
        self.settle(None, QuotaWaitReason::Superseded);
        for wait in self.inner.db.pending_quota_waits()? {
            if wait.state == QuotaWaitState::Waiting
                && let Err(error) = self.schedule(&wait)
            {
                tracing::warn!(%error, "kota beklemesi yeniden planlanamadı");
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<PolicySnapshot> {
        let settings = self
            .inner
            .settings
            .read()
            .map_err(|_| anyhow!("telegram ayarı kilidi zehirlendi"))?;
        Ok(PolicySnapshot {
            enabled: settings.auto_continue_enabled,
            window: Duration::minutes(i64::from(settings.auto_continue_wait_minutes)),
            fallback_order: settings.fallback_order.clone(),
            eligible: self.inner.eligible.clone(),
        })
    }

    fn append(
        &self,
        t: &Tracked,
        event: impl FnOnce(&Tracked) -> EngineEvent,
    ) -> Result<()> {
        self.inner
            .log
            .append(&t.chat_id, Some(&t.session_id), event(t))
            .map(|_| ())
    }

    fn emit_stopped(&self, t: &Tracked, reason: QuotaWaitReason) {
        if let Err(error) = self.append(t, |t| EngineEvent::QuotaWaitStopped {
            logical_turn_id: t.logical_turn_id.clone(),
            engine: t.engine,
            resets_at: t.reset_at,
            reason,
        }) {
            tracing::warn!(%error, "kota durdurma olayı yazılamadı");
        }
    }

    fn emit_started(&self, wait: &QuotaWait, reason: QuotaWaitReason) {
        if let Err(error) = self.append(&tracked_of(wait), |t| EngineEvent::QuotaWaitStarted {
            logical_turn_id: t.logical_turn_id.clone(),
            engine: t.engine,
            resets_at: t.reset_at,
            reason,
        }) {
            tracing::warn!(%error, "kota bekleme olayı yazılamadı");
        }
    }

    fn drop_timer(&self, id: &str) -> bool {
        match lock(&self.inner.timers).remove(id) {
            Some(entry) => {
                entry.abort.abort();
                true
            }
            None => false,
        }
    }

    /// Bu kaydı planlar. Aynı nesil ve aynı an için tekrar çağrı etkisizdir.
    fn schedule(&self, wait: &QuotaWait) -> Result<Plan> {
        let now = self.inner.clock.now_utc();
        let snap = self.snapshot()?;
        let Some(plan) = self.persisted_plan(wait, &snap, now)? else {
            return Ok(Plan::Hold { reason: QuotaWaitReason::Superseded });
        };
        lock(&self.inner.tracked).insert(wait.id.clone(), tracked_of(wait));
        match plan {
            Plan::Hold { reason } => {
                if self.drop_timer(&wait.id) && reason == QuotaWaitReason::Disabled {
                    self.emit_stopped(&tracked_of(wait), QuotaWaitReason::Disabled);
                }
            }
            Plan::Run { at, .. } => {
                let mut timers = lock(&self.inner.timers);
                if timers
                    .get(&wait.id)
                    .is_some_and(|t| t.generation == wait.generation && t.at == at)
                {
                    return Ok(plan);
                }
                let mut tasks = lock(&self.inner.tasks);
                let tasks = tasks
                    .as_mut()
                    .ok_or_else(|| anyhow!("otomatik devam kapatıldı"))?;
                while tasks.try_join_next().is_some() {}
                if let Some(old) = timers.remove(&wait.id) {
                    old.abort.abort();
                }
                // Somut bir zamanlayıcı beklerken (kısa, sınırlı) uyanık tutulur.
                let guard = self.inner.keep.acquire_for(KeepAwakeReason::QuotaTimer);
                let this = self.clone();
                let (id, generation) = (wait.id.clone(), wait.generation);
                let identity = Arc::new(());
                let timer_identity = identity.clone();
                let abort = tasks.spawn(async move { this.run_timer(id, generation, timer_identity, at, guard).await });
                timers.insert(
                    wait.id.clone(),
                    TimerEntry {
                        identity,
                        generation: wait.generation,
                        at,
                        abort,
                    },
                );
            }
        }
        Ok(plan)
    }

    fn persisted_plan(&self, wait: &QuotaWait, snap: &PolicySnapshot, now: DateTime<Utc>) -> Result<Option<Plan>> {
        let deadline = wait.reset_at.filter(|reset| *reset > now)
            .map(|reset| reset + Duration::seconds(RESET_GRACE_SECS + jitter_secs(&wait.id)));
        let schedule = self.inner.db.prepare_quota_wait_schedule(
            &wait.id, wait.generation, &snap.fallback_order, deadline,
        )?;
        Ok(schedule.map(|schedule| decide_scheduled(wait, snap, now, &schedule)))
    }

    /// Düşmüş (nesli değişmiş/iptal edilmiş) izlenen beklemeleri temizler ve bildirir.
    fn settle(&self, chat_id: Option<&str>, cancelled_reason: QuotaWaitReason) {
        let entries: Vec<Tracked> = lock(&self.inner.tracked)
            .values()
            .filter(|t| chat_id.is_none_or(|c| t.chat_id == c))
            .cloned()
            .collect();
        for tracked in entries {
            let current = self.inner.db.quota_wait(&tracked.id).ok().flatten();
            let gone = current
                .as_ref()
                .is_none_or(|w| w.state != QuotaWaitState::Waiting || w.generation != tracked.generation);
            if !gone {
                continue;
            }
            self.drop_timer(&tracked.id);
            lock(&self.inner.tracked).remove(&tracked.id);
            match current {
                Some(w) if w.state == QuotaWaitState::Cancelled => {
                    self.emit_stopped(&tracked, cancelled_reason);
                }
                Some(w) if w.state == QuotaWaitState::NeedsAttention => {
                    self.emit_stopped(&tracked, QuotaWaitReason::NeedsAttention);
                }
                Some(w) if w.state == QuotaWaitState::Waiting => {
                    let _ = self.schedule(&w);
                }
                _ => {}
            }
        }
    }

    fn spawn_settings_watcher(&self) -> Result<()> {
        let Some(mut updates) = lock(&self.inner.updates).take() else {
            return Ok(());
        };
        let mut tasks = lock(&self.inner.tasks);
        let tasks = tasks
            .as_mut()
            .ok_or_else(|| anyhow!("otomatik devam kapatıldı"))?;
        let this = self.clone();
        let cancel = self.inner.cancel.clone();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    changed = updates.changed() => {
                        if changed.is_err() { break; }
                        if let Err(error) = this.reconcile() {
                            tracing::warn!(%error, "kota beklemeleri ayar değişiminde uzlaştırılamadı");
                        }
                    }
                }
            }
        });
        Ok(())
    }

    async fn run_timer(self, id: String, generation: i64, identity: Arc<()>, at: DateTime<Utc>, guard: KeepAwakeGuard) {
        loop {
            let now = self.inner.clock.now_utc();
            if now >= at {
                break;
            }
            let remaining = (at - now).to_std().unwrap_or_default().min(MAX_SLEEP_CHUNK);
            let deadline = self.inner.clock.now_monotonic() + remaining;
            self.inner.clock.sleep_until(deadline).await;
        }
        #[cfg(test)]
        {
            let barrier = lock(&self.inner.timer_barrier).take();
            if let Some(barrier) = barrier {
                barrier.ready.wait();
                barrier.release.wait();
            }
        }
        // Timer iptal edilmişken bu senkron bölüme yine girilebilir. Kimlik eşleşmeden
        // claim yapılmaz; eşleşince yürütme ayrı göreve devredilir. Böylece reconcile
        // yalnız bekleyen timer'ı abort edebilir, kabul edilmiş turun completion'ını değil.
        let mut timers = lock(&self.inner.timers);
        if self.inner.cancel.is_cancelled() || !timers.get(&id).is_some_and(|t| Arc::ptr_eq(&t.identity, &identity)) {
            return;
        }
        timers.remove(&id);
        let mut tasks = lock(&self.inner.tasks);
        if let Some(tasks) = tasks.as_mut() {
            let this = self.clone();
            tasks.spawn(async move { this.fire(&id, generation, guard).await });
        }
    }

    /// Uyanış: kayıt, ayar ve plan yeniden doğrulanır; claim CAS'i kazanılırsa tek deneme.
    async fn fire(&self, id: &str, generation: i64, guard: KeepAwakeGuard) {
        let wait = match self.inner.db.quota_wait(id) {
            Ok(Some(w)) if w.generation == generation && w.state == QuotaWaitState::Waiting => w,
            Ok(_) => {
                self.settle(None, QuotaWaitReason::Superseded);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "kota beklemesi okunamadı");
                return;
            }
        };
        let now = self.inner.clock.now_utc();
        let Ok(snap) = self.snapshot() else { return };
        let Ok(Some(plan)) = self.persisted_plan(&wait, &snap, now) else { return };
        let (target, reason) = match plan {
            Plan::Hold { .. } => {
                // Ayar kapandı/motor uygunsuz: kayıt kalır, timer ve guard bırakılır.
                let _ = self.schedule(&wait);
                return;
            }
            Plan::Run { at, .. } if at > now => {
                // Saat geriye atladı veya plan değişti: yeni timer kurulur, sonra guard düşer.
                let _ = self.schedule(&wait);
                return;
            }
            Plan::Run { target, reason, .. } => (target, reason),
        };
        let claimed = match self.inner.db.claim_quota_wait(id, generation, now) {
            Ok(Some(claimed)) => claimed,
            Ok(None) => {
                self.settle(None, QuotaWaitReason::Superseded);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "kota beklemesi sahiplenilemedi");
                return;
            }
        };
        lock(&self.inner.tracked).remove(id);
        let tracked = tracked_of(&claimed);
        let engine = match target {
            Target::SameEngine => {
                // Olay yazılamıyorsa motor sessizce başlamaz.
                let event_reason = if reason == QuotaWaitReason::RetryBackoff {
                    QuotaWaitReason::ResetReached
                } else {
                    reason
                };
                if let Err(error) = self.append(&tracked, |t| EngineEvent::QuotaResumed {
                    logical_turn_id: t.logical_turn_id.clone(),
                    engine: t.engine,
                    resets_at: t.reset_at,
                    reason: event_reason,
                }) {
                    tracing::warn!(%error, "kota devam olayı yazılamadı");
                    self.to_attention(&claimed);
                    return;
                }
                None
            }
            Target::Fallback { engine, .. } => Some(engine),
        };
        let step = ResumeQuotaStep {
            wait_id: claimed.id.clone(),
            generation: claimed.generation,
        };
        let accepted = match self.resume_attempt(step, engine).await {
            Ok(ResumeStart::Accepted(accepted)) => accepted,
            Ok(ResumeStart::Disabled) => {
                match self.inner.db.release_quota_wait_claim(id, claimed.generation, self.inner.clock.now_utc()) {
                    Ok(Some(waiting)) if waiting.state == QuotaWaitState::Waiting => {
                        self.emit_stopped(&tracked_of(&waiting), QuotaWaitReason::Disabled);
                        let _ = self.schedule(&waiting);
                    }
                    Ok(Some(other)) => self.emit_stopped(&tracked_of(&other), QuotaWaitReason::NeedsAttention),
                    Ok(None) => {},
                    Err(error) => tracing::warn!(%error, "kota claim'i beklemeye bırakılamadı"),
                }
                return;
            }
            Err(GatewayError::StaleGeneration) => {
                self.settle(None, QuotaWaitReason::Superseded);
                return;
            }
            Err(GatewayError::Busy) => {
                // Sohbette başka tur çalışıyor: bu devam geçersizdir.
                self.finish_wait(&claimed, QuotaWaitState::Cancelled, QuotaWaitReason::Superseded);
                return;
            }
            Err(_) => {
                self.to_attention(&claimed);
                return;
            }
        };
        // Tur guard'ı artık TurnService'te; zamanlayıcı guard'ı geçişten sonra bırakılır.
        drop(guard);
        let result = accepted.completion.await;
        self.finish_attempt(
            id,
            match target {
                Target::Fallback { index, engine } => Some((index, engine)),
                Target::SameEngine => None,
            },
            result.ok().and_then(Result::ok),
        )
        .await;
    }

    /// Unavailable kabulden önce döner. Canlı ayar kapalıysa ayrı bir durdurma
    /// sonucudur; yalnız hâlâ Claimed olan nesil yeniden beklemeye alınabilir.
    async fn resume_attempt(&self, step: ResumeQuotaStep, engine: Option<EngineId>) -> Result<ResumeStart, GatewayError> {
        if !self.snapshot().map_err(|_| GatewayError::Unavailable)?.enabled {
            return Ok(ResumeStart::Disabled);
        }
        match self.inner.resumer.resume(step, engine).await {
            Ok(accepted) => Ok(ResumeStart::Accepted(accepted)),
            Err(GatewayError::Unavailable) if self.snapshot().is_ok_and(|snap| !snap.enabled) => Ok(ResumeStart::Disabled),
            Err(error) => Err(error),
        }
    }

    fn finish_wait(&self, wait: &QuotaWait, next: QuotaWaitState, reason: QuotaWaitReason) {
        let now = self.inner.clock.now_utc();
        match self
            .inner
            .db
            .advance_quota_wait(&wait.id, wait.generation, next, None, now)
        {
            Ok(Some(_)) => self.emit_stopped(&tracked_of(wait), reason),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "kota beklemesi kapatılamadı"),
        }
    }

    fn to_attention(&self, wait: &QuotaWait) {
        self.finish_wait(wait, QuotaWaitState::NeedsAttention, QuotaWaitReason::NeedsAttention);
    }

    /// Deneme bittikten sonra kalıcı durumu uzlaştırır. Yürütücü kendisi ilerlettiyse
    /// (Waiting/terminal) dokunmaz; yalnız Running nesli üzerinde CAS yapar.
    async fn finish_attempt(
        &self,
        id: &str,
        target: Option<(i64, EngineId)>,
        completion: Option<crate::telegram::contracts::TurnCompletion>,
    ) {
        let Ok(Some(current)) = self.inner.db.quota_wait(id) else {
            return;
        };
        match current.state {
            QuotaWaitState::Running => {}
            QuotaWaitState::Waiting => {
                let _ = self.schedule(&current);
                return;
            }
            QuotaWaitState::Claimed => {
                self.to_attention(&current);
                return;
            }
            QuotaWaitState::Cancelled => {
                self.emit_stopped(&tracked_of(&current), QuotaWaitReason::Cancelled);
                return;
            }
            QuotaWaitState::NeedsAttention => {
                // SessionManager cwd/araç belirsizliği hatasında terminal geçişi
                // completion'dan önce yapar; scheduler yine durdurma olayını borçludur.
                self.emit_stopped(&tracked_of(&current), QuotaWaitReason::NeedsAttention);
                return;
            }
            QuotaWaitState::Completed => return,
        }
        let Some(completion) = completion else {
            self.to_attention(&current);
            return;
        };
        let now = self.inner.clock.now_utc();
        let checkpoint_seq = completion.end_seq.max(current.checkpoint_seq);
        match completion.outcome {
            CompletionOutcome::Completed => {
                // Tamamlanma bildirimi turun kendi final yanıtıdır; ayrıca durdurma olayı yok.
                let progress = QuotaWaitProgress {
                    checkpoint_seq,
                    next_engine_index: current.next_engine_index,
                    reset_at: current.reset_at,
                    next_attempt_at: None,
                };
                if let Err(error) = self.inner.db.advance_quota_wait(
                    id,
                    current.generation,
                    QuotaWaitState::Completed,
                    Some(&progress),
                    now,
                ) {
                    tracing::warn!(%error, "kota beklemesi tamamlanamadı");
                }
                lock(&self.inner.tracked).remove(id);
            }
            CompletionOutcome::Cancelled => {
                self.finish_wait(&current, QuotaWaitState::Cancelled, QuotaWaitReason::Cancelled);
            }
            CompletionOutcome::Failed => self.to_attention(&current),
            CompletionOutcome::QuotaExhausted => {
                if current.attempt_count >= MAX_ATTEMPTS {
                    self.to_attention(&current);
                    return;
                }
                // Fallback denemesinin reset'i kaynak motorun reset'i değildir; eskisi korunur.
                let reset_at = if target.is_some() {
                    current.reset_at
                } else {
                    match self.quota_reset_in(&current.chat_id, completion.start_seq, completion.end_seq) {
                        Some(reset) => Some(reset),
                        None => self.inner.probe.blocking_reset(current.engine, now).await,
                    }
                };
                let progress = QuotaWaitProgress {
                    checkpoint_seq,
                    next_engine_index: target.map_or(current.next_engine_index, |(index, _)| index + 1),
                    reset_at,
                    next_attempt_at: Some(now + backoff(current.attempt_count)),
                };
                match self.inner.db.advance_quota_wait(
                    id,
                    current.generation,
                    QuotaWaitState::Waiting,
                    Some(&progress),
                    now,
                ) {
                    Ok(Some(waiting)) if waiting.state == QuotaWaitState::Waiting => {
                        if let Ok(snap) = self.snapshot() {
                            let reason = match decide(&waiting, &snap, now) {
                                Plan::Run { reason, .. } | Plan::Hold { reason } => reason,
                            };
                            self.emit_started(&waiting, reason);
                        }
                        if let Err(error) = self.schedule(&waiting) {
                            tracing::warn!(%error, "kota beklemesi yeniden planlanamadı");
                        }
                    }
                    Ok(Some(other)) => {
                        self.emit_stopped(&tracked_of(&other), QuotaWaitReason::NeedsAttention);
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "kota beklemesi ilerletilemedi"),
                }
            }
        }
    }

    /// Bu denemenin kalıcı olay aralığındaki son `quota_exhausted` reset'i.
    fn quota_reset_in(&self, chat_id: &str, start_seq: i64, end_seq: i64) -> Option<DateTime<Utc>> {
        let mut after = start_seq;
        let mut reset = None;
        loop {
            let page = self.inner.db.events_for_chat_page(chat_id, Some(after), 500).ok()?;
            if page.is_empty() {
                break;
            }
            for event in page {
                after = event.seq;
                if event.seq <= end_seq && event.kind == "quota_exhausted" {
                    reset = reset_from_quota_event(&event);
                }
            }
            if after >= end_seq {
                break;
            }
        }
        reset
    }
}

fn tracked_of(wait: &QuotaWait) -> Tracked {
    Tracked {
        id: wait.id.clone(),
        chat_id: wait.chat_id.clone(),
        session_id: wait.source_session_id.clone(),
        logical_turn_id: wait.logical_turn_id.clone(),
        engine: wait.engine,
        reset_at: wait.reset_at,
        generation: wait.generation,
    }
}

/// Dışarıya (Task 10 durum alanları) yardımcı: hangi beklemeler zamanlayıcısız.
pub fn holding_ids(waits: &[QuotaWait], snap: &PolicySnapshot, now: DateTime<Utc>) -> HashSet<String> {
    waits
        .iter()
        .filter(|w| w.state == QuotaWaitState::Waiting)
        .filter(|w| matches!(decide(w, snap, now), Plan::Hold { .. }))
        .map(|w| w.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::contracts::TurnCompletion;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use tokio::sync::oneshot;
    use ultron_core::db::{SessionRow, SessionStatus};

    // ---- sahte saat ------------------------------------------------------------
    struct FakeClock {
        utc: Mutex<DateTime<Utc>>,
        mono: Mutex<std::time::Instant>,
        tick: watch::Sender<u64>,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                utc: Mutex::new(Utc::now()),
                mono: Mutex::new(std::time::Instant::now()),
                tick: watch::channel(0).0,
            })
        }
        fn bump(&self) {
            self.tick.send_modify(|n| *n += 1);
        }
        /// Duvar ve monotonik saat birlikte ilerler.
        fn advance(&self, secs: i64) {
            *lock(&self.utc) += Duration::seconds(secs);
            *lock(&self.mono) += StdDuration::from_secs(u64::try_from(secs).unwrap());
            self.bump();
        }
        /// Yalnız duvar saati atlar (NTP/elle saat değişimi).
        fn jump_utc(&self, secs: i64) {
            *lock(&self.utc) += Duration::seconds(secs);
            self.bump();
        }
    }

    impl Clock for FakeClock {
        fn now_utc(&self) -> DateTime<Utc> {
            *lock(&self.utc)
        }
        fn now_monotonic(&self) -> std::time::Instant {
            *lock(&self.mono)
        }
        fn sleep_until(&self, deadline: std::time::Instant) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let mut rx = self.tick.subscribe();
                let _ = rx.wait_for(|_| self.now_monotonic() >= deadline).await;
            })
        }
    }

    // ---- sahte yürütücü ----------------------------------------------------------
    #[derive(Clone, Copy)]
    enum Script {
        Complete,
        DelayedComplete,
        Quota(Option<DateTime<Utc>>),
        Fail,
        ManagerFail(bool),
        Disable,
        Error(GatewayError),
    }

    type PendingCompletion = (oneshot::Sender<Result<TurnCompletion, GatewayError>>, TurnCompletion);

    struct FakeResumer {
        db: Arc<Db>,
        log: Arc<EventLog>,
        script: Mutex<VecDeque<Script>>,
        calls: Mutex<Vec<(String, i64, Option<EngineId>)>>,
        settings: Arc<RwLock<TelegramConfig>>,
        pending: Mutex<Option<PendingCompletion>>,
    }

    impl FakeResumer {
        fn calls(&self) -> usize {
            lock(&self.calls).len()
        }
    }

    impl QuotaResumer for FakeResumer {
        fn resume<'a>(
            &'a self,
            step: ResumeQuotaStep,
            engine: Option<EngineId>,
        ) -> BoxFuture<'a, Result<AcceptedTurn, GatewayError>> {
            Box::pin(async move {
                lock(&self.calls).push((step.wait_id.clone(), step.generation, engine));
                let script = lock(&self.script).pop_front().unwrap_or(Script::Complete);
                if let Script::Error(error) = script {
                    return Err(error);
                }
                if let Script::Disable = script {
                    self.settings.write().unwrap().auto_continue_enabled = false;
                    return Err(GatewayError::Unavailable);
                }
                // SessionManager::resume_quota_step'in yaptığı gibi claimed -> running.
                let wait = self.db.quota_wait(&step.wait_id).unwrap().unwrap();
                let next = if matches!(script, Script::ManagerFail(false)) {
                    QuotaWaitState::NeedsAttention
                } else {
                    QuotaWaitState::Running
                };
                self.db
                    .advance_quota_wait(&wait.id, step.generation, next, None, Utc::now())
                    .unwrap()
                    .ok_or(GatewayError::StaleGeneration)?;
                let start_seq = self.db.next_seq(&wait.chat_id).unwrap() - 1;
                let outcome = match script {
                    Script::Complete | Script::DelayedComplete => {
                        self.log.append(&wait.chat_id, None, EngineEvent::TurnCompleted).unwrap();
                        CompletionOutcome::Completed
                    }
                    Script::Quota(reset) => {
                        self.log
                            .append(&wait.chat_id, None, EngineEvent::QuotaExhausted { message: "sentetik".into(), resets_at: reset })
                            .unwrap();
                        CompletionOutcome::QuotaExhausted
                    }
                    Script::ManagerFail(true) => {
                        let current = self.db.quota_wait(&wait.id).unwrap().unwrap();
                        self.db.advance_quota_wait(&wait.id, current.generation, QuotaWaitState::NeedsAttention, None, Utc::now()).unwrap().unwrap();
                        self.log.append(&wait.chat_id, None, EngineEvent::Error { message: "belirsiz sentetik araç etkisi".into() }).unwrap();
                        CompletionOutcome::Failed
                    }
                    Script::Fail | Script::ManagerFail(false) | Script::Error(_) | Script::Disable => {
                        self.log.append(&wait.chat_id, None, EngineEvent::Error { message: "sentetik".into() }).unwrap();
                        CompletionOutcome::Failed
                    }
                };
                let end_seq = self.db.next_seq(&wait.chat_id).unwrap() - 1;
                let (tx, rx) = oneshot::channel();
                let completion = TurnCompletion {
                    logical_turn_id: wait.logical_turn_id.clone(),
                    chat_id: wait.chat_id.clone(),
                    start_seq,
                    end_seq,
                    outcome,
                    assistant_text: None,
                };
                if matches!(script, Script::DelayedComplete) {
                    *lock(&self.pending) = Some((tx, completion));
                } else {
                    let _ = tx.send(Ok(completion));
                }
                Ok(AcceptedTurn {
                    logical_turn_id: wait.logical_turn_id,
                    chat_id: wait.chat_id,
                    user_event_seq: wait.user_event_seq,
                    completion: rx,
                })
            })
        }
    }

    struct FixedProbe(Option<DateTime<Utc>>);
    impl ResetProbe for FixedProbe {
        fn blocking_reset<'a>(&'a self, _: EngineId, _: DateTime<Utc>) -> BoxFuture<'a, Option<DateTime<Utc>>> {
            let reset = self.0;
            Box::pin(async move { reset })
        }
    }

    // ---- ortam -----------------------------------------------------------------
    struct Env {
        _tmp: tempfile::TempDir,
        db: Arc<Db>,
        log: Arc<EventLog>,
        clock: Arc<FakeClock>,
        resumer: Arc<FakeResumer>,
        settings: Arc<RwLock<TelegramConfig>>,
        keep: KeepAwake,
        auto: AutoContinue,
        updates: watch::Sender<TelegramSettings>,
    }

    fn settings_of(config: &TelegramConfig) -> TelegramSettings {
        TelegramSettings::from(config)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        db: Arc<Db>,
        log: Arc<EventLog>,
        clock: Arc<FakeClock>,
        settings: Arc<RwLock<TelegramConfig>>,
        keep: KeepAwake,
        resumer: Arc<FakeResumer>,
        probe: Arc<dyn ResetProbe>,
        updates: &watch::Sender<TelegramSettings>,
    ) -> AutoContinue {
        AutoContinue::new(AutoContinueDeps {
            db,
            log,
            resumer,
            settings,
            settings_updates: Some(updates.subscribe()),
            eligible_engines: vec![EngineId::Claude, EngineId::Codex],
            clock,
            keep,
            probe,
        })
    }

    fn env_with(config: TelegramConfig, probe: Arc<dyn ResetProbe>) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let log = Arc::new(EventLog::new(db.clone(), paths));
        seed_chat(&db, &log, "chat1");
        let clock = FakeClock::new();
        let updates = watch::channel(settings_of(&config)).0;
        let settings = Arc::new(RwLock::new(config));
        let resumer = Arc::new(FakeResumer { db: db.clone(), log: log.clone(), script: Mutex::new(VecDeque::new()), calls: Mutex::new(Vec::new()), settings: settings.clone(), pending: Mutex::new(None) });
        let keep = KeepAwake::new();
        let auto = build(db.clone(), log.clone(), clock.clone(), settings.clone(), keep.clone(), resumer.clone(), probe, &updates);
        Env { _tmp: tmp, db, log, clock, resumer, settings, keep, auto, updates }
    }

    fn env() -> Env {
        env_with(TelegramConfig::default(), Arc::new(NoResetProbe))
    }

    /// Kullanıcı olayı seq 1, tükenme olayı seq 2.
    fn seed_chat(db: &Db, log: &EventLog, chat: &str) {
        db.insert_chat(chat, None, "sentetik", Utc::now()).unwrap();
        db.upsert_session(&SessionRow { id: format!("{chat}-session"), chat_id: chat.into(), engine: EngineId::Claude, native_session_id: None, status: SessionStatus::WaitingQuota, started_at: Utc::now(), ended_at: None }).unwrap();
        db.append_event(&StoredEvent { id: format!("{chat}-user"), chat_id: chat.into(), session_id: Some(format!("{chat}-session")), seq: 0, ts: Utc::now(), kind: "user_text".into(), payload: serde_json::json!({"type":"user_text","text":"Özgün sentetik hedef"}) }).unwrap();
        log.append(chat, Some(&format!("{chat}-session")), EngineEvent::QuotaExhausted { message: "sentetik".into(), resets_at: None }).unwrap();
    }

    fn attempt(chat: &str, turn: &str, reset: Option<DateTime<Utc>>, policy: ResumePolicy) -> QuotaExhaustedAttempt {
        QuotaExhaustedAttempt { logical_turn_id: turn.into(), chat_id: chat.into(), source_session_id: format!("{chat}-session"), engine: EngineId::Claude, user_event_seq: 1, checkpoint_seq: 2, cwd: PathBuf::from("sentetik-calisma-alani"), policy, reset_at: reset }
    }

    async fn settle_until(what: &str, mut condition: impl FnMut() -> bool) {
        let settled = tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                if condition() {
                    return;
                }
                tokio::time::sleep(StdDuration::from_millis(1)).await;
            }
        }).await;
        assert!(settled.is_ok(), "koşul gerçekleşmedi: {what}");
    }

    async fn idle() {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    }

    fn kinds(env: &Env, chat: &str) -> Vec<String> {
        env.db.events_for_chat(chat, None).unwrap().into_iter().map(|e| e.kind).collect()
    }

    fn state(env: &Env, id: &str) -> QuotaWaitState {
        env.db.quota_wait(id).unwrap().unwrap().state
    }

    // ---- saf karar --------------------------------------------------------------
    fn sample_wait(reset: Option<DateTime<Utc>>) -> QuotaWait {
        QuotaWait { id: "w".into(), logical_turn_id: "t".into(), chat_id: "c".into(), source_session_id: "s".into(), engine: EngineId::Claude, user_event_seq: 1, checkpoint_seq: 2, cwd: PathBuf::from("x"), policy: ResumePolicy::Workspace, reset_at: reset, state: QuotaWaitState::Waiting, generation: 0, next_engine_index: 0, attempt_count: 0, next_attempt_at: None, updated_at: Utc::now() }
    }

    fn snap(minutes: i64, fallback: &[EngineId]) -> PolicySnapshot {
        PolicySnapshot { enabled: true, window: Duration::minutes(minutes), fallback_order: fallback.to_vec(), eligible: vec![EngineId::Claude, EngineId::Codex] }
    }

    #[test]
    fn auto_continue_waits_at_threshold() {
        let now = Utc::now();
        let at_limit = sample_wait(Some(now + Duration::minutes(45)));
        let beyond = sample_wait(Some(now + Duration::minutes(45) + Duration::seconds(1)));
        assert!(matches!(decide(&at_limit, &snap(45, &[]), now), Plan::Run { target: Target::SameEngine, reason: QuotaWaitReason::ResetScheduled, .. }));
        assert_eq!(decide(&beyond, &snap(45, &[]), now), Plan::Hold { reason: QuotaWaitReason::ResetBeyondWait });
        // Pencere dışı ama fallback var: devir.
        assert!(matches!(decide(&beyond, &snap(45, &[EngineId::Codex]), now), Plan::Run { target: Target::Fallback { index: 0, engine: EngineId::Codex }, reason: QuotaWaitReason::FallbackSelected, .. }));
        // Zamanlayıcı reset + tolerans anındadır, reset'ten önce değil.
        let Plan::Run { at, .. } = decide(&at_limit, &snap(45, &[]), now) else { panic!() };
        assert!(at > now + Duration::minutes(45) && at <= now + Duration::minutes(45) + Duration::seconds(RESET_GRACE_SECS + 5));
    }

    #[test]
    fn unknown_past_disabled_and_fallback_exhaustion_plans() {
        let now = Utc::now();
        let unknown = sample_wait(None);
        assert_eq!(decide(&unknown, &snap(45, &[]), now), Plan::Hold { reason: QuotaWaitReason::ResetUnknown });
        let past = sample_wait(Some(now - Duration::minutes(3)));
        assert!(matches!(decide(&past, &snap(45, &[]), now), Plan::Run { at, reason: QuotaWaitReason::ResetReached, .. } if at == now));
        let mut off = snap(45, &[]);
        off.enabled = false;
        assert_eq!(decide(&past, &off, now), Plan::Hold { reason: QuotaWaitReason::Disabled });
        // Kaynak motor, devre dışı motor ve tükenmiş sıra atlanır.
        let mut s = snap(45, &[EngineId::Claude, EngineId::Agy]);
        assert_eq!(decide(&unknown, &s, now), Plan::Hold { reason: QuotaWaitReason::FallbackExhausted });
        s.fallback_order = vec![EngineId::Agy, EngineId::Codex];
        assert!(matches!(decide(&unknown, &s, now), Plan::Run { target: Target::Fallback { index: 1, engine: EngineId::Codex }, .. }));
        let mut tried = sample_wait(None);
        tried.next_engine_index = 2;
        assert_eq!(decide(&tried, &s, now), Plan::Hold { reason: QuotaWaitReason::FallbackExhausted });
        let mut full = sample_wait(None);
        full.policy = ResumePolicy::RequiresLocalApproval;
        assert_eq!(decide(&full, &s, now), Plan::Hold { reason: QuotaWaitReason::LocalApprovalRequired });
    }

    #[test]
    fn backoff_is_bounded_and_floor_delays_run() {
        assert_eq!(backoff(1), Duration::seconds(60));
        assert_eq!(backoff(3), Duration::seconds(240));
        assert_eq!(backoff(40), Duration::seconds(900));
        let now = Utc::now();
        let mut wait = sample_wait(Some(now - Duration::minutes(1)));
        wait.next_attempt_at = Some(now + Duration::seconds(60));
        assert!(matches!(decide(&wait, &snap(45, &[]), now), Plan::Run { at, reason: QuotaWaitReason::RetryBackoff, .. } if at == now + Duration::seconds(60)));
    }

    #[test]
    fn multiple_quota_windows_use_latest_reset_and_unknown_is_not_a_time() {
        use ultron_core::quota::{BudgetMode, PoolReport, QuotaWindow};
        let now = Utc::now();
        let window = |pct: Option<f32>, reset: Option<DateTime<Utc>>, source| QuotaWindow { label: "w".into(), used_pct: pct, resets_at: reset, window_secs: 0, source };
        let report = |windows| QuotaReport { fetched_at: now, pools: vec![PoolReport { pool: Pool::Claude, windows, reset_rights: None, mode: BudgetMode::Tam, note: None }] };
        let early = now + Duration::minutes(10);
        let late = now + Duration::minutes(40);
        let r = report(vec![window(Some(100.0), Some(early), QuotaSource::Live), window(Some(100.0), Some(late), QuotaSource::Live), window(Some(30.0), Some(now + Duration::hours(9)), QuotaSource::Live)]);
        assert_eq!(blocking_reset_from_report(&r, EngineId::Claude, now), Some(late));
        let r = report(vec![window(Some(100.0), Some(early), QuotaSource::Live), window(Some(100.0), None, QuotaSource::Live)]);
        assert_eq!(blocking_reset_from_report(&r, EngineId::Claude, now), None);
        let r = report(vec![window(Some(50.0), Some(early), QuotaSource::Live)]);
        assert_eq!(blocking_reset_from_report(&r, EngineId::Claude, now), None);
        let r = report(vec![window(Some(100.0), Some(early), QuotaSource::Unknown)]);
        assert_eq!(blocking_reset_from_report(&r, EngineId::Claude, now), None);
        assert_eq!(blocking_reset_from_report(&report(vec![window(Some(100.0), Some(early), QuotaSource::Live)]), EngineId::Agy, now), None);
    }

    // ---- zamanlayıcı ---------------------------------------------------------------
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replaced_expired_timer_cannot_claim_or_abort_completion() {
        let env = env();
        env.auto.start().unwrap();
        lock(&env.resumer.script).push_back(Script::DelayedComplete);
        let gate = || Arc::new(TimerBarrier {
            ready: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        let old_gate = gate();
        *lock(&env.auto.inner.timer_barrier) = Some(old_gate.clone());
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace)).await.unwrap();
        // Eski görev uyandı; timer mutex'ine girmeden aynı nesilde iptal ediliyor.
        old_gate.ready.wait();
        let old_abort = lock(&env.auto.inner.timers).get(&wait.id).unwrap().abort.clone();
        env.settings.write().unwrap().auto_continue_enabled = false;
        env.auto.reconcile().unwrap();
        let replacement_gate = gate();
        *lock(&env.auto.inner.timer_barrier) = Some(replacement_gate.clone());
        env.settings.write().unwrap().auto_continue_enabled = true;
        env.auto.reconcile().unwrap();
        replacement_gate.ready.wait();
        old_gate.release.wait();
        settle_until("eski timer bitti", || old_abort.is_finished()).await;
        let stale_calls = env.resumer.calls();
        let stale_state = state(&env, &wait.id);
        let replacement_count = env.auto.timer_count();
        // Regresyon durumunda assertion, başka bir runtime işçisini bariyerde bırakmasın.
        replacement_gate.release.wait();
        assert_eq!(stale_calls, 0, "iptal edilmiş timer claim yapamaz");
        assert_eq!(stale_state, QuotaWaitState::Waiting);
        assert_eq!(replacement_count, 1, "eski timer yeni kimliği silemez");
        settle_until("tur kabul edildi", || lock(&env.resumer.pending).is_some()).await;
        assert_eq!(state(&env, &wait.id), QuotaWaitState::Running);
        env.settings.write().unwrap().auto_continue_enabled = false;
        env.auto.reconcile().unwrap();
        let (tx, completion) = lock(&env.resumer.pending).take().unwrap();
        assert!(tx.send(Ok(completion)).is_ok(), "completion gözlemcisi abort edilmedi");
        settle_until("tamamlama uzlaştırıldı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
        env.auto.shutdown().await;
    }

    #[tokio::test]
    async fn fallback_chain_survives_reordering_and_database_reopen() {
        let config = TelegramConfig { fallback_order: vec![EngineId::Codex, EngineId::Agy], ..TelegramConfig::default() };
        let env = env_with(config, Arc::new(NoResetProbe));
        let make = |db| AutoContinue::new(AutoContinueDeps {
            db, log: env.log.clone(), resumer: env.resumer.clone(),
            settings: env.settings.clone(), settings_updates: None,
            eligible_engines: vec![EngineId::Claude, EngineId::Codex, EngineId::Agy],
            clock: env.clock.clone(), keep: env.keep.clone(), probe: Arc::new(NoResetProbe),
        });
        let auto = make(env.db.clone());
        auto.start().unwrap();
        lock(&env.resumer.script).push_back(Script::Quota(None));
        let wait = auto.register_exhausted(attempt("chat1", "t", None, ResumePolicy::Workspace)).await.unwrap();
        settle_until("Codex tükendi", || {
            let w = env.db.quota_wait(&wait.id).unwrap().unwrap();
            w.state == QuotaWaitState::Waiting && w.next_engine_index == 1
        }).await;
        assert_eq!(lock(&env.resumer.calls)[0].2, Some(EngineId::Codex));
        env.settings.write().unwrap().fallback_order = vec![EngineId::Agy, EngineId::Codex];
        auto.reconcile().unwrap();
        auto.shutdown().await;
        let reopened = Arc::new(Db::open(&Paths::from_root(env._tmp.path().join("ultron")).db).unwrap());
        let restarted = make(reopened);
        restarted.start().unwrap();
        env.clock.advance(59);
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        env.clock.advance(1);
        settle_until("Agy tamamladı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 2);
        assert_eq!(lock(&env.resumer.calls)[1].2, Some(EngineId::Agy));
        restarted.shutdown().await;
    }

    #[tokio::test]
    async fn reconcile_and_restart_during_reset_grace_preserve_deadline() {
        let env = env();
        env.auto.start().unwrap();
        let reset = env.clock.now_utc() + Duration::minutes(10);
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(reset), ResumePolicy::Workspace)).await.unwrap();
        let deadline = lock(&env.auto.inner.timers).get(&wait.id).unwrap().at;
        env.clock.advance(10 * 60);
        env.updates.send_replace(settings_of(&env.settings.read().unwrap()));
        env.auto.reconcile().unwrap();
        idle().await;
        assert_eq!(env.resumer.calls(), 0, "reset sonrası tolerans korunur");
        assert_eq!(lock(&env.auto.inner.timers).get(&wait.id).unwrap().at, deadline);
        env.auto.shutdown().await;
        let restarted = build(env.db.clone(), env.log.clone(), env.clock.clone(), env.settings.clone(), env.keep.clone(), env.resumer.clone(), Arc::new(NoResetProbe), &env.updates);
        restarted.start().unwrap();
        assert_eq!(lock(&restarted.inner.timers).get(&wait.id).unwrap().at, deadline);
        env.clock.advance((deadline - env.clock.now_utc()).num_seconds() - 1);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        env.clock.advance(1);
        settle_until("tolerans sonrası tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
        restarted.shutdown().await;
    }

    #[tokio::test]
    async fn disable_between_claim_and_service_acceptance_restores_wait_on_enable() {
        let env = env();
        env.auto.start().unwrap();
        lock(&env.resumer.script).push_back(Script::Disable);
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace)).await.unwrap();
        settle_until("başlamayan claim bırakıldı", || {
            let w = env.db.quota_wait(&wait.id).unwrap().unwrap();
            env.resumer.calls() == 1 && w.state == QuotaWaitState::Waiting && w.generation == 2
        }).await;
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
        assert_eq!(env.db.quota_wait(&wait.id).unwrap().unwrap().attempt_count, 0);
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        env.settings.write().unwrap().auto_continue_enabled = true;
        env.updates.send_replace(settings_of(&env.settings.read().unwrap()));
        settle_until("enable sonrası devam", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 2);
        assert_eq!(env.db.quota_wait(&wait.id).unwrap().unwrap().attempt_count, 1);
    }

    #[tokio::test]
    async fn manager_terminal_failures_emit_stopped_once_without_replay() {
        for running in [false, true] {
            let env = env();
            env.auto.start().unwrap();
            lock(&env.resumer.script).push_back(Script::ManagerFail(running));
            let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace)).await.unwrap();
            settle_until("manager hatası bildirildi", || kinds(&env, "chat1").iter().any(|kind| kind == "quota_wait_stopped")).await;
            assert_eq!(state(&env, &wait.id), QuotaWaitState::NeedsAttention);
            env.auto.reconcile().unwrap();
            env.auto.reconcile().unwrap();
            env.clock.advance(7200);
            idle().await;
            let stopped: Vec<_> = env.db.events_for_chat("chat1", None).unwrap().into_iter().filter(|e| e.kind == "quota_wait_stopped").collect();
            assert_eq!(stopped.len(), 1);
            assert_eq!(stopped[0].payload["reason"], "needs_attention");
            assert_eq!(env.resumer.calls(), 1);
            assert_eq!(env.auto.timer_count(), 0);
            assert_eq!(env.keep.holders(), 0);
        }
    }

    #[tokio::test]
    async fn known_reset_resumes_once_at_reset_and_guard_is_held_only_while_pending() {
        let env = env();
        env.auto.start().unwrap();
        let reset = env.clock.now_utc() + Duration::minutes(10);
        let wait = env.auto.register_exhausted(attempt("chat1", "turn1", Some(reset), ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(wait.state, QuotaWaitState::Waiting);
        assert_eq!(env.auto.timer_count(), 1);
        assert_eq!(env.keep.holders_for(KeepAwakeReason::QuotaTimer), 1);
        assert!(kinds(&env, "chat1").contains(&"quota_wait_started".to_string()));
        env.clock.advance(9 * 60);
        idle().await;
        assert_eq!(env.resumer.calls(), 0, "reset öncesi çalışmaz");
        env.clock.advance(60);
        idle().await;
        assert_eq!(env.resumer.calls(), 0, "tolerans bitmeden çalışmaz");
        env.clock.advance(15);
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
        assert_eq!(lock(&env.resumer.calls)[0].2, None);
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
        assert!(kinds(&env, "chat1").contains(&"quota_resumed".to_string()));
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 1, "tamamlanan iş yeniden çalışmaz");
    }

    #[tokio::test]
    async fn past_reset_runs_one_attempt_immediately() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(5)), ResumePolicy::Workspace)).await.unwrap();
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
    }

    #[tokio::test]
    async fn beyond_window_without_fallback_holds_without_timer_or_keepawake() {
        for reset in [None, Some(Duration::minutes(46))] {
            let env = env();
            env.auto.start().unwrap();
            let reset = reset.map(|d| env.clock.now_utc() + d);
            let wait = env.auto.register_exhausted(attempt("chat1", "t", reset, ResumePolicy::Workspace)).await.unwrap();
            assert_eq!(wait.state, QuotaWaitState::Waiting);
            assert_eq!(env.auto.timer_count(), 0);
            assert_eq!(env.keep.holders(), 0);
            let started: Vec<_> = env.db.events_for_chat("chat1", None).unwrap().into_iter().filter(|e| e.kind == "quota_wait_started").collect();
            assert_eq!(started.len(), 1);
            let expected = if reset.is_some() { "reset_beyond_wait" } else { "reset_unknown" };
            assert_eq!(started[0].payload["reason"], expected);
            env.clock.advance(100 * 3600);
            idle().await;
            assert_eq!(env.resumer.calls(), 0);
        }
    }

    #[tokio::test]
    async fn reset_unknown_uses_quota_window_probe() {
        let env = env_with(TelegramConfig::default(), Arc::new(FixedProbe(Some(Utc::now() + Duration::minutes(5)))));
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", None, ResumePolicy::Workspace)).await.unwrap();
        assert!(wait.reset_at.is_some());
        assert_eq!(env.auto.timer_count(), 1);
    }

    #[tokio::test]
    async fn fallback_hands_off_once_with_next_eligible_engine_and_never_cycles() {
        let config = TelegramConfig { fallback_order: vec![EngineId::Claude, EngineId::Agy, EngineId::Codex], ..TelegramConfig::default() };
        let env = env_with(config, Arc::new(NoResetProbe));
        env.auto.start().unwrap();
        lock(&env.resumer.script).push_back(Script::Quota(None));
        let wait = env.auto.register_exhausted(attempt("chat1", "t", None, ResumePolicy::Workspace)).await.unwrap();
        // Kaynak motor (Claude) ve devre dışı Agy atlanır: ilk devir Codex.
        settle_until("fallback çağrısı", || env.resumer.calls() == 1).await;
        assert_eq!(lock(&env.resumer.calls)[0].2, Some(EngineId::Codex));
        // Codex da tükendi: sıra bitti, A->B->A döngüsü yok; bekleme zamanlayıcısız.
        settle_until("waiting", || { let w = env.db.quota_wait(&wait.id).unwrap().unwrap(); w.state == QuotaWaitState::Waiting && w.next_engine_index == 3 }).await;
        idle().await;
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        let reasons: Vec<_> = env.db.events_for_chat("chat1", None).unwrap().into_iter().filter(|e| e.kind == "quota_wait_started").map(|e| e.payload["reason"].as_str().unwrap().to_string()).collect();
        assert_eq!(reasons, ["fallback_selected", "fallback_exhausted"]);
    }

    #[tokio::test]
    async fn repeated_quota_uses_bounded_backoff_then_needs_attention() {
        let env = env();
        env.auto.start().unwrap();
        let stale = env.clock.now_utc() - Duration::minutes(1);
        for _ in 0..10 {
            lock(&env.resumer.script).push_back(Script::Quota(Some(stale)));
        }
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(stale), ResumePolicy::Workspace)).await.unwrap();
        for expected in 1..=MAX_ATTEMPTS {
            settle_until("deneme", || i64::try_from(env.resumer.calls()).unwrap() == expected).await;
            settle_until("yeniden bekleme/dikkat", || state(&env, &wait.id) != QuotaWaitState::Running && state(&env, &wait.id) != QuotaWaitState::Claimed).await;
            idle().await;
            // Backoff dolmadan hızlı döngü yok.
            assert_eq!(i64::try_from(env.resumer.calls()).unwrap(), expected);
            if expected < MAX_ATTEMPTS {
                assert_eq!(state(&env, &wait.id), QuotaWaitState::Waiting);
                env.clock.advance(backoff(expected).num_seconds() - 1);
                idle().await;
                assert_eq!(i64::try_from(env.resumer.calls()).unwrap(), expected);
                env.clock.advance(1);
            }
        }
        assert_eq!(state(&env, &wait.id), QuotaWaitState::NeedsAttention);
        env.clock.advance(24 * 3600);
        idle().await;
        assert_eq!(env.resumer.calls(), usize::try_from(MAX_ATTEMPTS).unwrap());
        assert_eq!(env.keep.holders(), 0);
    }

    #[tokio::test]
    async fn requota_within_window_replans_from_new_reset() {
        let env = env();
        env.auto.start().unwrap();
        let first = env.clock.now_utc() - Duration::minutes(1);
        let second = env.clock.now_utc() + Duration::minutes(20);
        lock(&env.resumer.script).push_back(Script::Quota(Some(second)));
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(first), ResumePolicy::Workspace)).await.unwrap();
        settle_until("zamanlayıcı", || env.resumer.calls() == 1 && env.auto.timer_count() == 1).await;
        let stored = env.db.quota_wait(&wait.id).unwrap().unwrap();
        assert_eq!(stored.reset_at, Some(second));
        assert_eq!(env.keep.holders_for(KeepAwakeReason::QuotaTimer), 1);
        env.clock.advance(20 * 60 + 15);
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 2);
    }

    #[tokio::test]
    async fn failed_attempt_needs_attention_without_replay() {
        let env = env();
        env.auto.start().unwrap();
        lock(&env.resumer.script).push_back(Script::Fail);
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace)).await.unwrap();
        settle_until("dikkat", || state(&env, &wait.id) == QuotaWaitState::NeedsAttention).await;
        env.clock.advance(7200);
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        assert!(kinds(&env, "chat1").contains(&"quota_wait_stopped".to_string()));
    }

    #[tokio::test]
    async fn busy_or_unavailable_resume_never_leaves_claimed_nor_retries() {
        for (error, expected) in [(GatewayError::Busy, QuotaWaitState::Cancelled), (GatewayError::Unavailable, QuotaWaitState::NeedsAttention)] {
            let env = env();
            env.auto.start().unwrap();
            lock(&env.resumer.script).push_back(Script::Error(error));
            let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace)).await.unwrap();
            settle_until("kapandı", || state(&env, &wait.id) == expected).await;
            env.clock.advance(7200);
            idle().await;
            assert_eq!(env.resumer.calls(), 1);
            assert_eq!(env.keep.holders(), 0);
        }
    }

    #[tokio::test]
    async fn disabled_setting_stops_timers_keeps_record_and_reenable_restores() {
        let env = env();
        env.auto.start().unwrap();
        let reset = env.clock.now_utc() + Duration::minutes(10);
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(reset), ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(env.auto.timer_count(), 1);
        // Ayar kapatma: watch bildirimi timer'ı durdurur, kayıt beklemede kalır.
        env.settings.write().unwrap().auto_continue_enabled = false;
        env.updates.send_replace(settings_of(&env.settings.read().unwrap()));
        settle_until("timer durdu", || env.auto.timer_count() == 0).await;
        assert_eq!(env.keep.holders(), 0);
        assert_eq!(state(&env, &wait.id), QuotaWaitState::Waiting);
        assert!(env.db.events_for_chat("chat1", None).unwrap().iter().any(|e| e.kind == "quota_wait_stopped" && e.payload["reason"] == "disabled"));
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        // Yeniden açınca kalıcı kayıttan timer kurulur (reset geçmişte: tek deneme).
        env.settings.write().unwrap().auto_continue_enabled = true;
        env.updates.send_replace(settings_of(&env.settings.read().unwrap()));
        settle_until("devam", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
    }

    #[tokio::test]
    async fn disabled_at_registration_records_wait_without_timer() {
        let config = TelegramConfig { auto_continue_enabled: false, ..TelegramConfig::default() };
        let env = env_with(config, Arc::new(NoResetProbe));
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(5)), ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(wait.state, QuotaWaitState::Waiting);
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
    }

    #[tokio::test]
    async fn cancel_drops_timer_and_releases_keepawake() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(env.keep.holders(), 1);
        assert_eq!(env.auto.cancel_chat("chat1").unwrap(), 1);
        assert_eq!(env.auto.timer_count(), 0);
        idle().await; // abort edilen görev bir sonraki yoklamada düşer ve guard'ı bırakır
        assert_eq!(env.keep.holders(), 0);
        assert_eq!(state(&env, &wait.id), QuotaWaitState::Cancelled);
        assert!(env.db.events_for_chat("chat1", None).unwrap().iter().any(|e| e.kind == "quota_wait_stopped" && e.payload["reason"] == "cancelled"));
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
    }

    #[tokio::test]
    async fn new_user_message_supersedes_even_if_timer_still_fires() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        // Yeni kullanıcı mesajı DB'de nesli kapatır; scheduler'a henüz haber verilmedi.
        env.db.append_user_event_invalidating_quota(&StoredEvent { id: "u2".into(), chat_id: "chat1".into(), session_id: None, seq: 0, ts: Utc::now(), kind: "user_text".into(), payload: serde_json::json!({"type":"user_text","text":"yeni hedef"}) }).unwrap();
        assert_eq!(state(&env, &wait.id), QuotaWaitState::Cancelled);
        env.clock.advance(11 * 60);
        idle().await;
        assert_eq!(env.resumer.calls(), 0, "geç kalan timer nesil CAS'inde elenir");
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
    }

    #[tokio::test]
    async fn superseded_notification_is_written_once_for_tracked_wait() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        env.db.cancel_quota_waits("chat1", Utc::now()).unwrap();
        env.auto.on_chat_superseded("chat1");
        env.auto.on_chat_superseded("chat1");
        let stopped: Vec<_> = env.db.events_for_chat("chat1", None).unwrap().into_iter().filter(|e| e.kind == "quota_wait_stopped").collect();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].payload["reason"], "superseded");
        assert_eq!(state(&env, &wait.id), QuotaWaitState::Cancelled);
    }

    #[tokio::test]
    async fn clock_jumps_do_not_fire_early_or_double_run() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        // Geri atlama: monotonik süre dolsa bile duvar saati reset'e ulaşmadıysa çalışmaz.
        env.clock.jump_utc(-3600);
        env.clock.advance(11 * 60);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        // Duvar saati reset'i geçince (ileri atlama) bir sonraki parçada tek kez çalışır.
        env.clock.jump_utc(3600 + 3600);
        env.clock.advance(31);
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        assert_eq!(env.resumer.calls(), 1);
        env.clock.jump_utc(-7200);
        env.clock.advance(7200);
        env.clock.jump_utc(7200);
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
    }

    #[tokio::test]
    async fn two_schedulers_race_on_claim_runs_only_once() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        let second = build(env.db.clone(), env.log.clone(), env.clock.clone(), env.settings.clone(), env.keep.clone(), env.resumer.clone(), Arc::new(NoResetProbe), &env.updates);
        assert_eq!(second.start().unwrap().timers, 1);
        assert_eq!(env.auto.timer_count(), 1);
        env.clock.advance(11 * 60);
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        assert_eq!(env.keep.holders(), 0);
        second.shutdown().await;
    }

    #[tokio::test]
    async fn restart_restores_waiting_once() {
        let env = env();
        env.auto.start().unwrap();
        let wait = env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        env.auto.shutdown().await;
        assert_eq!(env.keep.holders(), 0, "kapanış guard'ı bırakır");
        // Yeni süreç: aynı DB, yeni scheduler.
        let restarted = build(env.db.clone(), env.log.clone(), env.clock.clone(), env.settings.clone(), env.keep.clone(), env.resumer.clone(), Arc::new(NoResetProbe), &env.updates);
        let report = restarted.start().unwrap();
        assert_eq!((report.needs_attention, report.timers, report.holding), (0, 1, 0));
        restarted.reconcile().unwrap();
        restarted.reconcile().unwrap();
        assert_eq!(restarted.timer_count(), 1, "tekrar uzlaştırma yinelenen timer kurmaz");
        env.clock.advance(11 * 60);
        settle_until("tamamlandı", || state(&env, &wait.id) == QuotaWaitState::Completed).await;
        idle().await;
        assert_eq!(env.resumer.calls(), 1);
        restarted.shutdown().await;
    }

    #[tokio::test]
    async fn crash_while_claimed_or_running_is_not_replayed_and_terminal_waits_stay_dead() {
        let env = env();
        // Çökme öncesi: biri claimed, biri running, biri completed, biri cancelled.
        seed_chat(&env.db, &env.log, "chat2");
        seed_chat(&env.db, &env.log, "chat3");
        seed_chat(&env.db, &env.log, "chat4");
        let now = Utc::now();
        let mk = |chat: &str, id: &str| env.db.create_quota_wait(&NewQuotaWait { id: id.into(), logical_turn_id: format!("turn-{id}"), chat_id: chat.into(), source_session_id: format!("{chat}-session"), engine: EngineId::Claude, user_event_seq: 1, checkpoint_seq: 2, cwd: PathBuf::from("x"), policy: ResumePolicy::Workspace, reset_at: Some(now - Duration::minutes(1)), next_attempt_at: None }, now).unwrap();
        let w1 = mk("chat1", "claimed");
        env.db.claim_quota_wait(&w1.id, w1.generation, now).unwrap().unwrap();
        let w2 = mk("chat2", "running");
        let c2 = env.db.claim_quota_wait(&w2.id, w2.generation, now).unwrap().unwrap();
        env.db.advance_quota_wait(&c2.id, c2.generation, QuotaWaitState::Running, None, now).unwrap().unwrap();
        let w3 = mk("chat3", "completed");
        let c3 = env.db.claim_quota_wait(&w3.id, w3.generation, now).unwrap().unwrap();
        let r3 = env.db.advance_quota_wait(&c3.id, c3.generation, QuotaWaitState::Running, None, now).unwrap().unwrap();
        env.db.advance_quota_wait(&r3.id, r3.generation, QuotaWaitState::Completed, None, now).unwrap().unwrap();
        mk("chat4", "cancelled");
        env.db.cancel_quota_waits("chat4", now).unwrap();
        let report = env.auto.start().unwrap();
        assert_eq!((report.needs_attention, report.timers), (2, 0));
        env.clock.advance(7200);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        assert_eq!(state(&env, "claimed"), QuotaWaitState::NeedsAttention);
        assert_eq!(state(&env, "running"), QuotaWaitState::NeedsAttention);
        assert_eq!(state(&env, "completed"), QuotaWaitState::Completed);
        assert_eq!(state(&env, "cancelled"), QuotaWaitState::Cancelled);
        let stopped = env.db.events_for_chat("chat1", None).unwrap().iter().filter(|e| e.kind == "quota_wait_stopped").count();
        assert_eq!(stopped, 1);
    }

    #[tokio::test]
    async fn full_policy_and_missing_source_event_need_local_attention_and_never_auto_run() {
        let env = env();
        env.auto.start().unwrap();
        let full = env.auto.register_exhausted(attempt("chat1", "t-full", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::RequiresLocalApproval)).await.unwrap();
        assert_eq!(full.state, QuotaWaitState::NeedsAttention);
        seed_chat(&env.db, &env.log, "chat2");
        let mut broken = attempt("chat2", "t-broken", Some(env.clock.now_utc() - Duration::minutes(1)), ResumePolicy::Workspace);
        broken.user_event_seq = 2; // kullanıcı metni olmayan olay: kaynak doğrulanamaz
        let broken = env.auto.register_exhausted(broken).await.unwrap();
        assert_eq!(broken.state, QuotaWaitState::NeedsAttention);
        env.clock.advance(7200);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        assert_eq!(env.auto.timer_count(), 0);
        assert_eq!(env.keep.holders(), 0);
    }

    #[tokio::test]
    async fn registration_is_idempotent_per_logical_turn() {
        let env = env();
        env.auto.start().unwrap();
        let first = env.auto.register_exhausted(attempt("chat1", "t", None, ResumePolicy::Workspace)).await.unwrap();
        let again = env.auto.register_exhausted(attempt("chat1", "t", None, ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(env.db.pending_quota_waits().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_aborts_timers_and_blocks_new_ones() {
        let env = env();
        env.auto.start().unwrap();
        env.auto.register_exhausted(attempt("chat1", "t", Some(env.clock.now_utc() + Duration::minutes(10)), ResumePolicy::Workspace)).await.unwrap();
        assert_eq!(env.keep.holders(), 1);
        env.auto.shutdown().await;
        assert_eq!(env.keep.holders(), 0);
        assert_eq!(env.auto.timer_count(), 0);
        env.clock.advance(3600);
        idle().await;
        assert_eq!(env.resumer.calls(), 0);
        assert!(env.auto.start().is_err());
    }

    /// Devam yalnız (wait_id, generation) taşır; yeni riskli araç adımı normal onay
    /// yolunda bekler, cevap gelmezse güvenli ret olur ve otomatik onay yoktur.
    #[tokio::test(start_paused = true)]
    async fn new_tool_approval_stops_unattended_resume() {
        use crate::server::approvals::{Approvals, MAX_APPROVAL_WAIT_SECS};
        use ultron_core::db::approvals::ApprovalStatus;
        use ultron_core::policy::{Action, Decision};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
        seed_chat(&db, &log, "chat1");
        let approvals = Arc::new(Approvals::new(db.clone(), log, paths.clone()));
        let pending = approvals.clone();
        let cwd = paths.root.clone();
        let task = tokio::spawn(async move {
            pending
                .check_owned(Action::Shell { command: "rm synthetic.txt".into(), cwd }, Some("chat1".into()), EngineId::Claude, MAX_APPROVAL_WAIT_SECS, None)
                .await
                .unwrap()
        });
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        assert!(!task.is_finished(), "yanıt gelmeden araç çalışmaz");
        assert_eq!(db.list_approvals(Some(ApprovalStatus::Pending)).unwrap().len(), 1);
        tokio::time::sleep(StdDuration::from_secs(u64::from(MAX_APPROVAL_WAIT_SECS))).await;
        assert!(matches!(task.await.unwrap(), Decision::Deny { .. }), "zaman aşımı güvenli ret");
        assert!(db.list_approvals(Some(ApprovalStatus::Pending)).unwrap().is_empty());
    }
}
