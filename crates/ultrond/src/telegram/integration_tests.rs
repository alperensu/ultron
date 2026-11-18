//! Gerçek daemon yaşam döngüsü; yalnız loopback Bot API ve sentetik motor/credential.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use ultron_core::config::{SummarizerMode, TelegramConfig};
use ultron_core::db::quota_wait::{NewQuotaWait, QuotaWaitState, ResumePolicy};
use ultron_core::db::telegram::{DeliveryState, InboxState, TelegramAcceptance, TelegramTurnAcceptance};
use ultron_core::engine::{Capabilities, CommandSpec, Engine, FakeEngine, RestrictionLevel, TurnRequest};
use ultron_core::ipc::{Client, PolicyArg, Request, Response};
use ultron_core::quota::{HttpClient, HttpFuture, QuotaService, QuotaSources, Token};
use ultron_core::{Config, Db, EngineEvent, EngineId, EventLog, Paths, SessionRow, SessionStatus, StoredEvent};

use crate::auto_continue::ResetProbe;
use crate::server::{self, ServerConfig, ServerHandle};
use super::bridge::PollSource;
use super::client::{BotApiClient, BotApiError};
use super::contracts::*;
use super::credentials::SecretToken;

const TOKEN: &str = "100:synthetic_integration_token";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self { self.clone() }
}

#[derive(Default)]
struct MemoryCredentials { value: Mutex<Option<String>>, reads: AtomicUsize, writes: AtomicUsize }
impl CredentialStore for MemoryCredentials {
    fn read_token(&self) -> BoxFuture<'_, Result<Option<SecretToken>, CredentialError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { self.value.lock().unwrap().clone().map(SecretToken::new).transpose() })
    }
    fn write_token<'a>(&'a self, token: &'a SecretToken) -> BoxFuture<'a, Result<(), CredentialError>> {
        Box::pin(async move {
            self.writes.fetch_add(1, Ordering::SeqCst);
            *self.value.lock().unwrap() = Some(token.expose().to_owned()); Ok(())
        })
    }
}

struct ManualClock { seconds: AtomicU64, changed: tokio::sync::Notify }
impl ManualClock {
    fn new() -> Self { Self { seconds: AtomicU64::new(0), changed: tokio::sync::Notify::new() } }
    fn advance(&self, seconds: u64) { self.seconds.fetch_add(seconds, Ordering::SeqCst); self.changed.notify_waiters(); }
}
impl Clock for ManualClock {
    fn now_utc(&self) -> chrono::DateTime<Utc> { Utc::now() + chrono::Duration::seconds(self.seconds.load(Ordering::SeqCst) as i64) }
    fn now_monotonic(&self) -> Instant { Instant::now() + Duration::from_secs(self.seconds.load(Ordering::SeqCst)) }
    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                let Some(wait) = deadline.checked_duration_since(self.now_monotonic()) else { return; };
                if wait.is_zero() { return; }
                tokio::select! { () = changed => {}, () = tokio::time::sleep(wait) => return }
            }
        })
    }
}

struct NoQuotaHttp;
impl HttpClient for NoQuotaHttp {
    fn get<'a>(&'a self, _: &'a str, _: &'a Token) -> HttpFuture<'a> { panic!("test gerçek kota ağına çıkamaz") }
}

#[derive(Default)]
struct MockState {
    updates: Mutex<VecDeque<Value>>, sent: Mutex<Vec<Value>>, edited: Mutex<Vec<Value>>,
    poll_error: AtomicUsize, next_message: AtomicUsize, polls: AtomicUsize,
    clients: AtomicUsize, max_clients: AtomicUsize, created: AtomicUsize,
    hold_send_response: AtomicBool,
}
struct MockBot { state: Arc<MockState>, address: std::net::SocketAddr, cancel: CancellationToken, task: tokio::task::JoinHandle<()> }
impl MockBot {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(MockState::default()); let shared = state.clone();
        let cancel = CancellationToken::new(); let stop = cancel.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! { biased;
                    () = stop.cancelled() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    connection = listener.accept() => {
                        let (mut socket, _) = connection.unwrap(); let state = shared.clone();
                        connections.spawn(async move {
                            let mut bytes = Vec::new(); let mut buffer = [0u8; 4096];
                            let (head, body) = loop {
                                let n = socket.read(&mut buffer).await.unwrap(); if n == 0 { return; }
                                bytes.extend_from_slice(&buffer[..n]); assert!(bytes.len() < 64 * 1024);
                                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                                    let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
                                    let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                                    if bytes.len() >= end + 4 + length { break (head, serde_json::from_slice::<Value>(&bytes[end+4..end+4+length]).unwrap()); }
                                }
                            };
                            let method = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().rsplit('/').next().unwrap();
                            let mut status = 200;
                            let result = match method {
                                "getMe" => json!({"id":100,"is_bot":true}),
                                "getUpdates" => {
                                    state.polls.fetch_add(1, Ordering::SeqCst);
                                    status = state.poll_error.load(Ordering::SeqCst);
                                    if status == 999 { return; } // Mock bağlantı kesintisi.
                                    if status == 0 { status = 200; }
                                    let offset = body["offset"].as_i64().unwrap_or(0);
                                    let mut updates = state.updates.lock().unwrap();
                                    let mut output = Vec::new();
                                    while let Some(update) = updates.pop_front() { if update["update_id"].as_i64().unwrap() >= offset { output.push(update); } }
                                    json!(output)
                                }
                                "sendMessage" => {
                                    let message = state.next_message.fetch_add(1, Ordering::SeqCst) + 1;
                                    let mut body = body; body["message_id"] = json!(message); state.sent.lock().unwrap().push(body);
                                    if state.hold_send_response.load(Ordering::SeqCst) {
                                        std::future::pending::<()>().await;
                                    }
                                    json!({"message_id":message})
                                }
                                "editMessageText" => { state.edited.lock().unwrap().push(body); json!({"message_id":1}) }
                                "answerCallbackQuery" => json!(true),
                                _ => panic!("beklenmeyen Bot API metodu"),
                            };
                            let payload = if status == 200 { json!({"ok":true,"result":result}) }
                                else { json!({"ok":false,"error_code":status,"description":TOKEN}) };
                            let payload = payload.to_string();
                            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
                            let _ = socket.write_all(response.as_bytes()).await;
                        });
                    }
                }
            }
            connections.shutdown().await;
        });
        Self { state, address, cancel, task }
    }
    fn push(&self, update: Value) { self.state.updates.lock().unwrap().push_back(update); }
    async fn shutdown(self) { self.cancel.cancel(); self.task.await.unwrap(); }
}

