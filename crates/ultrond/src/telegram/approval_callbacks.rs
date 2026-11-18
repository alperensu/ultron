//! RAM capability'leri: yalnız başarılı teslim, bağlı alıcı ve özgün onay süresi.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ultron_core::config::{MAX_TELEGRAM_ALLOWED_USERS, TelegramConfig};
use ultron_core::db::approvals::{Approval, ApprovalStatus};
use ultron_core::policy::RiskCategory;

use crate::server::approvals::{Approvals, MAX_APPROVAL_WAIT_SECS, MAX_NOTIFIABLE_APPROVALS};
use super::contracts::{
    AnswerCallbackQuery, Clock, EditMessageText, GatewayApplication, GatewayError,
    InlineKeyboardButton, InlineKeyboardMarkup, SendMessage, TelegramCallbackQuery,
    TelegramTransport, TransportError, TurnOrigin,
};

pub const MAX_CALLBACK_NONCES: usize = MAX_NOTIFIABLE_APPROVALS * MAX_TELEGRAM_ALLOWED_USERS * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackError {
    Gateway(GatewayError),
    Transport(TransportError),
}

impl From<GatewayError> for CallbackError {
    fn from(error: GatewayError) -> Self { Self::Gateway(error) }
}

/// ID değerleri Debug/Serialize ile günlük veya IPC'ye taşınmaz.
pub struct ApprovalNotification {
    pub message_id: i64,
    pub actionable: bool,
}

pub struct CallbackReport {
    pub status: ApprovalStatus,
    pub already_decided: bool,
    pub answer_error: Option<TransportError>,
    pub edit_error: Option<TransportError>,
}

#[derive(Clone)]
struct Capability {
    approval_id: String,
    user_id: i64,
    chat_id: i64,
    message_id: Option<i64>,
    approve: bool,
    /// Bu andan sonra düğme karar veremez (özgün onay süresi).
    deadline: Instant,
    /// Süre sonrası mezar taşı: mesaj kimlikleri düzenleme için kalır.
    retain_until: Instant,
    /// Kararı işlenmekte olan veya verilmiş düğme.
    consumed: bool,
}

/// Süresi dolan onayın mesajını düzenleyebilmek için capability kayıtları
/// son tarihten sonra bu süre kadar (1024 sınırı içinde) tutulur.
pub const TOMBSTONE_GRACE: Duration = Duration::from_secs(600);

pub struct ApprovalCallbacks {
    approvals: Arc<Approvals>,
    application: Arc<dyn GatewayApplication>,
    transport: Arc<dyn TelegramTransport>,
    settings: Arc<RwLock<TelegramConfig>>,
    clock: Arc<dyn Clock>,
    nonces: Mutex<HashMap<String, Capability>>,
}

impl Drop for ApprovalCallbacks {
    fn drop(&mut self) {
        self.approvals.close_telegram_gateway();
    }
}

impl ApprovalCallbacks {
    pub fn new(
        approvals: Arc<Approvals>,
        application: Arc<dyn GatewayApplication>,
        transport: Arc<dyn TelegramTransport>,
        settings: Arc<RwLock<TelegramConfig>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        approvals.open_telegram_gateway();
        Self { approvals, application, transport, settings, clock, nonces: Mutex::new(HashMap::new()) }
    }

    fn authorized(&self, user_id: i64, chat_id: i64) -> Result<(), GatewayError> {
        let settings = self.settings.read().map_err(|_| GatewayError::Unavailable)?;
        if user_id <= 0 || user_id != chat_id || settings.validate().is_err()
            || !settings.allowed_user_ids.contains(&user_id)
        {
            return Err(GatewayError::Unauthorized);
        }
        Ok(())
    }

    /// Task 10 bakım tick'i ve her giriş bu temizliği kullanabilir. Restart yeni
    /// nesne kurar; RAM capability'leri diske yazılmaz veya yeniden yüklenmez.
    pub fn prune_expired(&self) -> Result<(), GatewayError> {
        let now = self.clock.now_monotonic();
        self.nonces.lock().map_err(|_| GatewayError::Unavailable)?
            .retain(|_, capability| capability.retain_until > now);
        Ok(())
    }

    /// Kardeş nonce'ların "işleniyor" işaretini kaldırır (geçici çözüm hatası).
    fn release(&self, approval_id: &str) {
        if let Ok(mut nonces) = self.nonces.lock() {
            for entry in nonces.values_mut().filter(|entry| entry.approval_id == approval_id) {
                entry.consumed = false;
            }
        }
    }

    pub fn clear(&self) -> Result<(), GatewayError> {
        self.nonces.lock().map_err(|_| GatewayError::Unavailable)?.clear();
        Ok(())
    }

    fn approval(&self, id: &str) -> Result<Approval, GatewayError> {
        self.approvals.get(id).map_err(|_| GatewayError::Unavailable)?.ok_or(GatewayError::NotFound)
    }

