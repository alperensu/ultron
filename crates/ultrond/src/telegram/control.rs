//! Yerel yönetim sınırı. GatewayApplication bu yetenekleri taşımaz.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ultron_core::config::TelegramConfig;
use ultron_core::ipc::{TelegramErrorCode, TelegramSettings, TelegramStatusSnapshot, TelegramTestReport};
use ultron_core::Paths;

use super::contracts::{BoxFuture, Clock, CredentialError, CredentialStore, GatewayError, SendMessage, TelegramTransport};
use super::credentials::SecretToken;

/// Task 10, reset_rights yazımını da bu tek kilitten geçirmelidir.
pub static CONFIG_LOCK: Mutex<()> = Mutex::new(());
pub const TEST_MESSAGE: &str = "Ultron Telegram bağlantı testi.";
pub const DESKTOP_LEASE: Duration = Duration::from_secs(90);

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> { chrono::Utc::now() }
    fn now_monotonic(&self) -> Instant { Instant::now() }
    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ControlRole { Requester, Approver }

#[derive(Clone, Copy, Default)]
pub struct GatewayHealth {
    pub token_present: bool,
    pub verified: bool,
    pub error: Option<TelegramErrorCode>,
}

/// Task 5/10 adaptörü: restart eski poller'ı cancel+join etmeden dönemez.
/// Boş liste için token=None verilir; adaptör credential/ağ erişimi yapamaz.
pub trait GatewayLifecycle: Send + Sync {
    /// Yalnız önbellekteki sağlık; credential veya ağ erişimi yapmaz.
    fn health(&self) -> GatewayHealth;
    fn restart(&self, token: Option<SecretToken>) -> BoxFuture<'_, Result<(), TelegramErrorCode>>;
    fn transport(&self) -> Option<Arc<dyn TelegramTransport>>;
}

/// Aynı monotonic lease'i bildirim ve status tüketir; soket sayısı kullanılmaz.
pub struct DesktopPresence {
    clock: Arc<dyn Clock>,
    last: Mutex<Option<Instant>>,
}

impl DesktopPresence {
    pub fn new(clock: Arc<dyn Clock>) -> Self { Self { clock, last: Mutex::new(None) } }

    pub fn heartbeat(&self, role: ControlRole, instance_id: &str) -> Result<(), GatewayError> {
        require_approver(role)?;
        if instance_id.is_empty() || instance_id.len() > 128
            || !instance_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(GatewayError::InvalidInput);
        }
        *self.last.lock().map_err(|_| GatewayError::Unavailable)? = Some(self.clock.now_monotonic());
        Ok(())
    }

    pub fn is_away(&self) -> bool {
        self.last.lock().ok().and_then(|last| *last)
            .and_then(|at| self.clock.now_monotonic().checked_duration_since(at))
            .is_none_or(|elapsed| elapsed >= DESKTOP_LEASE)
    }
}

fn require_approver(role: ControlRole) -> Result<(), GatewayError> {
    if role == ControlRole::Approver { Ok(()) } else { Err(GatewayError::Unauthorized) }
}

fn credential_code(error: CredentialError) -> TelegramErrorCode {
    match error {
        CredentialError::InvalidToken => TelegramErrorCode::InvalidToken,
        CredentialError::Unavailable { .. } => TelegramErrorCode::CredentialUnavailable,
    }
}

pub struct TelegramControl {
    paths: Paths,
    config: Arc<RwLock<TelegramConfig>>,
    store: Arc<dyn CredentialStore>,
    gateway: Arc<dyn GatewayLifecycle>,
    settings_updates: tokio::sync::watch::Sender<TelegramSettings>,
    health_error: Mutex<Option<TelegramErrorCode>>,
    /// Token/test/settings işlemleri sıralıdır; döndürülen transport rotasyonla yarışmaz.
    mutation: tokio::sync::Mutex<()>,
}

impl TelegramControl {
    pub fn new(paths: Paths, config: Arc<RwLock<TelegramConfig>>, store: Arc<dyn CredentialStore>, gateway: Arc<dyn GatewayLifecycle>) -> Result<Self, GatewayError> {
        let settings = {
            let config = config.read().map_err(|_| GatewayError::Unavailable)?;
            config.validate().map_err(|_| GatewayError::InvalidInput)?;
            TelegramSettings::from(&*config)
        };
        let (settings_updates, _) = tokio::sync::watch::channel(settings);
        Ok(Self { paths, config, store, gateway, settings_updates, health_error: Mutex::new(None), mutation: tokio::sync::Mutex::new(()) })
    }

