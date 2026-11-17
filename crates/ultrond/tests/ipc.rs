mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use ultron_core::engine::{Engine, FakeEngine};
use ultron_core::ipc::{Client, Request, Response, sys};
use ultron_core::{Config, EngineId, Paths};
use ultrond::keepawake::KeepAwake;
use ultrond::server::{self, ServerConfig, ServerHandle};

const LINES: [&str; 3] = [STARTED, REPLY, DONE];

#[tokio::test]
async fn rejects_wrong_token() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let mut c = Client::open(h.pipe_name()).await.unwrap();
    c.send(&Request::Hello {
        token: "yanlış".into(),
    })
    .await
    .unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Error { .. })));
    assert!(next(&mut c).await.is_none(), "bağlantı kapanmalı");

    // Hello olmadan başka bir istek de reddedilir ve bağlantı kapanır.
    let mut c = Client::open(h.pipe_name()).await.unwrap();
    c.send(&Request::Status).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Error { .. })));
    assert!(next(&mut c).await.is_none());

    // Doğru token ile Status çalışır.
    let mut c = connect(&h).await;
    c.send(&Request::Status).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Ok { .. })));
    h.shutdown().await;
}

#[tokio::test]
async fn oversized_line_is_rejected_and_closes() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let big = vec![b'a'; 1024 * 1024 + 10];

    // Kimlik doğrulamadan önce.
    let mut c = Client::open(h.pipe_name()).await.unwrap();
    c.send_raw(&big).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Error { .. })));
    assert!(next(&mut c).await.is_none());

    // Kimlik doğrulamadan sonra.
    let mut c = connect(&h).await;
    c.send_raw(&big).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Error { .. })));
    assert!(next(&mut c).await.is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn send_returns_chat_id_and_streams_events() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let mut c = connect(&h).await;
    c.send(&send_req(&cwd, None, "selam")).await.unwrap();

    let Some(Response::Ok { data }) = next(&mut c).await else {
        panic!("ilk yanıt Ok{{chat_id}} olmalı");
    };
    let chat_id = data["chat_id"].as_str().expect("chat_id").to_string();
    let mut kinds = Vec::new();
    let outcome = loop {
        match next(&mut c).await.expect("akış erken kapandı") {
            Response::Event(ev) => {
                assert_eq!(ev.chat_id, chat_id);
                kinds.push(ev.kind);
            }
            Response::Ok { data } => break data,
            Response::Error { message } => panic!("{message}"),
        }
    };
    assert_eq!(outcome["outcome"], "completed");
    assert_eq!(kinds.first().map(String::as_str), Some("user_text"));
    assert!(kinds.contains(&"assistant_text".to_string()), "{kinds:?}");
    assert_eq!(kinds.last().map(String::as_str), Some("turn_completed"));
    h.shutdown().await;
}

#[tokio::test]
async fn events_follow_receives_live_events() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let mut watcher = connect(&h).await;
    watcher
        .send(&Request::Events {
            chat_id: Some("canli".into()),
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    expect_ack(&mut watcher).await;

    let mut sender = connect(&h).await;
    sender
        .send(&send_req(&cwd, Some("canli"), "merhaba"))
        .await
        .unwrap();

    let mut kinds = Vec::new();
    while !kinds.contains(&"turn_completed".to_string()) {
        match next(&mut watcher).await.expect("izleyici kapandı") {
            Response::Event(ev) => kinds.push(ev.kind),
            other => panic!("beklenmeyen yanıt: {other:?}"),
        }
    }
    assert!(kinds.contains(&"user_text".to_string()), "{kinds:?}");
    h.shutdown().await;
}

#[tokio::test]
async fn second_instance_refuses_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let first = start_with(&paths, &unique_pipe(), &LINES).await;

    // Aynı kök (kilit), farklı pipe adı: kilit reddeder.
    let err = server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: unique_pipe(),
        cfg: Config::default(),
        engines: fake_engines(&paths.root, &LINES),
    })
    .await
    .err()
    .expect("ikinci örnek başlamamalı");
    assert!(err.to_string().contains("ultrond"), "{err}");

    // Farklı kök, aynı pipe adı: FILE_FLAG_FIRST_PIPE_INSTANCE reddeder.
    let other = paths_in(&tmp.path().join("diger"));
    let err = server::start(ServerConfig {
        paths: other,
        pipe_name: first.pipe_name().to_string(),
        cfg: Config::default(),
        engines: fake_engines(&tmp.path().join("diger"), &LINES),
    })
    .await
    .err()
    .expect("aynı adlı pipe oluşturulmamalı");
    assert!(err.to_string().contains("pipe"), "{err}");

    // İlk örnek kapanınca kilit bırakılır; yeniden başlatılabilir.
    let name = first.pipe_name().to_string();
    first.shutdown().await;
    let again = start_with(&paths, &name, &LINES).await;
    again.shutdown().await;
}