struct TrackedClient { client: BotApiClient, state: Arc<MockState> }
impl Drop for TrackedClient { fn drop(&mut self) { self.state.clients.fetch_sub(1, Ordering::SeqCst); } }
impl TelegramTransport for TrackedClient {
    fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>> { self.client.get_me() }
    fn get_updates(&self, offset: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>> { self.client.get_updates(offset) }
    fn send_message(&self, message: SendMessage) -> BoxFuture<'_, Result<SentMessage, TransportError>> { self.client.send_message(message) }
    fn edit_message_text(&self, message: EditMessageText) -> BoxFuture<'_, Result<(), TransportError>> { self.client.edit_message_text(message) }
    fn answer_callback_query(&self, answer: AnswerCallbackQuery) -> BoxFuture<'_, Result<(), TransportError>> { self.client.answer_callback_query(answer) }
}
impl PollSource for TrackedClient {
    fn poll(&self, offset: Option<i64>) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, BotApiError>> { Box::pin(self.client.get_updates_checked(offset)) }
}

fn isolate_runtime() {
    server::isolate_test_runtime();
}
fn config(ids: Vec<i64>) -> Config {
    Config { default_engine: EngineId::Claude, summarizer: SummarizerMode::Off, claude_live_quota: false,
        telegram: TelegramConfig { allowed_user_ids: ids, ..Default::default() }, ..Default::default() }
}
fn paths(temp: &tempfile::TempDir) -> Paths { let paths = Paths::from_root(temp.path().join("state")); paths.ensure().unwrap(); paths }
fn engines(engine: Arc<dyn Engine>) -> HashMap<EngineId, Arc<dyn Engine>> { HashMap::from([(EngineId::Claude, engine)]) }
async fn start(paths: &Paths, cfg: Config, engines: HashMap<EngineId, Arc<dyn Engine>>, store: Arc<MemoryCredentials>, clock: Arc<ManualClock>, bot: &MockBot) -> ServerHandle {
    start_with_probe(paths, cfg, engines, store, clock, bot, None).await
}
async fn start_with_probe(paths: &Paths, cfg: Config, engines: HashMap<EngineId, Arc<dyn Engine>>, store: Arc<MemoryCredentials>, clock: Arc<ManualClock>, bot: &MockBot, probe: Option<Arc<dyn ResetProbe>>) -> ServerHandle {
    isolate_runtime();
    let quota = Arc::new(QuotaService::new(QuotaSources::from_home(&paths.root.join("quota")), Arc::new(NoQuotaHttp)));
    let state = bot.state.clone(); let address = bot.address;
    server::start_with_test_dependencies(ServerConfig { paths: paths.clone(), pipe_name: format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4()), cfg, engines }, store, clock,
        Arc::new(move |token, cancel| {
            let client = BotApiClient::for_test(token, cancel, address, Duration::from_secs(3)).map_err(|e| e.code)?;
            let active = state.clients.fetch_add(1, Ordering::SeqCst) + 1; state.max_clients.fetch_max(active, Ordering::SeqCst); state.created.fetch_add(1, Ordering::SeqCst);
            let client = Arc::new(TrackedClient { client, state: state.clone() }); Ok((client.clone(), client))
        }), quota, probe).await.unwrap()
}
async fn call(h: &ServerHandle, approver: bool, request: Request) -> Response {
    let mut client = Client::connect(h.pipe_name(), if approver { h.approver_token() } else { h.token() }).await.unwrap();
    client.send(&request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), client.next()).await.unwrap().unwrap().unwrap()
}
async fn data(h: &ServerHandle, approver: bool, request: Request) -> Value {
    match call(h, approver, request).await { Response::Ok { data } => data, other => panic!("başarı bekleniyordu: {other:?}") }
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async { while !predicate() { tokio::time::sleep(Duration::from_millis(20)).await; } }).await.expect("sentetik koşul oluşmadı");
}
fn message(update: i64, user: i64, text: &str) -> Value { json!({"update_id":update,"message":{"message_id":update+1,"from":{"id":user,"is_bot":false},"chat":{"id":user,"type":"private"},"text":text}}) }
fn callback(update: i64, notification: &Value, approve: bool) -> Value {
    json!({"update_id":update,"callback_query":{"id":format!("callback-{update}"),"from":{"id":101,"is_bot":false},"message":{"message_id":notification["message_id"],"chat":{"id":101,"type":"private"}},"data":notification["reply_markup"]["inline_keyboard"][0][if approve { 0 } else { 1 }]["callback_data"]}})
}

#[tokio::test]
async fn daemon_empty_allowlist_and_missing_token_are_offline() {
    let bot = MockBot::new().await;
    for ids in [vec![], vec![101]] {
        let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp); let store = Arc::new(MemoryCredentials::default());
        let h = start(&paths, config(ids.clone()), HashMap::new(), store.clone(), Arc::new(ManualClock::new()), &bot).await;
        let status = data(&h, false, Request::TelegramStatus).await;
        assert_eq!(status["state"], if ids.is_empty() { "missing_allowed_users" } else { "missing_token" });
        assert_eq!(store.reads.load(Ordering::SeqCst), usize::from(!ids.is_empty()));
        assert_eq!(bot.state.created.load(Ordering::SeqCst), 0);
        h.shutdown().await;
    }
    bot.shutdown().await;
}