    /// Task 8 timer/config değişikliğini uyanmadan önce de izleyebilir.
    pub fn subscribe_settings(&self) -> tokio::sync::watch::Receiver<TelegramSettings> { self.settings_updates.subscribe() }

    pub fn status(&self) -> Result<TelegramStatusSnapshot, GatewayError> {
        let config = self.config.read().map_err(|_| GatewayError::Unavailable)?;
        if config.allowed_user_ids.is_empty() {
            return Ok(TelegramStatusSnapshot::from_health(0, false, false, None, TelegramSettings::from(&*config)));
        }
        if !config.allowed_user_ids.is_empty()
            && let Some(error) = *self.health_error.lock().map_err(|_| GatewayError::Unavailable)? {
            return Ok(TelegramStatusSnapshot { state: ultron_core::ipc::TelegramConnectionState::Error,
                allowed_user_count: config.allowed_user_ids.len(), settings: TelegramSettings::from(&*config), error: Some(error) });
        }
        let health = self.gateway.health();
        Ok(TelegramStatusSnapshot::from_health(config.allowed_user_ids.len(), health.token_present,
            health.verified, health.error, TelegramSettings::from(&*config)))
    }

    pub fn settings(&self) -> Result<TelegramSettings, GatewayError> {
        Ok(TelegramSettings::from(&*self.config.read().map_err(|_| GatewayError::Unavailable)?))
    }

    /// Başlangıçta boş liste credential okumayı bile engeller.
    pub async fn initialize(&self) -> Result<(), TelegramErrorCode> {
        let _guard = self.mutation.lock().await;
        let result = self.restart_from_store().await;
        *self.health_error.lock().map_err(|_| TelegramErrorCode::CredentialUnavailable)? = result.err();
        result
    }

    async fn restart_from_store(&self) -> Result<(), TelegramErrorCode> {
        let empty = self.config.read().map_err(|_| TelegramErrorCode::CredentialUnavailable)?.allowed_user_ids.is_empty();
        let token = if empty { None } else { self.store.read_token().await.map_err(credential_code)? };
        self.gateway.restart(token).await
    }

    pub async fn set_token(&self, role: ControlRole, value: String) -> Result<(), TelegramErrorCode> {
        require_approver(role).map_err(|_| TelegramErrorCode::Unauthorized)?;
        let token = SecretToken::new(value).map_err(credential_code)?;
        let _guard = self.mutation.lock().await;
        self.store.write_token(&token).await.map_err(credential_code)?;
        let empty = self.config.read().map_err(|_| TelegramErrorCode::CredentialUnavailable)?.allowed_user_ids.is_empty();
        let result = self.gateway.restart(if empty { None } else { Some(token) }).await;
        *self.health_error.lock().map_err(|_| TelegramErrorCode::CredentialUnavailable)? = result.err();
        result
    }

    pub async fn set_settings(&self, role: ControlRole, settings: TelegramSettings) -> Result<(), GatewayError> {
        self.set_settings_with_writer(role, settings, update_settings_file).await
    }

