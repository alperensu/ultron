mod common;

use common::*;
use serde_json::Value;
use ultron_core::ipc::{Request, Response};
use ultron_core::{Config, Paths};

async fn call(h: &ultrond::server::ServerHandle, req: Request) -> Response {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    next(&mut c).await.expect("yanıt yok")
}

#[tokio::test]
async fn reset_rights_persist_and_quota_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let paths: Paths = paths_in(tmp.path());
    std::fs::create_dir_all(&paths.root).unwrap();
    // Canlı uç nokta çağrılmasın.
    std::fs::write(paths.root.join("config.toml"), "claude_live_quota = false\n").unwrap();
    // Gerçek ev dizini yerine sentetik kota kaynakları.
    let qhome = tmp.path().join("qhome");
    std::fs::create_dir_all(&qhome).unwrap();
    let sess = qhome.join(".codex").join("sessions");
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::write(
        sess.join("s.jsonl"),
        concat!(
            r#"{"timestamp":"2099-01-01T00:00:00Z","payload":{"rate_limits":{"limit_id":"codex","#,
            r#""primary":{"used_percent":42.0,"window_minutes":300,"resets_at":4102444800}}}}"#,
            "
"
        ),
    )
    .unwrap();
    // SAFETY: test ikilisinde tek test var; ortam yalnız burada yazılır.
    unsafe { std::env::set_var("ULTRON_QUOTA_HOME", &qhome) };
    let h = start_with(&paths, &unique_pipe(), &[]).await;

    let r = call(&h, Request::SetResetRights { pool: "claude".into(), count: 3 }).await;
    assert!(matches!(r, Response::Ok { .. }), "{r:?}");
    let r = call(&h, Request::SetResetRights { pool: "agy".into(), count: 1 }).await;
    assert!(matches!(r, Response::Error { .. }), "{r:?}");

    let cfg = Config::load(&paths).unwrap();
    assert_eq!(cfg.reset_rights.claude, Some(3));
    assert!(!cfg.claude_live_quota, "mevcut anahtarlar korunmalı");

    let Response::Ok { data } = call(&h, Request::GetQuota).await else {
        panic!("GetQuota hata verdi");
    };
    let pools = data["quota"]["pools"].as_array().unwrap();
    assert_eq!(pools.len(), 3);
    assert_eq!(pools[0]["reset_rights"], Value::from(3));
    assert_eq!(pools[1]["windows"][0]["used_pct"], Value::from(42.0));
    assert_eq!(pools[2]["mode"], "Dengeli");
    h.shutdown().await;
}
