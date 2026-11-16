//! Köprü testleri: gerçek `ultrond` süreci ve süreç içi sunucu (FakeEngine), geçici `ULTRON_HOME`
//! ve `ULTRON_PIPE_NAME` ile. Zamanlanmış görev kaydedilmez.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use ultron_core::engine::{Engine, FakeEngine};
use ultron_core::ipc::{PolicyArg, Request};
use ultron_core::{Config, EngineId, Paths};
use ultron_desktop::bridge::{Bridge, BridgeError};
use ultron_desktop::events::{Backoff, run_follow};
use ultrond::server::{self, ServerConfig};

const STARTED: &str = r#"{"type":"session_started","native_session_id":"nat-1"}"#;
const REPLY: &str = r#"{"type":"assistant_text","text":"merhaba"}"#;
const DONE: &str = r#"{"type":"turn_completed"}"#;

fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: testler `--test-threads=1` ile çalışır; ortam yalnız test başında yazılır.
    unsafe { std::env::set_var(key, value) };
}

/// Hedef dizinde (testin yanından yukarı) bir ikili arar; yoksa `cargo build` ile üretir.
fn ensure_bin(package: &str, bin: &str) -> PathBuf {
    let name = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    let find = || {
        let exe = std::env::current_exe().unwrap();
        exe.ancestors()
            .skip(1)
            .take(6)
            .map(|d| d.join(&name))
            .find(|p| p.is_file())
    };
    find().unwrap_or_else(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", package, "--bin", bin])
            .status()
            .unwrap();
        assert!(status.success(), "{bin} derlenemedi");
        find().expect("ikili bulunamadı")
    })
}

fn fake_engines(dir: &Path) -> HashMap<EngineId, Arc<dyn Engine>> {
    set_env("ULTRON_FAKE_CLI", ensure_bin("ultron-core", "ultron-fake-cli"));
    let fixture = dir.join("fixture.jsonl");
    std::fs::write(&fixture, [STARTED, REPLY, DONE].join("\n") + "\n").unwrap();
    let mut map: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    for id in [EngineId::Claude, EngineId::Codex] {
        map.insert(id, Arc::new(FakeEngine::from_fixture(&fixture)));
    }
    map
}

async fn start_server(paths: &Paths, pipe: &str) -> server::ServerHandle {
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: pipe.to_string(),
        cfg: Config::default(),
        engines: fake_engines(&paths.root),
    })
    .await
    .unwrap()
}

fn send(cwd: &Path, chat: &str) -> Request {
    Request::Send {
        chat_id: Some(chat.to_string()),
        cwd: cwd.to_string_lossy().into_owned(),
        engine: Some(EngineId::Claude),
        text: "selam".into(),
        policy: PolicyArg::ReadOnly,
        project_id: None,
    }
}

