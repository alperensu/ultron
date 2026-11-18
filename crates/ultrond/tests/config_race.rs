mod common;

use common::*;
use ultron_core::ipc::{Request, Response};
use ultron_core::{Config, Paths};

async fn call(h: &ultrond::server::ServerHandle, req: Request) -> Response {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    next(&mut c).await.expect("yanıt yok")
}

#[tokio::test]
async fn concurrent_reset_rights_updates_both_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let paths: Paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[]).await;
    let mut tasks = Vec::new();
    for i in 0..16u32 {
        let mut c = connect(&h).await;
        tasks.push(tokio::spawn(async move {
            let pool = if i % 2 == 0 { "claude" } else { "codex" };
            c.send(&Request::SetResetRights {
                pool: pool.into(),
                count: i + 1,
            })
            .await
            .unwrap();
            next(&mut c).await.expect("yanıt yok")
        }));
    }
    for t in tasks {
        let r = t.await.unwrap();
        assert!(matches!(r, Response::Ok { .. }), "{r:?}");
    }
    let cfg = Config::load(&paths).unwrap();
    assert!(cfg.reset_rights.claude.is_some() && cfg.reset_rights.codex.is_some());
    let leftovers = std::fs::read_dir(&paths.root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0, "geçici dosya kalmamalı");
    h.shutdown().await;
}

#[tokio::test]
async fn external_edit_not_clobbered() {
    let tmp = tempfile::tempdir().unwrap();
    let paths: Paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[]).await;
    let r = call(
        &h,
        Request::SetResetRights {
            pool: "claude".into(),
            count: 1,
        },
    )
    .await;
    assert!(matches!(r, Response::Ok { .. }), "{r:?}");
    // Dışarıdan yapılan düzenleme sonraki yazımda korunmalı.
    let file = paths.root.join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("claude_live_quota = false\n{text}")).unwrap();
    let r = call(
        &h,
        Request::SetResetRights {
            pool: "codex".into(),
            count: 2,
        },
    )
    .await;
    assert!(matches!(r, Response::Ok { .. }), "{r:?}");
    let cfg = Config::load(&paths).unwrap();
    assert!(!cfg.claude_live_quota);
    assert_eq!(cfg.reset_rights.claude, Some(1));
    assert_eq!(cfg.reset_rights.codex, Some(2));
    h.shutdown().await;
}

#[tokio::test]
async fn telegram_settings_and_reset_rights_share_atomic_merge_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[]).await;
    // Allowlist daemon açıldıktan sonra sentetik dosyaya yazılır: test credential/ağ açmaz.
    std::fs::write(paths.root.join("config.toml"),
        "claude_live_quota = false\n[telegram]\nallowed_user_ids = [101, 202]\nfuture_telegram = 'keep'\n[future]\nvalue = 7\n").unwrap();
    let mut tasks = Vec::new();
    for i in 0..12 {
        let mut client = ultron_core::ipc::Client::connect(h.pipe_name(), h.approver_token()).await.unwrap();
        tasks.push(tokio::spawn(async move {
            let request = if i % 2 == 0 { Request::SetResetRights { pool: "claude".into(), count: i + 1 } }
                else { Request::SetTelegramSettings { notify: ultron_core::config::NotifyMode::Away, auto_continue_enabled: false, auto_continue_wait_minutes: 13, fallback_order: vec![ultron_core::EngineId::Codex] } };
            client.send(&request).await.unwrap(); next(&mut client).await.unwrap()
        }));
    }
    for task in tasks { assert!(matches!(task.await.unwrap(), Response::Ok { .. })); }
    let cfg = Config::load(&paths).unwrap();
    assert_eq!(cfg.telegram.allowed_user_ids, vec![101, 202]);
    assert_eq!(cfg.telegram.auto_continue_wait_minutes, 13);
    assert!(!cfg.telegram.auto_continue_enabled);
    assert!(cfg.reset_rights.claude.is_some());
    let table: toml::Table = std::fs::read_to_string(paths.root.join("config.toml")).unwrap().parse().unwrap();
    assert_eq!(table["telegram"]["future_telegram"].as_str(), Some("keep"));
    assert_eq!(table["future"]["value"].as_integer(), Some(7));
    assert!(!std::fs::read_dir(&paths.root).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".tmp")));
    h.shutdown().await;
}