#[tokio::test]
async fn daemon_ipc_roles_and_ninety_second_heartbeat() {
    let bot = MockBot::new().await; let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp);
    let store = Arc::new(MemoryCredentials::default()); let clock = Arc::new(ManualClock::new());
    let h = start(&paths, config(vec![]), HashMap::new(), store.clone(), clock.clone(), &bot).await;
    for request in [Request::TelegramSetToken { token: TOKEN.into() }, Request::TelegramTest,
        Request::SetTelegramSettings { notify: ultron_core::config::NotifyMode::Away, auto_continue_enabled: false, auto_continue_wait_minutes: 13, fallback_order: vec![] },
        Request::DesktopHeartbeat { instance_id: "test-desktop".into() }] {
        assert!(matches!(call(&h, false, request).await, Response::Error { message } if message.contains("yetkisiz")));
    }
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert_eq!(data(&h, false, Request::Status).await["away"], true);
    data(&h, false, Request::GetTelegramSettings).await;
    data(&h, true, Request::DesktopHeartbeat { instance_id: "test-desktop".into() }).await;
    clock.advance(89); assert_eq!(data(&h, false, Request::Status).await["away"], false);
    clock.advance(1); assert_eq!(data(&h, false, Request::Status).await["away"], true);
    data(&h, true, Request::TelegramSetToken { token: TOKEN.into() }).await;
    assert_eq!(store.writes.load(Ordering::SeqCst), 1); assert_eq!(bot.state.created.load(Ordering::SeqCst), 0);
    data(&h, true, Request::SetTelegramSettings { notify: ultron_core::config::NotifyMode::Away, auto_continue_enabled: false, auto_continue_wait_minutes: 13, fallback_order: vec![] }).await;
    assert_eq!(data(&h, false, Request::GetTelegramSettings).await["auto_continue_wait_minutes"], 13);
    assert!(matches!(call(&h, true, Request::TelegramTest).await, Response::Error { .. }));
    h.shutdown().await; bot.shutdown().await;
}