#[tokio::test]
async fn token_is_fresh_per_start_and_file_is_user_only() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let first = start_with(&paths, &unique_pipe(), &LINES).await;
    let t1 = std::fs::read_to_string(paths.run.join("token")).unwrap();
    assert_eq!(t1.len(), 64);
    assert_eq!(t1, first.token());
    first.shutdown().await;
    let second = start_with(&paths, &unique_pipe(), &LINES).await;
    let t2 = std::fs::read_to_string(paths.run.join("token")).unwrap();
    assert_ne!(t1, t2, "token her başlangıçta yenilenmeli");

    // ACL: yalnız mevcut kullanıcı, devralınan giriş yok.
    let out = std::process::Command::new("icacls")
        .arg(paths.run.join("token"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let aces: Vec<&str> = text.lines().filter(|l| l.contains(":(")).collect();
    assert_eq!(aces.len(), 1, "yalnız bir ACE beklenir:\n{text}");
    assert!(!aces[0].contains("(I)"), "devralınan ACE var:\n{text}");
    for banned in ["Everyone", "BUILTIN", "NT AUTHORITY", "NT SERVICE"] {
        assert!(!text.contains(banned), "{banned} erişimi var:\n{text}");
    }
    second.shutdown().await;
}

#[tokio::test]
async fn pipe_dacl_grants_only_current_user() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let client = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(h.pipe_name())
        .unwrap();

    let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: tutamaç geçerli; sd LocalFree ile bırakılır.
    let rc = unsafe {
        GetSecurityInfo(
            client.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    assert_eq!(rc, 0, "GetSecurityInfo başarısız");
    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: sd geçerli bir güvenlik tanımlayıcısı.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            1,
            DACL_SECURITY_INFORMATION,
            &mut wide,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0);
    // SAFETY: NUL ile biten UTF-16.
    let sddl = unsafe {
        let mut n = 0;
        while *wide.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
        LocalFree(wide.cast());
        LocalFree(sd);
        s
    };
    let sid = sys::current_user_sid().unwrap();
    assert!(sddl.starts_with("D:P(A;;"), "{sddl}");
    assert!(sddl.ends_with(&format!(";;;{sid})")), "{sddl}");
    assert_eq!(sddl.matches("(A;").count(), 1, "tek ACE beklenir: {sddl}");
    drop(client);
    h.shutdown().await;
}

#[tokio::test]
async fn keep_awake_counts_holders() {
    let keep = KeepAwake::new();
    assert_eq!(keep.holders(), 0);
    let a = keep.acquire();
    let b = keep.acquire();
    assert_eq!(keep.holders(), 2);
    drop(a);
    assert_eq!(keep.holders(), 1);
    drop(b);
    assert_eq!(keep.holders(), 0);
}

#[allow(dead_code)]
fn _unused(_: &Paths) {}

/// Her motor için ayrı fixture ile sunucu başlatır (Claude ve Codex farklı yanıt verir).
async fn start_with_engines(
    paths: &Paths,
    pipe: &str,
    claude: Vec<String>,
    codex: Vec<String>,
) -> ServerHandle {
    ensure_fake_cli();
    let mut engines: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    for (id, lines, name) in [
        (EngineId::Claude, claude, "claude.jsonl"),
        (EngineId::Codex, codex, "codex.jsonl"),
    ] {
        let fixture = paths.root.join(name);
        std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
        engines.insert(id, Arc::new(FakeEngine::from_fixture(&fixture)));
    }
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: pipe.to_string(),
        cfg: config_without_agy(),
        engines,
    })
    .await
    .unwrap()
}

