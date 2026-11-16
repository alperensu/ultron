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
async fn run_turn(bridge: &Bridge, cwd: &Path, chat: &str) {
    let mut client = bridge.connect().await.unwrap();
    client.send(&send(cwd, chat)).await.unwrap();
    loop {
        let r = tokio::time::timeout(Duration::from_secs(20), client.next())
            .await
            .expect("tur zaman aşımı")
            .unwrap();
        match r {
            Some(ultron_core::ipc::Response::Ok { data }) if data.get("done").is_some() => return,
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
    // Henüz uygulanmamış istek uydurma veri yerine tipli servis hatası verir.
    match bridge.request(Request::GetQuota).await {
        Err(BridgeError::Daemon { message }) => assert!(message.contains("uygulanmadı")),
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