#[tokio::test]
async fn daemon_rotation_reconnect_shutdown_and_redaction() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE)
        .with_ansi(false).with_writer(capture.clone()).finish();
    let _logs = tracing::subscriber::set_default(subscriber);
    let bot = MockBot::new().await; let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp);
    let store = Arc::new(MemoryCredentials { value: Mutex::new(Some(TOKEN.into())), ..Default::default() });
    let clock = Arc::new(ManualClock::new());
    let h = start(&paths, config(vec![101]), HashMap::new(), store, clock.clone(), &bot).await;
    wait_for(|| bot.state.polls.load(Ordering::SeqCst) > 0).await;
    assert_eq!(data(&h, false, Request::TelegramStatus).await["state"], "connected");
    assert_eq!(data(&h, true, Request::TelegramTest).await["delivered"], 1);
    for _ in 0..3 { data(&h, true, Request::TelegramSetToken { token: TOKEN.into() }).await; }
    wait_for(|| bot.state.polls.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(bot.state.max_clients.load(Ordering::SeqCst), 1);
    // 409 terminal poller hatası: lifecycle yeniden denemeden önce geri çekilir.
    bot.state.poll_error.store(409, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let status = data(&h, false, Request::TelegramStatus).await;
    assert_eq!(status["state"], "error"); assert_eq!(status["error"], "poll_conflict");
    assert!(!status.to_string().contains(TOKEN));
    bot.state.poll_error.store(0, Ordering::SeqCst); clock.advance(64);
    let before = bot.state.polls.load(Ordering::SeqCst);
    wait_for(|| bot.state.polls.load(Ordering::SeqCst) > before).await;
    assert_eq!(data(&h, false, Request::TelegramStatus).await["state"], "connected");
    bot.state.poll_error.store(999, Ordering::SeqCst);
    let mut disconnected = false;
    for _ in 0..250 {
        if data(&h, false, Request::TelegramStatus).await["error"] == "network" { disconnected = true; break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(disconnected, "bağlantı kesintisi status'a yansımalı");
    bot.state.poll_error.store(0, Ordering::SeqCst); clock.advance(64);
    let before = bot.state.polls.load(Ordering::SeqCst);
    wait_for(|| bot.state.polls.load(Ordering::SeqCst) > before).await;
    assert_eq!(data(&h, false, Request::TelegramStatus).await["state"], "connected");
    let db = Db::open(&paths.db).unwrap();
    for chat in db.list_chats(None, true).unwrap() { assert!(!serde_json::to_string(&db.events_for_chat(&chat.id, None).unwrap()).unwrap().contains(TOKEN)); }
    h.shutdown().await; assert_eq!(bot.state.clients.load(Ordering::SeqCst), 0);
    let polls = bot.state.polls.load(Ordering::SeqCst); clock.advance(1000);
    tokio::time::sleep(Duration::from_millis(100)).await; assert_eq!(bot.state.polls.load(Ordering::SeqCst), polls);
    for file in std::fs::read_dir(&paths.transcripts).unwrap().flatten() { assert!(!std::fs::read_to_string(file.path()).unwrap().contains(TOKEN)); }
    assert!(!std::fs::read_to_string(paths.root.join("config.toml")).unwrap_or_default().contains(TOKEN));
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for secret in [TOKEN.to_owned(), TOKEN.replace(':', "%3A")] { assert!(!logs.contains(&secret)); }
    bot.shutdown().await;
}

type ChildConnection = Arc<Mutex<Option<(String, String)>>>;
struct PolicyEngine { connection: ChildConnection, calls: Arc<AtomicUsize> }
impl Engine for PolicyEngine {
    fn id(&self) -> EngineId { EngineId::Claude }
    fn capabilities(&self) -> Capabilities { Capabilities { streaming: true, resume: false, mid_turn_input: false, tool_restriction: RestrictionLevel::Full } }
    fn command(&self, request: &TurnRequest) -> CommandSpec {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CommandSpec { program: std::env::current_exe().unwrap().to_string_lossy().into_owned(), args: vec!["--exact".into(), "telegram::integration_tests::synthetic_policy_engine_child".into(), "--nocapture".into()], cwd: request.cwd.clone(), stdin: None }
    }
    fn prepare_command(&self, request: &TurnRequest, command: &mut tokio::process::Command) -> anyhow::Result<()> {
        // Claude talimat bayrağı gerçek CLI'ye aittir; sentetik test harness'ı
        // kendi argv'siyle çalışır. Runner'ın Job Object/turn-env sınırı devam eder.
        *command = tokio::process::Command::new(std::env::current_exe()?);
        command.args(["--exact", "telegram::integration_tests::synthetic_policy_engine_child", "--nocapture"])
            .current_dir(&request.cwd).stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let connection = self.connection.lock().unwrap(); let (pipe, token) = connection.as_ref().unwrap();
        command.env("ULTRON_SYNTHETIC_PIPE", pipe).env("ULTRON_SYNTHETIC_REQUESTER", token).env("ULTRON_SYNTHETIC_CHAT", &request.chat_id); Ok(())
    }
    fn parse_line(&self, line: &str) -> Vec<EngineEvent> { serde_json::from_str(line).ok().into_iter().collect() }
}

/// Ayrı süreçte yalnız sahte motor çalıştırır; normal test çağrısında yan etkisizdir.
#[test]
fn synthetic_policy_engine_child() {
    let Ok(pipe) = std::env::var("ULTRON_SYNTHETIC_PIPE") else { return; };
    let token = std::env::var("ULTRON_SYNTHETIC_REQUESTER").unwrap(); let chat = std::env::var("ULTRON_SYNTHETIC_CHAT").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        println!("{}", json!({"type":"session_started","native_session_id":"synthetic-native"}));
        let mut client = Client::connect(&pipe, &token).await.unwrap();
        // Eylem yalnız policy girdisidir; hiçbir Git komutu yürütülmez.
        client.send(&Request::PolicyCheck { action: ultron_core::policy::Action::ToolCall { tool: "Bash".into(), input: json!({"command":"git reset --hard"}) }, chat_id: Some(chat), engine: EngineId::Claude, wait_secs: 15 }).await.unwrap();
        let Response::Ok { data } = client.next().await.unwrap().unwrap() else { panic!("policy yanıtı yok"); };
        let decision: ultron_core::policy::Decision = serde_json::from_value(data["decision"].clone()).unwrap();
        let text = if matches!(decision, ultron_core::policy::Decision::Allow) { "Sentetik araç onaylandı." } else { "Sentetik araç reddedildi." };
        println!("{}", json!({"type":"assistant_text","text":text}));
        println!("{}", json!({"type":"turn_completed"}));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_telegram_command_approval_and_reply() {
    for scenario in ["approve", "deny", "desktop_wins"] {
        let bot = MockBot::new().await; let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp);
        let connection = Arc::new(Mutex::new(None)); let calls = Arc::new(AtomicUsize::new(0));
        let engine = Arc::new(PolicyEngine { connection: connection.clone(), calls: calls.clone() });
        let store = Arc::new(MemoryCredentials { value: Mutex::new(Some(TOKEN.into())), ..Default::default() });
        let h = start(&paths, config(vec![101]), engines(engine), store, Arc::new(ManualClock::new()), &bot).await;
        *connection.lock().unwrap() = Some((h.pipe_name().into(), h.token().into()));
        let mut event_stream = Client::connect(h.pipe_name(), h.token()).await.unwrap();
        event_stream.send(&Request::Events { chat_id: None, follow: true, after_seq: None, limit: None }).await.unwrap();
        assert!(matches!(event_stream.next().await.unwrap().unwrap(), Response::Ok { data } if data["subscribed"] == true));
        bot.push(message(1, 202, "izinsiz")); // İçerik/kimlik persist edilmez.
        bot.push(message(2, 101, "sentetik hedef"));
        let approval_wait = wait_for(|| bot.state.sent.lock().unwrap().iter().any(|m| m["reply_markup"].is_object()));
        tokio::select! {
            () = approval_wait => {},
            () = tokio::time::sleep(Duration::from_secs(9)) => {
                let db = Db::open(&paths.db).unwrap();
                let events = db.list_chats(None, true).unwrap().into_iter().map(|c| db.events_for_chat(&c.id, None).unwrap()).collect::<Vec<_>>();
                panic!("{scenario}: araç onayı oluşmadı; sentetik olaylar: {events:?}");
            }
        }
        let notification = bot.state.sent.lock().unwrap().iter().find(|m| m["reply_markup"].is_object()).unwrap().clone();
        let db = Db::open(&paths.db).unwrap();
        let pending = db.list_approvals(Some(ultron_core::db::approvals::ApprovalStatus::Pending)).unwrap(); assert_eq!(pending.len(), 1);
        if scenario == "desktop_wins" { data(&h, true, Request::ResolveApproval { id: pending[0].id.clone(), approve: false, by: "desktop".into() }).await; }
        bot.push(callback(3, &notification, scenario != "deny"));
        wait_for(|| bot.state.sent.lock().unwrap().iter().any(|m| m["text"].as_str().is_some_and(|s| s.contains("Tur tamamlandı.")))).await;
        let final_message = bot.state.sent.lock().unwrap().iter().find(|m| m["text"].as_str().is_some_and(|s| s.contains("Tur tamamlandı."))).unwrap().clone();
        assert!(final_message["text"].as_str().unwrap().contains(if scenario == "approve" { "onaylandı" } else { "reddedildi" }));
        assert_eq!(final_message["chat_id"], 101); assert!(final_message.get("parse_mode").is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let binding = db.telegram_binding(101).unwrap().unwrap(); let events = db.events_for_chat(&binding, None).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "user_text").count(), 1);
        assert!(!serde_json::to_string(&events).unwrap().contains("izinsiz"));
        let mut streamed = Vec::new();
        loop {
            let Response::Event(event) = tokio::time::timeout(Duration::from_secs(5), event_stream.next()).await.unwrap().unwrap().unwrap() else { panic!("olay bekleniyordu"); };
            let done = event.kind == "turn_completed"; streamed.push(event); if done { break; }
        }
        assert_eq!(streamed, events, "DB kabulü ve onay olayları global IPC'de tekrarsız sırayla telafi edilmeli");
        let transcript = std::fs::read_to_string(paths.transcripts.join(format!("{binding}.jsonl"))).unwrap();
        let transcript_ids = transcript.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(transcript_ids, events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
        assert!(!transcript.contains(TOKEN));
        // Yinelenen update yeni motor turu başlatamaz.
        bot.push(message(2, 101, "sentetik hedef")); tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        h.shutdown().await; assert_eq!(bot.state.clients.load(Ordering::SeqCst), 0); bot.shutdown().await;
    }
}

fn seed_wait(paths: &Paths, id: &str, state: QuotaWaitState) {
    let db = Arc::new(Db::open(&paths.db).unwrap()); db.insert_chat(id, None, "sentetik", Utc::now()).unwrap();
    let cwd = paths.root.join("workspaces").join(id); std::fs::create_dir_all(&cwd).unwrap();
    db.set_chat_engine(id, EngineId::Claude).unwrap();
    db.upsert_session(&SessionRow { id: format!("session-{id}"), chat_id: id.into(), engine: EngineId::Claude, native_session_id: Some("synthetic-native".into()), status: SessionStatus::WaitingQuota, started_at: Utc::now(), ended_at: None }).unwrap();
    let log = EventLog::new(db.clone(), paths.clone());
    let user = log.append(id, None, EngineEvent::UserText { text: "özgün hedef".into() }).unwrap();
    let quota = log.append(id, Some(&format!("session-{id}")), EngineEvent::QuotaExhausted { message: "sentetik kota".into(), resets_at: Some(Utc::now() + chrono::Duration::seconds(20)) }).unwrap();
    let wait = db.create_quota_wait(&NewQuotaWait { id: id.into(), logical_turn_id: format!("logical-{id}"), chat_id: id.into(), source_session_id: format!("session-{id}"), engine: EngineId::Claude, user_event_seq: user.seq, checkpoint_seq: quota.seq, cwd, policy: ResumePolicy::ReadOnly, reset_at: Some(Utc::now() + chrono::Duration::seconds(20)), next_attempt_at: None }, Utc::now()).unwrap();
    if state != QuotaWaitState::Waiting {
        if state == QuotaWaitState::Cancelled { db.cancel_quota_waits(id, Utc::now()).unwrap(); }
        else {
            let claimed = db.claim_quota_wait(&wait.id, wait.generation, Utc::now()).unwrap().unwrap();
            if state != QuotaWaitState::Claimed {
                let running = db.advance_quota_wait(&wait.id, claimed.generation, QuotaWaitState::Running, None, Utc::now()).unwrap().unwrap();
                if state == QuotaWaitState::Completed { db.advance_quota_wait(&wait.id, running.generation, QuotaWaitState::Completed, None, Utc::now()).unwrap(); }
            }
        }
    }
}

#[tokio::test]
async fn daemon_restart_resumes_quota_wait_once() {
    let bot = MockBot::new().await; let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp);
    for (id, state) in [("waiting", QuotaWaitState::Waiting), ("cancelled", QuotaWaitState::Cancelled), ("completed", QuotaWaitState::Completed), ("claimed", QuotaWaitState::Claimed), ("running", QuotaWaitState::Running)] { seed_wait(&paths, id, state); }
    let fixture = paths.root.join("done.jsonl");
    std::fs::write(&fixture, "{\"type\":\"session_started\",\"native_session_id\":\"synthetic-native\"}\n{\"type\":\"assistant_text\",\"text\":\"devam tamam\"}\n{\"type\":\"turn_completed\"}\n").unwrap();
    let clock = Arc::new(ManualClock::new()); let store = Arc::new(MemoryCredentials::default());
    let h = start(&paths, config(vec![]), engines(Arc::new(FakeEngine::from_fixture(&fixture))), store.clone(), clock.clone(), &bot).await;
    assert_eq!(h.running_turns(), 0);
    assert_eq!(data(&h, false, Request::Status).await["quota_wait_tasks"], 1);
    h.shutdown().await;
    // DB tekrar açılır ve gelecekteki timer restore edilir; yerel fake CLI bir kez çalışır.
    let h = start(&paths, config(vec![]), engines(Arc::new(FakeEngine::from_fixture(&fixture))), store, clock.clone(), &bot).await;
    let db = Db::open(&paths.db).unwrap(); clock.advance(40);
    wait_for(|| db.quota_wait("waiting").unwrap().unwrap().state == QuotaWaitState::Completed).await;
    clock.advance(1000); tokio::time::sleep(Duration::from_millis(100)).await;
    let events = db.events_for_chat("waiting", None).unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "user_text").count(), 1);
    assert_eq!(events.iter().filter(|e| e.kind == "turn_completed").count(), 1);
    assert!(!events.iter().any(|e| e.kind == "quota_wait_stopped"));
    for id in ["cancelled", "completed", "claimed", "running"] {
        assert!(!db.events_for_chat(id, None).unwrap().iter().any(|e| e.kind == "turn_completed"));
    }
    assert_eq!(db.quota_wait("claimed").unwrap().unwrap().state, QuotaWaitState::NeedsAttention);
    assert_eq!(db.quota_wait("running").unwrap().unwrap().state, QuotaWaitState::NeedsAttention);
    h.shutdown().await; bot.shutdown().await;
}

