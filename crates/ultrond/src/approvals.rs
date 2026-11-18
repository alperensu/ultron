//! Daemon onay kuyruğu. Kalıcı karar tek kaynaktır; oneshot yalnız bekleyeni uyandırır.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::sync::{broadcast, oneshot};
use tokio::time::Instant;
use ultron_core::db::StoredEvent;
use ultron_core::db::approvals::{Approval, ApprovalCapacity, ApprovalStatus, ResolvedBy};
use ultron_core::policy::{self, Action, Decision, PolicyConfig, PolicyContext};
use ultron_core::{Db, EngineId, EventLog, Paths};
use ultron_core::config::TelegramConfig;

use crate::telegram::contracts::{BoxFuture, GatewayError, TurnOrigin};
use crate::turn_service::ToolApprovalResolver;

pub struct Approvals {
    db: Arc<Db>,
    paths: Paths,
    log: Arc<EventLog>,
    waiters: Mutex<HashMap<String, Waiter>>,
    telegram_gateways: AtomicUsize,
    /// Sohbeti olmayan onaylar da küresel canlı akışta yayınlanır.
    global: broadcast::Sender<StoredEvent>,
    delivery: Mutex<()>,
}

pub const MAX_APPROVAL_WAIT_SECS: u32 = 300;
// 4 onay × 128 izinli alıcı × 2 düğme mevcut nonce bütçesine sığar.
pub(crate) const MAX_NOTIFIABLE_APPROVALS: usize = 4;

struct Waiter {
    sender: oneshot::Sender<(ApprovalStatus, ResolvedBy)>,
    deadline: Instant,
    telegram_offered: bool,
}

impl Approvals {
    pub fn new(db: Arc<Db>, log: Arc<EventLog>, paths: Paths) -> Self {
        Self {
            db,
            paths,
            log,
            waiters: Mutex::new(HashMap::new()),
            telegram_gateways: AtomicUsize::new(0),
            global: broadcast::channel(1024).0,
            delivery: Mutex::new(()),
        }
    }

    pub fn subscribe_global(&self) -> broadcast::Receiver<StoredEvent> {
        self.global.subscribe()
    }

    pub fn get(&self, id: &str) -> Result<Option<Approval>> {
        self.db.get_approval(id)
    }