    /// Kullanıcı ve karar başına ayrı nonce. sendMessage hatası/iptali halinde
    /// message_id yoktur: sunucu haritasındaki rezervasyon yetki vermez.
    pub async fn notify(
        &self,
        approval_id: &str,
        user_id: i64,
        private_chat_id: i64,
    ) -> Result<ApprovalNotification, CallbackError> {
        self.authorized(user_id, private_chat_id)?;
        self.prune_expired()?;
        let row = self.approval(approval_id)?;
        if row.status != ApprovalStatus::Pending {
            return Err(GatewayError::InvalidInput.into());
        }
        if !self.approvals.is_telegram_offered(&row).map_err(|_| GatewayError::Unavailable)? {
            let sent = self.transport.send_message(SendMessage {
                chat_id: private_chat_id,
                text: "Bu işlem için masaüstünde onay gerekli.".into(),
                disable_notification: false,
                reply_markup: None,
            }).await.map_err(CallbackError::Transport)?;
            return Ok(ApprovalNotification { message_id: sent.message_id, actionable: false });
        }
        let remaining = (row.created_at + chrono::Duration::seconds(i64::from(MAX_APPROVAL_WAIT_SECS)))
            .signed_duration_since(self.clock.now_utc()).to_std()
            .map_err(|_| GatewayError::InvalidInput)?;
        // UTC geri atlaması, en çok 300 sn sınırını genişletemez.
        if remaining.is_zero() || remaining > Duration::from_secs(u64::from(MAX_APPROVAL_WAIT_SECS)) {
            return Err(GatewayError::InvalidInput.into());
        }
        let remaining = self.approvals.remaining_wait(approval_id)
            .map_err(|_| GatewayError::Unavailable)?.map_or(remaining, |wait| wait.min(remaining));
        if remaining.is_zero() { return Err(GatewayError::InvalidInput.into()); }
        let deadline = self.clock.now_monotonic() + remaining;
        let keys = {
            let mut nonces = self.nonces.lock().map_err(|_| GatewayError::Unavailable)?;
            let now = self.clock.now_monotonic();
            if let Some(message_id) = nonces.values()
                .filter(|entry| entry.approval_id == approval_id && entry.user_id == user_id
                    && entry.deadline > now && !entry.consumed)
                .find_map(|entry| entry.message_id)
            {
                return Ok(ApprovalNotification { message_id, actionable: true });
            }
            if nonces.len() + 2 > MAX_CALLBACK_NONCES {
                // Bitmiş kararlar veya artık izinli olmayan alıcılar canlı onayı engellemez.
                let users = &self.settings.read().map_err(|_| GatewayError::Unavailable)?.allowed_user_ids;
                let pending = self.approvals.pending_ids().map_err(|_| GatewayError::Unavailable)?;
                nonces.retain(|_, capability| capability.deadline > now
                    && users.contains(&capability.user_id) && pending.contains(&capability.approval_id));
            }
            if nonces.len() + 2 > MAX_CALLBACK_NONCES { return Err(GatewayError::Busy.into()); }
            let mut keys = Vec::with_capacity(2);
            for approve in [true, false] {
                // İki bağımsız v4 UUID'den 48 hex: 180 rastgele bit (>128).
                let key = loop {
                    let random = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
                    let key = format!("{}:{}", if approve { "a" } else { "d" }, &random[..48]);
                    if !nonces.contains_key(&key) { break key; }
                };
                nonces.insert(key.clone(), Capability {
                    approval_id: row.id.clone(), user_id, chat_id: private_chat_id,
                    message_id: None, approve, deadline, retain_until: deadline + TOMBSTONE_GRACE,
                    consumed: false,
                });
                keys.push(key);
            }
            keys
        };
        let sent = self.transport.send_message(SendMessage {
            chat_id: private_chat_id,
            text: {
                let summary = row.telegram_summary();
                if summary.is_empty() {
                    format!("Araç onayı gerekiyor: {}.", category_label(row.category))
                } else {
                    format!("Araç onayı gerekiyor: {} ({}).", category_label(row.category), summary)
                }
            },
            disable_notification: false,
            reply_markup: Some(InlineKeyboardMarkup { inline_keyboard: vec![vec![
                InlineKeyboardButton { text: "Onayla".into(), callback_data: keys[0].clone() },
                InlineKeyboardButton { text: "Reddet".into(), callback_data: keys[1].clone() },
            ]] }),
        }).await;
        let mut nonces = self.nonces.lock().map_err(|_| GatewayError::Unavailable)?;
        match sent {
            Ok(sent) if sent.message_id > 0 => {
                for key in keys {
                    if let Some(capability) = nonces.get_mut(&key) { capability.message_id = Some(sent.message_id); }
                }
                Ok(ApprovalNotification { message_id: sent.message_id, actionable: true })
            }
            result => {
                for key in keys { nonces.remove(&key); }
                match result {
                    Err(error) => Err(CallbackError::Transport(error)),
                    Ok(_) => Err(GatewayError::InvalidInput.into()),
                }
            }
        }
    }