/// Bir turu sonuna kadar izler; `(olay türleri, son yanıt)` döner.
async fn run_turn(h: &ServerHandle, req: Request) -> Vec<String> {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    let mut kinds = Vec::new();
    loop {
        match next(&mut c).await.expect("akış erken kapandı") {
            Response::Event(ev) => kinds.push(ev.kind),
            // İlk Ok yalnız chat_id bildirir; tur bitişi `done` alanıyla gelir.
            Response::Ok { data } if data.get("done").is_some() => return kinds,
            Response::Ok { .. } => {}
            Response::Error { message } => panic!("{message}"),
        }
    }
}

#[tokio::test]
async fn send_without_engine_keeps_chat_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let codex_reply = r#"{"type":"assistant_text","text":"codex yanıtı"}"#.to_string();
    let h = start_with_engines(
        &paths,
        &unique_pipe(),
        LINES.iter().map(|s| s.to_string()).collect(),
        vec![codex_reply, DONE.to_string()],
    )
    .await;

    // Sohbet ilk mesajda açıkça Codex'e bağlanır.
    let first = Request::Send {
        chat_id: Some("motor-1".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        engine: Some(EngineId::Codex),
        text: "ilk".into(),
        policy: ultron_core::ipc::PolicyArg::ReadOnly,
        project_id: None,
    };
    run_turn(&h, first).await;

    // Motor belirtilmeden gönderilen mesaj sohbetin motorunu (Codex) kullanır.
    let second = Request::Send {
        chat_id: Some("motor-1".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        engine: None,
        text: "ikinci".into(),
        policy: ultron_core::ipc::PolicyArg::ReadOnly,
        project_id: None,
    };
    let kinds = run_turn(&h, second).await;
    assert!(!kinds.contains(&"engine_switched".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"assistant_text".to_string()));

    let mut c = connect(&h).await;
    c.send(&Request::HistorySearch {
        chat_id: Some("motor-1".into()),
        query: "codex".into(),
        limit: 10,
    })
    .await
    .unwrap();
    let Some(Response::Ok { data }) = next(&mut c).await else {
        panic!("arama yanıtı bekleniyordu");
    };
    let hits = data["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "Codex yanıtı aramada bulunmalı");
    h.shutdown().await;
}

#[tokio::test]
async fn global_follow_catches_up_after_lag() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    // Yayın kanalının kapasitesini (1024) aşacak kadar olay; izleyici okumadan bekler.
    let flood: Vec<String> = (0..2500)
        .map(|i| format!(r#"{{"type":"assistant_text","text":"satır {i} {}"}}"#, "x".repeat(200)))
        .collect();
    let h = start_with_engines(&paths, &unique_pipe(), flood, vec![]).await;

    let mut watcher = connect(&h).await;
    watcher
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    expect_ack(&mut watcher).await;

    let req = send_req(&cwd, Some("lag-1"), "merhaba");
    run_turn(&h, req).await;

    // 1 kullanıcı mesajı + 2500 asistan parçası; hiçbiri kaybolmamalı, sıra bozulmamalı.
    let mut seqs = Vec::new();
    while seqs.len() < 2501 {
        match next(&mut watcher).await {
            Some(Response::Event(ev)) if ev.chat_id == "lag-1" => seqs.push(ev.seq),
            Some(_) => {}
            None => break,
        }
    }
    assert_eq!(seqs, (1..=2501).collect::<Vec<i64>>());
    h.shutdown().await;
}

/// Follow akışının ilk karesi abonelik onayıdır.
async fn expect_ack(c: &mut ultron_core::ipc::Client) -> serde_json::Value {
    match next(c).await.expect("akış kapandı") {
        Response::Ok { data } if data["subscribed"] == true => data,
        other => panic!("ilk kare abonelik onayı olmalı: {other:?}"),
    }
}

#[tokio::test]
async fn follow_stream_starts_with_subscription_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let mut global = connect(&h).await;
    global
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(
        expect_ack(&mut global).await["head_seq"],
        serde_json::json!({})
    );

    let mut chat = connect(&h).await;
    chat.send(&Request::Events {
        chat_id: Some("yok-sohbet".into()),
        follow: true,
        after_seq: None,
        limit: None,
    })
    .await
    .unwrap();
    let data = expect_ack(&mut chat).await;
    assert_eq!(data["head_seq"], 0);
    h.shutdown().await;
}

#[tokio::test]
async fn subscription_ack_precedes_replay_and_reports_chat_heads() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let first = run_turn(&h, send_req(&cwd, Some("c1"), "ilk")).await;
    assert_eq!(
        first,
        [
            "user_text",
            "session_started",
            "assistant_text",
            "turn_completed"
        ]
    );
    let second = run_turn(&h, send_req(&cwd, Some("c1"), "ikinci")).await;
    let other = run_turn(&h, send_req(&cwd, Some("c2"), "ilk")).await;
    let c1_head = (first.len() + second.len()) as i64;
    let c2_head = other.len() as i64;

    let mut global = connect(&h).await;
    global
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(
        expect_ack(&mut global).await["head_seq"],
        serde_json::json!({"c1": c1_head, "c2": c2_head})
    );

    let mut chat = connect(&h).await;
    chat.send(&Request::Events {
        chat_id: Some("c1".into()),
        follow: true,
        after_seq: Some(4),
        limit: None,
    })
    .await
    .unwrap();
    assert_eq!(expect_ack(&mut chat).await["head_seq"], c1_head);
    for seq in 5..=c1_head {
        match next(&mut chat).await {
            Some(Response::Event(ev)) => assert_eq!((ev.chat_id.as_str(), ev.seq), ("c1", seq)),
            other => panic!("onaydan sonra replay bekleniyordu: {other:?}"),
        }
    }
    h.shutdown().await;
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn subscription_heads_are_frozen_before_ack_is_written() {
    use ultrond::server::{SUBSCRIBE_GATE, SubscribeGate};
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    run_turn(&h, send_req(&cwd, Some("c1"), "ilk")).await;
    let gate = Arc::new(SubscribeGate {
        root: paths.root.clone(),
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        subscribed: tokio::sync::Notify::new(),
        acknowledge: tokio::sync::Semaphore::new(0),
    });
    *SUBSCRIBE_GATE.lock().unwrap() = Some(gate.clone());
    struct ResetGate;
    impl Drop for ResetGate {
        fn drop(&mut self) {
            *SUBSCRIBE_GATE.lock().unwrap() = None;
        }
    }
    let _reset = ResetGate;
    let mut watcher = connect(&h).await;
    watcher
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("abonelik kapıya ulaşmadı");
    // subscribe() öncesi yazım başa dahildir; onay öncesi ama abonelik sonrası yazım değildir.
    run_turn(&h, send_req(&cwd, Some("c1"), "ikinci")).await;
    gate.release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        gate.subscribed.notified(),
    )
    .await
    .expect("abonelik kurulmadı");
    run_turn(&h, send_req(&cwd, Some("c1"), "üçüncü")).await;
    run_turn(&h, send_req(&cwd, Some("c2"), "ilk")).await;
    gate.acknowledge.add_permits(1);
    assert_eq!(
        expect_ack(&mut watcher).await["head_seq"],
        serde_json::json!({"c1": 8})
    );
    for (chat, seq) in (9..=12)
        .map(|seq| ("c1", seq))
        .chain((1..=4).map(|seq| ("c2", seq)))
    {
        match next(&mut watcher).await {
            Some(Response::Event(ev)) => assert_eq!((ev.chat_id.as_str(), ev.seq), (chat, seq)),
            other => panic!("onaydan sonra canlı olay bekleniyordu: {other:?}"),
        }
    }
    h.shutdown().await;
}
