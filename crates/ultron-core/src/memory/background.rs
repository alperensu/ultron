//! Turu bekletmeyen özet ve hafıza önerisi işleri (Aşama 5a Task 4).
//!
//! İşler `memory_jobs` tablosunda kalıcıdır; küçük sabit sayıda işçi bunları kiralayıp işler.
//! Kuyruk üst sınırlıdır (süreç içi sayaç), başarısızlıkta işçi başına üstel bekleme uygulanır,
//! deneme sayısı veritabanı sözleşmesindeki sınırla kapalıdır. Kapanışta işçiler iptal edilip
//! beklenir; yeniden başlatmada süresi dolmamış yarım kiralamalar da hemen devralınır.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::{Db, RECENT_EVENTS_WINDOW};
use crate::db::memory::{JobEnqueue, MemoryJobClaim};
use crate::memory::suggestions::store_candidates;
use crate::memory::{MemoryJobError, MemoryJobStatus};
use crate::summarizer::{SummaryQuotaError, SummaryTimeoutError, SummaryInvalidOutput};
use crate::workstate::{ApplyOutcome, WorkStateService, summary_status};

/// Arka plan işçisi ayarları. Üst sınırlar sabittir.
#[derive(Debug, Clone)]
pub struct BackgroundConfig {
    /// Eşzamanlı işçi sayısı (1..=4).
    pub workers: usize,
    /// Kuyrukta/çalışırken bulunabilecek en fazla iş (süreç içi).
    pub queue_capacity: usize,
    /// Kiralama süresi; model çağrısı süresinden uzun olmalıdır.
    pub lease: Duration,
    /// Tek işin üst süresi.
    pub job_timeout: Duration,
    /// Boşta bekleme üst sınırı (bildirim kaçarsa).
    pub poll_interval: Duration,
    /// Başarısızlık sonrası işçi beklemesi: `base * 2^(deneme-1)`, en çok `backoff_max`.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 64,
            lease: Duration::from_secs(300),
            job_timeout: Duration::from_secs(150),
            poll_interval: Duration::from_secs(5),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// Kuyruğa alma sonucu. `Queued`/`Duplicate` dışındakilerde tur sonucu yine saklanır.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    /// Aynı `(chat, tur, end_seq)` zaten kuyrukta.
    Duplicate,
    QueueFull,
    StoreFailed,
}

/// İş işleyici: bir kiralanmış işi çalıştırır. Hata kodu güvenli ve sınırlıdır.
pub trait JobProcessor: Send + Sync {
    fn process<'a>(
        &'a self,
        claim: &'a MemoryJobClaim,
    ) -> Pin<Box<dyn Future<Output = Result<(), MemoryJobError>> + Send + 'a>>;

    /// İş nihai olarak `failed` oldu (deneme bitti, panik, zaman aşımı, yetim/kiralama
    /// sonu...). Varsayılan: hiçbir şey. Uygulama güvenli durum kodunu yansıtmalıdır.
    fn job_failed(&self, _chat_id: &str, _end_seq: i64, _code: MemoryJobError) {}
}

struct Inner {
    db: Arc<Db>,
    cfg: BackgroundConfig,
    notify: Notify,
    /// Bu sürecin (kuyruk nesnesinin) başlangıç zamanı: daha önce kiralanmış `running` işler
    /// yetim sayılır.
    boot_started_at: DateTime<Utc>,
    /// Başarısız işlerin en son raporlandığı zaman.
    reported_until: std::sync::Mutex<DateTime<Utc>>,
}

/// Kalıcı iş kuyruğu + işçi başlatıcı. Ucuzdur; klonlanabilir.
#[derive(Clone)]
pub struct MemoryBackground {
    inner: Arc<Inner>,
}

impl MemoryBackground {
    pub fn new(db: Arc<Db>, mut cfg: BackgroundConfig) -> Self {
        cfg.workers = cfg.workers.clamp(1, 4);
        cfg.queue_capacity = cfg.queue_capacity.max(1);
        Self {
            inner: Arc::new(Inner {
                db,
                cfg,
                notify: Notify::new(),
                boot_started_at: Utc::now(),
                reported_until: std::sync::Mutex::new(Utc::now()),
            }),
        }
    }

    /// Veritabanındaki açık (`queued` + `running`) iş sayısı.
    pub fn open_jobs(&self) -> anyhow::Result<usize> {
        self.inner.db.memory_count_open_jobs()
    }

