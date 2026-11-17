//! Task 5 sözleşmeleri; sentetik motor ve geçici veri dizini. Koordinatörde çalıştırılır.
mod common;

use std::time::Duration;

use common::*;
use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
use ultron_core::ipc::{Client, PolicyArg, Request, Response};
use ultron_core::{Db, EngineId};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct HomeOverride(Option<std::ffi::OsString>);

impl HomeOverride {
    fn set(paths: &ultron_core::Paths) -> Self {
        let previous = std::env::var_os("ULTRON_HOME");
        // SAFETY: Bu test ikilisindeki tüm ortam kullanan testler ENV_LOCK tutar.
        unsafe { std::env::set_var("ULTRON_HOME", &paths.root) };
        Self(previous)
    }
}

impl Drop for HomeOverride {
    fn drop(&mut self) {
        // SAFETY: Sunucu/görevler kapatıldıktan sonra, ENV_LOCK hâlâ tutulurken geri alınır.
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var("ULTRON_HOME", value),
                None => std::env::remove_var("ULTRON_HOME"),
            }
        }
    }
}

async fn terminal(sender: &mut Client) -> Result<Vec<ultron_core::db::StoredEvent>, String> {
    let mut events = Vec::new();
    loop {
        match next(sender).await {
            Some(Response::Event(event)) => events.push(event),
            Some(Response::Ok { data }) if data["done"] == true => return Ok(events),
            Some(Response::Error { message }) => return Err(message),
            Some(_) => {}
            None => panic!("Send akışı erken kapandı"),
        }
    }
}

#[tokio::test]
async fn full_turn_waits_for_approver_and_requester_cannot_approve() {
    let _env = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let _home = HomeOverride::set(&paths);
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
    let db = Db::open(&paths.db).unwrap();

    // Hem açık Full hem elevated:true tek kullanımlık kullanıcı onayını gerektirir.
    for (chat, policy, elevated, approve) in [
        ("full", PolicyArg::Full, false, true),
        ("elevated", PolicyArg::Workspace, true, true),
        ("denied", PolicyArg::Full, true, false),
    ] {
        let mut subscriber = connect(&h).await;
        subscriber.send(&Request::Events {
            chat_id: None, follow: true, after_seq: None, limit: None,
        }).await.unwrap();
        assert!(matches!(next(&mut subscriber).await, Some(Response::Ok { data }) if data["subscribed"] == true));
        let mut sender = connect(&h).await;
        sender.send(&Request::Send {
            chat_id: Some(chat.into()), cwd: cwd.to_string_lossy().into_owned(),
            engine: Some(EngineId::Codex), text: "sentetik tur".into(), policy, elevated,
            project_id: None,
        }).await.unwrap();
        assert!(matches!(next(&mut sender).await, Some(Response::Ok { data }) if data["chat_id"] == chat));
        let row = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(row) = db.list_approvals(Some(ApprovalStatus::Pending)).unwrap()
                    .into_iter().find(|row| row.chat_id.as_deref() == Some(chat)) {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("Full için onay kaydı oluşmadı");
        let requested = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match next(&mut subscriber).await {
                    Some(Response::Event(event))
                        if event.kind == "approval_requested" && event.chat_id == chat => break event,
                    Some(Response::Event(_)) => {}
                    other => panic!("küresel onay akışı erken kapandı: {other:?}"),
                }
            }
        }).await.expect("onay kararı öncesinde küresel ApprovalRequested olayı gelmedi");
        assert_eq!(requested.payload["approval_id"], row.id);
        assert_eq!(db.get_approval(&row.id).unwrap().unwrap().status, ApprovalStatus::Pending);
        assert!(db.latest_session(chat).unwrap().is_none(), "onay öncesi motor başladı");
        let mut requester = connect(&h).await;
        requester.send(&Request::ResolveApproval {
            id: row.id.clone(), approve: true, by: "desktop".into(),
        }).await.unwrap();
        assert!(matches!(next(&mut requester).await, Some(Response::Error { message }) if message.contains("yetkisiz")));
        assert_eq!(db.get_approval(&row.id).unwrap().unwrap().status, ApprovalStatus::Pending);
        assert!(db.latest_session(chat).unwrap().is_none());

        let mut approver = Client::connect(h.pipe_name(), h.approver_token()).await.unwrap();
        approver.send(&Request::ResolveApproval {
            id: row.id.clone(), approve, by: "desktop".into(),
        }).await.unwrap();
        assert!(matches!(next(&mut approver).await, Some(Response::Ok { .. })));
        let result = terminal(&mut sender).await;
        if approve {
            assert!(result.unwrap().iter().any(|event| event.kind == "assistant_text"));
        } else {
            assert!(result.is_err());
            assert!(db.latest_session(chat).unwrap().is_none());
        }
        assert_eq!(db.get_approval(&row.id).unwrap().unwrap().resolved_by, Some(ResolvedBy::Approver));
    }
    h.shutdown().await;
}

#[tokio::test]
async fn risky_command_is_persisted_and_broadcast_as_policy_violation() {
    let _env = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let _home = HomeOverride::set(&paths);
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[
        STARTED,
        r#"{"type":"command","command":"rm synthetic.txt; token=synthetic-secret","exit_code":1,"output_excerpt":""}"#,
        DONE,
    ]).await;
    let mut subscriber = connect(&h).await;
    subscriber.send(&Request::Events {
        chat_id: None, follow: true, after_seq: None, limit: None,
    }).await.unwrap();
    assert!(matches!(next(&mut subscriber).await, Some(Response::Ok { data }) if data["subscribed"] == true));
    let mut sender = connect(&h).await;
    let mut request = send_req(&cwd, Some("audit"), "sentetik");
    if let Request::Send { engine, .. } = &mut request { *engine = Some(EngineId::Codex); }
    sender.send(&request).await.unwrap();
    let events = terminal(&mut sender).await.unwrap();
    let violations: Vec<_> = events.iter().filter(|event| event.kind == "policy_violation").collect();
    assert_eq!(violations.len(), 1);
    let stored = violations[0];
    assert_eq!(stored.payload["category"], "Delete");
    assert!(!stored.payload.to_string().contains("synthetic-secret"));
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Response::Event(event)) = next(&mut subscriber).await
                && event.kind == "policy_violation" { break event; }
        }
    }).await.expect("genel abone ihlali almadı");
    assert_eq!(&live, stored);
    let db = Db::open(&paths.db).unwrap();
    assert_eq!(db.events_for_chat("audit", None).unwrap().iter()
        .filter(|event| event.kind == "policy_violation").count(), 1);
    let transcript = std::fs::read_to_string(paths.transcripts.join("audit.jsonl")).unwrap();
    assert!(transcript.contains("policy_violation"));
    assert!(!transcript.contains("synthetic-secret"));
    h.shutdown().await;
}

#[tokio::test]
async fn legacy_send_without_elevated_still_runs() {
    let _env = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let _home = HomeOverride::set(&paths);
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &[STARTED, REPLY, DONE]).await;
    let mut sender = connect(&h).await;
    let legacy = serde_json::json!({
        "type": "send", "chat_id": "legacy", "cwd": cwd,
        "engine": "codex", "text": "eski istemci", "policy": "workspace"
    });
    let mut wire = serde_json::to_vec(&legacy).unwrap();
    wire.push(b'\n');
    sender.send_raw(&wire).await.unwrap();
    assert!(terminal(&mut sender).await.unwrap().iter().any(|event| event.kind == "assistant_text"));
    assert!(Db::open(&paths.db).unwrap().list_approvals(None).unwrap().is_empty());
    h.shutdown().await;
}
