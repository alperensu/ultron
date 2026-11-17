use chrono::Utc;
use serde_json::json;
use ultron_core::config::EngineId;
use ultron_core::db::approvals::{ApprovalKind, ApprovalStatus, ResolvedBy};
use ultron_core::db::telegram::{
    DeliveryState, InboxState, TelegramAcceptance, TelegramTurnAcceptance,
};
use ultron_core::policy::{Action, RiskCategory};
use ultron_core::{Db, StoredEvent};

fn fixture() -> (tempfile::TempDir, Db) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    db.insert_chat("chat-a", None, "sentetik", Utc::now())
        .unwrap();
    db.insert_chat("chat-b", None, "sentetik", Utc::now())
        .unwrap();
    (tmp, db)
}

fn event(id: &str, chat: &str) -> StoredEvent {
    StoredEvent {
        id: id.into(),
        chat_id: chat.into(),
        session_id: None,
        seq: 0,
        ts: Utc::now(),
        kind: "user_text".into(),
        payload: json!({"type": "user_text", "text": "Türkçe sentetik hedef"}),
    }
}

fn accept(db: &Db, update: i64, logical: &str, event: &StoredEvent) -> TelegramAcceptance {
    db.accept_telegram_turn(&TelegramTurnAcceptance {
        bot_id: 100,
        update_id: update,
        user_id: 101,
        private_chat_id: 101,
        logical_turn_id: logical,
        event,
    })
    .unwrap()
}

#[test]
fn binding_restart_roundtrip_and_user_isolation() {
    let (tmp, db) = fixture();
    db.set_telegram_binding(101, Some("chat-a")).unwrap();
    db.set_telegram_binding(102, Some("chat-b")).unwrap();
    assert!(db.set_telegram_binding(0, None).is_err());
    assert!(db.set_telegram_binding(103, Some("missing")).is_err());
    drop(db);
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    assert_eq!(db.telegram_binding(101).unwrap().as_deref(), Some("chat-a"));
    db.set_telegram_binding(101, None).unwrap();
    assert!(db.telegram_binding(101).unwrap().is_none());
    assert_eq!(db.telegram_binding(102).unwrap().as_deref(), Some("chat-b"));
}

#[test]
fn accepted_turn_and_duplicate_update_have_one_user_event_and_fts_entry() {
    let (_tmp, db) = fixture();
    let TelegramAcceptance::Accepted(stored) =
        accept(&db, 5, "turn-a", &event("event-a", "chat-a"))
    else {
        panic!("ilk kabul gerekli");
    };
    assert_eq!(stored.seq, 1);
    let TelegramAcceptance::Duplicate(inbox) =
        accept(&db, 5, "unused", &event("event-b", "chat-b"))
    else {
        panic!("yinelenen güncelleme");
    };
    assert_eq!(inbox.logical_turn_id.as_deref(), Some("turn-a"));
    assert_eq!(db.events_for_chat("chat-a", None).unwrap().len(), 1);
    assert!(db.events_for_chat("chat-b", None).unwrap().is_empty());
    assert_eq!(db.search_messages(None, "sentetik", 10).unwrap().len(), 1);
    assert_eq!(db.telegram_next_update_id(100).unwrap(), Some(6));
    assert_eq!(db.telegram_binding(101).unwrap().as_deref(), Some("chat-a"));
}

#[test]
fn concurrent_duplicate_acceptance_has_one_event_and_one_execution_owner() {
    let (tmp, db) = fixture();
    let a = Db::open(&tmp.path().join("store.db")).unwrap();
    let b = Db::open(&tmp.path().join("store.db")).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            accept(&a, 1, "turn-a", &event("event-a", "chat-a"))
        });
        let second = scope.spawn(|| {
            barrier.wait();
            accept(&b, 1, "turn-b", &event("event-b", "chat-a"))
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, TelegramAcceptance::Accepted(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, TelegramAcceptance::Duplicate(_)))
            .count(),
        1
    );
    assert_eq!(db.events_for_chat("chat-a", None).unwrap().len(), 1);
    assert_eq!(db.search_messages(None, "sentetik", 10).unwrap().len(), 1);
}

#[test]
fn acceptance_discards_extra_payload_and_masks_live_turn_grants() {
    let (_tmp, db) = fixture();
    let grant =
        ultron_core::turn_grant::issue("masking-test", std::path::Path::new("synthetic-workspace"));
    let mut ev = event("masked", "chat-a");
    ev.payload = json!({"type":"user_text", "text":format!("Sentetik {}", grant.token()),
        "raw_telegram_json":"discarded", "nonce":"discarded"});
    let TelegramAcceptance::Accepted(stored) = accept(&db, 1, "turn", &ev) else {
        panic!("ilk kabul gerekli")
    };
    assert!(!stored.payload.to_string().contains(grant.token()));
    assert!(stored.payload.get("raw_telegram_json").is_none());
    assert!(stored.payload.get("nonce").is_none());
    assert!(
        stored.payload["text"]
            .as_str()
            .unwrap()
            .contains("[MASKED]")
    );
}