async fn send_to_completion(h: &ServerHandle, chat: &str) {
    let mut client = Client::connect(h.pipe_name(), h.token()).await.unwrap();
    client.send(&Request::Send { chat_id: Some(chat.into()), cwd: String::new(), engine: Some(EngineId::Claude),
        text: "özgün sentetik hedef".into(), policy: ultron_core::ipc::PolicyArg::ReadOnly, elevated: false, project_id: None }).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), client.next()).await.unwrap().unwrap().unwrap() {
            Response::Ok { data } if data["done"] == true => { assert_eq!(data["outcome"], "quota_exhausted"); return; }
            Response::Error { message } => panic!("sentetik kota gönderimi: {message}"),
            _ => {},
        }
    }
}

#[tokio::test]
async fn daemon_normal_quota_registration_cancel_switch_and_supersession() {
    let bot = MockBot::new().await; let temp = tempfile::tempdir().unwrap(); let paths = paths(&temp);
    let fixture = paths.root.join("quota.jsonl");
    let reset = Utc::now() + chrono::Duration::minutes(10);
    std::fs::write(&fixture, format!("{}\n{}\n", json!({"type":"session_started","native_session_id":"synthetic-native"}),
        json!({"type":"quota_exhausted","message":"sentetik kota","resets_at":reset}))).unwrap();
    let clock = Arc::new(ManualClock::new());
    let engine: Arc<dyn Engine> = Arc::new(FakeEngine::from_fixture(&fixture));
    let mut registered = engines(engine.clone()); registered.insert(EngineId::Codex, engine);
    let h = start(&paths, config(vec![]), registered, Arc::new(MemoryCredentials::default()), clock.clone(), &bot).await;
    let db = Db::open(&paths.db).unwrap();
    for chat in ["cancel", "switch", "new-user"] { send_to_completion(&h, chat).await; }
    assert_eq!(h.running_turns(), 0);
    assert_eq!(data(&h, false, Request::Status).await["quota_wait_tasks"], 3);
    let waits = db.pending_quota_waits().unwrap(); assert_eq!(waits.len(), 3);
    data(&h, false, Request::Cancel { chat_id: "cancel".into() }).await;
    data(&h, false, Request::Switch { chat_id: "switch".into(), to: EngineId::Codex }).await;
    send_to_completion(&h, "new-user").await;
    for wait in &waits { assert_eq!(db.quota_wait(&wait.id).unwrap().unwrap().state, QuotaWaitState::Cancelled); }
    assert_eq!(data(&h, false, Request::Status).await["quota_wait_tasks"], 1);
    // Disabled ayarı watcher'dan scheduler'a ulaşır ve timer keep-awake tutucusunu bırakır.
    data(&h, true, Request::SetTelegramSettings { notify: ultron_core::config::NotifyMode::Always,
        auto_continue_enabled: false, auto_continue_wait_minutes: 45, fallback_order: vec![] }).await;
    let mut disabled = false;
    for _ in 0..50 {
        if data(&h, false, Request::Status).await["quota_wait_tasks"] == 0 { disabled = true; break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(disabled); assert_eq!(h.running_turns(), 0);
    clock.advance(1000); tokio::time::sleep(Duration::from_millis(50)).await;
    for chat in ["cancel", "switch"] { assert_eq!(db.events_for_chat(chat, None).unwrap().iter().filter(|e| e.kind == "user_text").count(), 1); }
    h.shutdown().await; bot.shutdown().await;
}

fn local_send(chat: &str, cwd: String, policy: PolicyArg) -> Request {
    Request::Send {
        chat_id: Some(chat.into()), cwd, engine: Some(EngineId::Claude),
        text: "sentetik yerel mesaj".into(), policy, elevated: false, project_id: None,
    }
}

async fn start_send(h: &ServerHandle, request: Request) -> Client {
    let mut client = Client::connect(h.pipe_name(), h.token()).await.unwrap();
    client.send(&request).await.unwrap();
    client
}

async fn next_send(client: &mut Client) -> Response {
    tokio::time::timeout(Duration::from_secs(10), client.next()).await.unwrap().unwrap().unwrap()
}

async fn expect_outcome(client: &mut Client, expected: &str) {
    loop {
        match next_send(client).await {
            Response::Ok { data } if data["done"] == true => {
                assert_eq!(data["outcome"], expected);
                return;
            }
            Response::Error { message } => panic!("sentetik Send başarısız: {message}"),
            _ => {}
        }
    }
}

struct FirstResetBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: AtomicUsize,
}

impl ResetProbe for FirstResetBarrier {
    fn blocking_reset(&self, _: EngineId, now: chrono::DateTime<Utc>) -> BoxFuture<'_, Option<chrono::DateTime<Utc>>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Some(now + chrono::Duration::minutes(10))
        })
    }
}