/// Bir turu başlatıp bitene dek bekler (olaylar günlüğe yazılır).
async fn run_turn(bridge: &Bridge, cwd: &Path, chat: &str) -> i64 {
    let mut client = bridge.connect().await.unwrap();
    client.send(&send(cwd, chat)).await.unwrap();
    let mut head = 0;
    loop {
        let r = tokio::time::timeout(Duration::from_secs(20), client.next())
            .await
            .expect("tur zaman aşımı")
            .unwrap();
        match r {
            Some(ultron_core::ipc::Response::Ok { data }) if data.get("done").is_some() => {
                return head;
            }
            Some(ultron_core::ipc::Response::Event(ev)) => head = ev.seq,
            Some(ultron_core::ipc::Response::Error { message }) => panic!("{message}"),
            None => panic!("akış erken kapandı"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn bridge_starts_daemon_or_reports_error() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    set_env("ULTRON_HOME", &paths.root);
    set_env(
        "ULTRON_PIPE_NAME",
        format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4()),
    );

    // Servis yok ve başlatılamıyor: tipli DaemonUnavailable.
    let missing = Bridge::new(paths.clone(), Some(tmp.path().join("yok").join("ultrond.exe")));
    match missing.request(Request::Status).await {
        Err(BridgeError::DaemonUnavailable { reason }) => assert!(!reason.is_empty()),
        other => panic!("DaemonUnavailable beklenirdi: {other:?}"),
    }

    // Servis yok ama başlatılabiliyor: köprü ultrond'u açar ve Status yanıtlar.
    let exe = ensure_bin("ultrond", "ultrond");
    let bridge = Bridge::new(paths, Some(exe));
    let status = bridge.request(Request::Status).await.expect("servis başlamalı");
    let pid = status["pid"].as_u64().expect("pid");
    // Başlatılan servis diğer komutlara da gerçekten yanıt verir.
    bridge.request(Request::GetQuota).await.expect("GetQuota yanıtlanmalı");
    let chats = bridge
        .request(Request::ListChats {
            project_id: None,
            include_archived: false,
        })
        .await
        .expect("ListChats yanıtlanmalı");
    assert!(chats["chats"].is_array());
    // Var olmayan sohbet: tipli servis hatası (uydurma veri yok).
    match bridge
        .request(Request::GetChat {
            chat_id: "yok".into(),
        })
        .await
    {
        Err(BridgeError::Daemon { message }) => assert!(message.contains("bulunamadı")),
        other => panic!("Daemon hatası beklenirdi: {other:?}"),
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

#[tokio::test]
async fn event_bridge_resumes_after_reconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();

    let bridge = Arc::new(Bridge::new(paths.clone(), None));
    let server = start_server(&paths, &pipe).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let follower = {
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let backoff = Backoff {
                initial: Duration::from_millis(100),
                max: Duration::from_millis(400),
            };
            run_follow(&bridge, backoff, cancel, move |ev| {
                let _ = tx.send((ev.chat_id.clone(), ev.seq));
            })
            .await;
        })
    };

    run_turn(&bridge, &cwd, "c1").await;
    let mut seen: Vec<i64> = Vec::new();
    let first = collect(&mut rx, &mut seen, |s| s.len() >= 3).await;
    assert!(first, "ilk turun olayları gelmedi: {seen:?}");

    // Servisi kapat, aynı veri dizini ve pipe ile yeniden aç, ikinci turu çalıştır.
    server.shutdown().await;
    let server = start_server(&paths, &pipe).await;
    run_turn(&bridge, &cwd, "c1").await;
    let total = collect(&mut rx, &mut seen, |s| s.len() >= 6).await;
    assert!(total, "yeniden bağlanınca olaylar tamamlanmadı: {seen:?}");
    // Kaçırma ve tekrar yok: seq 1..=n ardışık.
    let expected: Vec<i64> = (1..=seen.len() as i64).collect();
    assert_eq!(seen, expected);

    cancel.cancel();
    let _ = follower.await;
    server.shutdown().await;
}

#[tokio::test]
async fn events_page_respects_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let bridge = Bridge::new(paths.clone(), None);
    let server = start_server(&paths, &pipe).await;
    run_turn(&bridge, &cwd, "c1").await;

    // Servis sınırı kendisi uygular: tam `limit` olay, sonra `done`.
    let mut client = bridge.connect().await.unwrap();
    client
        .send(&Request::Events {
            chat_id: Some("c1".into()),
            follow: false,
            after_seq: None,
            limit: Some(2),
        })
        .await
        .unwrap();
    let mut seqs = Vec::new();
    loop {
        match client.next().await.unwrap() {
            Some(ultron_core::ipc::Response::Event(e)) => seqs.push(e.seq),
            Some(ultron_core::ipc::Response::Ok { data }) => {
                assert_eq!(data["done"], true);
                break;
            }
            other => panic!("beklenmeyen: {other:?}"),
        }
    }
    assert_eq!(seqs, vec![1, 2]);

    let page = ultron_desktop::commands::read_events_page(&bridge, "c1", None, 2)
        .await
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["next_after_seq"], 2);
    let rest = ultron_desktop::commands::read_events_page(&bridge, "c1", Some(2), 50)
        .await
        .unwrap();
    assert_eq!(rest["has_more"], false);
    server.shutdown().await;
}

type Seen = Vec<(String, i64)>;

fn spawn_follower(
    bridge: &Arc<Bridge>,
    backoff: Backoff,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<(String, i64)>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let handle = {
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            run_follow(&bridge, backoff, cancel, move |ev| {
                let _ = tx.send((ev.chat_id.clone(), ev.seq));
            })
            .await;
        })
    };
    (rx, cancel, handle)
}

async fn collect_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, i64)>,
    seen: &mut Seen,
    done: impl Fn(&Seen) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while !done(seen) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(item)) => seen.push(item),
            _ => return false,
        }
    }
    true
}

fn seqs_of(seen: &Seen, chat: &str) -> Vec<i64> {
    seen.iter()
        .filter(|(c, _)| c == chat)
        .map(|(_, s)| *s)
        .collect()
}

