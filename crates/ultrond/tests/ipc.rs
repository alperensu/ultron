mod common;

use std::time::Duration;

use common::*;
use ultron_core::ipc::{Client, Request, Response, sys};
use ultron_core::{Config, Paths};
use ultrond::keepawake::KeepAwake;
use ultrond::server::{self, ServerConfig};

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
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

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
