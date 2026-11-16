//! Masaüstü kabulünün gerçek IPC/SQLite ayağı; canlı motor veya Playwright kullanmaz.
//! Üretim CLI'sine test bayrağı eklemek yerine bu test ikilisi ayrı süreçte gerçek
//! `ultrond::server` çalıştırır; motorlar mevcut `common::fake_engines` fikstürüdür.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use ultron_core::ipc::{self, ChatDetail, ChatInfo, Client, PolicyArg, Request, Response};
use ultron_core::{Config, EngineId, Paths, StoredEvent};
use ultrond::server::{self, ServerConfig};

const CHILD_MARKER: &str = "ULTRON_DESKTOP_ACCEPTANCE_CHILD";
const PROMPT: &str = "Örnek masaüstü kabul mesajı";
const WAIT: Duration = Duration::from_secs(20);

/// Süreç, doğrulama hatasında/panic'te veya async zaman aşımında da sonlandırılıp beklenir.
struct DaemonProcess(Child);

impl DaemonProcess {
    fn spawn(paths: &Paths, pipe: &str, quota_home: &Path) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("test ikilisi"));
        command
            .args([
                "--exact",
                "desktop_acceptance_daemon_process",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env("ULTRON_HOME", &paths.root)
            .env("ULTRON_PIPE_NAME", pipe)
            .env("ULTRON_QUOTA_HOME", quota_home)
            .current_dir(&paths.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        Self(command.spawn().expect("sahte motorlu daemon süreci"))
    }

    async fn connect(&mut self, paths: &Paths, pipe: &str) -> Client {
        tokio::time::timeout(WAIT, async {
            loop {
                assert!(
                    self.0.try_wait().expect("daemon durumu").is_none(),
                    "daemon hazır olmadan kapandı"
                );
                // Her denemede yeniden oku: restart yeni bir token üretir.
                if let Ok(token) = ipc::read_token(paths)
                    && let Ok(client) = tokio::time::timeout(
                        Duration::from_millis(500),
                        Client::connect(pipe, &token),
                    )
                    .await
                    && let Ok(client) = client
                {
                    return client;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("daemon IPC bağlantısı zaman aşımı")
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Yalnız üstteki RAII sürücüsü tarafından çalıştırılan ayrı daemon sürecinin girişi.
/// Normal test koşusunda atlanır; `--ignored` tek başına gerçek servis başlatmaz.
#[tokio::test]
#[ignore = "desktop_acceptance sürücüsünün özel alt süreç girişi"]
async fn desktop_acceptance_daemon_process() {
    if std::env::var(CHILD_MARKER).as_deref() != Ok("1") {
        return;
    }
    // Gerçek kullanıcı evine/pipe'ına düşmeyi yasakla.
    assert!(std::env::var_os("ULTRON_HOME").is_some());
    assert!(std::env::var_os("ULTRON_PIPE_NAME").is_some());
    assert!(std::env::var_os("ULTRON_QUOTA_HOME").is_some());
    let paths = Paths::resolve().expect("geçici ULTRON_HOME");
    paths.ensure().unwrap();
    let mut cfg = Config {
        claude_live_quota: false,
        ..Config::default()
    };
    // Gerçek agy özetleyicisi de çalışmamalı; yalnız sahte Claude/Codex kayıtlı.
    cfg.engines.retain(|engine| engine.id != EngineId::Agy);
    let engines = common::fake_engines(&paths.root, &[common::STARTED, common::REPLY, common::DONE]);
    let _server = server::start(ServerConfig {
        paths,
        pipe_name: ipc::pipe_name().unwrap(),
        cfg,
        engines,
    })
    .await
    .expect("gerçek daemon sunucusu başlatılamadı");
    std::future::pending::<()>().await;
}

async fn next(client: &mut Client) -> Response {
    tokio::time::timeout(WAIT, client.next())
        .await
        .expect("IPC yanıtı zaman aşımı")
        .expect("IPC okuma hatası")
        .expect("IPC akışı erken kapandı")
}

async fn ok(client: &mut Client, request: Request) -> Value {
    client.send(&request).await.unwrap();
    match next(client).await {
        Response::Ok { data } => data,
        other => panic!("Ok yanıtı bekleniyordu: {other:?}"),
    }
}

async fn list_chats(client: &mut Client) -> Vec<ChatInfo> {
    let data = ok(
        client,
        Request::ListChats {
            project_id: None,
            include_archived: false,
        },
    )
    .await;
    serde_json::from_value(data["chats"].clone()).unwrap()
}

async fn read_events(client: &mut Client, chat_id: &str) -> Vec<StoredEvent> {
    client
        .send(&Request::Events {
            chat_id: Some(chat_id.into()),
            follow: false,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        match next(client).await {
            Response::Event(event) => events.push(event),
            Response::Ok { .. } => return events,
            Response::Error { message } => panic!("olay geçmişi: {message}"),
        }
    }
}

fn assert_unique(events: &[StoredEvent]) {
    assert_eq!(
        events.iter().map(|e| &e.id).collect::<HashSet<_>>().len(),
        events.len(),
        "yinelenen olay kimliği"
    );
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<HashSet<_>>().len(),
        events.len(),
        "yinelenen olay sırası"
    );
    assert!(
        events.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "olay sırası artmalı"
    );
}

#[tokio::test]
async fn desktop_acceptance_chat_and_events_survive_daemon_restart() {
    // Kurulum, yalnız sentetik CLI'yi bulur/derler; canlı motor başlatmaz.
    common::ensure_fake_cli();
    let tmp = tempfile::tempdir().unwrap();
    let paths = common::paths_in(tmp.path());
    let pipe = common::unique_pipe();
    let quota_home = tmp.path().join("empty-quota-home");
    std::fs::create_dir(&quota_home).unwrap();

    let mut daemon = DaemonProcess::spawn(&paths, &pipe, &quota_home);
    let mut client = daemon.connect(&paths, &pipe).await;
    assert!(list_chats(&mut client).await.is_empty());
    let status = ok(&mut client, Request::Status).await;
    assert_eq!(status["pid"].as_u64(), Some(u64::from(daemon.0.id())));

    // Boş cwd + project_id=None gerçek projesiz çalışma alanı sözleşmesini kullanır.
    let mut sender = daemon.connect(&paths, &pipe).await;
    sender
        .send(&Request::Send {
            chat_id: None,
            cwd: String::new(),
            engine: Some(EngineId::Claude),
            text: PROMPT.into(),
            policy: PolicyArg::ReadOnly,
            project_id: None,
        })
        .await
        .unwrap();
    let chat_id = match next(&mut sender).await {
        Response::Ok { data } => data["chat_id"].as_str().expect("chat_id").to_owned(),
        other => panic!("Send kabul edilmedi: {other:?}"),
    };

    // Abonelik Send sonrasında açılsa da replay + follow tamamlanma olayını kaçırmamalı.
    client
        .send(&Request::Events {
            chat_id: Some(chat_id.clone()),
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    let mut followed = Vec::new();
    loop {
        match next(&mut client).await {
            Response::Event(event) => {
                assert_eq!(event.chat_id, chat_id);
                let done = event.kind == "turn_completed";
                followed.push(event);
                if done {
                    break;
                }
            }
            Response::Ok { .. } => {} // Follow replay sonu/abonelik onayı.
            Response::Error { message } => panic!("olay aboneliği: {message}"),
        }
    }
    assert_unique(&followed);
    assert_eq!(followed.iter().filter(|e| e.kind == "user_text").count(), 1);
    assert!(
        followed
            .iter()
            .any(|e| e.kind == "assistant_text" && e.payload["text"] == "merhaba dünya")
    );
    // Motorun ve tur kilidinin tamamen kapanmasını bekle; yalnız UI olayı yeterli değil.
    loop {
        match next(&mut sender).await {
            Response::Ok { data } => {
                assert_eq!(data["done"], true);
                assert_eq!(data["outcome"], "completed");
                break;
            }
            Response::Event(_) => {}
            Response::Error { message } => panic!("tur başarısız: {message}"),
        }
    }
    drop(sender);
    drop(client); // follow bağlantısını bırak; sonraki istekler ayrı bağlantıda.

    let mut client = daemon.connect(&paths, &pipe).await;
    let data = ok(
        &mut client,
        Request::GetChat {
            chat_id: chat_id.clone(),
        },
    )
    .await;
    let detail: ChatDetail = serde_json::from_value(data["chat"].clone()).unwrap();
    assert_eq!(detail.chat.title, PROMPT);
    assert_eq!(detail.chat.project_id, None);
    assert_eq!(detail.chat.engine, Some(EngineId::Claude));
    assert_eq!(detail.last_engine, Some(EngineId::Claude));
    assert!(paths.root.join("workspaces").join(&chat_id).is_dir());
    let before_chats = list_chats(&mut client).await;
    assert_eq!(before_chats.len(), 1);
    assert_eq!(before_chats[0].id, chat_id);
    let before = read_events(&mut client, &chat_id).await;
    assert_eq!(before, followed);
    assert_unique(&before);
    assert_eq!(
        before.iter().filter(|e| e.kind == "turn_completed").count(),
        1
    );
    drop(client);
    drop(daemon); // Süreci gerçekten öldür ve wait ile kapandığını doğrula.

    let mut daemon = DaemonProcess::spawn(&paths, &pipe, &quota_home);
    let mut client = daemon.connect(&paths, &pipe).await;
    let status = ok(&mut client, Request::Status).await;
    assert_eq!(status["pid"].as_u64(), Some(u64::from(daemon.0.id())));
    assert_eq!(list_chats(&mut client).await, before_chats);
    let after = read_events(&mut client, &chat_id).await;
    assert_eq!(
        after, before,
        "restart olay eklememeli, silmemeli veya değiştirmemeli"
    );
    assert_unique(&after);
    drop(client);
    drop(daemon); // tmp temizlenmeden önce; panic durumunda da Drop çalışır.
}