    /// İşi hızlıca kalıcı kuyruğa alır (sınır ve ekleme tek işlemde); model çağrısı beklenmez.
    pub fn enqueue(&self, chat_id: &str, logical_turn_id: &str, end_seq: i64) -> EnqueueOutcome {
        let inner = &self.inner;
        match inner
            .db
            .memory_enqueue_job_bounded(chat_id, logical_turn_id, end_seq, inner.cfg.queue_capacity)
        {
            Ok(JobEnqueue::Queued) => {
                inner.notify.notify_one();
                EnqueueOutcome::Queued
            }
            Ok(JobEnqueue::Duplicate) => EnqueueOutcome::Duplicate,
            Ok(JobEnqueue::Full) => EnqueueOutcome::QueueFull,
            Err(err) => {
                tracing::warn!(error = %err, "hafıza özet işi kuyruğa alınamadı");
                EnqueueOutcome::StoreFailed
            }
        }
    }

    /// İşçileri başlatır. Önce (işçiler doğmadan, senkron) bu süreçten önce kiralanmış yetim
    /// `running` işler geri kuyruğa alınır. Tek daemon varsayımıyla süreç başına bir kez çağrılır;
    /// sonradan kiralanan canlı işlere dokunulmaz.
    pub fn start(&self, processor: Arc<dyn JobProcessor>) -> MemoryWorkerHandle {
        self.start_with_boot(processor, self.inner.boot_started_at)
    }

    /// [`MemoryBackground::start`] ile aynı; yetim sınırı dışarıdan verilir (testler için).
    pub fn start_with_boot(
        &self,
        processor: Arc<dyn JobProcessor>,
        boot_started_at: DateTime<Utc>,
    ) -> MemoryWorkerHandle {
        let before_recovery = Utc::now();
        if let Err(err) = self.inner.db.memory_requeue_orphaned(boot_started_at) {
            tracing::warn!(error = %err, "yetim hafıza işleri kurtarılamadı");
        }
        if let Ok(mut since) = self.inner.reported_until.lock() {
            *since = before_recovery;
        }
        let cancel = CancellationToken::new();
        let tasks = (0..self.inner.cfg.workers)
            .map(|_| {
                tokio::spawn(worker_loop(
                    self.inner.clone(),
                    processor.clone(),
                    cancel.clone(),
                ))
            })
            .collect();
        MemoryWorkerHandle { cancel, tasks }
    }
}

/// Çalışan işçilerin tutacağı: `shutdown` iptal eder ve hepsinin bitmesini bekler.
pub struct MemoryWorkerHandle {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl MemoryWorkerHandle {
    /// İşçileri iptal eder ve bekler; yarım iş deneme hakkı harcanmadan kuyruğa döner.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Kiralama sonu/yetim işaretlemesiyle `failed` olan işleri (kuyruk dışı yollar) işleyiciye
/// bildirir; çağrılar idempotenttir (durum yazımı CAS'lıdır).
fn report_failures(inner: &Inner, processor: &dyn JobProcessor) {
    let Ok(mut since) = inner.reported_until.lock() else {
        return;
    };
    let checked_at = Utc::now();
    match inner.db.memory_failed_jobs_since(*since) {
        Ok(rows) => {
            for (chat_id, end_seq, code) in rows {
                processor.job_failed(&chat_id, end_seq, code.unwrap_or(MemoryJobError::Timeout));
            }
            *since = checked_at;
        }
        Err(err) => tracing::warn!(error = %err, "başarısız hafıza işleri okunamadı"),
    }
}

fn backoff(cfg: &BackgroundConfig, attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    cfg.backoff_base.saturating_mul(factor).min(cfg.backoff_max)
}

async fn worker_loop(inner: Arc<Inner>, processor: Arc<dyn JobProcessor>, cancel: CancellationToken) {
    let cfg = &inner.cfg;
    let lease = chrono::Duration::from_std(cfg.lease).unwrap_or_else(|_| chrono::Duration::minutes(5));
    while !cancel.is_cancelled() {
        let claim = match inner.db.memory_claim_job(Utc::now(), lease) {
            Ok(claim) => claim,
            Err(err) => {
                tracing::warn!(error = %err, "hafıza işi kiralanamadı");
                tokio::select! {
                    () = cancel.cancelled() => {}
                    () = tokio::time::sleep(cfg.poll_interval) => {}
                }
                continue;
            }
        };
        let Some(claim) = claim else {
            report_failures(&inner, processor.as_ref());
            tokio::select! {
                () = cancel.cancelled() => {}
                () = inner.notify.notified() => {}
                () = tokio::time::sleep(cfg.poll_interval) => {}
            }
            continue;
        };
        // İş ayrı görevde çalışır: panik yakalanır, iptal/zaman aşımında görev düşürülür.
        let job = {
            let (processor, claim) = (processor.clone(), claim.clone());
            tokio::spawn(async move { processor.process(&claim).await })
        };
        let abort = job.abort_handle();
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                abort.abort();
                None
            }
            res = tokio::time::timeout(cfg.job_timeout, job) => Some(match res {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(join)) => {
                    tracing::warn!(panic = join.is_panic(), "hafıza işi görevi sonlandı");
                    Err(MemoryJobError::EngineUnavailable)
                }
                Err(_) => {
                    abort.abort();
                    Err(MemoryJobError::Timeout)
                }
            }),
        };
        match result {
            None => {
                let _ = inner.db.memory_release_job(claim.id, claim.generation);
            }
            Some(Ok(())) => {
                let _ = inner.db.memory_complete_job(claim.id, claim.generation);
            }
            Some(Err(code)) => {
                let wait = chrono::Duration::from_std(backoff(cfg, claim.attempt))
                    .unwrap_or_else(|_| chrono::Duration::seconds(60));
                let next = inner
                    .db
                    .memory_retry_job(claim.id, claim.generation, code, Utc::now() + wait);
                if matches!(next, Ok(Some(MemoryJobStatus::Failed))) {
                    processor.job_failed(&claim.chat_id, claim.end_seq, code);
                }
            }
        }
    }
}

