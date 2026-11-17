//! Sentetik, izole named-pipe sözleşme testleri; gerçek motor veya görev başlatmaz.
mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{connect, next, paths_in, unique_pipe};
use ultron_core::db::approvals::{Approval, ApprovalStatus, ResolvedBy};
use ultron_core::ipc::{Client, Request, Response};
use ultron_core::policy::{Action, Decision};
use ultron_core::{Config, Db, EngineId, Paths};
use ultrond::server::{self, ServerConfig, ServerHandle};

async fn start(paths: &Paths) -> ServerHandle {
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: unique_pipe(),
        cfg: Config::default(),
        engines: HashMap::new(),
    })
    .await
    .unwrap()
}

async fn call(h: &ServerHandle, request: Request) -> Response {
    let mut client = Client::connect(h.pipe_name(), h.approver_token())
        .await
        .unwrap();
    client.send(&request).await.unwrap();
    next(&mut client).await.unwrap()
}

async fn begin(h: &ServerHandle, paths: &Paths, chat_id: Option<&str>, wait_secs: u32) -> Client {
    let mut client = connect(h).await;
    client
        .send(&Request::PolicyCheck {
            action: Action::Shell {
                command: "rm synthetic.txt; token=synthetic-value".into(),
                cwd: paths.root.clone(),
            },
            chat_id: chat_id.map(str::to_owned),
            engine: EngineId::Claude,
            wait_secs,
        })
        .await
        .unwrap();
    client
}

async fn pending(h: &ServerHandle) -> Approval {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Response::Ok { data } = call(
                h,
                Request::ListApprovals {
                    status: Some(ApprovalStatus::Pending),
                    limit: None,
                    cursor: None,
                },
            )
            .await
            else {
                panic!("liste Ok olmalı");
            };
            let rows: Vec<Approval> = serde_json::from_value(data["approvals"].clone()).unwrap();
            if let Some(row) = rows.into_iter().next() {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bekleyen onay oluşmadı")
}

async fn decision(client: &mut Client) -> Decision {
    let Response::Ok { data } = next(client).await.unwrap() else {
        panic!("karar Ok olmalı");
    };
    serde_json::from_value(data["decision"].clone()).unwrap()
}

#[tokio::test]
async fn requester_cannot_resolve_or_list_another_connections_approvals() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start(&paths).await;
    assert_ne!(h.token(), h.approver_token());
    for (token, role) in [(h.token(), "requester"), (h.approver_token(), "approver")] {
        let mut client = Client::open(h.pipe_name()).await.unwrap();
        client
            .send(&Request::Hello {
                token: token.into(),
            })
            .await
            .unwrap();
        assert!(
            matches!(next(&mut client).await, Some(Response::Ok { data }) if data["role"] == role)
        );
    }
    let mut checking = begin(&h, &paths, None, 30).await;
    let row = pending(&h).await;
    let mut requester = connect(&h).await;
    requester
        .send(&Request::ListApprovals {
            status: None,
            limit: None,
            cursor: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(next(&mut requester).await, Some(Response::Ok { data }) if data["approvals"].as_array().unwrap().is_empty())
    );
    requester
        .send(&Request::ResolveApproval {
            id: row.id.clone(),
            approve: true,
            by: "desktop".into(),
        })
        .await
        .unwrap();
    assert!(
        matches!(next(&mut requester).await, Some(Response::Error { message }) if message.contains("yetkisiz"))
    );
    assert_eq!(
        Db::open(&paths.db)
            .unwrap()
            .get_approval(&row.id)
            .unwrap()
            .unwrap()
            .status,
        ApprovalStatus::Pending
    );
    assert!(matches!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id.clone(),
                approve: true,
                by: "telegram".into()
            }
        )
        .await,
        Response::Ok { .. }
    ));
    assert_eq!(decision(&mut checking).await, Decision::Allow);
    // Etiket telegram olsa da kimlik onaylayıcı bağlantısından türetilir.
    assert_eq!(
        Db::open(&paths.db)
            .unwrap()
            .get_approval(&row.id)
            .unwrap()
            .unwrap()
            .resolved_by,
        Some(ResolvedBy::Approver)
    );
    checking
        .send(&Request::ListApprovals {
            status: None,
            limit: None,
            cursor: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(next(&mut checking).await, Some(Response::Ok { data }) if data["approvals"][0]["id"] == row.id)
    );
    h.shutdown().await;
}