#[tokio::test]
async fn reconnect_discovers_chat_created_while_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let bridge = Arc::new(Bridge::new(paths.clone(), None));
    let server = start_server(&paths, &pipe).await;
    // Uzun geri çekilme: servis dönene dek takipçi çevrimdışı kalır.
    let slow = Backoff {
        initial: Duration::from_secs(4),
        max: Duration::from_secs(4),
    };
    let (mut rx, cancel, follower) = spawn_follower(&bridge, slow);
    run_turn(&bridge, &cwd, "c1").await;
    let mut seen = Seen::new();
    assert!(collect_until(&mut rx, &mut seen, |s| s.len() >= 3).await);

    server.shutdown().await;
    let server = start_server(&paths, &pipe).await;
    // Takipçi uykudayken yeni sohbet oluşur ve tamamlanır.
    run_turn(&bridge, &cwd, "c2").await;
    let ok = collect_until(&mut rx, &mut seen, |s| seqs_of(s, "c2").len() >= 3).await;
    assert!(ok, "çevrimdışıyken oluşan sohbet bulunamadı: {seen:?}");
    // Baştan, ardışık, tekrarsız (c2 çevrimdışıyken tamamlandı).
    let c2 = seqs_of(&seen, "c2");
    assert_eq!(c2, (1..=c2.len() as i64).collect::<Vec<_>>());
    let c1 = seqs_of(&seen, "c1");
    assert!(c1.len() >= 3);
    assert_eq!(c1, (1..=c1.len() as i64).collect::<Vec<_>>());

    cancel.cancel();
    let _ = follower.await;
    server.shutdown().await;
}

#[tokio::test]
async fn first_event_gap_fetches_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let bridge = Arc::new(Bridge::new(paths.clone(), None));
    let server = start_server(&paths, &pipe).await;
    // Takipçi başlamadan c1'in ilk turu (seq 1..=3) yazılır.
    run_turn(&bridge, &cwd, "c1").await;
    let (mut rx, cancel, follower) = spawn_follower(&bridge, Backoff::default());
    // Taban çizgisi alınana dek bekle, sonra ikinci tur: takipçinin gördüğü ilk canlı olay seq 4.
    tokio::time::sleep(Duration::from_secs(2)).await;
    run_turn(&bridge, &cwd, "c1").await;
    let mut seen = Seen::new();
    let ok = collect_until(&mut rx, &mut seen, |s| s.len() >= 6).await;
    assert!(ok, "olaylar tamamlanmadı: {seen:?}");
    assert_eq!(seqs_of(&seen, "c1"), vec![1, 2, 3, 4, 5, 6]);

    cancel.cancel();
    let _ = follower.await;
    server.shutdown().await;
}

async fn next_status(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ultron_desktop::events::DaemonStatus>,
) -> ultron_desktop::events::DaemonStatus {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("durum zaman aşımı")
        .expect("kanal")
}

#[tokio::test]
async fn daemon_state_follows_connection() {
    use ultron_desktop::events::{DaemonStatus, run_follow_with_state};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let bridge = Arc::new(Bridge::new(paths.clone(), None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let follower = {
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let backoff = Backoff {
                initial: Duration::from_millis(100),
                max: Duration::from_millis(300),
            };
            run_follow_with_state(&bridge, backoff, cancel, |_| {}, move |s| {
                let _ = tx.send(s);
            })
            .await;
        })
    };
    // Servis yok: unavailable (tekrarlar tek bildirimde birleşir).
    assert!(matches!(next_status(&mut rx).await, DaemonStatus::Unavailable(_)));
    // Servis sonradan gelir: ready.
    let server = start_server(&paths, &pipe).await;
    assert_eq!(next_status(&mut rx).await, DaemonStatus::Ready);
    // Servis düşer: unavailable.
    server.shutdown().await;
    assert!(matches!(next_status(&mut rx).await, DaemonStatus::Unavailable(_)));

    cancel.cancel();
    let _ = follower.await;
}