    /// Üçlü auth burada tekrar yapılır; Telegram'dan gelen karar biti, approval ID
    /// veya hedef ID asla doğrudan çözücüye aktarılmaz.
    pub async fn handle_callback(
        &self,
        update_id: i64,
        query: &TelegramCallbackQuery,
    ) -> Result<CallbackReport, GatewayError> {
        if update_id < 0 || query.from.is_bot || query.inline_message_id.is_some()
            || query.id.is_empty() || query.id.len() > 256
        { return Err(GatewayError::Unauthorized); }
        let message = query.message.as_ref().ok_or(GatewayError::Unauthorized)?;
        if message.chat.kind != "private" || message.message_id <= 0 {
            return Err(GatewayError::Unauthorized);
        }
        self.authorized(query.from.id, message.chat.id)?;
        self.prune_expired()?;
        let key = query.data.as_deref().filter(|key| key.len() == 50)
            .ok_or(GatewayError::InvalidInput)?;
        let capability = {
            let mut nonces = self.nonces.lock().map_err(|_| GatewayError::Unavailable)?;
            let capability = nonces.get(key).ok_or(GatewayError::InvalidInput)?.clone();
            if capability.user_id != query.from.id || capability.chat_id != message.chat.id
                || capability.message_id != Some(message.message_id)
            { return Err(GatewayError::Unauthorized); }
            // Süresi dolan düğme karar veremez; yalnız çözülmüş kararı gösterebilir.
            let expired = capability.deadline <= self.clock.now_monotonic();
            // Kardeş nonce'lar "işleniyor" sayılır: tekrar DB'yi değiştiremez. Çözüm geçici
            // hata verir ve onay hâlâ beklemedeyse aşağıda geri bırakılır.
            if !expired {
                for sibling in nonces.values_mut().filter(|entry| entry.approval_id == capability.approval_id) {
                    sibling.consumed = true;
                }
            }
            (capability, expired)
        };
        let (capability, expired) = capability;
        let before = match self.approval(&capability.approval_id) {
            Ok(row) if row.is_telegram_tool() => row,
            Ok(_) => { self.release(&capability.approval_id); return Err(GatewayError::Unauthorized); }
            Err(error) => { self.release(&capability.approval_id); return Err(error); }
        };
        if expired && before.status == ApprovalStatus::Pending { return Err(GatewayError::InvalidInput); }
        let already_decided = capability.consumed || before.status != ApprovalStatus::Pending;
        if !already_decided {
            let result = self.application.resolve_tool_approval(
                TurnOrigin::Telegram { user_id: query.from.id, private_chat_id: message.chat.id, update_id },
                &capability.approval_id, capability.approve,
            ).await;
            if let Err(error) = result {
                // Masaüstü yarışının veya yayın hatasının gerçek DB kararı korunur.
                let pending = self.approval(&capability.approval_id)
                    .map_or(true, |row| row.status == ApprovalStatus::Pending);
                if pending {
                    self.release(&capability.approval_id);
                    let _ = self.transport.answer_callback_query(AnswerCallbackQuery {
                        callback_query_id: query.id.clone(),
                        text: "Onay işlenemedi, tekrar deneyin.".into(),
                    }).await;
                    return Err(error);
                }
            }
        }
        let status = self.approval(&capability.approval_id)?.status;
        let answer_error = self.transport.answer_callback_query(AnswerCallbackQuery {
            callback_query_id: query.id.clone(),
            text: if already_decided && status != ApprovalStatus::Pending {
                format!("Zaten karara bağlandı. {}", decision_label(status))
            } else { decision_label(status).into() },
        }).await.err();
        let edit_error = if status != ApprovalStatus::Pending {
            self.edit_decision(&capability.approval_id, status).await?
        } else { None };
        Ok(CallbackReport { status, already_decided, answer_error, edit_error })
    }

    /// Task 10 ApprovalResolved akışı/DB telafisi masaüstü kararında da çağırır.
    pub async fn refresh_resolved(&self, approval_id: &str) -> Result<Option<TransportError>, GatewayError> {
        let row = self.approval(approval_id)?;
        if row.status == ApprovalStatus::Pending { return Ok(None); }
        self.edit_decision(approval_id, row.status).await
    }

    async fn edit_decision(&self, approval_id: &str, status: ApprovalStatus) -> Result<Option<TransportError>, GatewayError> {
        let messages: HashSet<_> = {
            let mut nonces = self.nonces.lock().map_err(|_| GatewayError::Unavailable)?;
            nonces.values_mut().filter(|entry| entry.approval_id == approval_id).filter_map(|entry| {
                entry.consumed = true;
                entry.message_id.map(|message_id| (entry.user_id, entry.chat_id, message_id))
            }).collect()
        };
        let mut error = None;
        for (user_id, chat_id, message_id) in messages {
            if self.authorized(user_id, chat_id).is_err() { continue; }
            if let Err(failure) = self.transport.edit_message_text(EditMessageText {
                chat_id, message_id, text: decision_label(status).into(),
                reply_markup: InlineKeyboardMarkup { inline_keyboard: vec![] },
            }).await { error = Some(failure); }
        }
        Ok(error)
    }
}

fn decision_label(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "Onay işleniyor.",
        ApprovalStatus::Approved => "Onaylandı.",
        ApprovalStatus::Denied => "Reddedildi.",
        ApprovalStatus::Expired => "Onay süresi doldu.",
    }
}