#[tokio::test]
async fn chatless_events_advance_the_existing_cursor_after_daemon_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let mut cursor = 0;
    for _ in 0..2 {
        let h = start(&paths).await;
        let mut stream = connect(&h).await;
        stream
            .send(&Request::Events {
                chat_id: None,
                follow: true,
                after_seq: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(matches!(next(&mut stream).await, Some(Response::Ok { .. })));
        let mut checking = begin(&h, &paths, None, 30).await;
        let row = pending(&h).await;
        call(
            &h,
            Request::ResolveApproval {
                id: row.id,
                approve: false,
                by: "desktop".into(),
            },
        )
        .await;
        assert!(matches!(
            decision(&mut checking).await,
            Decision::Deny { .. }
        ));
        for kind in ["approval_requested", "approval_resolved"] {
            let Some(Response::Event(event)) = next(&mut stream).await else {
                panic!("onay olayı bekleniyordu");
            };
            assert!(event.chat_id.is_empty());
            assert_eq!(event.kind, kind);
            // Masaüstünün Cursors::accept sözleşmesi: eski imleç korunur.
            assert!(event.seq > cursor);
            cursor = event.seq;
        }
        h.shutdown().await;
    }
}

#[tokio::test]
async fn ask_creates_pending_and_approve_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let db = Db::open(&paths.db).unwrap();
    db.insert_chat("approval-chat", None, "test", chrono::Utc::now())
        .unwrap();
    let h = start(&paths).await;
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
    assert!(matches!(next(&mut global).await, Some(Response::Ok { .. })));
    let mut checking = begin(&h, &paths, Some("approval-chat"), 30).await;
    let row = pending(&h).await;
    assert_eq!(row.chat_id.as_deref(), Some("approval-chat"));
    assert_eq!(row.resolved_at, None);
    assert!(
        matches!(next(&mut global).await, Some(Response::Event(ev)) if ev.kind == "approval_requested")
    );
    assert!(matches!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id.clone(),
                approve: true,
                by: "desktop".into()
            }
        )
        .await,
        Response::Ok { .. }
    ));
    assert_eq!(decision(&mut checking).await, Decision::Allow);
    assert!(
        matches!(next(&mut global).await, Some(Response::Event(ev)) if ev.kind == "approval_resolved")
    );
    let events = db.events_for_chat("approval-chat", None).unwrap();
    assert_eq!(
        events.iter().map(|ev| ev.kind.as_str()).collect::<Vec<_>>(),
        ["approval_requested", "approval_resolved"]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn deny_or_timeout_denies() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start(&paths).await;
    let mut checking = begin(&h, &paths, None, 30).await;
    let row = pending(&h).await;
    assert!(matches!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id,
                approve: false,
                by: "desktop".into()
            }
        )
        .await,
        Response::Ok { .. }
    ));
    assert!(matches!(
        decision(&mut checking).await,
        Decision::Deny { .. }
    ));
    let mut timed = begin(&h, &paths, None, 0).await;
    assert!(matches!(decision(&mut timed).await, Decision::Deny { .. }));
    let db = Db::open(&paths.db).unwrap();
    let rows = db.list_approvals(None).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r.status == ApprovalStatus::Denied && r.resolved_at.is_some())
    );
    assert!(
        rows.iter()
            .any(|r| r.resolved_by == Some(ResolvedBy::Timeout))
    );
    h.shutdown().await;
}

#[tokio::test]
async fn approval_resolves_once() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start(&paths).await;
    let mut checking = begin(&h, &paths, None, 30).await;
    let row = pending(&h).await;
    assert!(matches!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id.clone(),
                approve: true,
                by: "restart".into()
            }
        )
        .await,
        Response::Error { .. }
    ));
    let (a, b) = tokio::join!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id.clone(),
                approve: true,
                by: "desktop".into()
            }
        ),
        call(
            &h,
            Request::ResolveApproval {
                id: row.id.clone(),
                approve: false,
                by: "telegram".into()
            }
        ),
    );
    assert_eq!(
        usize::from(matches!(&a, Response::Ok { .. }))
            + usize::from(matches!(&b, Response::Ok { .. })),
        1
    );
    let error = if let Response::Error { message } = &a {
        message
    } else if let Response::Error { message } = &b {
        message
    } else {
        panic!("ikinci karar reddedilmeli");
    };
    assert!(error.contains("onay zaten çözüldü"));
    let stored = Db::open(&paths.db)
        .unwrap()
        .get_approval(&row.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        matches!(decision(&mut checking).await, Decision::Allow),
        stored.status == ApprovalStatus::Approved
    );
    h.shutdown().await;
}

#[tokio::test]
async fn pending_approval_expires_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start(&paths).await;
    let mut checking = begin(&h, &paths, None, 30).await;
    let row = pending(&h).await;
    h.shutdown().await;
    assert!(!matches!(
        next(&mut checking).await,
        Some(Response::Ok { .. })
    ));
    let h = start(&paths).await;
    let stored = Db::open(&paths.db)
        .unwrap()
        .get_approval(&row.id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ApprovalStatus::Expired);
    assert_eq!(stored.resolved_by, Some(ResolvedBy::Restart));
    assert!(stored.resolved_at.is_some());
    assert!(matches!(
        call(
            &h,
            Request::ResolveApproval {
                id: row.id,
                approve: true,
                by: "desktop".into()
            }
        )
        .await,
        Response::Error { .. }
    ));
    h.shutdown().await;
}

#[tokio::test]
async fn action_text_is_masked_in_db() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start(&paths).await;
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
    assert!(matches!(next(&mut global).await, Some(Response::Ok { .. })));
    let mut checking = begin(&h, &paths, None, 30).await;
    let row = pending(&h).await;
    assert!(row.chat_id.is_none());
    assert!(row.action_json.contains("[MASKED]"));
    assert!(!row.action_json.contains("synthetic-value"));
    assert!(serde_json::from_str::<serde_json::Value>(&row.action_json).is_ok());
    let Some(Response::Event(event)) = next(&mut global).await else {
        panic!("küresel onay olayı bekleniyor");
    };
    assert_eq!(event.kind, "approval_requested");
    assert!(!event.payload.to_string().contains("synthetic-value"));
    let stored = Db::open(&paths.db)
        .unwrap()
        .get_approval(&row.id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.action_json, row.action_json);
    call(
        &h,
        Request::ResolveApproval {
            id: row.id,
            approve: false,
            by: "desktop".into(),
        },
    )
    .await;
    assert!(matches!(
        decision(&mut checking).await,
        Decision::Deny { .. }
    ));
    h.shutdown().await;
}