#[tokio::test]
async fn bridge_waits_for_subscription_ack_before_ready() {
    use ultron_desktop::events::{DaemonStatus, run_follow_with_state};
    use ultrond::server::{SUBSCRIBE_GATE, SubscribeGate};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let make_gate = || {
        Arc::new(SubscribeGate {
            root: paths.root.clone(),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            subscribed: tokio::sync::Notify::new(),
            acknowledge: tokio::sync::Semaphore::new(0),
        })
    };
    let gate = make_gate();
    *SUBSCRIBE_GATE.lock().unwrap() = Some(gate.clone());
    struct ResetGate;
    impl Drop for ResetGate {
        fn drop(&mut self) {
            *SUBSCRIBE_GATE.lock().unwrap() = None;
        }
    }
    let _reset = ResetGate;
    let server = start_server(&paths, &pipe).await;
    let bridge = Arc::new(Bridge::new(paths.clone(), None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let follower = {
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            run_follow_with_state(
                &bridge,
                Backoff::default(),
                cancel,
                move |ev| {
                    let _ = event_tx.send((ev.chat_id, ev.seq));
                },
                move |s| {
                    let _ = tx.send(s);
                },
            )
            .await;
        })
    };
    // İlk bağlantı da onay gelmeden Ready veya olay iletemez.
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("ilk bağlantı kapıya ulaşmadı");
    assert!(rx.try_recv().is_err(), "abonelikten önce durum bildirildi");
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(20), gate.subscribed.notified())
        .await
        .expect("ilk abonelik kurulmadı");
    let first_head = run_turn(&bridge, &cwd, "c1").await;
    assert_eq!(first_head, 4, "kullanıcı mesajı ve üç fixture olayı");
    assert!(rx.try_recv().is_err(), "ilk onaydan önce Ready bildirildi");
    assert!(
        event_rx.try_recv().is_err(),
        "ilk onaydan önce olay iletildi"
    );
    gate.acknowledge.add_permits(1);
    assert_eq!(next_status(&mut rx).await, DaemonStatus::Ready);
    let mut seen = Seen::new();
    // Turun TÜM olaylarını tüket: kanalda kalan turn_completed erken uzlaşma değildir.
    assert!(collect_until(&mut event_rx, &mut seen, |s| s.len() >= first_head as usize).await);
    assert_eq!(seqs_of(&seen, "c1"), (1..=first_head).collect::<Vec<_>>());
    server.shutdown().await;
    assert!(matches!(
        next_status(&mut rx).await,
        DaemonStatus::Unavailable(_)
    ));

    // Yeniden bağlantıda hem subscribe() hem de onay ayrı bariyerlerde bekletilir.
    let server = start_server(&paths, &pipe).await;
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("sunucu kapıya ulaşmadı");
    // Önceki imleci olan ve yeni oluşturulan iki sohbet de abonelikten önce tamamlanır.
    run_turn(&bridge, &cwd, "c1").await;
    let c2_head = run_turn(&bridge, &cwd, "c2").await;
    while let Ok(status) = rx.try_recv() {
        assert_ne!(status, DaemonStatus::Ready, "onaydan önce Ready bildirildi");
    }
    assert!(event_rx.try_recv().is_err(), "onaydan önce uzlaşma yapıldı");
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(20), gate.subscribed.notified())
        .await
        .expect("abonelik kurulmadı");
    // Bu tur onaydaki sabit baştan sonradır; yalnız canlı kuyruğa girer.
    run_turn(&bridge, &cwd, "c1").await;
    assert!(rx.try_recv().is_err(), "abonelik tek başına Ready olmamalı");
    assert!(event_rx.try_recv().is_err(), "onaydan önce olay iletildi");
    // Onay beklerken hata: bir sonraki deneme eski aboneliğin onayını kullanamaz.
    let retry_gate = make_gate();
    *SUBSCRIBE_GATE.lock().unwrap() = Some(retry_gate.clone());
    gate.acknowledge.close();
    assert!(matches!(
        next_status(&mut rx).await,
        DaemonStatus::Unavailable(_)
    ));
    tokio::time::timeout(Duration::from_secs(20), retry_gate.reached.notified())
        .await
        .expect("hata sonrası deneme kapıya ulaşmadı");
    assert!(
        rx.try_recv().is_err(),
        "hata sonrası onaydan önce Ready bildirildi"
    );
    assert!(
        event_rx.try_recv().is_err(),
        "hata sonrası onaydan önce uzlaşma yapıldı"
    );
    retry_gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(20), retry_gate.subscribed.notified())
        .await
        .expect("hata sonrası abonelik kurulmadı");
    let c1_head = run_turn(&bridge, &cwd, "c1").await;
    assert!(rx.try_recv().is_err(), "yeni bağlantının onayı beklenmedi");
    assert!(
        event_rx.try_recv().is_err(),
        "yeni onaydan önce olay iletildi"
    );
    retry_gate.acknowledge.add_permits(1);
    assert_eq!(next_status(&mut rx).await, DaemonStatus::Ready);
    // Ready geldiğinde onaydaki başa kadar uzlaşma tamamlanmış olmalı.
    while let Ok(event) = event_rx.try_recv() {
        seen.push(event);
    }
    assert_eq!(seqs_of(&seen, "c1"), (1..=c1_head).collect::<Vec<_>>());
    assert_eq!(seqs_of(&seen, "c2"), (1..=c2_head).collect::<Vec<_>>());

    // Sonraki canlı tur, kuyruktaki kopyalar tüketildikten sonra da sıralı ve tekrarsızdır.
    let live_head = run_turn(&bridge, &cwd, "c1").await;
    assert!(
        collect_until(&mut event_rx, &mut seen, |s| s.len()
            >= (live_head + c2_head) as usize)
        .await
    );
    assert_eq!(seqs_of(&seen, "c1"), (1..=live_head).collect::<Vec<_>>());
    assert_eq!(seqs_of(&seen, "c2"), (1..=c2_head).collect::<Vec<_>>());

    cancel.cancel();
    let _ = follower.await;
    server.shutdown().await;
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().expect("nesne").keys().cloned().collect();
    k.sort();
    k
}