#[tokio::test]
async fn daemon_new_send_supersedes_delayed_quota_registration() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let fixture = paths.root.join("no-reset.jsonl");
    std::fs::write(&fixture, "{\"type\":\"session_started\",\"native_session_id\":\"synthetic-native\"}\n{\"type\":\"quota_exhausted\",\"message\":\"sentetik kota\",\"resets_at\":null}\n").unwrap();
    let probe = Arc::new(FirstResetBarrier { entered: Default::default(), release: Default::default(), calls: AtomicUsize::new(0) });
    let h = start_with_probe(&paths, config(vec![]), engines(Arc::new(FakeEngine::from_fixture(&fixture))),
        Arc::new(MemoryCredentials::default()), Arc::new(ManualClock::new()), &bot, Some(probe.clone())).await;
    let chat = "quota-race";
    let mut first = start_send(&h, local_send(chat, String::new(), PolicyArg::ReadOnly)).await;
    assert!(matches!(next_send(&mut first).await, Response::Ok { data } if data["chat_id"] == chat));
    tokio::time::timeout(Duration::from_secs(10), probe.entered.notified()).await.unwrap();
    assert_eq!(h.running_turns(), 0);

    // Probe kilit dışında beklediği için yeni Send hemen kabul edilir ve eski nesli geçersiz kılar.
    let mut second = start_send(&h, local_send(chat, String::new(), PolicyArg::ReadOnly)).await;
    assert!(matches!(next_send(&mut second).await, Response::Ok { data } if data["chat_id"] == chat));
    probe.release.notify_one();
    expect_outcome(&mut first, "quota_exhausted").await;
    expect_outcome(&mut second, "quota_exhausted").await;

    let db = Db::open(&paths.db).unwrap();
    let events = db.events_for_chat(chat, None).unwrap();
    let users = events.iter().filter(|e| e.kind == "user_text").collect::<Vec<_>>();
    let waits = db.pending_quota_waits().unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].user_event_seq, users[1].seq);
    assert_eq!(waits[0].state, QuotaWaitState::Waiting);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.tracked_turns().await, 0);
    h.shutdown().await;
    bot.shutdown().await;
}

#[tokio::test]
async fn daemon_slow_reset_probe_does_not_block_send_on_another_chat() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let fixture = paths.root.join("no-reset.jsonl");
    std::fs::write(&fixture, "{\"type\":\"session_started\",\"native_session_id\":\"synthetic-native\"}
{\"type\":\"quota_exhausted\",\"message\":\"sentetik kota\",\"resets_at\":null}
").unwrap();
    let probe = Arc::new(FirstResetBarrier { entered: Default::default(), release: Default::default(), calls: AtomicUsize::new(0) });
    let h = start_with_probe(&paths, config(vec![]), engines(Arc::new(FakeEngine::from_fixture(&fixture))),
        Arc::new(MemoryCredentials::default()), Arc::new(ManualClock::new()), &bot, Some(probe.clone())).await;
    let mut first = start_send(&h, local_send("slow-a", String::new(), PolicyArg::ReadOnly)).await;
    assert!(matches!(next_send(&mut first).await, Response::Ok { data } if data["chat_id"] == "slow-a"));
    tokio::time::timeout(Duration::from_secs(10), probe.entered.notified()).await.unwrap();
    let mut other = start_send(&h, local_send("slow-b", String::new(), PolicyArg::ReadOnly)).await;
    assert!(matches!(next_send(&mut other).await, Response::Ok { data } if data["chat_id"] == "slow-b"));
    expect_outcome(&mut other, "quota_exhausted").await;
    probe.release.notify_one();
    expect_outcome(&mut first, "quota_exhausted").await;
    h.shutdown().await;
    bot.shutdown().await;
}

#[tokio::test]
async fn daemon_full_send_streams_elevation_after_ack_before_decision() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let h = start(&paths, config(vec![]), engines(Arc::new(FakeEngine::with_mode(&[], false))),
        Arc::new(MemoryCredentials::default()), Arc::new(ManualClock::new()), &bot).await;
    let chat = "full-send";
    let mut stream = start_send(&h, local_send(chat, String::new(), PolicyArg::Full)).await;
    assert!(matches!(next_send(&mut stream).await, Response::Ok { data } if data == json!({"chat_id": chat})));
    let Response::Event(event) = next_send(&mut stream).await else { panic!("Full onay olayı bekleniyordu"); };
    let EngineEvent::ApprovalRequested { approval_id, .. } = serde_json::from_value(event.payload).unwrap()
        else { panic!("Full onayı kullanıcı olayından önce gelmeli"); };
    let db = Db::open(&paths.db).unwrap();
    assert_eq!(db.get_approval(&approval_id).unwrap().unwrap().status, ultron_core::db::approvals::ApprovalStatus::Pending);
    data(&h, true, Request::ResolveApproval { id: approval_id, approve: false, by: "desktop".into() }).await;
    while !matches!(next_send(&mut stream).await, Response::Error { .. }) {}
    h.shutdown().await;
    bot.shutdown().await;
}