    async fn set_settings_with_writer(&self, role: ControlRole, settings: TelegramSettings,
        writer: fn(&Path, &TelegramSettings) -> Result<TelegramConfig, GatewayError>) -> Result<(), GatewayError> {
        require_approver(role)?;
        settings.validate().map_err(|_| GatewayError::InvalidInput)?;
        let _guard = self.mutation.lock().await;
        let paths = self.paths.clone();
        let config = self.config.clone();
        let settings_updates = self.settings_updates.clone();
        tokio::task::spawn_blocking(move || {
            // Kilit sırası tüm yazıcılarda CONFIG_LOCK -> config olmalıdır.
            let _file_guard = CONFIG_LOCK.lock().map_err(|_| GatewayError::Unavailable)?;
            let mut snapshot = config.write().map_err(|_| GatewayError::Unavailable)?;
            let next = writer(&paths.root.join("config.toml"), &settings)?;
            *snapshot = next;
            settings_updates.send_replace(settings);
            Ok(())
        }).await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn test(&self, role: ControlRole) -> Result<TelegramTestReport, TelegramErrorCode> {
        require_approver(role).map_err(|_| TelegramErrorCode::Unauthorized)?;
        let _guard = self.mutation.lock().await;
        let ids = {
            let config = self.config.read().map_err(|_| TelegramErrorCode::CredentialUnavailable)?;
            config.validate().map_err(|_| TelegramErrorCode::InvalidResponse)?;
            config.allowed_user_ids.clone()
        };
        if ids.is_empty() { return Err(TelegramErrorCode::NotVerified); }
        let health = self.gateway.health();
        if !health.token_present { return Err(TelegramErrorCode::InvalidToken); }
        let transport = self.gateway.transport().ok_or(TelegramErrorCode::NotVerified)?;
        let mut report = TelegramTestReport { attempted: 0, delivered: 0, failed: 0, uncertain: 0 };
        for id in ids {
            report.attempted += 1;
            match transport.send_message(SendMessage { chat_id: id, text: TEST_MESSAGE.into(), disable_notification: false, reply_markup: None }).await {
                Ok(_) => report.delivered += 1,
                Err(error) if matches!(error.code, TelegramErrorCode::DeliveryUncertain | TelegramErrorCode::Timeout | TelegramErrorCode::Network) => report.uncertain += 1,
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }
}

/// Çağıran CONFIG_LOCK'ı tutar. Ham TOML ayrıştırma hatası dışarı çıkmaz.
fn update_settings_file(path: &Path, settings: &TelegramSettings) -> Result<TelegramConfig, GatewayError> {
    update_settings_file_with_rename(path, settings, |source, target| std::fs::rename(source, target))
}

fn update_settings_file_with_rename(path: &Path, settings: &TelegramSettings,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>) -> Result<TelegramConfig, GatewayError> {
    let mut text = String::new();
    match File::open(path) {
        Ok(file) => {
            let meta = std::fs::symlink_metadata(path).map_err(|_| GatewayError::Unavailable)?;
            #[cfg(windows)] {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 { return Err(GatewayError::InvalidInput); }
            }
            if !meta.is_file() { return Err(GatewayError::InvalidInput); }
            file.take(1024 * 1024 + 1).read_to_string(&mut text).map_err(|_| GatewayError::Unavailable)?;
            if text.len() > 1024 * 1024 { return Err(GatewayError::InvalidInput); }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => return Err(GatewayError::Unavailable),
    }
    let mut table: toml::Table = text.parse().map_err(|_| GatewayError::InvalidInput)?;
    let telegram = table.entry("telegram").or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut().ok_or(GatewayError::InvalidInput)?;
    let values = toml::Value::try_from(settings).map_err(|_| GatewayError::InvalidInput)?;
    telegram.extend(values.as_table().ok_or(GatewayError::InvalidInput)?.clone());
    let next: TelegramConfig = toml::Value::Table(telegram.clone()).try_into().map_err(|_| GatewayError::InvalidInput)?;
    next.validate().map_err(|_| GatewayError::InvalidInput)?;
    let encoded = toml::to_string(&table).map_err(|_| GatewayError::InvalidInput)?;
    let tmp = path.with_file_name(format!("config.toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(encoded.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        rename(&tmp, path)
    })();
    if result.is_err() { let _ = std::fs::remove_file(tmp); }
    result.map_err(|_| GatewayError::Unavailable)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::contracts::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MemoryStore { value: Mutex<Option<String>>, reads: AtomicUsize, writes: AtomicUsize }
    impl CredentialStore for MemoryStore {
        fn read_token(&self) -> BoxFuture<'_, Result<Option<SecretToken>, CredentialError>> {
            Box::pin(async {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.value.lock().unwrap().clone().map(SecretToken::new).transpose()
            })
        }
        fn write_token<'a>(&'a self, token: &'a SecretToken) -> BoxFuture<'a, Result<(), CredentialError>> {
            Box::pin(async move {
                self.writes.fetch_add(1, Ordering::SeqCst);
                *self.value.lock().unwrap() = Some(token.expose().into());
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct MockTransport { sent: Mutex<Vec<SendMessage>> }
    impl TelegramTransport for MockTransport {
        fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>> { panic!("test ağ doğrulaması yapmaz") }
        fn get_updates(&self, _: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>> { panic!("test poller başlatmaz") }
        fn send_message(&self, message: SendMessage) -> BoxFuture<'_, Result<SentMessage, TransportError>> {
            Box::pin(async move {
                let id = message.chat_id;
                self.sent.lock().unwrap().push(message);
                match id {
                    101 => Ok(SentMessage { message_id: 1 }),
                    102 => Err(TransportError { code: TelegramErrorCode::Unauthorized, retry_after_seconds: None }),
                    _ => Err(TransportError { code: TelegramErrorCode::DeliveryUncertain, retry_after_seconds: None }),
                }
            })
        }
        fn edit_message_text(&self, _: EditMessageText) -> BoxFuture<'_, Result<(), TransportError>> { panic!("test mesaj düzenlemez") }
        fn answer_callback_query(&self, _: AnswerCallbackQuery) -> BoxFuture<'_, Result<(), TransportError>> { panic!("test callback çözmez") }
    }

    #[derive(Default)]
    struct MockGateway { health: Mutex<GatewayHealth>, restarts: AtomicUsize, transport: Arc<MockTransport> }
    impl GatewayLifecycle for MockGateway {
        fn health(&self) -> GatewayHealth { *self.health.lock().unwrap() }
        fn restart(&self, token: Option<SecretToken>) -> BoxFuture<'_, Result<(), TelegramErrorCode>> {
            Box::pin(async move {
                self.restarts.fetch_add(1, Ordering::SeqCst);
                *self.health.lock().unwrap() = GatewayHealth { token_present: token.is_some(), verified: false, error: None };
                Ok(())
            })
        }
        fn transport(&self) -> Option<Arc<dyn TelegramTransport>> { Some(self.transport.clone()) }
    }

    struct Env { _tmp: tempfile::TempDir, control: TelegramControl, store: Arc<MemoryStore>, gateway: Arc<MockGateway>, config: Arc<RwLock<TelegramConfig>> }
    fn env(ids: Vec<i64>) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(RwLock::new(TelegramConfig { allowed_user_ids: ids, ..Default::default() }));
        let store = Arc::new(MemoryStore::default());
        let gateway = Arc::new(MockGateway::default());
        let control = TelegramControl::new(Paths::from_root(tmp.path().into()), config.clone(), store.clone(), gateway.clone()).unwrap();
        Env { _tmp: tmp, control, store, gateway, config }
    }

    #[tokio::test]
    async fn requester_cannot_set_token_test_settings_or_heartbeat() {
        let env = env(vec![101]);
        assert_eq!(env.control.set_token(ControlRole::Requester, "101:synthetic".into()).await, Err(TelegramErrorCode::Unauthorized));
        assert_eq!(env.control.test(ControlRole::Requester).await, Err(TelegramErrorCode::Unauthorized));
        assert_eq!(env.control.set_settings(ControlRole::Requester, env.control.settings().unwrap()).await, Err(GatewayError::Unauthorized));
        let presence = DesktopPresence::new(Arc::new(SystemClock));
        assert_eq!(presence.heartbeat(ControlRole::Requester, "desktop-1"), Err(GatewayError::Unauthorized));
        assert!(presence.is_away());
        assert_eq!(env.store.reads.load(Ordering::SeqCst), 0);
        assert_eq!(env.store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(env.gateway.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_list_and_missing_token_never_touch_network() {
        let env = env(vec![]);
        env.control.initialize().await.unwrap();
        assert_eq!(env.store.reads.load(Ordering::SeqCst), 0);
        assert_eq!(env.control.status().unwrap().state, ultron_core::ipc::TelegramConnectionState::MissingAllowedUsers);
        assert_eq!(env.control.test(ControlRole::Approver).await, Err(TelegramErrorCode::NotVerified));
        env.config.write().unwrap().allowed_user_ids.push(101);
        env.control.initialize().await.unwrap();
        assert_eq!(env.control.test(ControlRole::Approver).await, Err(TelegramErrorCode::InvalidToken));
        assert_eq!(env.control.status().unwrap().state, ultron_core::ipc::TelegramConnectionState::MissingToken);
        assert!(env.gateway.transport.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_rotation_and_test_report_are_truthful_and_redacted() {
        let env = env(vec![101, 102, 103]);
        env.control.set_token(ControlRole::Approver, "101:synthetic_secret\r\n".into()).await.unwrap();
        assert_eq!(env.store.writes.load(Ordering::SeqCst), 1);
        assert_eq!(env.gateway.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(env.control.status().unwrap().error, Some(TelegramErrorCode::NotVerified));
        let report = env.control.test(ControlRole::Approver).await.unwrap();
        assert_eq!(report, TelegramTestReport { attempted: 3, delivered: 1, failed: 1, uncertain: 1 });
        for message in env.gateway.transport.sent.lock().unwrap().iter() {
            assert_eq!(message.text, TEST_MESSAGE);
            assert!([101, 102, 103].contains(&message.chat_id));
        }
        let status = serde_json::to_string(&env.control.status().unwrap()).unwrap();
        assert!(!status.contains("synthetic_secret") && !status.contains("101"));
    }

    #[tokio::test]
    async fn config_update_preserves_telegram_ids_and_unknown_tables() {
        let env = env(vec![101, 102]);
        let path = env.control.paths.root.join("config.toml");
        std::fs::write(&path, "[telegram]\nallowed_user_ids = [101, 102]\nfuture_option = 'preserved'\n[reset_rights]\nclaude = 2\n[future_table]\nvalue = 7\n[[engines]]\nid = 'claude'\nbinary = 'synthetic'\n").unwrap();
        let mut settings = env.control.settings().unwrap();
        let mut updates = env.control.subscribe_settings();
        settings.auto_continue_enabled = false;
        settings.auto_continue_wait_minutes = 123;
        settings.fallback_order = vec![ultron_core::EngineId::Codex];
        env.control.set_settings(ControlRole::Approver, settings.clone()).await.unwrap();
        updates.changed().await.unwrap();
        assert_eq!(*updates.borrow_and_update(), settings);
        assert_eq!(env.control.settings().unwrap(), settings);
        assert_eq!(env.config.read().unwrap().allowed_user_ids, [101, 102]);
        let table: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(table["telegram"]["allowed_user_ids"].as_array().unwrap().len(), 2);
        assert_eq!(table["telegram"]["future_option"].as_str(), Some("preserved"));
        assert_eq!(table["future_table"]["value"].as_integer(), Some(7));
        assert_eq!(table["reset_rights"]["claude"].as_integer(), Some(2));
        assert_eq!(table["engines"].as_array().unwrap()[0]["binary"].as_str(), Some("synthetic"));
        assert_eq!(std::fs::read_dir(&env.control.paths.root).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn rejected_update_preserves_file_and_snapshot() {
        let env = env(vec![101]);
        let path = env.control.paths.root.join("config.toml");
        let original = "[telegram]\nallowed_user_ids = [101]\nnotify = 'broken'\n";
        std::fs::write(&path, original).unwrap();
        let old = env.control.settings().unwrap();
        let mut next = old.clone();
        next.auto_continue_wait_minutes = 0;
        assert_eq!(env.control.set_settings(ControlRole::Approver, next).await, Err(GatewayError::InvalidInput));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(env.control.settings().unwrap(), old);
        // Geçerli ayar bile bozuk ID tablosunu düzeltiyormuş gibi yazılmaz.
        std::fs::write(&path, "[telegram]\nallowed_user_ids = 'synthetic-invalid'\n").unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        assert_eq!(env.control.set_settings(ControlRole::Approver, old.clone()).await, Err(GatewayError::InvalidInput));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(env.control.settings().unwrap(), old);
    }

    #[tokio::test]
    async fn failed_atomic_rename_preserves_file_snapshot_and_watch() {
        let env = env(vec![101]);
        let path = env.control.paths.root.join("config.toml");
        let original = "[telegram]\nallowed_user_ids = [101]\n";
        std::fs::write(&path, original).unwrap();
        let old = env.control.settings().unwrap();
        let watch = env.control.subscribe_settings();
        let mut next = old.clone();
        next.auto_continue_enabled = false;
        let result = env.control.set_settings_with_writer(ControlRole::Approver, next, |path, settings| {
            update_settings_file_with_rename(path, settings, |_, _| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)))
        }).await;
        assert_eq!(result, Err(GatewayError::Unavailable));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
        assert_eq!(env.control.settings().unwrap(), old);
        assert!(!watch.has_changed().unwrap());
        assert_eq!(std::fs::read_dir(&env.control.paths.root).unwrap().count(), 1);
    }

    struct ManualClock { at: Mutex<Instant> }
    impl Clock for ManualClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> { chrono::Utc::now() }
        fn now_monotonic(&self) -> Instant { *self.at.lock().unwrap() }
        fn sleep_until(&self, _: Instant) -> BoxFuture<'_, ()> { panic!("presence saat bekletmez") }
    }
    #[test]
    fn desktop_heartbeat_uses_monotonic_ninety_second_lease() {
        let clock = Arc::new(ManualClock { at: Mutex::new(Instant::now()) });
        let presence = DesktopPresence::new(clock.clone());
        assert!(presence.is_away());
        presence.heartbeat(ControlRole::Approver, "desktop-1").unwrap();
        *clock.at.lock().unwrap() += Duration::from_millis(89_999);
        assert!(!presence.is_away());
        *clock.at.lock().unwrap() += Duration::from_millis(1);
        assert!(presence.is_away());
        presence.heartbeat(ControlRole::Approver, "desktop-2").unwrap();
        assert!(!presence.is_away());
    }
}
