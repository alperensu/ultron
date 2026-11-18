//! Tek poller, sıralı kabul ve sınırlı çıkış kuyruğu. LLM completion'ı poll
//! yolunda beklenmez; yayın kaybı kalıcı olaylar ve onay tablosundan telafi edilir.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use ultron_core::{Db, EventLog};
use ultron_core::config::TelegramConfig;
use ultron_core::db::approvals::ApprovalStatus;
use ultron_core::ipc::TelegramErrorCode;
use crate::server::approvals::Approvals;
use super::approval_callbacks::CallbackError;
use super::client::{BotApiClient, BotApiError};
use super::commands::{CommandProcessor, CommandResult};
use super::contracts::*;
use super::control::{DesktopPresence, GatewayHealth};
use super::notifications::{self, InFlight, NotificationCursor, OriginReader, Outbound};

const OUTBOUND_CAPACITY: usize = 64;
const MAX_ACTIVE_USERS: usize = 32;
/// Komut yanıtları için replay'in dokunmadığı boş kuyruk payı.
const OUTBOUND_RESERVE: usize = 16;
const APPROVAL_RETRY_CAP_SECS: u64 = 300;
const APPROVAL_SEND_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_RECIPIENT_INTERVAL: Duration = Duration::from_secs(1);

// Task 10 için sağlık sözleşmesi: poller terminal hatayla (InvalidToken, PollConflict/409,
// Unauthorized, ResponseTooLarge->InvalidResponse) biterken `verified=false` ve `error=Some(kod)`
// yayınlar ve `Err` döner. Geçici hatada (Network/429/5xx) `verified` true kalır, yalnız `error`
// dolar. Lifecycle terminal durumda sabit aralıkla yeniden başlatmamalı, üstel geri çekilmeli
// ve InvalidToken/Unauthorized'da kullanıcı müdahalesini beklemelidir.