struct CwdEngine {
    inner: FakeEngine,
    seen: Mutex<Vec<std::path::PathBuf>>,
}

impl Engine for CwdEngine {
    fn id(&self) -> EngineId { self.inner.id() }
    fn capabilities(&self) -> Capabilities { self.inner.capabilities() }
    fn command(&self, request: &TurnRequest) -> CommandSpec {
        self.seen.lock().unwrap().push(request.cwd.clone());
        self.inner.command(request)
    }
    fn parse_line(&self, line: &str) -> Vec<EngineEvent> { self.inner.parse_line(line) }
}

#[tokio::test]
async fn daemon_home_aliases_use_a_scratch_cwd() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let home = paths.root.join("synthetic-user-home");
    std::fs::create_dir(&home).unwrap();
    let junction = paths.root.join("home-junction");
    let result = std::process::Command::new("cmd.exe").args(["/D", "/C", "mklink", "/J"])
        .arg(&junction).arg(&home).output().unwrap();
    assert!(result.status.success(), "sentetik junction oluşturulamadı: {:?}", result.status);
    let fixture = paths.root.join("cwd.jsonl");
    std::fs::write(&fixture, "{\"type\":\"session_started\",\"native_session_id\":\"synthetic-native\"}\n{\"type\":\"turn_completed\"}\n").unwrap();
    let engine = Arc::new(CwdEngine { inner: FakeEngine::from_fixture(&fixture), seen: Mutex::new(Vec::new()) });
    let h = start(&paths, config(vec![]), engines(engine.clone()), Arc::new(MemoryCredentials::default()),
        Arc::new(ManualClock::new()), &bot).await;
    for (chat, cwd) in [("home-dot", home.join(".")), ("home-junction", junction)] {
        let mut stream = start_send(&h, local_send(chat, cwd.to_string_lossy().into_owned(), PolicyArg::ReadOnly)).await;
        expect_outcome(&mut stream, "completed").await;
        let actual = engine.seen.lock().unwrap().last().unwrap().canonicalize().unwrap();
        assert_eq!(actual, paths.root.join("workspaces").join(chat).canonicalize().unwrap());
        assert_ne!(actual, home.canonicalize().unwrap());
    }
    h.shutdown().await;
    bot.shutdown().await;
}