    pub(crate) fn open_telegram_gateway(&self) {
        self.telegram_gateways.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn close_telegram_gateway(&self) {
        self.telegram_gateways.fetch_sub(1, Ordering::SeqCst);
    }

    pub(crate) fn is_telegram_offered(&self, row: &Approval) -> Result<bool> {
        let waiters = self.waiters.lock().map_err(|_| anyhow!("onay kilidi zehirlendi"))?;
        Ok(row.is_telegram_tool() && waiters.get(&row.id).is_some_and(|waiter| waiter.telegram_offered))
    }

    pub(crate) fn pending_ids(&self) -> Result<HashSet<String>> {
        Ok(self.db.list_approvals(Some(ApprovalStatus::Pending))?
            .into_iter().map(|row| row.id).collect())
    }

    /// Bildirim teslimi bekleyenin özgün (en çok 300 sn) süresini uzatamaz.
    pub fn remaining_wait(&self, id: &str) -> Result<Option<Duration>> {
        let waiters = self.waiters.lock().map_err(|_| anyhow!("onay kilidi zehirlendi"))?;
        Ok(waiters.get(id).map(|waiter| waiter.deadline.saturating_duration_since(Instant::now())))
    }

    /// Local yalnız IPC require_approver sonrasında kullanılmalıdır. Telegram kaynağı
    /// nonce doğrulayan daemon iç capability'sidir; istemciden serileştirilmez.
    pub fn resolve_tool_approval(
        &self,
        origin: TurnOrigin,
        id: &str,
        approve: bool,
        telegram: &TelegramConfig,
    ) -> Result<ApprovalStatus, GatewayError> {
        let by = if let TurnOrigin::Telegram { user_id, private_chat_id, update_id } = origin {
            if telegram.validate().is_err() || user_id <= 0 || private_chat_id != user_id
                || update_id < 0 || !telegram.allowed_user_ids.contains(&user_id)
            {
                return Err(GatewayError::Unauthorized);
            }
            let row = self.get(id).map_err(|_| GatewayError::Unavailable)?
                .ok_or(GatewayError::NotFound)?;
            if !self.is_telegram_offered(&row).map_err(|_| GatewayError::Unavailable)? {
                return Err(GatewayError::Unauthorized);
            }
            if row.status != ApprovalStatus::Pending { return Err(GatewayError::InvalidInput); }
            // Kalıcı yaş sınırı, kaybolmuş waiter veya saat geri atlamasında da izin vermez.
            let age = chrono::Utc::now().signed_duration_since(row.created_at);
            if age < chrono::Duration::zero()
                || age >= chrono::Duration::seconds(i64::from(MAX_APPROVAL_WAIT_SECS))
            {
                let _ = self.resolve(id, false, ResolvedBy::Timeout);
                return Err(GatewayError::InvalidInput);
            }
            ResolvedBy::Telegram
        } else {
            ResolvedBy::Approver
        };
        self.resolve(id, approve, by)
            .map(|row| row.status).map_err(|_| GatewayError::Unavailable)
    }

    /// Oturum yöneticisinin oluşturduğu onayları da aynı kalıcı outbox yoluyla yayınlar.
    pub(crate) fn publish_pending(&self) -> Result<()> {
        self.flush_outbox()
    }

    /// Outbox silinmeden önce döküm yazılır; hata halinde kayıt yeniden denemek için kalır.
    fn flush_outbox(&self) -> Result<()> {
        let _guard = self
            .delivery
            .lock()
            .map_err(|_| anyhow!("onay yayın kilidi zehirlendi"))?;
        loop {
            let page = self.db.approval_outbox()?;
            if page.is_empty() {
                return Ok(());
            }
            for (seq, ev) in page {
                if !ev.chat_id.is_empty() {
                    self.log.write_persisted_transcript(&ev)?;
                }
                // Yayın teslim kanıtı değildir; masaüstü ListApprovals ile kalıcı durumu uzlaştırır.
                let _ = self.global.send(ev);
                self.db.acknowledge_approval_event(seq)?;
            }
        }
    }

    pub fn recover_pending(&self) -> Result<usize> {
        let mut count = 0;
        loop {
            let pending = self.db.list_approvals(Some(ApprovalStatus::Pending))?;
            if pending.is_empty() {
                break;
            }
            for row in &pending {
                self.db
                    .resolve_approval(&row.id, false, ResolvedBy::Restart)?;
            }
            count += pending.len();
            self.flush_outbox()?;
        }
        self.db.prune_approvals()?;
        self.flush_outbox()?;
        Ok(count)
    }

    pub fn resolve(&self, id: &str, approve: bool, by: ResolvedBy) -> Result<Approval> {
        let mut waiters = self
            .waiters
            .lock()
            .map_err(|_| anyhow!("onay kilidi zehirlendi"))?;
        // Zamanlayıcının henüz çalışmamış olması süreyi uzatmaz.
        let late = matches!(
            by,
            ResolvedBy::Desktop | ResolvedBy::Telegram | ResolvedBy::Approver
        ) && waiters
            .get(id)
            .is_some_and(|waiter| Instant::now() >= waiter.deadline);
        let (approve, by) = if late {
            (false, ResolvedBy::Timeout)
        } else {
            (approve, by)
        };
        let row = self.db.resolve_approval(id, approve, by)?;
        let emitted = self.flush_outbox();
        // Günlük yazımı başarısızsa izin verme. Kalıcı ilk karar yine değişmez.
        if let Some(waiter) = waiters.remove(id) {
            let status = if emitted.is_ok() {
                row.status
            } else {
                ApprovalStatus::Denied
            };
            let _ = waiter.sender.send((status, by));
        }
        emitted?;
        if late {
            bail!("onay süresi doldu");
        }
        Ok(row)
    }

    #[cfg(test)]
    pub async fn check(
        &self,
        action: Action,
        chat_id: Option<String>,
        engine: EngineId,
        wait_secs: u32,
    ) -> Result<Decision> {
        self.check_owned(action, chat_id, engine, wait_secs, None)
            .await
    }

    pub async fn check_owned(
        &self,
        action: Action,
        chat_id: Option<String>,
        engine: EngineId,
        wait_secs: u32,
        requester_id: Option<&str>,
    ) -> Result<Decision> {
        ultron_core::db::approvals::validate_action_size(&action)?;
        if wait_secs > MAX_APPROVAL_WAIT_SECS {
            bail!("onay bekleme süresi en çok 300 saniye olabilir");
        }
        let project_root = if let Some(chat_id) = &chat_id {
            if chat_id.is_empty()
                || chat_id.len() > 128
                || !chat_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("geçersiz sohbet kimliği");
            }
            let chat = self
                .db
                .get_chat(chat_id)?
                .ok_or_else(|| anyhow!("sohbet bulunamadı"))?;
            chat.project_id
                .as_deref()
                .map(|id| self.db.get_project(id))
                .transpose()?
                .flatten()
                .map(|p| p.path.into())
        } else {
            None
        };
        let context = PolicyContext {
            project_root,
            workspace_root: self
                .paths
                .root
                .join("workspaces")
                .join(chat_id.as_deref().unwrap_or("policy")),
        };
        // Bozuk ayarlar hata döndürür; hiçbir hata izin kararına dönüşmez.
        let cfg = match PolicyConfig::load(&self.paths) {
            Ok(cfg) => cfg,
            // TOML hata zinciri ayar metnini içerebilir; istemciye taşınmaz.
            Err(_) => {
                return Ok(Decision::Deny {
                    reason: "güvenlik kuralları okunamadı; işlem reddedildi".into(),
                });
            }
        };
        let (reason, category) = match policy::evaluate(&action, &cfg, &context) {
            Decision::Ask { reason, category } => (reason, category),
            decision => return Ok(decision),
        };
        let deadline = Instant::now() + Duration::from_secs(u64::from(wait_secs));
        let (row, mut rx) = {
            let mut waiters = self
                .waiters
                .lock()
                .map_err(|_| anyhow!("onay kilidi zehirlendi"))?;
            // Elevation nitelikli eylem burada PolicyTool kaynağı kazanamaz.
            let row = match self.db.create_policy_tool_approval(
                chat_id.as_deref(),
                engine,
                category,
                &reason,
                &action,
                requester_id,
            ) {
                Ok(row) => row,
                Err(error) => {
                    let Some(capacity) = error.downcast_ref::<ApprovalCapacity>() else {
                        return Err(error);
                    };
                    let reason_code = match capacity {
                        ApprovalCapacity::Global => "approval_global_capacity",
                        ApprovalCapacity::Requester => "approval_requester_capacity",
                    };
                    tracing::warn!(reason_code, "politika onayı kapasite nedeniyle reddedildi");
                    return Ok(Decision::Deny { reason: capacity.to_string() });
                }
            };
            // Bekleme süresince kanal seçimi sabit kalır; yeniden başlatma bekleyenleri kapatır.
            let telegram_offered = self.telegram_gateways.load(Ordering::SeqCst) > 0
                && row.is_telegram_tool()
                && waiters.values().filter(|waiter| waiter.telegram_offered).count() < MAX_NOTIFIABLE_APPROVALS;
            let (tx, rx) = oneshot::channel();
            waiters.insert(
                row.id.clone(),
                Waiter {
                    sender: tx,
                    deadline,
                    telegram_offered,
                },
            );
            if let Err(error) = self.flush_outbox() {
                waiters.remove(&row.id);
                self.db
                    .resolve_approval(&row.id, false, ResolvedBy::Timeout)?;
                return Err(error);
            }
            (row, rx)
        };
        let (status, by) = match tokio::time::timeout_at(deadline, &mut rx).await {
            Ok(Ok((status, by))) => (status, Some(by)),
            Ok(Err(_)) => (ApprovalStatus::Denied, None),
            Err(_) => {
                // Son anda gelen masaüstü kararı timeout ile yarışır; DB'deki ilk karar geçerlidir.
                match self.resolve(&row.id, false, ResolvedBy::Timeout) {
                    Ok(row) => (row.status, row.resolved_by),
                    // Kazanan resolver kilidi bırakmadan önce oneshot'u doldurur.
                    // Günlük yazılamadıysa DB approved olsa bile gönderilen karar Denied'dır.
                    Err(_) => match rx.try_recv() {
                        Ok((status, by)) => (status, Some(by)),
                        Err(_) => (ApprovalStatus::Denied, self.get(&row.id)?.and_then(|row| row.resolved_by)),
                    },
                }
            }
        };
        Ok(if status == ApprovalStatus::Approved {
            // Silme içeren her onaylı eylem (kategoriden bağımsız) ham olarak çalışmaz: Claude
            // `ultron trash` ile silmeye yönlendirilir; orada TrashPut kendi denetimlerini yapar.
            if category == policy::RiskCategory::Delete || policy::is_delete_like(&action) {
                Decision::Deny {
                    reason: policy::APPROVED_DELETE_REDIRECT.into(),
                }
            } else {
                Decision::Allow
            }
        } else {
            Decision::Deny {
                reason: if reason == policy::UNPARSEABLE_COMMAND_REASON
                    && matches!(by, None | Some(ResolvedBy::Timeout | ResolvedBy::Restart))
                {
                    reason
                } else if matches!(by, Some(ResolvedBy::Approver | ResolvedBy::Desktop | ResolvedBy::Telegram)) {
                    "işlem reddedildi".into()
                } else {
                    "işlem reddedildi veya onay süresi doldu".into()
                },
            }
        })
    }
}