#[test]
fn poll_offsets_are_monotonic_bot_scoped_and_rejections_store_no_identity() {
    let (tmp, db) = fixture();
    db.checkpoint_telegram_poll(100, 20).unwrap();
    db.checkpoint_telegram_poll(100, 10).unwrap();
    assert_eq!(db.telegram_next_update_id(100).unwrap(), Some(20));
    assert_eq!(db.telegram_next_update_id(200).unwrap(), None);
    assert!(db.telegram_inbox_entry(100, 19).unwrap().is_none());
    assert!(!db.accept_telegram_update(100, 19, 101).unwrap());
    assert!(
        db.accept_telegram_turn(&TelegramTurnAcceptance {
            bot_id: 100,
            update_id: 19,
            user_id: 101,
            private_chat_id: 101,
            logical_turn_id: "old",
            event: &event("old", "chat-a"),
        })
        .is_err()
    );
    assert!(db.accept_telegram_update(200, 1, 101).unwrap());
    assert!(!db.accept_telegram_update(200, 1, 101).unwrap());
    assert!(db.checkpoint_telegram_poll(0, 1).is_err());
    assert!(db.checkpoint_telegram_poll(100, -1).is_err());
    assert!(db.accept_telegram_update(200, i64::MAX, 101).is_err());
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM telegram_inbox WHERE bot_id = 100",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn inbox_acceptance_rolls_back_event_fts_binding_origin_and_offset() {
    let (tmp, db) = fixture();
    db.set_telegram_binding(101, Some("chat-b")).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_checkpoint BEFORE INSERT ON telegram_poll_state
        BEGIN SELECT RAISE(ABORT, 'synthetic rollback'); END;",
    )
    .unwrap();
    assert!(
        db.accept_telegram_turn(&TelegramTurnAcceptance {
            bot_id: 100,
            update_id: 5,
            user_id: 101,
            private_chat_id: 101,
            logical_turn_id: "turn-a",
            event: &event("event-a", "chat-a"),
        })
        .is_err()
    );
    assert!(db.events_for_chat("chat-a", None).unwrap().is_empty());
    assert!(db.search_messages(None, "sentetik", 10).unwrap().is_empty());
    assert!(db.telegram_turn_origin("turn-a").unwrap().is_none());
    assert!(db.telegram_inbox_entry(100, 5).unwrap().is_none());
    assert!(db.telegram_next_update_id(100).unwrap().is_none());
    assert_eq!(db.telegram_binding(101).unwrap().as_deref(), Some("chat-b"));
    conn.execute_batch("DROP TRIGGER reject_checkpoint;")
        .unwrap();
    assert!(matches!(
        accept(&db, 5, "turn-a", &event("event-a", "chat-a")),
        TelegramAcceptance::Accepted(_)
    ));
}

#[test]
fn origin_keeps_original_recipient_and_final_seq_after_rebinding() {
    let (_tmp, db) = fixture();
    accept(&db, 1, "turn-a", &event("user", "chat-a"));
    assert!(db.start_telegram_inbox(100, 1).unwrap());
    assert!(!db.start_telegram_inbox(100, 1).unwrap());
    db.set_telegram_binding(101, Some("chat-b")).unwrap();
    let mut final_event = event("final", "chat-a");
    final_event.kind = "assistant_text".into();
    final_event.payload = json!({"type":"assistant_text", "text":"Sonuç"});
    let final_event = db.append_event(&final_event).unwrap().unwrap();
    assert!(!db.complete_telegram_turn("turn-a", 99).unwrap());
    assert!(
        db.complete_telegram_turn("turn-a", final_event.seq)
            .unwrap()
    );
    assert!(
        !db.complete_telegram_turn("turn-a", final_event.seq)
            .unwrap()
    );
    let origin = db.telegram_turn_origin("turn-a").unwrap().unwrap();
    assert_eq!(origin.chat_id, "chat-a");
    assert_eq!(origin.user_id, 101);
    assert_eq!(origin.private_chat_id, 101);
    assert_eq!(origin.user_event_seq, 1);
    assert_eq!(origin.start_seq, 1);
    assert_eq!(origin.end_seq, Some(final_event.seq));
    assert_eq!(origin.delivery_state, DeliveryState::Ready);
    assert!(db.finish_telegram_delivery("turn-a", false).unwrap());
    assert!(!db.finish_telegram_delivery("turn-a", true).unwrap());
    assert_eq!(
        db.telegram_turn_origin("turn-a")
            .unwrap()
            .unwrap()
            .delivery_state,
        DeliveryState::NeedsAttention
    );
}