fn seed_telegram_wait(paths: &Paths, chat: &str) -> ultron_core::db::quota_wait::QuotaWait {
    let db = Arc::new(Db::open(&paths.db).unwrap());
    db.insert_chat(chat, None, "sentetik", Utc::now()).unwrap();
    db.set_chat_engine(chat, EngineId::Claude).unwrap();
    let session = format!("session-{chat}");
    db.upsert_session(&SessionRow { id: session.clone(), chat_id: chat.into(), engine: EngineId::Claude,
        native_session_id: Some("synthetic-native".into()), status: SessionStatus::WaitingQuota,
        started_at: Utc::now(), ended_at: None }).unwrap();
    let logical = format!("logical-{chat}");
    let event = StoredEvent { id: "seed-user".into(), chat_id: chat.into(), session_id: None, seq: 0, ts: Utc::now(),
        kind: "user_text".into(), payload: json!({"type":"user_text","text":"sentetik Telegram mesajı"}) };
    let TelegramAcceptance::Accepted(user) = db.accept_telegram_turn(&TelegramTurnAcceptance { bot_id: 100, update_id: 1,
        user_id: 101, private_chat_id: 101, logical_turn_id: &logical, event: &event }).unwrap()
        else { panic!("sentetik Telegram turu kabul edilemedi"); };
    assert!(db.start_telegram_inbox(100, 1).unwrap());
    let reset = Utc::now() + chrono::Duration::minutes(10);
    let log = EventLog::new(db.clone(), paths.clone());
    let quota = log.append(chat, Some(&session), EngineEvent::QuotaExhausted {
        message: "sentetik kota".into(), resets_at: Some(reset),
    }).unwrap();
    let cwd = paths.root.join("workspaces").join(chat);
    std::fs::create_dir_all(&cwd).unwrap();
    db.create_quota_wait(&NewQuotaWait { id: format!("wait-{chat}"), logical_turn_id: logical, chat_id: chat.into(),
        source_session_id: session, engine: EngineId::Claude, user_event_seq: user.seq, checkpoint_seq: quota.seq,
        cwd, policy: ResumePolicy::ReadOnly, reset_at: Some(reset), next_attempt_at: None }, Utc::now()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_stopped_telegram_quota_does_not_route_later_local_approval() {
    for scenario in ["cancel", "switch", "supersede", "restart"] {
        let bot = MockBot::new().await;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let wait = seed_telegram_wait(&paths, scenario);
        let db = Db::open(&paths.db).unwrap();
        if scenario == "restart" {
            // Eski daemon yalnız kota kaydını kapatıp gelen kutusunu açık bırakmıştı.
            assert!(db.advance_quota_wait(&wait.id, wait.generation, QuotaWaitState::Cancelled, None, Utc::now()).unwrap().is_some());
        }
        let connection = Arc::new(Mutex::new(None));
        let engine = Arc::new(PolicyEngine { connection: connection.clone(), calls: Arc::new(AtomicUsize::new(0)) });
        let mut registered = engines(engine.clone());
        registered.insert(EngineId::Codex, engine);
        let mut cfg = config(vec![101]);
        cfg.telegram.notify = ultron_core::config::NotifyMode::Away;
        let store = Arc::new(MemoryCredentials { value: Mutex::new(Some(TOKEN.into())), ..Default::default() });
        let h = start(&paths, cfg, registered, store, Arc::new(ManualClock::new()), &bot).await;
        *connection.lock().unwrap() = Some((h.pipe_name().into(), h.token().into()));
        data(&h, true, Request::DesktopHeartbeat { instance_id: "sentetik-desktop".into() }).await;
        wait_for(|| bot.state.polls.load(Ordering::SeqCst) > 0).await;
        match scenario {
            "cancel" => { data(&h, false, Request::Cancel { chat_id: scenario.into() }).await; }
            "switch" => { data(&h, false, Request::Switch { chat_id: scenario.into(), to: EngineId::Codex }).await; }
            _ => {}
        }
        if scenario != "supersede" {
            assert_eq!(db.telegram_inbox_entry(100, 1).unwrap().unwrap().state, InboxState::NeedsAttention, "{scenario}");
            let origin = db.telegram_turn_origin(&wait.logical_turn_id).unwrap().unwrap();
            assert_eq!(origin.delivery_state, DeliveryState::NeedsAttention, "{scenario}");
            assert!(origin.end_seq.is_some_and(|seq| seq >= wait.checkpoint_seq), "{scenario}");
        }

        let mut local = start_send(&h, local_send(scenario, String::new(), PolicyArg::Workspace)).await;
        assert!(matches!(next_send(&mut local).await, Response::Ok { data } if data["chat_id"] == scenario));
        let policy_approval = loop {
            match next_send(&mut local).await {
                Response::Event(event) if event.kind == "approval_requested" => {
                    break (event.seq, event.payload["approval_id"].as_str().unwrap().to_owned());
                }
                Response::Error { message } => panic!("{scenario}: araç onayı yok: {message}"),
                _ => {}
            }
        };
        assert_eq!(db.quota_wait(&wait.id).unwrap().unwrap().state, QuotaWaitState::Cancelled, "{scenario}");
        assert_eq!(db.telegram_inbox_entry(100, 1).unwrap().unwrap().state, InboxState::NeedsAttention, "{scenario}");
        let origin = db.telegram_turn_origin(&wait.logical_turn_id).unwrap().unwrap();
        assert_eq!(origin.delivery_state, DeliveryState::NeedsAttention, "{scenario}");
        let local_seq = db.events_for_chat(scenario, Some(wait.user_event_seq)).unwrap().into_iter()
            .find(|event| event.kind == "user_text").unwrap().seq;
        assert!(origin.end_seq.is_some_and(|seq| seq >= wait.checkpoint_seq && seq < local_seq), "{scenario}");
        assert!(local_seq < policy_approval.0, "{scenario}");

        let polls = bot.state.polls.load(Ordering::SeqCst);
        wait_for(|| bot.state.polls.load(Ordering::SeqCst) >= polls + 3).await;
        assert!(!bot.state.sent.lock().unwrap().iter().any(|message| message["reply_markup"].is_object()), "{scenario}");
        // Olumlu kontrol: aynı onay açık komutla istenirse köprü gerçekten iletir.
        bot.push(message(2, 101, "/onay"));
        wait_for(|| bot.state.sent.lock().unwrap().iter().any(|message| message["reply_markup"].is_object())).await;
        data(&h, true, Request::ResolveApproval { id: policy_approval.1, approve: false, by: "desktop".into() }).await;
        expect_outcome(&mut local, "completed").await;
        h.shutdown().await;
        bot.shutdown().await;
    }
}

#[tokio::test]
async fn daemon_clearing_allowlist_joins_gateway_and_drops_queued_replies() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let clock = Arc::new(ManualClock::new());
    let store = Arc::new(MemoryCredentials { value: Mutex::new(Some(TOKEN.into())), ..Default::default() });
    let h = start(&paths, config(vec![101]), HashMap::new(), store.clone(), clock.clone(), &bot).await;
    let db = Db::open(&paths.db).unwrap();
    bot.state.hold_send_response.store(true, Ordering::SeqCst);
    bot.push(message(1, 101, "/durum"));
    wait_for(|| bot.state.sent.lock().unwrap().len() == 1).await;
    bot.push(message(2, 101, "/durum"));
    wait_for(|| db.telegram_next_update_id(100).unwrap() == Some(3)).await;
    assert_eq!(bot.state.clients.load(Ordering::SeqCst), 1);

    std::fs::write(paths.root.join("config.toml"), "[telegram]\nallowed_user_ids = []\n").unwrap();
    data(&h, true, Request::SetTelegramSettings { notify: ultron_core::config::NotifyMode::Away,
        auto_continue_enabled: true, auto_continue_wait_minutes: 45, fallback_order: vec![] }).await;
    assert_eq!(bot.state.clients.load(Ordering::SeqCst), 0, "IPC yanıtından önce gateway join edilmeli");
    assert_eq!(store.reads.load(Ordering::SeqCst), 1);
    assert_eq!(data(&h, false, Request::TelegramStatus).await["state"], "missing_allowed_users");
    assert!(matches!(call(&h, true, Request::TelegramTest).await, Response::Error { .. }));

    let polls = bot.state.polls.load(Ordering::SeqCst);
    bot.push(message(3, 101, "/durum"));
    clock.advance(60);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(bot.state.polls.load(Ordering::SeqCst), polls);
    assert_eq!(bot.state.sent.lock().unwrap().len(), 1);
    assert_eq!(db.telegram_next_update_id(100).unwrap(), Some(3));
    h.shutdown().await;
    bot.shutdown().await;
}

#[tokio::test]
async fn daemon_failed_cancel_and_switch_do_not_retain_chat_generations() {
    let bot = MockBot::new().await;
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let h = start(&paths, config(vec![]), HashMap::new(), Arc::new(MemoryCredentials::default()),
        Arc::new(ManualClock::new()), &bot).await;
    let db = Db::open(&paths.db).unwrap();
    db.insert_chat("idle", None, "", Utc::now()).unwrap();
    let ids = ["idle".into(), "".into(), "bad/id".into()]
        .into_iter().chain((0..16).flat_map(|index| [format!("missing-{index}"), format!("missing-{index}-{}", "x".repeat(256))]));
    for chat_id in ids {
        assert!(matches!(call(&h, false, Request::Cancel { chat_id: chat_id.clone() }).await, Response::Error { .. }));
        assert!(matches!(call(&h, false, Request::Switch { chat_id, to: EngineId::Codex }).await, Response::Error { .. }));
    }
    assert_eq!(h.tracked_turns().await, 0);
    h.shutdown().await;
    bot.shutdown().await;
}