/// TurnService ve approver IPC aynı resolver yolunu paylaşır. Ayar okuma kilidi
/// karar bitene kadar tutulur; izinli listeden çıkarma eski snapshot ile yarışmaz.
pub struct SharedToolApprovalResolver {
    approvals: Arc<Approvals>,
    telegram: Arc<RwLock<TelegramConfig>>,
}

impl SharedToolApprovalResolver {
    pub fn new(approvals: Arc<Approvals>, telegram: Arc<RwLock<TelegramConfig>>) -> Self {
        Self { approvals, telegram }
    }
}

impl ToolApprovalResolver for SharedToolApprovalResolver {
    fn resolve<'a>(&'a self, origin: TurnOrigin, approval_id: &'a str, approve: bool)
        -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>>
    {
        Box::pin(async move {
            let telegram = self.telegram.read().map_err(|_| GatewayError::Unavailable)?;
            self.approvals.resolve_tool_approval(origin, approval_id, approve, &telegram)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_engine_and_approval_transcripts_remain_valid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("chat", None, "test", chrono::Utc::now())
            .unwrap();
        let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
        let queue = Approvals::new(db.clone(), log.clone(), paths.clone());
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..64 {
                    log.append(
                        "chat",
                        None,
                        ultron_core::events::EngineEvent::AssistantText {
                            text: "sentetik-çıkış".repeat(4096),
                        },
                    )
                    .unwrap();
                }
            });
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..64 {
                    let row = db
                        .create_approval(
                            Some("chat"),
                            EngineId::Claude,
                            policy::RiskCategory::Delete,
                            "test",
                            &Action::FileDelete {
                                path: "synthetic.txt".into(),
                            },
                        )
                        .unwrap();
                    queue.resolve(&row.id, false, ResolvedBy::Timeout).unwrap();
                }
            });
        });
        let transcript = std::fs::read_to_string(paths.transcripts.join("chat.jsonl")).unwrap();
        let mut ids = std::collections::HashSet::new();
        for line in transcript.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(ids.insert(value["id"].as_str().unwrap().to_owned()));
        }
        assert_eq!(ids.len(), 64 * 3);
        let stored = db.events_for_chat("chat", None).unwrap();
        assert_eq!(stored.len(), ids.len());
        assert!(stored.iter().all(|event| ids.contains(&event.id)));
    }

    #[tokio::test]
    async fn cli_and_yaml_secret_variants_never_reach_approval_db_events_transcript_or_list() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("chat", None, "test", chrono::Utc::now())
            .unwrap();
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths.clone(),
        );
        let value = ["synthetic", "runtime", "value"].join("_");
        let parts = ["first", "second"].map(|part| ["synthetic", part, "runtime"].join("_"));
        let mut live = queue.subscribe_global();
        for name in [
            "api-key",
            "api_key",
            "apikey",
            "token",
            "password",
            "passwd",
            "secret",
            "access-token",
            "access_token",
            "auth-token",
            "auth_token",
            "client-secret",
            "client_secret",
        ] {
            for separator in [" ", "=", ":"] {
                queue
                    .check(
                        Action::Shell {
                            command: format!(
                                "rm synthetic.txt; example --{name}{separator}{value}"
                            ),
                            cwd: paths.root.clone(),
                        },
                        Some("chat".into()),
                        EngineId::Claude,
                        0,
                    )
                    .await
                    .unwrap();
            }
            for quote in ["'", "\""] {
                queue
                    .check(
                        Action::Shell {
                            command: format!(
                                "rm synthetic.txt; example {quote}--{name}={value} {} {}{quote}",
                                parts[0], parts[1]
                            ),
                            cwd: paths.root.clone(),
                        },
                        Some("chat".into()),
                        EngineId::Claude,
                        0,
                    )
                    .await
                    .unwrap();
            }
            for argument in [
                format!("'--{name}={value}''{} {}'", parts[0], parts[1]),
                format!("--{name}='{value}''{} {}'", parts[0], parts[1]),
                format!(r"--{name}='{value}\'{} {}'", parts[0], parts[1]),
                format!(r"--{name}='{value}'\''{} {}'", parts[0], parts[1]),
                format!(r"--{name}={value}\'{}\ {}", parts[0], parts[1]),
                format!("--{name}='{value} {} {}", parts[0], parts[1]),
                format!("'--{name}:{value}''{} {}'", parts[0], parts[1]),
                format!("--{name}='{value}\n{}\n{}'", parts[0], parts[1]),
                format!("--{name}=\"{value}\r\n{}\r\n{}\"", parts[0], parts[1]),
                format!("'--{name}={value}\n{}\n{}'", parts[0], parts[1]),
                format!("--{name}='{value}\n{}\n{}", parts[0], parts[1]),
                format!("--{name}=\"{value}\r\n{}\r\n{}", parts[0], parts[1]),
            ] {
                queue
                    .check(
                        Action::Shell {
                            command: format!("rm synthetic.txt; example {argument}"),
                            cwd: paths.root.clone(),
                        },
                        Some("chat".into()),
                        EngineId::Claude,
                        0,
                    )
                    .await
                    .unwrap();
            }
            queue
                .check(
                    Action::ToolCall {
                        tool: "Read".into(),
                        input: serde_json::json!({
                            "path": "run/approver-token",
                            "content": format!("{name}: |\n  {value}\n  {}\n    {}\nsafe: retained\n", parts[0], parts[1])
                        }),
                    },
                    Some("chat".into()),
                    EngineId::Claude,
                    0,
                )
                .await
                .unwrap();
            for separator in [": ", "="] {
                for quote in ["", "'", "\""] {
                    let content = if quote.is_empty() {
                        format!(
                            "{name}{separator}{value} {} {}\nsafe: retained",
                            parts[0], parts[1]
                        )
                    } else {
                        format!(
                            "{name}{separator}{quote}{value}\n{}\n{}{quote}\nsafe: retained",
                            parts[0], parts[1]
                        )
                    };
                    queue.check(
                        Action::ToolCall {
                            tool: "Read".into(),
                            input: serde_json::json!({"path": "run/approver-token", "content": content}),
                        },
                        Some("chat".into()),
                        EngineId::Claude,
                        0,
                    ).await.unwrap();
                }
            }
        }
        let live_payloads: Vec<_> = std::iter::from_fn(|| live.try_recv().ok())
            .map(|event| event.payload.to_string())
            .collect();
        assert!(!live_payloads.is_empty());
        for value in [&value, &parts[0], &parts[1]] {
            for row in db.list_approvals(None).unwrap() {
                assert!(
                    !serde_json::to_string(&row)
                        .unwrap()
                        .contains(value.as_str())
                );
                assert!(
                    !serde_json::to_string(&db.get_approval(&row.id).unwrap())
                        .unwrap()
                        .contains(value.as_str())
                );
            }
            let mut cursor = None;
            loop {
                let page = db
                    .list_approvals_page(None, None, Some(4), cursor.as_deref())
                    .unwrap();
                assert!(
                    !serde_json::to_string(&page)
                        .unwrap()
                        .contains(value.as_str())
                );
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert!(
                !serde_json::to_string(&db.events_for_chat("chat", None).unwrap())
                    .unwrap()
                    .contains(value.as_str())
            );
            for payload in &live_payloads {
                assert!(!payload.contains(value.as_str()));
            }
            assert!(
                !std::fs::read_to_string(paths.transcripts.join("chat.jsonl"))
                    .unwrap()
                    .contains(value.as_str())
            );
        }
    }

    #[tokio::test]
    async fn transcript_failure_still_persists_terminal_event_and_recovers_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("chat", None, "test", chrono::Utc::now())
            .unwrap();
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths.clone(),
        );
        // Dosya yerine dizin oluşturmak, döküm yazımını sentetik olarak bozar.
        let transcript = paths.transcripts.join("chat.jsonl");
        std::fs::create_dir(&transcript).unwrap();
        assert!(
            queue
                .check(
                    Action::FileDelete {
                        path: "synthetic.txt".into()
                    },
                    Some("chat".into()),
                    EngineId::Claude,
                    0
                )
                .await
                .is_err()
        );
        let row = db.list_approvals(None).unwrap().remove(0);
        assert_eq!(row.status, ApprovalStatus::Denied);
        let events = db.events_for_chat("chat", None).unwrap();
        assert_eq!(
            events.iter().map(|ev| ev.kind.as_str()).collect::<Vec<_>>(),
            ["approval_requested", "approval_resolved"]
        );
        assert_eq!(db.approval_outbox().unwrap().len(), 2);
        std::fs::remove_dir(&transcript).unwrap();
        assert_eq!(queue.recover_pending().unwrap(), 0);
        let text = std::fs::read_to_string(transcript).unwrap();
        assert!(text.contains("approval_resolved"));
        assert!(db.approval_outbox().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bearer_headers_and_cli_secrets_never_reach_approval_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("chat", None, "test", chrono::Utc::now())
            .unwrap();
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths.clone(),
        );
        let value = ["clearly", "synthetic", "credential"].join("_");
        let mut events = queue.subscribe_global();
        for command in [
            format!(
                "rm synthetic.txt; curl -H 'Authorization: Bearer {value}' https://example.invalid"
            ),
            format!("rm synthetic.txt; example --password '{value} with spaces'"),
            format!("rm synthetic.txt; example --access-token {value}"),
            format!("rm synthetic.txt; example Bearer {value}"),
            format!("rm synthetic.txt; curl -H 'Authorization: Basic {value}'"),
        ] {
            queue
                .check(
                    Action::Shell {
                        command,
                        cwd: paths.root.clone(),
                    },
                    Some("chat".into()),
                    EngineId::Claude,
                    0,
                )
                .await
                .unwrap();
        }
        for row in db.list_approvals(None).unwrap() {
            assert!(!serde_json::to_string(&row).unwrap().contains(&value));
            assert!(serde_json::from_str::<serde_json::Value>(&row.action_json).is_ok());
        }
        for event in db.events_for_chat("chat", None).unwrap() {
            assert!(!event.payload.to_string().contains(&value));
        }
        while let Ok(event) = events.try_recv() {
            assert!(!event.payload.to_string().contains(&value));
        }
        assert!(
            !std::fs::read_to_string(paths.transcripts.join("chat.jsonl"))
                .unwrap()
                .contains(&value)
        );
        assert!(db.approval_outbox().unwrap().is_empty());
    }

    #[tokio::test]
    async fn server_wait_max_rejects_without_creating_a_row() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths,
        );
        assert!(
            queue
                .check(
                    Action::FileDelete {
                        path: "synthetic.txt".into()
                    },
                    None,
                    EngineId::Claude,
                    u32::MAX
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("300")
        );
        assert!(db.list_approvals(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn late_answer_denies_even_before_timeout_task_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths,
        );
        let row = db
            .create_approval(
                None,
                EngineId::Claude,
                policy::RiskCategory::Delete,
                "test",
                &Action::FileDelete {
                    path: "synthetic.txt".into(),
                },
            )
            .unwrap();
        let (tx, rx) = oneshot::channel();
        queue.waiters.lock().unwrap().insert(
            row.id.clone(),
            Waiter {
                sender: tx,
                deadline: Instant::now() - Duration::from_secs(1),
                telegram_offered: false,
            },
        );
        assert_eq!(
            queue
                .resolve(&row.id, true, ResolvedBy::Approver)
                .unwrap_err()
                .to_string(),
            "onay süresi doldu"
        );
        assert_eq!(rx.await.unwrap(), (ApprovalStatus::Denied, ResolvedBy::Timeout));
        let stored = db.get_approval(&row.id).unwrap().unwrap();
        assert_eq!(stored.status, ApprovalStatus::Denied);
        assert_eq!(stored.resolved_by, Some(ResolvedBy::Timeout));
        assert_eq!(
            queue
                .resolve(&row.id, true, ResolvedBy::Telegram)
                .unwrap_err()
                .to_string(),
            "onay zaten çözüldü"
        );
    }

    #[tokio::test]
    async fn direct_decisions_and_broken_config_never_create_approval() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths.clone(),
        );
        let action = |command: &str| Action::Shell {
            command: command.into(),
            cwd: paths.root.clone(),
        };
        assert_eq!(
            queue
                .check(action("git status"), None, EngineId::Claude, 0)
                .await
                .unwrap(),
            Decision::Allow
        );
        assert!(matches!(
            queue
                .check(action("diskpart"), None, EngineId::Claude, 0)
                .await
                .unwrap(),
            Decision::Deny { .. }
        ));
        assert!(
            queue
                .check(
                    action("rm x"),
                    Some("olmayan-sohbet".into()),
                    EngineId::Claude,
                    0
                )
                .await
                .is_err()
        );
        std::fs::write(
            paths.root.join("policy.toml"),
            "finance_paths = [synthetic-sensitive-value",
        )
        .unwrap();
        let decision = queue
            .check(action("git status"), None, EngineId::Claude, 0)
            .await
            .unwrap();
        assert!(matches!(&decision, Decision::Deny { .. }));
        assert!(
            !serde_json::to_string(&decision)
                .unwrap()
                .contains("synthetic-sensitive-value")
        );
        assert!(db.list_approvals(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn policy_unparseable_command_retains_retry_hint_after_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths.clone(),
        );
        let decision = queue
            .check(
                Action::Shell {
                    command: "rm (Get-Item sentetik.txt)".into(),
                    cwd: paths.root,
                },
                None,
                EngineId::Claude,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            Decision::Deny { reason: policy::UNPARSEABLE_COMMAND_REASON.into() }
        );
        let row = &db.list_approvals(None).unwrap()[0];
        assert_eq!(row.category, policy::RiskCategory::SystemConfig);
        assert!(!row.is_telegram_tool());
        assert_eq!(row.resolved_by, Some(ResolvedBy::Timeout));
    }

    #[tokio::test]
    async fn explicit_denial_of_unparseable_command_does_not_request_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(db.clone(), Arc::new(EventLog::new(db, paths.clone())), paths.clone());
        let mut events = queue.subscribe_global();
        let checking = queue.check(Action::Shell {
            command: "rm synthetic.txt; echo done".into(),
            cwd: paths.root,
        }, None, EngineId::Claude, 30);
        let resolving = async {
            let requested = events.recv().await.unwrap();
            let id = requested.payload["approval_id"].as_str().unwrap();
            queue.resolve(id, false, ResolvedBy::Approver).unwrap();
        };
        let (decision, ()) = tokio::join!(checking, resolving);
        assert_eq!(decision.unwrap(), Decision::Deny { reason: "işlem reddedildi".into() });
    }

    #[test]
    fn policy_approval_without_waiter_is_desktop_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(), Arc::new(EventLog::new(db.clone(), paths.clone())), paths,
        );
        queue.open_telegram_gateway();
        let row = db.create_policy_tool_approval(
            None, EngineId::Claude, policy::RiskCategory::Delete, "sentetik",
            &Action::FileDelete { path: "synthetic.txt".into() }, None,
        ).unwrap();
        assert!(row.is_telegram_tool());
        assert!(!queue.is_telegram_offered(&row).unwrap());
        assert_eq!(
            queue.resolve_tool_approval(
                TurnOrigin::Telegram { user_id: 101, private_chat_id: 101, update_id: 1 },
                &row.id, true, &TelegramConfig { allowed_user_ids: vec![101], ..Default::default() },
            ),
            Err(GatewayError::Unauthorized),
        );
        assert_eq!(queue.get(&row.id).unwrap().unwrap().status, ApprovalStatus::Pending);
        assert_eq!(
            queue.resolve_tool_approval(TurnOrigin::Local, &row.id, false, &TelegramConfig::default()).unwrap(),
            ApprovalStatus::Denied,
        );
    }

    #[tokio::test]
    async fn five_policy_approvals_remain_pending_with_telegram_off() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Arc::new(Approvals::new(
            db.clone(), Arc::new(EventLog::new(db.clone(), paths.clone())), paths,
        ));
        let mut events = queue.subscribe_global();
        let mut checks = tokio::task::JoinSet::new();
        for _ in 0..5 {
            let queue = queue.clone();
            checks.spawn(async move {
                queue.check(Action::FileDelete { path: "synthetic.txt".into() },
                    None, EngineId::Claude, 30).await.unwrap()
            });
            assert_eq!(events.recv().await.unwrap().kind, "approval_requested");
        }
        let rows = db.list_approvals(Some(ApprovalStatus::Pending)).unwrap();
        assert_eq!(rows.len(), 5);
        for row in rows {
            assert!(row.is_telegram_tool());
            assert!(!queue.is_telegram_offered(&row).unwrap());
            queue.resolve(&row.id, false, ResolvedBy::Desktop).unwrap();
        }
        while let Some(decision) = checks.join_next().await {
            assert!(matches!(decision.unwrap(), Decision::Deny { .. }));
        }
    }

    #[tokio::test]
    async fn pending_capacity_denials_return_distinct_reasons_and_audit_codes() {
        use tracing::instrument::WithSubscriber;
        use ultron_core::db::approvals::{MAX_PENDING_APPROVALS, MAX_PENDING_APPROVALS_PER_REQUESTER};
        use ultron_core::policy::RiskCategory;

        struct AuditWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for AuditWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(), Arc::new(EventLog::new(db.clone(), paths.clone())), paths,
        );
        let audit = Arc::new(Mutex::new(Vec::new()));
        let captured = audit.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false)
            .with_writer(move || AuditWriter(captured.clone())).finish();
        let action = Action::FileDelete { path: "synthetic.txt".into() };
        async {
            for _ in 0..MAX_PENDING_APPROVALS_PER_REQUESTER {
                db.create_policy_tool_approval(None, EngineId::Claude, RiskCategory::Delete,
                    "test", &action, Some("synthetic-requester")).unwrap();
            }
            let deny = queue.check_owned(action.clone(), None, EngineId::Claude, 0,
                Some("synthetic-requester")).await.unwrap();
            assert!(matches!(deny, Decision::Deny { reason } if reason.contains("bu bağlantının")));
            assert_eq!(db.list_approvals(None).unwrap().len(), MAX_PENDING_APPROVALS_PER_REQUESTER);
            // Başka bağlantılar için yer var, genel sınırın altında kota bağımsızdır.
            let deny = queue.check_owned(action.clone(), None, EngineId::Claude, 0,
                Some("another")).await.unwrap();
            assert!(matches!(deny, Decision::Deny { reason } if reason.contains("süresi doldu")));
            for _ in MAX_PENDING_APPROVALS_PER_REQUESTER..MAX_PENDING_APPROVALS {
                db.create_approval(None, EngineId::Claude, RiskCategory::Delete, "test", &action).unwrap();
            }
            let deny = queue.check_owned(action, None, EngineId::Claude, 0,
                Some("another")).await.unwrap();
            assert!(matches!(deny, Decision::Deny { reason } if reason.contains("bekleyen onay kuyruğu dolu")));
            assert_eq!(db.list_approvals(Some(ApprovalStatus::Pending)).unwrap().len(), MAX_PENDING_APPROVALS);
        }.with_subscriber(subscriber).await;
        let audit = String::from_utf8(audit.lock().unwrap().clone()).unwrap();
        assert!(audit.contains("approval_requester_capacity"));
        assert!(audit.contains("approval_global_capacity"));
        assert!(!audit.contains("synthetic-requester"));
        assert!(!audit.contains("synthetic.txt"));
    }

    #[tokio::test]
    async fn in_process_timeout_and_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let queue = Approvals::new(
            db.clone(),
            Arc::new(EventLog::new(db.clone(), paths.clone())),
            paths,
        );
        let action = Action::FileDelete {
            path: tmp.path().join("synthetic.txt"),
        };
        assert!(matches!(
            queue
                .check(action.clone(), None, EngineId::Claude, 0)
                .await
                .unwrap(),
            Decision::Deny { .. }
        ));
        let row = &db.list_approvals(None).unwrap()[0];
        assert_eq!(row.status, ApprovalStatus::Denied);
        assert_eq!(row.resolved_by, Some(ResolvedBy::Timeout));
        let pending = db
            .create_approval(
                None,
                EngineId::Claude,
                ultron_core::policy::RiskCategory::Delete,
                "test",
                &action,
            )
            .unwrap();
        assert_eq!(queue.recover_pending().unwrap(), 1);
        assert_eq!(
            db.get_approval(&pending.id).unwrap().unwrap().status,
            ApprovalStatus::Expired
        );
        assert_eq!(queue.recover_pending().unwrap(), 0);
    }

    #[tokio::test]
    async fn in_process_approval_and_denial_wake_once_and_emit_masked_events() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        let log = Arc::new(EventLog::new(db.clone(), paths.clone()));
        let queue = Approvals::new(db.clone(), log.clone(), paths.clone());
        db.insert_chat("chat", None, "test", chrono::Utc::now())
            .unwrap();
        for (approve, chat) in [(true, Some("chat")), (false, None)] {
            let mut events = queue.subscribe_global();
            let checking = queue.check(
                Action::Shell {
                    command: "rm synthetic.txt; token=synthetic-value".into(),
                    cwd: paths.root.clone(),
                },
                chat.map(str::to_owned),
                EngineId::Claude,
                30,
            );
            let resolving = async {
                let requested = events.recv().await.unwrap();
                assert_eq!(requested.kind, "approval_requested");
                assert!(!requested.payload.to_string().contains("synthetic-value"));
                let id = requested.payload["approval_id"].as_str().unwrap();
                queue.resolve(id, approve, ResolvedBy::Desktop).unwrap();
                assert!(queue.resolve(id, !approve, ResolvedBy::Telegram).is_err());
                assert_eq!(events.recv().await.unwrap().kind, "approval_resolved");
                assert!(events.try_recv().is_err());
            };
            let (decision, ()) = tokio::join!(checking, resolving);
            let decision = decision.unwrap();
            assert!(matches!(decision, Decision::Deny { .. }), "ham silme Allow olmaz");
            assert_eq!(
                matches!(&decision, Decision::Deny { reason } if reason.contains("ultron trash")),
                approve
            );
        }
        assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
        assert!(queue.waiters.lock().unwrap().is_empty());
    }
}