/// Sözleşme: webview'in aldığı JSON biçimi `src/lib/bridge.ts` tipleriyle birebir (alan kümeleri
/// ve çekirdek tiplerle tipli ayrıştırma). TS tarafı aynı alan listelerini `bridge.test.ts` sınar.
#[tokio::test]
async fn command_json_shapes_match_ts_types() {
    use ultron_core::ipc::{CatalogImportReport, ChatDetail, ChatInfo, ImportReport, ProjectInfo};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    set_env("ULTRON_HOME", &paths.root);
    set_env("ULTRON_PIPE_NAME", &pipe);
    let cwd = tmp.path().join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let bridge = Bridge::new(paths.clone(), None);
    let server = start_server(&paths, &pipe).await;
    run_turn(&bridge, &cwd, "c1").await;

    // getChat: zarf açılır, doğrudan ChatDetail.
    let detail = ultron_desktop::commands::read_chat_detail(&bridge, "c1")
        .await
        .unwrap();
    assert_eq!(keys(&detail), ["chat", "last_engine", "work_state"]);
    assert_eq!(
        keys(&detail["chat"]),
        ["archived", "engine", "id", "pinned", "project_id", "title", "updated_at"]
    );
    serde_json::from_value::<ChatDetail>(detail).unwrap();

    // listChats / updateChat: {chats: ChatInfo[]} ve {chat: ChatInfo}.
    let list = bridge
        .request(Request::ListChats {
            project_id: None,
            include_archived: false,
        })
        .await
        .unwrap();
    assert_eq!(keys(&list), ["chats"]);
    serde_json::from_value::<Vec<ChatInfo>>(list["chats"].clone()).unwrap();

    // importCatalog: ImportReport değil, {imported, updated}.
    let md = tmp.path().join("projects.md");
    std::fs::write(
        &md,
        include_str!("../../../../crates/ultron-core/tests/fixtures/catalog/projects.md"),
    )
    .unwrap();
    let cat = bridge
        .request(Request::ImportCatalog {
            path: md.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
    assert_eq!(keys(&cat), ["imported", "updated"]);
    serde_json::from_value::<CatalogImportReport>(cat).unwrap();
    let projects = bridge.request(Request::ListProjects).await.unwrap();
    assert_eq!(keys(&projects), ["projects"]);
    let parsed: Vec<ProjectInfo> = serde_json::from_value(projects["projects"].clone()).unwrap();
    assert!(!parsed.is_empty());

    // importClaudeHistory: zarf açılır, doğrudan ImportReport.
    let root = tmp.path().join("claude-projects");
    std::fs::create_dir_all(&root).unwrap();
    let report = ultron_desktop::commands::read_history_import(
        &bridge,
        Some(root.to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    assert_eq!(keys(&report), ["chats", "files", "masked", "messages", "skipped"]);
    serde_json::from_value::<ImportReport>(report).unwrap();

    // status.
    let status = bridge.request(Request::Status).await.unwrap();
    assert_eq!(
        keys(&status),
        ["default_engine", "pid", "running_turns", "version"]
    );
    server.shutdown().await;
}

async fn collect(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, i64)>,
    seen: &mut Vec<i64>,
    done: impl Fn(&Vec<i64>) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !done(seen) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((chat, seq))) => {
                assert_eq!(chat, "c1");
                seen.push(seq);
            }
            _ => return false,
        }
    }
    true
}