/// Model özetini çalıştırıp CAS ile uygulayan ve aday önerileri saklayan işleyici.
pub struct DigestProcessor {
    db: Arc<Db>,
    workstate: Arc<WorkStateService>,
}

impl DigestProcessor {
    pub fn new(db: Arc<Db>, workstate: Arc<WorkStateService>) -> Self {
        Self { db, workstate }
    }

    async fn run(&self, claim: &MemoryJobClaim) -> Result<(), MemoryJobError> {
        let store = |_: anyhow::Error| MemoryJobError::StoreUnavailable;
        let events: Vec<_> = self
            .db
            .events_tail(&claim.chat_id, RECENT_EVENTS_WINDOW)
            .map_err(store)?
            .into_iter()
            .filter(|e| e.seq <= claim.end_seq)
            .collect();
        if events.is_empty() {
            return Ok(());
        }
        let summarizer = self.workstate.summarizer();
        if summarizer.kind() == "null" {
            return Ok(());
        }
        let state = self.workstate.load(&claim.chat_id).map_err(store)?;
        let digest = match summarizer.digest(&state, &events).await {
            Ok(digest) => digest,
            Err(err) => {
                let code = classify(&err);
                let status = format!("{}{}", summary_status::FAILED_PREFIX, error_word(code));
                let _ = self.workstate.set_summary_status(&claim.chat_id, &status, claim.end_seq);
                return Err(code);
            }
        };
        let outcome = self
            .workstate
            .apply_digest_patch(&claim.chat_id, claim.end_seq, digest.patch, digest.engine, digest.fell_back)
            .map_err(store)?;
        if outcome == ApplyOutcome::Stale {
            tracing::debug!(chat_id = %claim.chat_id, end_seq = claim.end_seq, "eski özet atıldı");
        }
        store_candidates(
            &self.db,
            &claim.chat_id,
            &claim.logical_turn_id,
            claim.end_seq,
            &events,
            &digest.candidates,
        )
        .map_err(store)?;
        Ok(())
    }
}

impl JobProcessor for DigestProcessor {
    fn job_failed(&self, chat_id: &str, end_seq: i64, code: MemoryJobError) {
        let status = format!("{}{}", summary_status::FAILED_PREFIX, error_word(code));
        let _ = self.workstate.set_summary_status(chat_id, &status, end_seq);
    }

    fn process<'a>(
        &'a self,
        claim: &'a MemoryJobClaim,
    ) -> Pin<Box<dyn Future<Output = Result<(), MemoryJobError>> + Send + 'a>> {
        Box::pin(self.run(claim))
    }
}

/// Özetleyici hatasını güvenli iş koduna çevirir.
pub fn classify(err: &anyhow::Error) -> MemoryJobError {
    if err.downcast_ref::<SummaryQuotaError>().is_some() {
        MemoryJobError::Quota
    } else if err.downcast_ref::<SummaryTimeoutError>().is_some() {
        MemoryJobError::Timeout
    } else if err.downcast_ref::<SummaryInvalidOutput>().is_some() {
        MemoryJobError::InvalidOutput
    } else {
        MemoryJobError::EngineUnavailable
    }
}

fn error_word(code: MemoryJobError) -> &'static str {
    match code {
        MemoryJobError::Timeout => "timeout",
        MemoryJobError::EngineUnavailable => "engine_unavailable",
        MemoryJobError::InvalidOutput => "invalid_output",
        MemoryJobError::StoreUnavailable => "store_unavailable",
        MemoryJobError::Cancelled => "cancelled",
        MemoryJobError::Quota => "quota",
    }
}