#[test]
fn restart_marks_ambiguous_acceptance_without_replaying() {
    let (tmp, db) = fixture();
    accept(&db, 1, "turn-a", &event("user", "chat-a"));
    assert!(db.accept_telegram_update(100, 2, 101).unwrap());
    assert!(db.accept_telegram_update(100, 3, 101).unwrap());
    assert!(db.complete_telegram_update(100, 3).unwrap());
    drop(db);
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    assert_eq!(db.recover_telegram_inbox().unwrap(), 2);
    assert_eq!(db.recover_telegram_inbox().unwrap(), 0);
    assert_eq!(
        db.telegram_inbox_entry(100, 1).unwrap().unwrap().state,
        InboxState::NeedsAttention
    );
    assert_eq!(
        db.telegram_inbox_entry(100, 3).unwrap().unwrap().state,
        InboxState::Completed
    );
    assert!(matches!(
        accept(&db, 1, "unused", &event("unused", "chat-a")),
        TelegramAcceptance::Duplicate(_)
    ));
    assert!(!db.start_telegram_inbox(100, 1).unwrap());
    assert_eq!(db.events_for_chat("chat-a", None).unwrap().len(), 1);
    assert_eq!(db.telegram_next_update_id(100).unwrap(), Some(4));
}

#[test]
fn invalid_identity_or_event_never_creates_acceptance() {
    let (_tmp, db) = fixture();
    let ev = event("user", "chat-a");
    for (user, chat) in [(101, 102), (0, 0), (-1, -1)] {
        assert!(
            db.accept_telegram_turn(&TelegramTurnAcceptance {
                bot_id: 100,
                update_id: 1,
                user_id: user,
                private_chat_id: chat,
                logical_turn_id: "turn",
                event: &ev,
            })
            .is_err()
        );
    }
    let mut bad = ev;
    bad.kind = "assistant_text".into();
    assert!(
        db.accept_telegram_turn(&TelegramTurnAcceptance {
            bot_id: 100,
            update_id: 1,
            user_id: 101,
            private_chat_id: 101,
            logical_turn_id: "turn",
            event: &bad,
        })
        .is_err()
    );
    assert!(db.telegram_next_update_id(100).unwrap().is_none());
}

#[test]
fn approval_kind_is_explicit_and_elevation_never_becomes_policy_tool() {
    let (_tmp, db) = fixture();
    let action = Action::FileDelete {
        path: "sentetik.txt".into(),
    };
    let old = db
        .create_owned_approval(
            None,
            EngineId::Claude,
            RiskCategory::Delete,
            "sentetik",
            &action,
            None,
        )
        .unwrap();
    assert_eq!(old.kind, ApprovalKind::Unknown);
    assert!(!old.is_policy_tool());
    let tool = db
        .create_policy_tool_approval(
            None,
            EngineId::Claude,
            RiskCategory::Delete,
            "sentetik",
            &action,
            Some("requester"),
        )
        .unwrap();
    assert!(tool.is_policy_tool());
    assert!(db.get_approval(&tool.id).unwrap().unwrap().is_policy_tool());
    let elevate = Action::ToolCall {
        tool: "ultron.turn.elevate".into(),
        input: json!({}),
    };
    assert!(
        db.create_policy_tool_approval(
            None,
            EngineId::Claude,
            RiskCategory::SystemConfig,
            "sentetik",
            &elevate,
            None
        )
        .is_err()
    );
    let elevation = db
        .create_turn_elevation_approval("chat-a", EngineId::Claude, "sentetik", &elevate)
        .unwrap();
    assert_eq!(elevation.kind, ApprovalKind::TurnElevation);
    assert!(!elevation.is_policy_tool());
    assert!(
        db.create_turn_elevation_approval("chat-a", EngineId::Claude, "sentetik", &action)
            .is_err()
    );
    db.resolve_approval(&tool.id, true, ResolvedBy::Approver)
        .unwrap();
    assert!(
        db.resolve_approval(&tool.id, false, ResolvedBy::Telegram)
            .is_err()
    );
    assert_eq!(
        db.get_approval(&tool.id).unwrap().unwrap().status,
        ApprovalStatus::Approved
    );
    assert!(
        db.list_approvals(None)
            .unwrap()
            .iter()
            .any(|a| a.kind == ApprovalKind::TurnElevation)
    );
    assert!(
        db.list_approvals_page(None, None, None, None)
            .unwrap()
            .approvals
            .iter()
            .any(|a| a.kind == ApprovalKind::PolicyTool)
    );
}