/// BotApiClient'in boyut aşımını genel InvalidResponse içinde kaybetmez.
pub trait PollSource: Send + Sync {
    fn poll(&self, offset: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, BotApiError>>;
}
impl PollSource for BotApiClient {
    fn poll(&self, offset: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, BotApiError>> {
        Box::pin(self.get_updates_checked(offset))
    }
}

/// Task 6 adaptörü nonce/resolver/mesaj düzenleme davranışının tek sahibidir.
/// Çağıran burada da üçlü kimlik filtresi ve kalıcı update kabulü uygular.
pub trait GatewayCallbacks: Send + Sync {
    fn handle<'a>(&'a self, callback: &'a TelegramCallbackQuery, origin: TurnOrigin)
        -> BoxFuture<'a, Result<(), GatewayError>>;
    fn notify<'a>(&'a self, approval_id: &'a str, recipient: i64)
        -> BoxFuture<'a, Result<(), CallbackError>>;
}

pub struct GatewayDependencies {
    pub db: Arc<Db>,
    pub log: Arc<EventLog>,
    pub approvals: Arc<Approvals>,
    pub app: Arc<dyn GatewayApplication>,
    pub transport: Arc<dyn TelegramTransport>,
    pub poller: Arc<dyn PollSource>,
    pub callbacks: Arc<dyn GatewayCallbacks>,
    pub origins: Arc<dyn OriginReader>,
    pub clock: Arc<dyn Clock>,
    pub presence: Arc<DesktopPresence>,
    pub config: Arc<RwLock<TelegramConfig>>,
    pub health: watch::Sender<GatewayHealth>,
    /// getMe sonrası, ilk update öncesi TurnService::set_bot_identity bağlanır.
    pub on_verified: Arc<dyn Fn(i64) -> Result<(), GatewayError> + Send + Sync>,
}

#[derive(Default)]
pub struct GatewayReport {
    pub rejected_updates: u64,
    pub delivered_messages: u64,
    pub failed_deliveries: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayRunError { Application(GatewayError), Transport(TelegramErrorCode), ResponseTooLarge }
impl From<GatewayError> for GatewayRunError { fn from(error: GatewayError) -> Self { Self::Application(error) } }

/// Hiçbir reddedilen kimlik/içerik bu işlevden loga veya depoya taşınmaz.
pub fn authorized_update(update: &TelegramUpdate, allowed: &[i64]) -> Option<i64> {
    if update.update_id < 0 || update.update_id == i64::MAX || update.edited_message.is_some()
        || !update.extra.is_empty() { return None; }
    match (&update.message, &update.callback_query) {
        (Some(message), None) => {
            let user = message.from.as_ref()?;
            authorized_user(user, &message.chat, allowed).then_some(user.id)
        }
        (None, Some(callback)) => {
            if callback.inline_message_id.is_some() { return None; }
            let message = callback.message.as_ref()?;
            authorized_user(&callback.from, &message.chat, allowed).then_some(callback.from.id)
        }
        _ => None,
    }
}

fn authorized_user(user: &TelegramUser, chat: &TelegramChat, allowed: &[i64]) -> bool {
    user.id > 0 && !user.is_bot && allowed.contains(&user.id) && chat.kind == "private" && chat.id == user.id
}

struct Batch { updates: Vec<TelegramUpdate>, acknowledged: oneshot::Sender<Option<i64>> }

async fn poll_loop(deps: &GatewayDependencies, bot: i64, cancel: &CancellationToken, batches: mpsc::Sender<Batch>) -> Result<(), GatewayRunError> {
    // Ofset doğrulanmış bot kimliğine aittir. Kaydı olmayan bot ofsetsiz başlar;
    // Telegram'ın beklettiği güncellemeler mesaj tarihine bakılmadan işlenir.
    let mut offset = deps.db.telegram_next_update_id(bot).map_err(|_| GatewayError::Unavailable)?;
    let mut failures = 0u32;
    loop {
        if deps.config.read().map_err(|_| GatewayError::Unavailable)?.allowed_user_ids.is_empty() {
            return Ok(());
        }
        let result = tokio::select! { biased; () = cancel.cancelled() => return Ok(()), result = deps.poller.poll(offset) => result };
        match result {
            Ok(mut updates) => {
                failures = 0;
                deps.health.send_modify(|health| { health.verified = true; health.error = None; });
                updates.sort_by_key(|update| update.update_id);
                let empty = updates.is_empty();
                let (tx, rx) = oneshot::channel();
                tokio::select! { biased; () = cancel.cancelled() => return Ok(()), result = batches.send(Batch { updates, acknowledged: tx }) => if result.is_err() { return Ok(()); } }
                offset = tokio::select! { biased; () = cancel.cancelled() => return Ok(()), result = rx => match result { Ok(offset) => offset, Err(_) => return Ok(()) } };
                // Mock/boş anlık yanıt da hızlı poll döngüsü oluşturamaz.
                if empty { cancellable_sleep(deps.clock.as_ref(), Duration::from_millis(100), cancel).await; }
            }
            Err(BotApiError::ResponseTooLarge) => {
                deps.health.send_modify(|health| { health.verified = false; health.error = Some(TelegramErrorCode::InvalidResponse); });
                return Err(GatewayRunError::ResponseTooLarge);
            }
            Err(BotApiError::Transport(error)) => {
                let terminal = matches!(error.code, TelegramErrorCode::InvalidToken | TelegramErrorCode::PollConflict | TelegramErrorCode::Unauthorized);
                deps.health.send_modify(|health| { if terminal { health.verified = false; } health.error = Some(error.code); });
                if terminal {
                    return Err(GatewayRunError::Transport(error.code));
                }
                failures = failures.saturating_add(1);
                let seconds = error.retry_after_seconds.map(u64::from).unwrap_or_else(|| 1u64 << failures.min(6));
                cancellable_sleep(deps.clock.as_ref(), Duration::from_secs(seconds.max(1)), cancel).await;
            }
        }
    }
}

async fn cancellable_sleep(clock: &dyn Clock, duration: Duration, cancel: &CancellationToken) {
    tokio::select! { biased; () = cancel.cancelled() => {}, () = clock.sleep_until(clock.now_monotonic() + duration) => {} }
}

#[derive(Default)]
struct DeliveryCounters { delivered: AtomicU64, failed: AtomicU64 }

async fn send_loop(deps: &GatewayDependencies, cancel: &CancellationToken, mut receiver: mpsc::Receiver<Outbound>, counters: &DeliveryCounters, in_flight: &InFlight) -> Result<(), GatewayRunError> {
    let release = |id: &str| { if let Ok(mut set) = in_flight.lock() { set.remove(id); } };
    loop {
        let outbound = tokio::select! { biased; () = cancel.cancelled() => break, message = receiver.recv() => match message { Some(message) => message, None => break } };
        let logical = outbound.logical_turn_id;
        if !deps.config.read().map_err(|_| GatewayError::Unavailable)?.allowed_user_ids.contains(&outbound.message.chat_id) {
            if let Some(id) = &logical { release(id); }
            continue;
        }
        let result = tokio::select! { biased;
            // İptal: final Ready kalır ve sonraki çalıştırmada yeniden denenir.
            () = cancel.cancelled() => { if let Some(id) = &logical { release(id); } break; },
            result = deps.transport.send_message(outbound.message) => result,
        };
        let retry_delay = match &result {
            Err(error) if matches!(error.code, TelegramErrorCode::RateLimited | TelegramErrorCode::Network) =>
                Some(error.retry_after_seconds.map(u64::from).unwrap_or(2).clamp(1, 3600)),
            _ => None,
        };
        if let Some(id) = &logical {
            // Kesin gönderilmemiş/geçici hatada Ready kalır; yalnız belirsiz/terminal sonuç NeedsAttention olur.
            if result.is_ok() || retry_delay.is_none() {
                deps.db.finish_telegram_delivery(id, result.is_ok()).map_err(|_| GatewayError::Unavailable)?;
            }
            release(id);
        }
        match result {
            Ok(_) => { counters.delivered.fetch_add(1, Ordering::Relaxed); }
            Err(error) => {
                counters.failed.fetch_add(1, Ordering::Relaxed);
                deps.health.send_modify(|health| { health.error = Some(error.code); });
            }
        }
        if let Some(seconds) = retry_delay { cancellable_sleep(deps.clock.as_ref(), Duration::from_secs(seconds), cancel).await; }
    }
    Ok(())
}

/// Dolu kuyrukta final düşürülmez: Ready kalır, in_flight bırakılır, sonraki tick'te yeniden üretilir.
fn enqueue(deps: &GatewayDependencies, sender: &mpsc::Sender<Outbound>, in_flight: &InFlight, outbound: Outbound, counters: &DeliveryCounters) -> Result<(), GatewayError> {
    if let Err(error) = sender.try_send(outbound) {
        let outbound = error.into_inner();
        if let Some(id) = &outbound.logical_turn_id {
            if let Ok(mut set) = in_flight.lock() { set.remove(id); }
        } else {
            counters.failed.fetch_add(1, Ordering::Relaxed);
            deps.health.send_modify(|health| { health.error = Some(TelegramErrorCode::DeliveryUncertain); });
        }
    }
    Ok(())
}

fn live_allowed(deps: &GatewayDependencies) -> Result<Vec<i64>, GatewayError> {
    Ok(deps.config.read().map_err(|_| GatewayError::Unavailable)?.allowed_user_ids.clone())
}

#[derive(Default)]
struct ApprovalState {
    notified: HashSet<(String, i64)>,
    backoff: HashMap<(String, i64), (u32, std::time::Instant)>,
    recipient_next: HashMap<i64, std::time::Instant>,
}

/// Çağıran lifecycle eski future'ı cancel+join etmeden yenisini başlatmamalıdır.
/// Boş allowlist ilk ağ çağrısından önce döner. Credential okuma burada yoktur.
pub async fn run_gateway(deps: GatewayDependencies, cancel: CancellationToken) -> Result<GatewayReport, GatewayRunError> {
    let initially_empty = {
        let config = deps.config.read().map_err(|_| GatewayError::Unavailable)?;
        config.validate().map_err(|_| GatewayError::InvalidInput)?;
        config.allowed_user_ids.is_empty()
    };
    if initially_empty { return Ok(GatewayReport::default()); }
    let mut events = deps.log.subscribe();
    let mut approvals = deps.approvals.subscribe_global();
    let mut notifications = NotificationCursor::new(deps.db.clone(), deps.origins.clone());
    notifications.initialize()?;
    let me = tokio::select! { biased; () = cancel.cancelled() => return Ok(GatewayReport::default()), me = deps.transport.get_me() => me };
    let me = me.map_err(|error| { deps.health.send_modify(|health| health.error = Some(error.code)); GatewayRunError::Transport(error.code) })?;
    if me.id <= 0 || !me.is_bot { return Err(GatewayRunError::Transport(TelegramErrorCode::InvalidResponse)); }
    (deps.on_verified)(me.id)?;
    deps.health.send_replace(GatewayHealth { token_present: true, verified: true, error: None });
    let bot = me.id;
    let child_cancel = cancel.child_token();
    let (batch_tx, mut batch_rx) = mpsc::channel(1);
    let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let counters = DeliveryCounters::default();
    let polling = poll_loop(&deps, bot, &child_cancel, batch_tx);
    let in_flight = notifications.in_flight_handle();
    let sending = send_loop(&deps, &child_cancel, outbound_rx, &counters, &in_flight);
    tokio::pin!(polling, sending);
    let mut commands = CommandProcessor::new(deps.db.clone(), deps.app.clone(), deps.clock.clone());
    let mut active = HashMap::new();
    let mut completions = JoinSet::new();
    let (approval_requests, requested) = mpsc::channel(OUTBOUND_RESERVE);
    let approval_notices = approval_notification_loop(&deps, requested);
    let mut rejected = 0u64;
    let main_loop = async {
        loop {
            let tick = deps.clock.sleep_until(deps.clock.now_monotonic() + Duration::from_secs(1));
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                result = &mut polling => return result,
                result = &mut sending => return result,
                completion = completions.join_next(), if !completions.is_empty() => {
                    if let Some(Ok(user)) = completion { active.remove(&user); }
                }
                batch = batch_rx.recv() => {
                    let Some(batch) = batch else { continue; };
                    for update in batch.updates {
                        // Allowlist her güncellemede canlı okunur; kaldırılan kullanıcı restart beklemez.
                        let allowed = live_allowed(&deps)?;
                        let Some(user) = authorized_update(&update, &allowed) else {
                            rejected = rejected.saturating_add(1);
                            if let Some(next) = update.update_id.checked_add(1).filter(|next| *next >= 0) {
                                deps.db.checkpoint_telegram_poll(bot, next).map_err(|_| GatewayError::Unavailable)?;
                            }
                            continue;
                        };
                        // Kabul ve ofset birlikte kalıcıdır. Kurtarma yarım kalan işi
                        // işaretler; yan etkileri yinelememek için tekrar yürütmez.
                        if deps.db.telegram_next_update_id(bot).map_err(|_| GatewayError::Unavailable)?.is_some_and(|offset| update.update_id < offset) { continue; }
                        let busy = active.contains_key(&user) || active.len() >= MAX_ACTIVE_USERS;
                        let operation = async {
                            let result = if let Some(message) = &update.message {
                                commands.message(bot, update.update_id, message, busy).await?
                            } else if let Some(callback) = &update.callback_query {
                                let result = if let Some(result) = commands.selection(bot, update.update_id, callback, busy).await? { result }
                                else {
                                    if deps.db.accept_telegram_update(bot, update.update_id, user).map_err(|_| GatewayError::Unavailable)? {
                                        deps.db.start_telegram_inbox(bot, update.update_id).map_err(|_| GatewayError::Unavailable)?;
                                        let result = deps.callbacks.handle(callback, TurnOrigin::Telegram { user_id: user, private_chat_id: user, update_id: update.update_id }).await;
                                        deps.db.complete_telegram_update(bot, update.update_id).map_err(|_| GatewayError::Unavailable)?;
                                        if result.is_err() { enqueue(&deps, &outbound_tx, &in_flight, notifications::reply(user, "Onay kullanılamıyor; /onay ile yeniden kontrol edin."), &counters)?; }
                                    }
                                    CommandResult::default()
                                };
                                // Callback bildirimi başarısız olsa da kabul edilmiş tur replay edilmez.
                                let _ = deps.transport.answer_callback_query(AnswerCallbackQuery { callback_query_id: callback.id.clone(), text: "İşlendi.".into() }).await;
                                result
                            } else { CommandResult::default() };
                            for outbound in result.outbound { enqueue(&deps, &outbound_tx, &in_flight, outbound, &counters)?; }
                            if result.show_approvals {
                                let pending = deps.db.list_approvals(Some(ApprovalStatus::Pending)).map_err(|_| GatewayError::Unavailable)?;
                                let response = if !pending.iter().any(|approval| approval.is_policy_tool()) {
                                    "Telegram'dan çözülebilen bekleyen araç onayı yok."
                                } else if approval_requests.try_send(user).is_err() {
                                    "Onay bildirimleri sırada, lütfen bekleyin."
                                } else {
                                    "Bekleyen onayları bildirimlerinizden kontrol edin."
                                };
                                enqueue(&deps, &outbound_tx, &in_flight, notifications::reply(user, response), &counters)?;
                            }
                            if let Some(turn) = result.accepted {
                                active.insert(user, turn.logical_turn_id.clone());
                                completions.spawn(async move { let _ = turn.completion.await; user });
                            }
                            Ok::<(), GatewayError>(())
                        };
                        tokio::select! { biased; () = cancel.cancelled() => return Ok(()), result = operation => result? }
                    }
                    let next = deps.db.telegram_next_update_id(bot).map_err(|_| GatewayError::Unavailable)?;
                    let _ = batch.acknowledged.send(next);
                }
                _ = events.recv() => {},
                _ = approvals.recv() => {},
                () = tick => {},
            }
            let mode = deps.config.read().map_err(|_| GatewayError::Unavailable)?.notify;
            let allowed = live_allowed(&deps)?;
            let capacity = outbound_tx.capacity().saturating_sub(OUTBOUND_RESERVE);
            let replay = async {
                for message in notifications.replay(&allowed, mode, deps.presence.is_away(), capacity).await? { enqueue(&deps, &outbound_tx, &in_flight, message, &counters)?; }
                Ok::<(), GatewayError>(())
            };
            tokio::select! { biased; () = cancel.cancelled() => return Ok(()), result = replay => result? }
        }
    };
    let result = tokio::select! {
        biased;
        result = main_loop => result,
        result = approval_notices => result.map_err(Into::into),
    };
    child_cancel.cancel();
    // Futures aşağıda drop edilir; spawned completion bekleyicileri kesin join edilir.
    completions.shutdown().await;
    result?;
    Ok(GatewayReport { rejected_updates: rejected, delivered_messages: counters.delivered.load(Ordering::Relaxed), failed_deliveries: counters.failed.load(Ordering::Relaxed) })
}

async fn approval_notification_loop(deps: &GatewayDependencies, mut requests: mpsc::Receiver<i64>) -> Result<(), GatewayError> {
    let mut events = deps.approvals.subscribe_global();
    let mut state = ApprovalState::default();
    let mut explicit = HashSet::new();
    let mut next_send = deps.clock.now_monotonic();
    loop {
        let allowed = live_allowed(deps)?;
        explicit.retain(|user| allowed.contains(user));
        if deps.clock.now_monotonic() >= next_send
            && let Some(delay) = notify_approvals(deps, &allowed, &mut state, &mut explicit).await?
        {
            next_send = deps.clock.now_monotonic() + delay;
        }
        let now = deps.clock.now_monotonic();
        let wake = if next_send > now { next_send } else { now + Duration::from_secs(1) };
        tokio::select! {
            user = requests.recv() => match user {
                Some(user) => { explicit.insert(user); }
                None => return Ok(()),
            },
            _ = events.recv() => {},
            () = deps.clock.sleep_until(wake) => {},
        }
    }
}

/// Her adım tek ağ isteği yapar; 429 bütün onay gönderimlerini verilen süre kadar erteler.
async fn notify_approvals(deps: &GatewayDependencies, allowed: &[i64], state: &mut ApprovalState, explicit: &mut HashSet<i64>) -> Result<Option<Duration>, GatewayError> {
    let mut pending = deps.db.list_approvals(Some(ApprovalStatus::Pending)).map_err(|_| GatewayError::Unavailable)?;
    let now = deps.clock.now_monotonic();
    state.notified.retain(|(id, user)| allowed.contains(user) && pending.iter().any(|a| a.id == *id));
    state.backoff.retain(|(id, user), _| allowed.contains(user) && pending.iter().any(|a| a.id == *id));
    state.recipient_next.retain(|_, next| *next > now);
    pending.retain(|row| row.is_policy_tool());
    pending.sort_by_cached_key(|row| !row.is_telegram_tool());
    explicit.retain(|user| pending.iter().any(|row| !state.notified.contains(&(row.id.clone(), *user))));
    let mode = deps.config.read().map_err(|_| GatewayError::Unavailable)?.notify;
    let away = deps.presence.is_away();
    let mut selected = None;
    for approval in pending {
        let origins = if let Some(chat) = &approval.chat_id { deps.origins.origins_for_chat(chat).await? } else { Vec::new() };
        for user in allowed.iter().copied() {
            let remote = origins.iter().any(|origin| origin.user_id == user && origin.private_chat_id == user && origin.end_seq.is_none());
            let key = (approval.id.clone(), user);
            let requested = explicit.contains(&user);
            if (!requested && !notifications::should_notify(mode, away, remote, true))
                || state.notified.contains(&key) || state.recipient_next.contains_key(&user)
                || state.backoff.get(&key).is_some_and(|(_, next)| *next > now)
            { continue; }
            // Açık istekler ve turun sahibi, genel dağıtımı beklemez.
            let priority = (!requested, !remote);
            if selected.as_ref().is_none_or(|(best, _)| priority < *best) {
                selected = Some((priority, key));
            }
        }
    }
    let Some((_, key)) = selected else { return Ok(None); };
    let result = deps.callbacks.notify(&key.0, key.1).await;
    let now = deps.clock.now_monotonic();
    state.recipient_next.insert(key.1, now + APPROVAL_RECIPIENT_INTERVAL);
    let mut pace = APPROVAL_SEND_INTERVAL;
    match result {
        Ok(()) => {
            state.backoff.remove(&key);
            state.notified.insert(key);
        }
        Err(error) => {
            let failures = state.backoff.get(&key).map_or(0, |(n, _)| *n).saturating_add(1);
            let mut retry = Duration::from_secs((1u64 << failures.min(10)).min(APPROVAL_RETRY_CAP_SECS));
            if let CallbackError::Transport(transport) = error
                && transport.code == TelegramErrorCode::RateLimited
            {
                pace = Duration::from_secs(transport.retry_after_seconds.map(u64::from).unwrap_or(2).clamp(1, 3600));
                retry = retry.max(pace);
            }
            state.backoff.insert(key, (failures, now + retry));
        }
    }
    Ok(Some(pace))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(value: serde_json::Value) -> TelegramUpdate { serde_json::from_value(value).unwrap() }
    #[test]
    fn auth_filter_requires_user_private_chat_and_matching_chat_id() {
        let base = serde_json::json!({"update_id":1,"message":{"message_id":1,"from":{"id":101,"is_bot":false},"chat":{"id":101,"type":"private"},"text":"synthetic-private-content"}});
        assert_eq!(authorized_update(&update(base.clone()), &[101]), Some(101));
        for kind in ["group", "supergroup", "channel"] {
            let mut value = base.clone(); value["message"]["chat"]["type"] = kind.into();
            assert_eq!(authorized_update(&update(value), &[101]), None);
        }
        for (path, value) in [("from", serde_json::Value::Null), ("from", serde_json::json!({"id":102,"is_bot":false})), ("from", serde_json::json!({"id":101,"is_bot":true})), ("chat", serde_json::json!({"id":102,"type":"private"}))] {
            let mut invalid = base.clone(); invalid["message"][path] = value;
            assert_eq!(authorized_update(&update(invalid), &[101]), None);
        }
        let callback = serde_json::json!({"update_id":2,"callback_query":{"id":"synthetic","from":{"id":101,"is_bot":false},"message":{"message_id":1,"chat":{"id":101,"type":"private"}},"data":"sel:synthetic"}});
        assert_eq!(authorized_update(&update(callback.clone()), &[101]), Some(101));
        let mut invalid = callback.clone(); invalid["callback_query"]["message"] = serde_json::Value::Null;
        assert_eq!(authorized_update(&update(invalid), &[101]), None);
        let mut invalid = callback; invalid["callback_query"]["from"]["id"] = 102.into();
        assert_eq!(authorized_update(&update(invalid), &[101]), None);
        assert_eq!(authorized_update(&update(base), &[]), None);
    }

    use std::collections::VecDeque;
    use std::sync::Mutex;
    use ultron_core::{Paths, EngineId};
    use ultron_core::db::telegram::TelegramTurnOrigin;
    use ultron_core::ipc::{ChatInfo, RouteResult};
    use ultron_core::quota::QuotaReport;

    struct TestClock;
    impl Clock for TestClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> { chrono::Utc::now() }
        fn now_monotonic(&self) -> std::time::Instant { tokio::time::Instant::now().into_std() }
        fn sleep_until(&self, deadline: std::time::Instant) -> BoxFuture<'_, ()> {
            Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)))
        }
    }

    #[derive(Default)]
    struct Poller { script: Mutex<VecDeque<Result<Vec<TelegramUpdate>, BotApiError>>>, offsets: Mutex<Vec<Option<i64>>>, wake: tokio::sync::Notify }
    impl PollSource for Poller {
        fn poll(&self, offset: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, BotApiError>> {
            self.offsets.lock().unwrap().push(offset);
            Box::pin(async move {
                loop {
                    let next = self.script.lock().unwrap().pop_front();
                    if let Some(result) = next { return result; }
                    self.wake.notified().await;
                }
            })
        }
    }

    #[derive(Default)]
    struct Transport {
        sent: Mutex<Vec<(i64, String)>>, markups: Mutex<Vec<String>>, me_calls: Mutex<u32>,
        me_error: Mutex<Option<TransportError>>, failures: Mutex<VecDeque<TransportError>>,
        attempts: Mutex<u32>, delay_ms: Mutex<u64>,
    }
    impl TelegramTransport for Transport {
        fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>> {
            *self.me_calls.lock().unwrap() += 1;
            let error = *self.me_error.lock().unwrap();
            Box::pin(async move { match error { Some(error) => Err(error), None => Ok(TelegramUser { id: 100, is_bot: true }) } })
        }
        fn get_updates(&self, _: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>> { panic!("poller kullanılır") }
        fn send_message(&self, message: SendMessage) -> BoxFuture<'_, Result<SentMessage, TransportError>> {
            *self.attempts.lock().unwrap() += 1;
            let failure = self.failures.lock().unwrap().pop_front();
            let delay = *self.delay_ms.lock().unwrap();
            Box::pin(async move {
                if delay > 0 { tokio::time::sleep(Duration::from_millis(delay)).await; }
                if let Some(failure) = failure { return Err(failure); }
                if let Some(markup) = &message.reply_markup {
                    for row in &markup.inline_keyboard { for button in row { self.markups.lock().unwrap().push(button.callback_data.clone()); } }
                }
                self.sent.lock().unwrap().push((message.chat_id, message.text));
                Ok(SentMessage { message_id: 1 })
            })
        }
        fn edit_message_text(&self, _: EditMessageText) -> BoxFuture<'_, Result<(), TransportError>> { Box::pin(async { Ok(()) }) }
        fn answer_callback_query(&self, _: AnswerCallbackQuery) -> BoxFuture<'_, Result<(), TransportError>> { Box::pin(async { Ok(()) }) }
    }

    struct App { db: Arc<Db>, submits: Mutex<Vec<Option<String>>>, held: Mutex<Vec<oneshot::Sender<Result<TurnCompletion, GatewayError>>>>, quota_calls: AtomicU64 }
    impl GatewayApplication for App {
        fn submit_user_message(&self, request: SubmitUserMessage) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> {
            self.submits.lock().unwrap().push(request.chat_id.clone());
            let (tx, rx) = oneshot::channel();
            self.held.lock().unwrap().push(tx);
            Box::pin(async move { Ok(AcceptedTurn { logical_turn_id: "synthetic-turn".into(), chat_id: "c1".into(), user_event_seq: 1, completion: rx }) })
        }
        fn resume_quota_step(&self, _: ResumeQuotaStep) -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>> { panic!("devam yok") }
        fn resolve_tool_approval<'a>(&'a self, _: TurnOrigin, _: &'a str, _: bool) -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>> { panic!("onay yok") }
        fn status_snapshot(&self) -> BoxFuture<'_, Result<GatewayStatusSnapshot, GatewayError>> {
            Box::pin(async { Ok(GatewayStatusSnapshot { active_turns: vec![], pending_approvals: vec![], quota_waits: vec![], away: true }) })
        }
        fn list_chats(&self) -> BoxFuture<'_, Result<Vec<ChatInfo>, GatewayError>> {
            Box::pin(async { Ok(vec![ChatInfo { id: "c1".into(), title: "Sentetik sohbet".into(), project_id: None, pinned: false,
                archived: false, updated_at: "2026-01-01T00:00:00Z".into(), engine: Some(EngineId::Claude) }]) })
        }
        fn route<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, Result<RouteResult, GatewayError>> {
            Box::pin(async { Ok(RouteResult { project_id: None, confidence: 1.0, reason: String::new(), alternatives: vec![] }) })
        }
        fn quota_report(&self) -> BoxFuture<'_, Result<QuotaReport, GatewayError>> {
            self.quota_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(QuotaReport { fetched_at: chrono::Utc::now(), pools: vec![] }) })
        }
        fn get_binding(&self, user: i64) -> BoxFuture<'_, Result<Option<String>, GatewayError>> {
            Box::pin(async move { self.db.telegram_binding(user).map_err(|_| GatewayError::Unavailable) })
        }
        fn set_binding<'a>(&'a self, user: i64, chat: Option<&'a str>) -> BoxFuture<'a, Result<(), GatewayError>> {
            Box::pin(async move { self.db.set_telegram_binding(user, chat).map_err(|_| GatewayError::Unavailable) })
        }
    }

    #[derive(Default)]
    struct Callbacks {
        fail: std::sync::atomic::AtomicBool,
        calls: std::sync::atomic::AtomicU32,
        retry_after: std::sync::atomic::AtomicU32,
        hold_next: std::sync::atomic::AtomicBool,
        release: tokio::sync::Notify,
    }
    impl GatewayCallbacks for Callbacks {
        fn handle<'a>(&'a self, _: &'a TelegramCallbackQuery, _: TurnOrigin) -> BoxFuture<'a, Result<(), GatewayError>> { Box::pin(async { Ok(()) }) }
        fn notify<'a>(&'a self, _: &'a str, _: i64) -> BoxFuture<'a, Result<(), CallbackError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let fail = self.fail.load(Ordering::Relaxed);
            Box::pin(async move {
                if self.hold_next.swap(false, Ordering::Relaxed) { self.release.notified().await; }
                let retry_after = self.retry_after.swap(0, Ordering::Relaxed);
                if retry_after != 0 {
                    Err(CallbackError::Transport(transport_error(TelegramErrorCode::RateLimited, Some(retry_after))))
                } else if fail {
                    Err(GatewayError::Unavailable.into())
                } else {
                    Ok(())
                }
            })
        }
    }
    struct DbOrigins { db: Arc<Db>, ids: Mutex<Vec<String>> }
    impl OriginReader for DbOrigins {
        fn origins_for_chat<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<TelegramTurnOrigin>, GatewayError>> {
            let ids = self.ids.lock().unwrap().clone();
            Box::pin(async move { Ok(ids.iter().filter_map(|id| self.db.telegram_turn_origin(id).unwrap()).collect()) })
        }
    }

    struct Env {
        _temp: tempfile::TempDir, db: Arc<Db>, app: Arc<App>, poller: Arc<Poller>, transport: Arc<Transport>,
        deps_parts: (Arc<EventLog>, Arc<Approvals>), callbacks: Arc<Callbacks>, origins: Arc<DbOrigins>,
        config: Arc<RwLock<TelegramConfig>>, presence: Arc<DesktopPresence>, health: Mutex<Option<watch::Receiver<GatewayHealth>>>,
    }
    impl Env {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let paths = Paths::from_root(temp.path().join("synthetic"));
            paths.ensure().unwrap();
            let db = Arc::new(Db::open(&paths.db).unwrap());
            db.insert_chat("c1", None, "sentetik", chrono::Utc::now()).unwrap();
            let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
            let approvals = Arc::new(Approvals::new(db.clone(), log.clone(), paths));
            let app = Arc::new(App { db: db.clone(), submits: Mutex::default(), held: Mutex::default(), quota_calls: AtomicU64::default() });
            let clock: Arc<dyn Clock> = Arc::new(TestClock);
            Self { _temp: temp, origins: Arc::new(DbOrigins { db: db.clone(), ids: Mutex::default() }), db, app, poller: Arc::new(Poller::default()),
                transport: Arc::new(Transport::default()), deps_parts: (log, approvals), callbacks: Arc::new(Callbacks::default()),
                config: Arc::new(RwLock::new(TelegramConfig::default())), presence: Arc::new(DesktopPresence::new(clock)), health: Mutex::new(None) }
        }
        fn push(&self, value: serde_json::Value) { self.poller.script.lock().unwrap().push_back(Ok(vec![update(value)])); self.poller.wake.notify_one(); }
        fn run(&self, allowed: Vec<i64>) -> (CancellationToken, tokio::task::JoinHandle<Result<GatewayReport, GatewayRunError>>) {
            self.config.write().unwrap().allowed_user_ids = allowed;
            let clock: Arc<dyn Clock> = Arc::new(TestClock);
            let (health, health_rx) = watch::channel(GatewayHealth::default());
            *self.health.lock().unwrap() = Some(health_rx);
            let deps = GatewayDependencies {
                db: self.db.clone(), log: self.deps_parts.0.clone(), approvals: self.deps_parts.1.clone(), app: self.app.clone(),
                transport: self.transport.clone(), poller: self.poller.clone(), callbacks: self.callbacks.clone(), origins: self.origins.clone(),
                presence: self.presence.clone(), clock, config: self.config.clone(), health, on_verified: Arc::new(|_| Ok(())),
            };
            let cancel = CancellationToken::new();
            (cancel.clone(), tokio::spawn(run_gateway(deps, cancel)))
        }
        fn health(&self) -> GatewayHealth { *self.health.lock().unwrap().as_ref().unwrap().borrow() }
        fn texts(&self) -> Vec<String> { self.transport.sent.lock().unwrap().iter().map(|(_, t)| t.clone()).collect() }
        /// Kabul edilmiş, tamamlanmış (Ready) bir Telegram turu ve asistan finali yazar.
        fn ready_turn(&self, logical: &str, update_id: i64, text: &str) {
            let event = ultron_core::db::StoredEvent { id: uuid::Uuid::new_v4().to_string(), chat_id: "c1".into(), session_id: None, seq: 0,
                ts: chrono::Utc::now(), kind: "user_text".into(), payload: serde_json::json!({"type": "user_text", "text": "sentetik hedef"}) };
            let ultron_core::db::telegram::TelegramAcceptance::Accepted(_) = self.db.accept_telegram_turn(&ultron_core::db::telegram::TelegramTurnAcceptance {
                bot_id: 100, update_id, user_id: 101, private_chat_id: 101, logical_turn_id: logical, event: &event }).unwrap()
            else { panic!("kabul bekleniyordu"); };
            self.deps_parts.0.append("c1", None, ultron_core::events::EngineEvent::AssistantText { text: text.into() }).unwrap();
            let done = self.deps_parts.0.append("c1", None, ultron_core::events::EngineEvent::TurnCompleted).unwrap();
            assert!(self.db.complete_telegram_turn(logical, done.seq).unwrap());
            self.origins.ids.lock().unwrap().push(logical.into());
        }
        fn switch_event(&self) {
            self.deps_parts.0.append("c1", None, ultron_core::events::EngineEvent::EngineSwitched { from: EngineId::Claude, to: EngineId::Codex,
                reason: ultron_core::events::SwitchReason::Quota }).unwrap();
        }
    }
    fn message(update_id: i64, text: &str) -> serde_json::Value {
        serde_json::json!({"update_id": update_id, "message": {"message_id": 1, "from": {"id": 101, "is_bot": false}, "chat": {"id": 101, "type": "private"}, "text": text}})
    }
    async fn settle() { tokio::time::sleep(Duration::from_secs(3)).await; }

    #[tokio::test(start_paused = true)]
    async fn empty_allowlist_touches_no_network() {
        let env = Env::new();
        let (_cancel, handle) = env.run(Vec::new());
        let report = handle.await.unwrap().unwrap();
        assert_eq!(*env.transport.me_calls.lock().unwrap(), 0);
        assert!(env.poller.offsets.lock().unwrap().is_empty());
        assert_eq!(report.delivered_messages, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_update_across_restart_runs_once() {
        let env = Env::new();
        env.push(message(5, "/durum"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        cancel.cancel();
        handle.await.unwrap().unwrap();
        assert_eq!(env.texts().len(), 1);
        assert_eq!(env.db.telegram_next_update_id(100).unwrap(), Some(6));
        // Telegram aynı update'i yeniden teslim eder: yeni daemon ikinci yanıt üretmez.
        env.push(message(5, "/durum"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        cancel.cancel();
        handle.await.unwrap().unwrap();
        assert_eq!(env.texts().len(), 1);
        assert_eq!(env.poller.offsets.lock().unwrap().last().copied().flatten(), Some(6));
    }

    #[tokio::test(start_paused = true)]
    async fn backlog_uses_verified_bot_offset_and_replays_only_unaccepted_updates() {
        use ultron_core::db::telegram::InboxState;

        let env = Env::new();
        env.db.checkpoint_telegram_poll(200, 1000).unwrap();
        let old_message = |id| {
            let mut value = message(id, "/durum");
            value["message"]["date"] = serde_json::json!(1);
            value
        };
        env.push(old_message(5));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        cancel.cancel();
        handle.await.unwrap().unwrap();
        assert_eq!(env.poller.offsets.lock().unwrap()[0], None);
        assert_eq!(env.texts().len(), 1);

        // Önceki süreç güncellemeyi kabul etti, ancak yanıtı gönderemeden durdu.
        assert!(env.db.accept_telegram_update(100, 6, 101).unwrap());
        assert!(env.db.start_telegram_inbox(100, 6).unwrap());
        assert_eq!(env.db.recover_telegram_inbox().unwrap(), 1);
        let offset_index = env.poller.offsets.lock().unwrap().len();
        env.push(old_message(6));
        env.push(old_message(7));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        cancel.cancel();
        handle.await.unwrap().unwrap();
        assert_eq!(env.poller.offsets.lock().unwrap()[offset_index], Some(7));
        assert_eq!(env.texts().len(), 2);
        assert_eq!(env.db.telegram_inbox_entry(100, 6).unwrap().unwrap().state, InboxState::NeedsAttention);
        assert_eq!(env.db.telegram_inbox_entry(100, 7).unwrap().unwrap().state, InboxState::Completed);
        assert_eq!(env.db.telegram_next_update_id(100).unwrap(), Some(8));
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_updates_are_counted_without_content_and_offset_advances() {
        let env = Env::new();
        env.push(serde_json::json!({"update_id": 9, "message": {"message_id": 1, "from": {"id": 999, "is_bot": false}, "chat": {"id": 999, "type": "private"}, "text": "/durum"}}));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        cancel.cancel();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(report.rejected_updates, 1);
        assert!(env.texts().is_empty());
        assert_eq!(env.db.telegram_next_update_id(100).unwrap(), Some(10));
        assert!(env.db.telegram_inbox_entry(100, 9).unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn poll_backoff_is_bounded_and_cancel_stops_the_poller() {
        let env = Env::new();
        for _ in 0..3 { env.poller.script.lock().unwrap().push_back(Err(BotApiError::Transport(TransportError { code: TelegramErrorCode::Network, retry_after_seconds: None }))); }
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(5)).await;
        // 1 sn, 2 sn, 4 sn gecikmeleriyle 5 sn içinde en çok üç hata + bir bekleyen yoklama.
        assert!(env.poller.offsets.lock().unwrap().len() <= 4);
        cancel.cancel();
        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn busy_user_gets_explicit_reply_and_no_second_turn() {
        let env = Env::new();
        env.poller.script.lock().unwrap().push_back(Ok(vec![update(message(1, "ilk")), update(message(2, "ikinci"))]));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert_eq!(env.app.submits.lock().unwrap().len(), 1);
        assert!(env.texts().iter().any(|t| t == super::super::commands::BUSY));
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn new_command_selection_stale_and_binding_survive_restart() {
        let env = Env::new();
        env.db.set_telegram_binding(101, Some("c1")).unwrap();
        env.push(message(1, "/yeni"));
        env.push(message(2, "/sohbetler"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert_eq!(env.db.telegram_binding(101).unwrap(), None);
        let sent_before = env.texts().len();
        assert_eq!(sent_before, 2);
        // Eski/bilinmeyen seçim işlem yapmaz.
        env.push(serde_json::json!({"update_id": 3, "callback_query": {"id": "q", "from": {"id": 101, "is_bot": false},
            "message": {"message_id": 7, "chat": {"id": 101, "type": "private"}}, "data": "sel:00000000000000000000000000000000"}}));
        settle().await;
        assert!(env.texts().last().unwrap().contains("Seçim eskidi"));
        assert_eq!(env.db.telegram_binding(101).unwrap(), None);
        cancel.cancel();
        handle.await.unwrap().unwrap();
        // Bağlama restart sonrası kalıcıdır ve yeni metin bağlı sohbete gider.
        env.db.set_telegram_binding(101, Some("c1")).unwrap();
        env.push(message(4, "devam"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert_eq!(env.app.submits.lock().unwrap().as_slice(), &[Some("c1".to_string())]);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }
    fn transport_error(code: TelegramErrorCode, retry_after: Option<u32>) -> TransportError { TransportError { code, retry_after_seconds: retry_after } }
    fn delivery(env: &Env, id: &str) -> ultron_core::db::telegram::DeliveryState { env.db.telegram_turn_origin(id).unwrap().unwrap().delivery_state }

    #[tokio::test(start_paused = true)]
    async fn terminal_poll_and_verify_errors_stop_gateway_with_visible_health() {
        for (code, expected) in [(TelegramErrorCode::PollConflict, TelegramErrorCode::PollConflict), (TelegramErrorCode::InvalidToken, TelegramErrorCode::InvalidToken),
            (TelegramErrorCode::Unauthorized, TelegramErrorCode::Unauthorized)] {
            let env = Env::new();
            env.poller.script.lock().unwrap().push_back(Err(BotApiError::Transport(transport_error(code, None))));
            let (_cancel, handle) = env.run(vec![101]);
            assert_eq!(handle.await.unwrap().err(), Some(GatewayRunError::Transport(expected)));
            let health = env.health();
            assert!(!health.verified);
            assert_eq!(health.error, Some(expected));
        }
        let env = Env::new();
        env.poller.script.lock().unwrap().push_back(Err(BotApiError::ResponseTooLarge));
        let (_cancel, handle) = env.run(vec![101]);
        assert_eq!(handle.await.unwrap().err(), Some(GatewayRunError::ResponseTooLarge));
        assert!(!env.health().verified);
        assert_eq!(env.health().error, Some(TelegramErrorCode::InvalidResponse));
        // getMe aşamasında InvalidToken: poll'a hiç geçilmez.
        let env = Env::new();
        *env.transport.me_error.lock().unwrap() = Some(transport_error(TelegramErrorCode::InvalidToken, None));
        let (_cancel, handle) = env.run(vec![101]);
        assert_eq!(handle.await.unwrap().err(), Some(GatewayRunError::Transport(TelegramErrorCode::InvalidToken)));
        assert!(env.poller.offsets.lock().unwrap().is_empty());
        assert_eq!(env.health().error, Some(TelegramErrorCode::InvalidToken));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_retry_after_is_honored_by_poller() {
        let env = Env::new();
        env.poller.script.lock().unwrap().push_back(Err(BotApiError::Transport(transport_error(TelegramErrorCode::RateLimited, Some(30)))));
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(env.poller.offsets.lock().unwrap().len(), 1);
        assert!(env.health().verified || env.health().error == Some(TelegramErrorCode::RateLimited));
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(env.poller.offsets.lock().unwrap().len(), 2);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn valid_selection_nonce_binds_chat_through_gateway() {
        let env = Env::new();
        env.push(message(1, "/sohbetler"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        let nonce = env.transport.markups.lock().unwrap().first().cloned().expect("seçim düğmesi");
        assert!(nonce.starts_with("sel:"));
        env.push(serde_json::json!({"update_id": 2, "callback_query": {"id": "q", "from": {"id": 101, "is_bot": false},
            "message": {"message_id": 7, "chat": {"id": 101, "type": "private"}}, "data": nonce}}));
        settle().await;
        assert_eq!(env.db.telegram_binding(101).unwrap().as_deref(), Some("c1"));
        assert_eq!(env.texts().last().unwrap(), "Sohbet bağlandı.");
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn limit_and_approvals_commands_work_through_gateway() {
        let env = Env::new();
        env.push(message(1, "/limit"));
        env.push(message(2, "/onay"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        let texts = env.texts();
        assert!(texts.iter().any(|t| t.starts_with("Kotalar")));
        assert!(texts.iter().any(|t| t.contains("bekleyen araç onayı yok")));
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn limit_cooldown_is_per_user_and_skips_the_quota_provider() {
        let env = Env::new();
        let (cancel, handle) = env.run(vec![101, 102]);
        env.push(message(1, "/limit"));
        env.push(message(2, "/limit"));
        let mut other = message(3, "/limit");
        other["message"]["from"]["id"] = 102.into();
        other["message"]["chat"]["id"] = 102.into();
        env.push(other);
        settle().await;
        assert_eq!(env.app.quota_calls.load(Ordering::Relaxed), 2);
        assert_eq!(env.texts().iter().filter(|text| text.contains("lütfen bekleyin")).count(), 1);
        tokio::time::sleep(Duration::from_secs(30)).await;
        env.push(message(4, "/limit"));
        settle().await;
        assert_eq!(env.app.quota_calls.load(Ordering::Relaxed), 3);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn replay_fans_out_to_49_users_once_and_advances_to_later_event() {
        let env = Env::new();
        env.config.write().unwrap().notify = ultron_core::config::NotifyMode::Always;
        let allowed: Vec<_> = (101..150).collect();
        let (cancel, handle) = env.run(allowed.clone());
        settle().await;
        env.switch_event();
        env.deps_parts.0.append("c1", None, ultron_core::events::EngineEvent::EngineSwitched {
            from: EngineId::Codex, to: EngineId::Claude, reason: ultron_core::events::SwitchReason::Quota,
        }).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        let sent = env.transport.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2 * allowed.len());
        for user in allowed {
            let texts: Vec<_> = sent.iter().filter(|(chat, _)| *chat == user).map(|(_, text)| text.as_str()).collect();
            assert_eq!(texts, ["Motor değişti: Claude → Codex.", "Motor değişti: Codex → Claude."]);
        }
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn final_is_delivered_to_origin_and_marked_delivered() {
        let env = Env::new();
        env.ready_turn("turn-a", 1, "A-final");
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert!(env.texts().iter().any(|t| t.contains("A-final")));
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::Delivered);
        cancel.cancel();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(report.delivered_messages, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn definite_send_failure_keeps_final_ready_and_retries() {
        let env = Env::new();
        env.ready_turn("turn-a", 1, "A-final");
        env.transport.failures.lock().unwrap().push_back(transport_error(TelegramErrorCode::Network, None));
        env.transport.failures.lock().unwrap().push_back(transport_error(TelegramErrorCode::RateLimited, Some(5)));
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::Ready);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::Delivered);
        assert_eq!(env.texts().iter().filter(|t| t.contains("A-final")).count(), 1);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn uncertain_send_failure_needs_attention_and_is_not_resent() {
        let env = Env::new();
        env.ready_turn("turn-a", 1, "A-final");
        env.transport.failures.lock().unwrap().push_back(transport_error(TelegramErrorCode::DeliveryUncertain, None));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::NeedsAttention);
        assert_eq!(*env.transport.attempts.lock().unwrap(), 1);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_send_leaves_final_ready() {
        let env = Env::new();
        env.ready_turn("turn-a", 1, "A-final");
        *env.transport.delay_ms.lock().unwrap() = 60_000;
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(*env.transport.attempts.lock().unwrap(), 1);
        cancel.cancel();
        handle.await.unwrap().unwrap();
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn away_mode_suppresses_background_notifications_through_gateway() {
        let env = Env::new();
        env.config.write().unwrap().notify = ultron_core::config::NotifyMode::Away;
        env.presence.heartbeat(super::super::control::ControlRole::Approver, "desktop-1").unwrap();
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        env.switch_event();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(env.texts().is_empty());
        // Masaüstü uzaklaşınca (lease bitince) aynı tür olay iletilir.
        tokio::time::sleep(Duration::from_secs(600)).await;
        env.switch_event();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(env.texts().len(), 1);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queue_overflow_applies_backpressure_without_dropping() {
        let env = Env::new();
        *env.transport.delay_ms.lock().unwrap() = 50;
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        for _ in 0..300 { env.switch_event(); }
        tokio::time::sleep(Duration::from_secs(60)).await;
        cancel.cancel();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(env.texts().len(), 300);
        assert_eq!(report.failed_deliveries, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn final_survives_a_full_queue() {
        let env = Env::new();
        *env.transport.delay_ms.lock().unwrap() = 50;
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(1)).await;
        for _ in 0..200 { env.switch_event(); }
        env.ready_turn("turn-a", 1, "A-final");
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(delivery(&env, "turn-a"), ultron_core::db::telegram::DeliveryState::Delivered);
        assert!(env.texts().iter().any(|t| t.contains("A-final")));
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn allowlist_removal_takes_effect_without_restart() {
        let env = Env::new();
        env.push(message(1, "/durum"));
        let (cancel, handle) = env.run(vec![101]);
        settle().await;
        assert_eq!(env.texts().len(), 1);
        env.config.write().unwrap().allowed_user_ids = vec![202];
        env.push(message(2, "/durum"));
        settle().await;
        assert_eq!(env.texts().len(), 1);
        cancel.cancel();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(report.rejected_updates, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn approval_notification_retries_back_off_when_telegram_is_down() {
        let env = Env::new();
        env.callbacks.fail.store(true, Ordering::Relaxed);
        env.db.create_policy_tool_approval(None, EngineId::Claude, ultron_core::policy::RiskCategory::Delete, "sentetik",
            &ultron_core::policy::Action::FileDelete { path: "synthetic.txt".into() }, None).unwrap();
        let (cancel, handle) = env.run(vec![101]);
        tokio::time::sleep(Duration::from_secs(30)).await;
        let calls = env.callbacks.calls.load(Ordering::Relaxed);
        assert!((2..=7).contains(&calls), "geri çekilme bekleniyordu, çağrı: {calls}");
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn approval_429_and_slow_send_do_not_block_updates_or_explicit_backoff() {
        let env = Env::new();
        env.callbacks.hold_next.store(true, Ordering::Relaxed);
        env.callbacks.retry_after.store(20, Ordering::Relaxed);
        env.db.create_policy_tool_approval(None, EngineId::Claude, ultron_core::policy::RiskCategory::Delete, "sentetik",
            &ultron_core::policy::Action::FileDelete { path: "synthetic.txt".into() }, None).unwrap();
        let (cancel, handle) = env.run(vec![101, 102]);
        settle().await;
        assert_eq!(env.callbacks.calls.load(Ordering::Relaxed), 1);
        env.push(message(1, "/durum"));
        settle().await;
        assert!(env.texts().iter().any(|text| text.starts_with("Aktif turlar:")));
        assert_eq!(env.db.telegram_next_update_id(100).unwrap(), Some(2));
        env.callbacks.release.notify_one();
        settle().await;
        env.push(message(2, "/onay"));
        settle().await;
        assert_eq!(env.db.telegram_next_update_id(100).unwrap(), Some(3));
        assert_eq!(env.callbacks.calls.load(Ordering::Relaxed), 1);
        tokio::time::sleep(Duration::from_secs(22)).await;
        assert_eq!(env.callbacks.calls.load(Ordering::Relaxed), 3);
        cancel.cancel();
        handle.await.unwrap().unwrap();
    }
}
