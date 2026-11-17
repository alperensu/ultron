//! Proje içe aktarma / sohbet silme yetki testleri (sentetik; gerçek ev dizini okunmaz).
mod common;

use common::{connect, next, paths_in, start_with, unique_pipe};
use serde_json::Value;
use ultron_core::ipc::{Client, Request, Response};

fn assert_unauthorized(resp: Option<Response>, what: &str) {
    match resp {
        Some(Response::Error { message }) => {
            assert!(message.contains("yetkisiz"), "{what}: {message}")
        }
        other => panic!("{what}: yetkisiz hatası bekleniyordu: {other:?}"),
    }
}

#[tokio::test]
async fn requester_token_cannot_import_projects_remove_chats_or_import_history() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[]).await;
    let empty = tmp.path().join("bos");
    std::fs::create_dir_all(&empty).unwrap();
    for (name, req) in [
        ("ImportProjects", Request::ImportProjects { dry_run: true, home: None }),
        ("RemoveImportedChats", Request::RemoveImportedChats { dry_run: true }),
        (
            "ImportClaudeHistory",
            Request::ImportClaudeHistory { root: Some(empty.to_string_lossy().into_owned()) },
        ),
    ] {
        let mut c = connect(&h).await;
        c.send(&req).await.unwrap();
        assert_unauthorized(next(&mut c).await, name);
    }
    h.shutdown().await;
}

#[tokio::test]
async fn approver_import_ignores_home_override_in_production_and_rejects_unc() {
    // Sistem geçici dizini proje sayılmadığından sentetik ev dizinleri `target/tmp` altında kurulur.
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[]).await;
    let mk_home = |name: &str| {
        let home = tmp.path().join(name);
        let proj = home.join("work").join(format!("proje-{name}"));
        std::fs::create_dir_all(&proj).unwrap();
        let line = serde_json::json!({"type":"user","cwd":proj.to_string_lossy(),
            "timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":"merhaba"}});
        let f = home.join(".claude/projects/x/s.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, format!("{line}\n")).unwrap();
        home
    };
    let (home_a, home_b) = (mk_home("a"), mk_home("b"));
    // SAFETY: bu test ikilisinde ortam yalnız bu testte yazılır.
    unsafe {
        std::env::set_var("USERPROFILE", &home_a);
        std::env::remove_var("ULTRON_TEST_ALLOW_HOME_OVERRIDE");
    }
    let ask = |home: Option<String>| Request::ImportProjects { dry_run: true, home };
    let mut c = Client::connect(h.pipe_name(), h.approver_token()).await.unwrap();

    // Geçersiz kılma açık değil: `home` yok sayılır, USERPROFILE (a) kullanılır.
    c.send(&ask(Some(home_b.to_string_lossy().into_owned()))).await.unwrap();
    let Some(Response::Ok { data }) = next(&mut c).await else { panic!("yanıt yok") };
    let paths_seen = |data: &Value| -> Vec<String> {
        data["report"]["rows"].as_array().unwrap().iter()
            .map(|r| r["path"].as_str().unwrap().to_lowercase()).collect()
    };
    let seen = paths_seen(&data);
    assert_eq!(seen.len(), 1);
    assert!(seen[0].ends_with("proje-a"), "{seen:?}");

    // Test geçersiz kılması açılınca (b) kullanılır; UNC/verbatim reddedilir.
    unsafe { std::env::set_var("ULTRON_TEST_ALLOW_HOME_OVERRIDE", "1") };
    c.send(&ask(Some(home_b.to_string_lossy().into_owned()))).await.unwrap();
    let Some(Response::Ok { data }) = next(&mut c).await else { panic!("yanıt yok") };
    assert!(paths_seen(&data)[0].ends_with("proje-b"));
    for bad in [r"\\sunucu\paylasim", r"\\?\C:\Users\x", "//sunucu/paylasim"] {
        c.send(&ask(Some(bad.into()))).await.unwrap();
        match next(&mut c).await {
            Some(Response::Error { message }) => assert!(message.contains("kabul edilmez"), "{bad}: {message}"),
            other => panic!("{bad}: hata bekleniyordu: {other:?}"),
        }
    }
    // Onaylayıcı sohbet silme dry-run'ı çalıştırabilir.
    c.send(&Request::RemoveImportedChats { dry_run: true }).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Ok { .. })));
    unsafe { std::env::remove_var("ULTRON_TEST_ALLOW_HOME_OVERRIDE") };
    h.shutdown().await;
}
