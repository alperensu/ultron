mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use ultron_core::engine::{Engine, FakeEngine};
use ultron_core::ipc::{Request, Response};
use ultron_core::{Config, EngineId};
use ultrond::server::{self, ServerConfig};

/// Yalnız Claude kayıtlı, `sleep` kipinde (tur bitmez) bir sunucu başlatır.
async fn start_sleeping(paths: &ultron_core::Paths, pidfile: &std::path::Path) -> server::ServerHandle {
    ensure_fake_cli();
    let mut engines: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    engines.insert(
        EngineId::Claude,
        Arc::new(FakeEngine::with_mode(
            &["sleep", &pidfile.to_string_lossy()],
            false,
        )),
    );
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: unique_pipe(),
        cfg: Config::default(),
        engines,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn send_rejects_when_turn_running_before_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_sleeping(&paths, &tmp.path().join("pids")).await;

    let mut first = connect(&h).await;
    first
        .send(&send_req(&cwd, Some("mesgul"), "bir"))
        .await
        .unwrap();
    let Some(Response::Ok { data }) = next(&mut first).await else {
        panic!("ilk gönderim kabul edilmedi");
    };
    assert_eq!(data["chat_id"], "mesgul");

    // İkinci gönderimin ilk yanıtı Error olmalı; kabul (Ok chat_id) görülmemeli.
    let mut second = connect(&h).await;
    second
        .send(&send_req(&cwd, Some("mesgul"), "iki"))
        .await
        .unwrap();
    match next(&mut second).await.expect("yanıt yok") {
        Response::Error { message } => assert!(message.contains("çalışan bir tur"), "{message}"),
        other => panic!("kabul edilmemeliydi: {other:?}"),
    }

    let mut c = connect(&h).await;
    c.send(&Request::Cancel { chat_id: "mesgul".into() })
        .await
        .unwrap();
    drop(first);
    h.shutdown().await;
}

#[tokio::test]
async fn unregistered_engine_is_rejected_before_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_sleeping(&paths, &tmp.path().join("pids")).await;
    let mut c = connect(&h).await;
    let mut req = send_req(&cwd, Some("yok-motor"), "x");
    if let Request::Send { engine, .. } = &mut req {
        *engine = Some(EngineId::Codex);
    }
    c.send(&req).await.unwrap();
    assert!(matches!(
        next(&mut c).await,
        Some(Response::Error { .. })
    ));
    h.shutdown().await;
}

#[tokio::test]
async fn post_accept_failure_is_visible_as_event() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;

    // İlk tur normal biter.
    let mut c = connect(&h).await;
    c.send(&send_req(&cwd, Some("hata-1"), "bir")).await.unwrap();
    while let Some(r) = next(&mut c).await {
        if matches!(&r, Response::Ok { data } if data.get("done").is_some()) {
            break;
        }
    }
    // Çalışma dizini kaybolunca ikinci tur kabul edilir ama başarısız olur.
    std::fs::remove_dir_all(&cwd).unwrap();
    let mut s = connect(&h).await;
    s.send(&send_req(&cwd, Some("hata-1"), "iki")).await.unwrap();
    while let Some(r) = next(&mut s).await {
        if matches!(r, Response::Ok { ref data } if data.get("done").is_some())
            || matches!(r, Response::Error { .. })
        {
            break;
        }
    }

    // Başka bir abone, günlükten hata olayını görmeli.
    let mut watcher = connect(&h).await;
    watcher
        .send(&Request::Events {
            chat_id: Some("hata-1".into()),
            follow: false,
            after_seq: None,
        })
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while let Some(r) = next(&mut watcher).await {
        match r {
            Response::Event(ev) => kinds.push(ev.kind),
            _ => break,
        }
    }
    assert!(kinds.iter().any(|k| k == "error"), "{kinds:?}");
    h.shutdown().await;
}