fn category_label(category: RiskCategory) -> &'static str {
    match category {
        RiskCategory::Delete => "silme",
        RiskCategory::GitHistory => "Git geçmişi veya uzak depo",
        RiskCategory::Deploy => "dağıtım",
        RiskCategory::Finance => "finans erişimi",
        RiskCategory::Secrets => "gizli bilgi erişimi",
        RiskCategory::Install => "kurulum",
        RiskCategory::SystemConfig => "sistem ayarı",
        RiskCategory::Publish => "yayınlama",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use chrono::{DateTime, Utc};
    use ultron_core::{Db, EngineId, EventLog, Paths};
    use ultron_core::db::approvals::{ApprovalKind, ResolvedBy};
    use ultron_core::ipc::{ChatInfo, RouteResult, TelegramErrorCode};
    use ultron_core::policy::Action;
    use ultron_core::quota::QuotaReport;
    use crate::server::approvals::SharedToolApprovalResolver;
    use crate::turn_service::ToolApprovalResolver;
    use super::super::contracts::{
        AcceptedTurn, BoxFuture, GatewayStatusSnapshot, ResumeQuotaStep, SentMessage,
        SubmitUserMessage, TelegramUpdate, TelegramUser,
    };

    struct FakeClock { utc: Mutex<DateTime<Utc>>, monotonic: Mutex<Instant> }
    impl Clock for FakeClock {
        fn now_utc(&self) -> DateTime<Utc> { *self.utc.lock().unwrap() }
        fn now_monotonic(&self) -> Instant { *self.monotonic.lock().unwrap() }
        fn sleep_until(&self, _: Instant) -> BoxFuture<'_, ()> { panic!("saat beklemesi yok") }
    }
    impl FakeClock {
        fn advance(&self, seconds: u64) {
            *self.utc.lock().unwrap() += chrono::Duration::seconds(seconds as i64);
            *self.monotonic.lock().unwrap() += Duration::from_secs(seconds);
        }
    }

    #[derive(Default)]
    struct MockTransport {
        sent: Mutex<Vec<serde_json::Value>>,
        edited: Mutex<Vec<serde_json::Value>>,
        answers: Mutex<Vec<serde_json::Value>>,
        next: AtomicI64,
        fail_send: AtomicBool,
        fail_answer: AtomicBool,
        fail_edit: AtomicBool,
    }
    fn failure() -> TransportError {
        TransportError { code: TelegramErrorCode::Network, retry_after_seconds: None }
    }
    impl TelegramTransport for MockTransport {
        fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>> { panic!("ağ yok") }
        fn get_updates(&self, _: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>> { panic!("ağ yok") }
        fn send_message(&self, message: SendMessage) -> BoxFuture<'_, Result<SentMessage, TransportError>> {
            Box::pin(async move {
                self.sent.lock().unwrap().push(serde_json::to_value(message).unwrap());
                if self.fail_send.load(Ordering::SeqCst) { return Err(failure()); }
                Ok(SentMessage { message_id: self.next.fetch_add(1, Ordering::SeqCst) + 1 })
            })
        }
        fn edit_message_text(&self, message: EditMessageText) -> BoxFuture<'_, Result<(), TransportError>> {
            Box::pin(async move {
                self.edited.lock().unwrap().push(serde_json::to_value(message).unwrap());
                if self.fail_edit.load(Ordering::SeqCst) { Err(failure()) } else { Ok(()) }
            })
        }
        fn answer_callback_query(&self, message: AnswerCallbackQuery) -> BoxFuture<'_, Result<(), TransportError>> {
            Box::pin(async move {
                self.answers.lock().unwrap().push(serde_json::to_value(message).unwrap());
                if self.fail_answer.load(Ordering::SeqCst) { Err(failure()) } else { Ok(()) }
            })
        }
    }

    struct App { resolver: SharedToolApprovalResolver, calls: AtomicUsize, fail_next: AtomicBool }
    impl GatewayApplication for App {
        fn submit_user_message(&self, _: SubmitUserMessage) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> { panic!("tur yok") }
        fn resume_quota_step(&self, _: ResumeQuotaStep) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> { panic!("tur yok") }
        fn resolve_tool_approval<'a>(&'a self, origin: TurnOrigin, id: &'a str, approve: bool) -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Box::pin(async { Err(GatewayError::Unavailable) });
            }
            self.resolver.resolve(origin, id, approve)
        }
        fn status_snapshot(&self) -> BoxFuture<'_, Result<GatewayStatusSnapshot, GatewayError>> { panic!("durum yok") }
        fn list_chats(&self) -> BoxFuture<'_, Result<Vec<ChatInfo>, GatewayError>> { panic!("sohbet yok") }
        fn route<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, Result<RouteResult, GatewayError>> { panic!("yönlendirme yok") }
        fn quota_report(&self) -> BoxFuture<'_, Result<QuotaReport, GatewayError>> { panic!("kota yok") }
        fn get_binding(&self, _: i64) -> BoxFuture<'_, Result<Option<String>, GatewayError>> { panic!("bağlama yok") }
        fn set_binding<'a>(&'a self, _: i64, _: Option<&'a str>) -> BoxFuture<'a, Result<(), GatewayError>> { panic!("bağlama yok") }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        db: Arc<Db>,
        queue: Arc<Approvals>,
        settings: Arc<RwLock<TelegramConfig>>,
        clock: Arc<FakeClock>,
        transport: Arc<MockTransport>,
        app: Arc<App>,
        callbacks: Arc<ApprovalCallbacks>,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let paths = Paths::from_root(temp.path().join("synthetic"));
            paths.ensure().unwrap();
            let db = Arc::new(Db::open(&paths.db).unwrap());
            db.insert_chat("chat", None, "sentetik", Utc::now()).unwrap();
            let queue = Arc::new(Approvals::new(db.clone(), Arc::new(EventLog::new(db.clone(), paths.clone())), paths));
            let settings = Arc::new(RwLock::new(TelegramConfig { allowed_user_ids: vec![101, 202], ..Default::default() }));
            let clock = Arc::new(FakeClock { utc: Mutex::new(Utc::now()), monotonic: Mutex::new(Instant::now()) });
            let transport = Arc::new(MockTransport::default());
            let app = Arc::new(App { resolver: SharedToolApprovalResolver::new(queue.clone(), settings.clone()), calls: AtomicUsize::new(0), fail_next: AtomicBool::new(false) });
            let callbacks = Arc::new(ApprovalCallbacks::new(queue.clone(), app.clone(), transport.clone(), settings.clone(), clock.clone()));
            Self { _temp: temp, db, queue, settings, clock, transport, app, callbacks }
        }
        async fn row(&self) -> Approval {
            let mut events = self.queue.subscribe_global();
            let queue = self.queue.clone();
            tokio::spawn(async move {
                queue.check(
                    Action::FileDelete { path: "synthetic/dir/synthetic.txt".into() },
                    Some("chat".into()), EngineId::Claude, MAX_APPROVAL_WAIT_SECS,
                ).await.unwrap();
            });
            let requested = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            assert_eq!(requested.kind, "approval_requested");
            let id = requested.payload["approval_id"].as_str().unwrap();
            *self.clock.utc.lock().unwrap() = Utc::now();
            self.queue.get(id).unwrap().unwrap()
        }
        fn key(&self, notification: usize, approve: bool) -> String {
            self.transport.sent.lock().unwrap()[notification]["reply_markup"]["inline_keyboard"][0][usize::from(!approve)]["callback_data"]
                .as_str().unwrap().into()
        }
        fn query(&self, key: &str, user: i64, message: i64) -> TelegramCallbackQuery {
            serde_json::from_value(serde_json::json!({
                "id": "synthetic-query", "from": { "id": user, "is_bot": false },
                "message": { "message_id": message, "chat": { "id": user, "type": "private" } },
                "data": key,
            })).unwrap()
        }
        fn status(&self, row: &Approval) -> ApprovalStatus { self.db.get_approval(&row.id).unwrap().unwrap().status }
    }

    #[tokio::test]
    async fn telegram_decision_is_audited_and_callback_nonce_is_single_use() {
        let f = Fixture::new();
        let row = f.row().await;
        let first_message = f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        let repeated = f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        assert_eq!(repeated.message_id, first_message.message_id);
        f.callbacks.notify(&row.id, 202, 202).await.unwrap();
        let key = f.key(0, true);
        assert_eq!(key.len(), 50);
        assert!(!key.contains(&row.id));
        assert_ne!(key, f.key(1, true));
        let first = f.callbacks.handle_callback(1, &f.query(&key, 101, 1)).await.unwrap();
        assert_eq!(first.status, ApprovalStatus::Approved);
        assert!(!first.already_decided);
        let again = f.callbacks.handle_callback(2, &f.query(&key, 101, 1)).await.unwrap();
        assert!(again.already_decided);
        let sibling = f.callbacks.handle_callback(3, &f.query(&f.key(1, false), 202, 2)).await.unwrap();
        assert!(sibling.already_decided);
        assert_eq!(sibling.status, ApprovalStatus::Approved);
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.db.get_approval(&row.id).unwrap().unwrap().resolved_by, Some(ResolvedBy::Telegram));
        let audit = f.db.events_for_chat("chat", None).unwrap();
        assert!(audit.iter().any(|event| event.kind == "approval_resolved" && event.payload["by"] == "telegram"));
        for edit in f.transport.edited.lock().unwrap().iter() {
            assert_eq!(edit["text"], "Onaylandı.");
            assert_eq!(edit["reply_markup"]["inline_keyboard"], serde_json::json!([]));
        }
        assert!(f.transport.answers.lock().unwrap()[1]["text"].as_str().unwrap().contains("Zaten karara bağlandı"));
        let output = f.transport.sent.lock().unwrap()[0].to_string();
        assert!(output.contains("sil synthetic/dir/synthetic.txt"));
        assert!(!output.contains("parse_mode"));
        assert!(output.len() < 4096);
    }

    #[tokio::test]
    async fn callback_binds_identity_chat_message_and_decision() {
        let f = Fixture::new();
        let row = f.row().await;
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        let key = f.key(0, true);
        let mut variants = vec![f.query(&key, 202, 1), f.query(&key, 101, 2)];
        let mut group = f.query(&key, 101, 1); group.message.as_mut().unwrap().chat.kind = "group".into(); variants.push(group);
        let mut chat = f.query(&key, 101, 1); chat.message.as_mut().unwrap().chat.id = 202; variants.push(chat);
        let mut bot = f.query(&key, 101, 1); bot.from.is_bot = true; variants.push(bot);
        let mut inline = f.query(&key, 101, 1); inline.inline_message_id = Some("synthetic".into()); variants.push(inline);
        let mut missing = f.query(&key, 101, 1); missing.message = None; variants.push(missing);
        let changed = key.replacen("a:", "d:", 1); variants.push(f.query(&changed, 101, 1));
        variants.push(f.query(&format!("a:{}", "0".repeat(48)), 101, 1));
        for query in variants {
            assert!(f.callbacks.handle_callback(1, &query).await.is_err());
            assert_eq!(f.status(&row), ApprovalStatus::Pending);
        }
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 0);
        f.callbacks.handle_callback(2, &f.query(&key, 101, 1)).await.unwrap();
        assert_eq!(f.status(&row), ApprovalStatus::Approved);
    }

    #[tokio::test]
    async fn expiry_restart_expired_approval_and_removed_user_are_rejected() {
        for scenario in ["expiry", "restart", "expired_approval", "removed_user"] {
            let f = Fixture::new();
            let row = f.row().await;
            f.callbacks.notify(&row.id, 101, 101).await.unwrap();
            let query = f.query(&f.key(0, true), 101, 1);
            match scenario {
                "expiry" => f.clock.advance(300),
                "restart" => { f.callbacks.clear().unwrap(); f.queue.recover_pending().unwrap(); },
                "expired_approval" => { f.queue.resolve(&row.id, false, ResolvedBy::Restart).unwrap(); },
                _ => f.settings.write().unwrap().allowed_user_ids.clear(),
            }
            let result = f.callbacks.handle_callback(1, &query).await;
            if scenario == "expired_approval" {
                let report = result.unwrap();
                assert_eq!(report.status, ApprovalStatus::Expired);
                assert!(report.already_decided);
            } else { assert!(result.is_err(), "{scenario}"); }
            assert_ne!(f.status(&row), ApprovalStatus::Approved);
            assert_eq!(f.app.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn failed_delivery_gives_no_capability_and_edit_failure_keeps_decision() {
        let f = Fixture::new();
        let row = f.row().await;
        f.transport.fail_send.store(true, Ordering::SeqCst);
        assert!(f.callbacks.notify(&row.id, 101, 101).await.is_err());
        let lost = f.key(0, true);
        assert!(f.callbacks.handle_callback(1, &f.query(&lost, 101, 1)).await.is_err());
        assert!(f.callbacks.nonces.lock().unwrap().is_empty());
        f.transport.fail_send.store(false, Ordering::SeqCst);
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        f.transport.fail_answer.store(true, Ordering::SeqCst);
        f.transport.fail_edit.store(true, Ordering::SeqCst);
        let key = f.key(1, false);
        let report = f.callbacks.handle_callback(2, &f.query(&key, 101, 1)).await.unwrap();
        assert_eq!(report.status, ApprovalStatus::Denied);
        assert!(report.answer_error.is_some() && report.edit_error.is_some());
        f.callbacks.handle_callback(3, &f.query(&key, 101, 1)).await.unwrap();
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.status(&row), ApprovalStatus::Denied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_telegram_race_first_decision_wins() {
        let f = Fixture::new();
        let row = f.row().await;
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        let query = f.query(&f.key(0, true), 101, 1);
        let queue = f.queue.clone();
        let id = row.id.clone();
        let desktop = tokio::task::spawn_blocking(move || queue.resolve(&id, false, ResolvedBy::Approver));
        let telegram = f.callbacks.handle_callback(1, &query).await.unwrap();
        let desktop = desktop.await.unwrap();
        assert!(matches!(telegram.status, ApprovalStatus::Approved | ApprovalStatus::Denied));
        assert_eq!(telegram.status, f.status(&row));
        assert_eq!(desktop.is_ok(), telegram.status == ApprovalStatus::Denied);
        let duplicate = f.callbacks.handle_callback(2, &query).await.unwrap();
        assert!(duplicate.already_decided);
        assert_eq!(duplicate.status, telegram.status);
    }

    #[tokio::test]
    async fn desktop_decision_is_reported_and_refresh_removes_buttons() {
        let f = Fixture::new();
        let row = f.row().await;
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        f.queue.resolve(&row.id, false, ResolvedBy::Approver).unwrap();
        f.callbacks.refresh_resolved(&row.id).await.unwrap();
        let report = f.callbacks.handle_callback(1, &f.query(&f.key(0, true), 101, 1)).await.unwrap();
        assert_eq!(report.status, ApprovalStatus::Denied);
        assert!(report.already_decided);
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.transport.edited.lock().unwrap()[0]["text"], "Reddedildi.");
    }

    #[tokio::test]
    async fn transient_resolve_failure_releases_nonces_and_answers() {
        let f = Fixture::new();
        let row = f.row().await;
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        let key = f.key(0, true);
        f.app.fail_next.store(true, Ordering::SeqCst);
        assert!(f.callbacks.handle_callback(1, &f.query(&key, 101, 1)).await.is_err());
        assert_eq!(f.status(&row), ApprovalStatus::Pending);
        assert_eq!(f.transport.answers.lock().unwrap().len(), 1);
        assert!(f.callbacks.nonces.lock().unwrap().values().all(|entry| !entry.consumed));
        let retry = f.callbacks.handle_callback(2, &f.query(&key, 101, 1)).await.unwrap();
        assert_eq!(retry.status, ApprovalStatus::Approved);
        assert!(!retry.already_decided);
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn timeout_after_deadline_still_edits_message_within_grace() {
        let f = Fixture::new();
        let row = f.row().await;
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
        let key = f.key(0, true);
        f.clock.advance(300);
        f.callbacks.prune_expired().unwrap();
        // Süresi dolan düğme hâlâ bekleyen onayı çözemez.
        assert!(f.callbacks.handle_callback(1, &f.query(&key, 101, 1)).await.is_err());
        assert_eq!(f.app.calls.load(Ordering::SeqCst), 0);
        f.queue.resolve(&row.id, false, ResolvedBy::Timeout).unwrap();
        f.callbacks.refresh_resolved(&row.id).await.unwrap();
        let edited = f.transport.edited.lock().unwrap();
        assert_eq!(edited.len(), 1);
        assert_eq!(edited[0]["text"], "Reddedildi.");
        assert_eq!(edited[0]["reply_markup"]["inline_keyboard"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn telegram_cannot_resolve_elevation_unknown_or_admin_actions() {
        let f = Fixture::new();
        let unknown = f.db.create_approval(None, EngineId::Claude, RiskCategory::Delete, "synthetic",
            &Action::FileDelete { path: "synthetic.txt".into() }).unwrap();
        let elevation = f.db.create_turn_elevation_approval("chat", EngineId::Claude, "synthetic",
            &Action::ToolCall { tool: "ultron.turn.elevate".into(), input: serde_json::json!({}) }).unwrap();
        let admin = f.db.create_policy_tool_approval(None, EngineId::Claude, RiskCategory::Delete, "synthetic",
            &Action::ToolCall { tool: "ultron.trash.purge".into(), input: serde_json::json!({}) }, None).unwrap();
        for row in [unknown, elevation, admin] {
            assert!(!row.is_telegram_tool());
            let notification = f.callbacks.notify(&row.id, 101, 101).await.unwrap();
            assert!(!notification.actionable);
            let result = f.app.resolver.resolve(TurnOrigin::Telegram { user_id: 101, private_chat_id: 101, update_id: 1 }, &row.id, true).await;
            assert_eq!(result, Err(GatewayError::Unauthorized));
            assert_eq!(f.status(&row), ApprovalStatus::Pending);
        }
        assert!(f.callbacks.nonces.lock().unwrap().is_empty());
        for sent in f.transport.sent.lock().unwrap().iter() {
            assert!(sent.get("reply_markup").is_none());
            assert_eq!(sent["text"], "Bu işlem için masaüstünde onay gerekli.");
        }
        assert_eq!(ApprovalKind::default(), ApprovalKind::Unknown);
    }

    #[tokio::test]
    async fn nonce_capacity_is_bounded_and_expiry_releases_it() {
        let f = Fixture::new();
        let row = f.row().await;
        let template = Capability { approval_id: row.id.clone(), user_id: 202, chat_id: 202,
            message_id: Some(1), approve: true, deadline: f.clock.now_monotonic() + Duration::from_secs(1),
            retain_until: f.clock.now_monotonic() + Duration::from_secs(1) + TOMBSTONE_GRACE, consumed: false };
        for index in 0..MAX_CALLBACK_NONCES {
            f.callbacks.nonces.lock().unwrap().insert(index.to_string(), template.clone());
        }
        assert!(matches!(f.callbacks.notify(&row.id, 101, 101).await, Err(CallbackError::Gateway(GatewayError::Busy))));
        f.clock.advance(1);
        // Süresi dolmuş mezar taşları grace boyunca kalır, ama kapasite baskısında feda edilir.
        f.callbacks.prune_expired().unwrap();
        assert_eq!(f.callbacks.nonces.lock().unwrap().len(), MAX_CALLBACK_NONCES);
        f.clock.advance(TOMBSTONE_GRACE.as_secs());
        f.callbacks.prune_expired().unwrap();
        assert!(f.callbacks.nonces.lock().unwrap().is_empty());
        // Onayın kendi 300 sn penceresi için UTC saati yeniden hizalanır.
        *f.clock.utc.lock().unwrap() = Utc::now();
        f.callbacks.notify(&row.id, 101, 101).await.unwrap();
    }

    #[tokio::test]
    async fn finished_approvals_do_not_starve_new_callbacks_at_nonce_capacity() {
        let f = Fixture::new();
        let old = f.row().await;
        let new = f.row().await;
        let now = f.clock.now_monotonic();
        let stale = Capability { approval_id: old.id.clone(), user_id: 101, chat_id: 101,
            message_id: Some(1), approve: true, deadline: now + Duration::from_secs(30),
            retain_until: now + TOMBSTONE_GRACE, consumed: false };
        for index in 0..MAX_CALLBACK_NONCES {
            f.callbacks.nonces.lock().unwrap().insert(index.to_string(), stale.clone());
        }
        f.queue.resolve(&old.id, false, ResolvedBy::Approver).unwrap();
        assert!(f.callbacks.notify(&new.id, 101, 101).await.unwrap().actionable);
        assert_eq!(f.callbacks.nonces.lock().unwrap().len(), 2);
        let key = f.key(0, true);
        assert_eq!(f.callbacks.handle_callback(1, &f.query(&key, 101, 1)).await.unwrap().status, ApprovalStatus::Approved);
    }

    #[tokio::test(start_paused = true)]
    async fn telegram_capacity_preserves_the_fifth_approval_for_desktop() {
        let f = Fixture::new();
        let mut events = f.queue.subscribe_global();
        let mut checks = tokio::task::JoinSet::new();
        let mut ids = Vec::new();
        for index in 0..=MAX_NOTIFIABLE_APPROVALS + 1 {
            let queue = f.queue.clone();
            checks.spawn(async move {
                let command = if index == 0 { "rm synthetic.txt; echo done" } else { "rm synthetic.txt" };
                queue.check_owned(Action::Shell { command: command.into(), cwd: "workspace/harness".into() },
                    Some("chat".into()), EngineId::Claude, 30, Some("synthetic-requester")).await.unwrap()
            });
            let requested = events.recv().await.unwrap();
            assert_eq!(requested.kind, "approval_requested");
            let id = requested.payload["approval_id"].as_str().unwrap();
            *f.clock.utc.lock().unwrap() = Utc::now();
            let offered = f.callbacks.notify(id, 101, 101).await.unwrap();
            assert_eq!(offered.actionable, (1..=MAX_NOTIFIABLE_APPROVALS).contains(&index));
            let sent = f.transport.sent.lock().unwrap();
            if offered.actionable {
                assert!(sent[index]["text"].as_str().unwrap().contains("rm synthetic.txt @harness"));
            } else {
                assert!(sent[index]["reply_markup"].is_null());
            }
            ids.push(id.to_owned());
        }
        assert_eq!(f.callbacks.nonces.lock().unwrap().len(), 2 * MAX_NOTIFIABLE_APPROVALS);
        assert_eq!(f.queue.pending_ids().unwrap().len(), MAX_NOTIFIABLE_APPROVALS + 2);
        f.queue.resolve(&ids[1], false, ResolvedBy::Desktop).unwrap();
        let fifth = ids.last().unwrap();
        assert!(!f.callbacks.notify(fifth, 101, 101).await.unwrap().actionable);
        assert_eq!(f.app.resolve_tool_approval(
            TurnOrigin::Telegram { user_id: 101, private_chat_id: 101, update_id: 1 }, fifth, true,
        ).await, Err(GatewayError::Unauthorized));
        for id in ids.iter().filter(|id| *id != &ids[1]) {
            let row = f.queue.resolve(id, false, ResolvedBy::Desktop).unwrap();
            assert_eq!(row.resolved_by, Some(ResolvedBy::Desktop));
        }
        while let Some(result) = checks.join_next().await {
            assert!(matches!(result.unwrap(), ultron_core::policy::Decision::Deny { .. }));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn notification_does_not_extend_waiter_and_delete_redirect_is_preserved() {
        let f = Fixture::new();
        for approve in [true, false] {
            let queue = f.queue.clone();
            let check = tokio::spawn(async move {
                queue.check_owned(Action::FileDelete { path: "synthetic.txt".into() },
                    Some("chat".into()), EngineId::Claude, 5, Some("synthetic-requester")).await.unwrap()
            });
            tokio::task::yield_now().await;
            let row = f.db.list_approvals(Some(ApprovalStatus::Pending)).unwrap().pop().unwrap();
            assert_eq!(row.kind, ApprovalKind::PolicyTool);
            *f.clock.utc.lock().unwrap() = Utc::now();
            let index = f.transport.sent.lock().unwrap().len();
            f.callbacks.notify(&row.id, 101, 101).await.unwrap();
            let key = f.key(index, true);
            if approve {
                f.callbacks.handle_callback(1, &f.query(&key, 101, index as i64 + 1)).await.unwrap();
            } else {
                tokio::time::advance(Duration::from_secs(5)).await;
                // Monotonic callback clock still has time, but original waiter wins.
                let _ = f.callbacks.handle_callback(2, &f.query(&key, 101, index as i64 + 1)).await;
            }
            let result = check.await.unwrap();
            if approve {
                assert!(matches!(result, ultron_core::policy::Decision::Deny { reason } if reason.contains("ultron trash")));
                assert_eq!(f.status(&row), ApprovalStatus::Approved);
            } else {
                assert!(matches!(result, ultron_core::policy::Decision::Deny { .. }));
                assert_eq!(f.status(&row), ApprovalStatus::Denied);
            }
        }
    }

    #[tokio::test]
    async fn shared_resolver_revalidates_identity_and_local_capability_handles_elevation() {
        let f = Fixture::new();
        let row = f.row().await;
        for (user_id, private_chat_id, update_id) in [(303, 303, 1), (101, 202, 1), (101, 101, -1), (0, 0, 1)] {
            assert_eq!(f.app.resolver.resolve(TurnOrigin::Telegram { user_id, private_chat_id, update_id }, &row.id, true).await,
                Err(GatewayError::Unauthorized));
        }
        f.settings.write().unwrap().allowed_user_ids.clear();
        assert_eq!(f.app.resolver.resolve(TurnOrigin::Telegram { user_id: 101, private_chat_id: 101, update_id: 1 }, &row.id, true).await,
            Err(GatewayError::Unauthorized));
        assert!(matches!(f.callbacks.notify(&row.id, 101, 101).await, Err(CallbackError::Gateway(GatewayError::Unauthorized))));
        assert!(f.transport.sent.lock().unwrap().is_empty());
        let elevation = f.db.create_turn_elevation_approval("chat", EngineId::Claude, "synthetic",
            &Action::ToolCall { tool: "ultron.turn.elevate".into(), input: serde_json::json!({}) }).unwrap();
        assert_eq!(f.app.resolver.resolve(TurnOrigin::Local, &elevation.id, true).await, Ok(ApprovalStatus::Approved));
        assert_eq!(f.status(&row), ApprovalStatus::Pending);
    }
}
