use chrono::{Duration, Utc};
use serde_json::json;
use ultron_core::config::EngineId;
use ultron_core::db::quota_wait::{
    NewQuotaWait, QuotaWaitProgress, QuotaWaitState as State, ResumePolicy,
};
use ultron_core::db::telegram::{TelegramAcceptance, TelegramTurnAcceptance};
use ultron_core::db::{SessionRow, SessionStatus};
use ultron_core::ipc::PolicyArg;
use ultron_core::{Db, StoredEvent};

fn event(id: &str, kind: &str, payload: serde_json::Value) -> StoredEvent {
    StoredEvent {
        id: id.into(),
        chat_id: "chat".into(),
        session_id: Some("session".into()),
        seq: 0,
        ts: Utc::now(),
        kind: kind.into(),
        payload,
    }
}

fn fixture() -> (tempfile::TempDir, Db, NewQuotaWait) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    db.insert_chat("chat", None, "sentetik", Utc::now())
        .unwrap();
    db.upsert_session(&SessionRow {
        id: "session".into(),
        chat_id: "chat".into(),
        engine: EngineId::Claude,
        native_session_id: None,
        status: SessionStatus::WaitingQuota,
        started_at: Utc::now(),
        ended_at: None,
    })
    .unwrap();
    db.append_event(&event(
        "user",
        "user_text",
        json!({"type":"user_text", "text":"Özgün sentetik hedef"}),
    ))
    .unwrap();
    db.append_event(&event(
        "quota",
        "quota_exhausted",
        json!({"type":"quota_exhausted", "message":"sentetik", "resets_at":null}),
    ))
    .unwrap();
    let wait = NewQuotaWait {
        id: "wait".into(),
        logical_turn_id: "turn".into(),
        chat_id: "chat".into(),
        source_session_id: "session".into(),
        engine: EngineId::Claude,
        user_event_seq: 1,
        checkpoint_seq: 2,
        cwd: "synthetic-workspace".into(),
        policy: ResumePolicy::Workspace,
        reset_at: None,
        next_attempt_at: None,
    };
    (tmp, db, wait)
}

#[test]
fn scheduler_plan_persists_fixed_chain_and_deadline_across_restart() {
    let (tmp, db, mut new) = fixture();
    let now = Utc::now();
    let reset = now + Duration::minutes(10);
    let deadline = reset + Duration::seconds(8);
    new.reset_at = Some(reset);
    let wait = db.create_quota_wait(&new, now).unwrap();
    let first = db.prepare_quota_wait_schedule(
        &wait.id, wait.generation, &[EngineId::Codex, EngineId::Agy], Some(deadline),
    ).unwrap().unwrap();
    assert_eq!(first.planned_reset_at, Some(reset));
    assert_eq!(first.reset_deadline, Some(deadline));
    drop(db);
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    let restored = db.prepare_quota_wait_schedule(
        &wait.id, wait.generation, &[EngineId::Agy, EngineId::Codex], None,
    ).unwrap().unwrap();
    assert_eq!(restored, first, "reset geçince veya sıra değişince plan değişmez");
    let claimed = db.claim_quota_wait(&wait.id, wait.generation, deadline).unwrap().unwrap();
    assert!(db.prepare_quota_wait_schedule(&wait.id, wait.generation, &[], None).unwrap().is_none());
    assert!(db.prepare_quota_wait_schedule(&wait.id, claimed.generation, &[], None).unwrap().is_none());
    let running = db.advance_quota_wait(&wait.id, claimed.generation, State::Running, None, deadline).unwrap().unwrap();
    let new_reset = reset + Duration::hours(1);
    let progress = QuotaWaitProgress { checkpoint_seq: 2, next_engine_index: 1, reset_at: Some(new_reset), next_attempt_at: None };
    let waiting = db.advance_quota_wait(&wait.id, running.generation, State::Waiting, Some(&progress), deadline).unwrap().unwrap();
    let replanned = db.prepare_quota_wait_schedule(
        &wait.id, waiting.generation, &[EngineId::Agy, EngineId::Codex], Some(new_reset + Duration::seconds(8)),
    ).unwrap().unwrap();
    assert_eq!(replanned.fallback_order, first.fallback_order);
    assert_eq!(replanned.planned_reset_at, Some(new_reset));
    assert_eq!(replanned.reset_deadline, Some(new_reset + Duration::seconds(8)));
}

#[test]
fn scheduler_migration_preserves_existing_waits_and_events() {
    let (tmp, db, new) = fixture();
    let wait = db.create_quota_wait(&new, Utc::now()).unwrap();
    drop(db);
    // Önceki v12 şemasını kur; yeniden açılış gerçek ekleyici v13 migration'ını uygular.
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    // v14 (hafıza) nesneleri de kaldırılır: yeniden açılış v13 ve v14'ü sırayla uygular.
    conn.execute_batch(
        "DROP TABLE memory_fts; DROP TABLE memory_item_receipts; DROP TABLE memory_receipts;
         DROP TABLE memory_items; DROP TABLE memory_suggestions; DROP TABLE memory_jobs;
         DROP TABLE memory_operations; DROP TABLE memory_changes; DROP TABLE approval_shadow_scores;
         ALTER TABLE quota_waits DROP COLUMN scheduler_json; PRAGMA user_version = 12;",
    ).unwrap();
    drop(conn);
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    let restored = db.quota_wait(&wait.id).unwrap().unwrap();
    assert_eq!(restored.generation, wait.generation);
    assert_eq!(restored.state, State::Waiting);
    assert_eq!(restored.policy, ResumePolicy::Workspace);
    assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
    assert!(db.prepare_quota_wait_schedule(&wait.id, wait.generation, &[], None).unwrap().is_some());
}

#[test]
fn releasing_unstarted_claim_uses_cas_and_never_reopens_running_or_terminal() {
    let (_tmp, db, new) = fixture();
    let now = Utc::now();
    let wait = db.create_quota_wait(&new, now).unwrap();
    let claimed = db.claim_quota_wait(&wait.id, wait.generation, now).unwrap().unwrap();
    assert!(db.release_quota_wait_claim(&wait.id, wait.generation, now).unwrap().is_none());
    let waiting = db.release_quota_wait_claim(&wait.id, claimed.generation, now).unwrap().unwrap();
    assert_eq!(waiting.state, State::Waiting);
    assert_eq!(waiting.attempt_count, 0);
    assert!(waiting.generation > claimed.generation);
    assert!(db.quota_resume_context(&wait.id, claimed.generation, now).unwrap().is_none());
    let claimed = db.claim_quota_wait(&wait.id, waiting.generation, now).unwrap().unwrap();
    let running = db.advance_quota_wait(&wait.id, claimed.generation, State::Running, None, now).unwrap().unwrap();
    assert!(db.release_quota_wait_claim(&wait.id, running.generation, now).unwrap().is_none());
    let terminal = db.advance_quota_wait(&wait.id, running.generation, State::NeedsAttention, None, now).unwrap().unwrap();
    assert!(db.release_quota_wait_claim(&wait.id, terminal.generation, now).unwrap().is_none());
}

#[test]
fn wait_restart_roundtrip_preserves_goal_policy_checkpoint_and_unknown_reset() {
    let (tmp, db, new) = fixture();
    let now = Utc::now();
    let wait = db.create_quota_wait(&new, now).unwrap();
    assert_eq!(wait.state, State::Waiting);
    assert_eq!(wait.generation, 0);
    assert!(wait.reset_at.is_none());
    drop(db);
    let db = Db::open(&tmp.path().join("store.db")).unwrap();
    assert_eq!(db.recover_quota_waits(now).unwrap(), 0);
    let claimed = db.claim_quota_wait("wait", 0, now).unwrap().unwrap();
    assert_eq!(claimed.attempt_count, 1);
    let context = db
        .quota_resume_context("wait", claimed.generation, now)
        .unwrap()
        .unwrap();
    assert_eq!(context.wait.policy, ResumePolicy::Workspace);
    assert_eq!(context.user_event.payload["text"], "Özgün sentetik hedef");
    assert_eq!(context.checkpoint.seq, 2);
    assert_eq!(context.wait.source_session_id, "session");
    assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
}

#[test]
fn only_one_active_wait_per_chat_and_logical_turn_is_unique_forever() {
    let (_tmp, db, mut new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    new.id = "other".into();
    new.logical_turn_id = "other-turn".into();
    assert!(db.create_quota_wait(&new, now).is_err());
    assert_eq!(db.cancel_quota_waits("chat", now).unwrap(), 1);
    new.logical_turn_id = "turn".into();
    assert!(db.create_quota_wait(&new, now).is_err());
    new.logical_turn_id = "other-turn".into();
    assert_eq!(
        db.create_quota_wait(&new, now).unwrap().state,
        State::Waiting
    );
}

#[test]
fn concurrent_claims_on_independent_connections_have_one_winner() {
    let (tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    let a = Db::open(&tmp.path().join("store.db")).unwrap();
    let b = Db::open(&tmp.path().join("store.db")).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            a.claim_quota_wait("wait", 0, now)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            b.claim_quota_wait("wait", 0, now)
        });
        [
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
        ]
    });
    assert_eq!(outcomes.iter().filter(|r| r.is_some()).count(), 1);
    let wait = db.quota_wait("wait").unwrap().unwrap();
    assert_eq!(wait.generation, 1);
    assert_eq!(wait.attempt_count, 1);
}

#[test]
fn generations_prevent_aba_and_terminal_states_cannot_reopen() {
    let (_tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    assert!(
        db.advance_quota_wait("wait", 0, State::Running, None, now)
            .is_err()
    );
    assert_eq!(
        db.claim_quota_wait("wait", 0, now)
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    assert!(db.claim_quota_wait("wait", 0, now).unwrap().is_none());
    assert!(db.quota_resume_context("wait", 0, now).unwrap().is_none());
    let running = db
        .advance_quota_wait("wait", 1, State::Running, None, now)
        .unwrap()
        .unwrap();
    assert_eq!(running.generation, 2);
    let progress = QuotaWaitProgress {
        checkpoint_seq: 2,
        next_engine_index: 1,
        reset_at: None,
        next_attempt_at: None,
    };
    let waiting = db
        .advance_quota_wait("wait", 2, State::Waiting, Some(&progress), now)
        .unwrap()
        .unwrap();
    assert_eq!(waiting.generation, 3);
    assert_eq!(waiting.next_engine_index, 1);
    assert!(db.claim_quota_wait("wait", 0, now).unwrap().is_none());
    let claimed = db.claim_quota_wait("wait", 3, now).unwrap().unwrap();
    assert_eq!(claimed.attempt_count, 2);
    let running = db
        .advance_quota_wait("wait", claimed.generation, State::Running, None, now)
        .unwrap()
        .unwrap();
    let done = db
        .advance_quota_wait("wait", running.generation, State::Completed, None, now)
        .unwrap()
        .unwrap();
    assert_eq!(done.state, State::Completed);
    assert!(
        db.advance_quota_wait("wait", done.generation, State::Waiting, None, now)
            .is_err()
    );
    assert!(
        db.claim_quota_wait("wait", done.generation, now)
            .unwrap()
            .is_none()
    );
    assert!(db.pending_quota_waits().unwrap().is_empty());
}

#[test]
fn timer_cannot_claim_before_due_and_cancellation_invalidates_claim() {
    let (_tmp, db, mut new) = fixture();
    let now = Utc::now();
    new.next_attempt_at = Some(now + Duration::minutes(5));
    new.reset_at = Some(now + Duration::minutes(10));
    db.create_quota_wait(&new, now).unwrap();
    assert!(db.claim_quota_wait("wait", 0, now).unwrap().is_none());
    let claimed = db
        .claim_quota_wait("wait", 0, now + Duration::minutes(5))
        .unwrap()
        .unwrap();
    assert_eq!(db.cancel_quota_waits("chat", now).unwrap(), 1);
    assert_eq!(db.cancel_quota_waits("chat", now).unwrap(), 0);
    assert!(
        db.advance_quota_wait("wait", claimed.generation, State::Running, None, now)
            .unwrap()
            .is_none()
    );
    assert!(
        db.quota_resume_context("wait", claimed.generation, now)
            .unwrap()
            .is_none()
    );
    let cancelled = db.quota_wait("wait").unwrap().unwrap();
    assert_eq!(cancelled.state, State::Cancelled);
    assert!(cancelled.next_attempt_at.is_none());
}

#[test]
fn deleted_or_corrupt_goal_and_checkpoint_require_attention() {
    for sql in [
        "DELETE FROM events WHERE seq = 1",
        "UPDATE events SET payload_json = '{broken' WHERE seq = 1",
        "UPDATE events SET payload_json = '{}' WHERE seq = 1",
        "UPDATE events SET kind = 'assistant_text' WHERE seq = 1",
        "DELETE FROM events WHERE seq = 2",
        "UPDATE events SET payload_json = '{}' WHERE seq = 2",
    ] {
        let (tmp, db, new) = fixture();
        let now = Utc::now();
        db.create_quota_wait(&new, now).unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
        conn.execute_batch(sql).unwrap();
        assert!(
            db.claim_quota_wait("wait", 0, now).unwrap().is_none(),
            "{sql}"
        );
        let wait = db.quota_wait("wait").unwrap().unwrap();
        assert_eq!(wait.state, State::NeedsAttention, "{sql}");
        assert_eq!(wait.attempt_count, 0);
        assert_eq!(db.recover_quota_waits(now).unwrap(), 0);
    }
}

#[test]
fn invalid_initial_reference_never_becomes_a_new_job() {
    let (_tmp, db, mut new) = fixture();
    new.user_event_seq = 2; // quota olayı kullanıcı hedefi değildir.
    assert_eq!(
        db.create_quota_wait(&new, Utc::now()).unwrap().state,
        State::NeedsAttention
    );
    assert!(
        db.claim_quota_wait("wait", 1, Utc::now())
            .unwrap()
            .is_none()
    );
}

#[test]
fn missing_session_foreign_key_and_invalid_seq_do_not_leave_partial_waits() {
    let (_tmp, db, mut new) = fixture();
    let now = Utc::now();
    new.source_session_id = "missing".into();
    assert!(db.create_quota_wait(&new, now).is_err());
    assert!(db.pending_quota_waits().unwrap().is_empty());
    new.source_session_id = "session".into();
    new.user_event_seq = 0;
    assert!(db.create_quota_wait(&new, now).is_err());
    assert!(db.pending_quota_waits().unwrap().is_empty());
}

#[test]
fn restart_preserves_waiting_but_never_replays_ambiguous_claimed_or_running() {
    for state in [
        State::Waiting,
        State::Claimed,
        State::Running,
        State::Cancelled,
        State::Completed,
    ] {
        let (tmp, db, new) = fixture();
        let now = Utc::now();
        db.create_quota_wait(&new, now).unwrap();
        if matches!(state, State::Claimed | State::Running | State::Completed) {
            db.claim_quota_wait("wait", 0, now).unwrap();
        }
        if matches!(state, State::Running | State::Completed) {
            db.advance_quota_wait("wait", 1, State::Running, None, now)
                .unwrap();
        }
        if state == State::Completed {
            db.advance_quota_wait("wait", 2, State::Completed, None, now)
                .unwrap();
        }
        if state == State::Cancelled {
            db.cancel_quota_waits("chat", now).unwrap();
        }
        drop(db);
        let db = Db::open(&tmp.path().join("store.db")).unwrap();
        let ambiguous = matches!(state, State::Claimed | State::Running);
        assert_eq!(db.recover_quota_waits(now).unwrap(), usize::from(ambiguous));
        let wait = db.quota_wait("wait").unwrap().unwrap();
        assert_eq!(
            wait.state,
            if ambiguous {
                State::NeedsAttention
            } else {
                state
            }
        );
        assert_eq!(db.recover_quota_waits(now).unwrap(), 0);
    }
}

#[test]
fn new_user_event_cancels_wait_in_same_transaction_and_rollback_preserves_it() {
    let (tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_cancel BEFORE UPDATE ON quota_waits
        BEGIN SELECT RAISE(ABORT, 'synthetic rollback'); END;",
    )
    .unwrap();
    let next = event(
        "next-user",
        "user_text",
        json!({"type":"user_text", "text":"Yeni hedef"}),
    );
    assert!(db.append_user_event_invalidating_quota(&next).is_err());
    assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
    assert_eq!(
        db.quota_wait("wait").unwrap().unwrap().state,
        State::Waiting
    );
    conn.execute_batch("DROP TRIGGER reject_cancel;").unwrap();
    assert_eq!(
        db.append_user_event_invalidating_quota(&next).unwrap().seq,
        3
    );
    assert_eq!(
        db.quota_wait("wait").unwrap().unwrap().state,
        State::Cancelled
    );
    assert!(db.claim_quota_wait("wait", 0, now).unwrap().is_none());
}

#[test]
fn telegram_acceptance_cancellation_rolls_back_with_inbox_and_user_event() {
    let (tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_origin BEFORE INSERT ON telegram_turn_origins
        BEGIN SELECT RAISE(ABORT, 'synthetic rollback'); END;",
    )
    .unwrap();
    let ev = event(
        "next",
        "user_text",
        json!({"type":"user_text", "text":"Yeni hedef"}),
    );
    let request = TelegramTurnAcceptance {
        bot_id: 100,
        update_id: 1,
        user_id: 101,
        private_chat_id: 101,
        logical_turn_id: "next-turn",
        event: &ev,
    };
    assert!(db.accept_telegram_turn(&request).is_err());
    assert_eq!(
        db.quota_wait("wait").unwrap().unwrap().state,
        State::Waiting
    );
    assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
    assert!(db.telegram_next_update_id(100).unwrap().is_none());
    conn.execute_batch("DROP TRIGGER reject_origin;").unwrap();
    assert!(matches!(
        db.accept_telegram_turn(&request).unwrap(),
        TelegramAcceptance::Accepted(_)
    ));
    assert_eq!(
        db.quota_wait("wait").unwrap().unwrap().state,
        State::Cancelled
    );
}

#[test]
fn full_work_requires_new_local_approval_and_never_restores_old_grant() {
    let (tmp, db, mut new) = fixture();
    new.policy = ResumePolicy::from(PolicyArg::Full);
    let wait = db.create_quota_wait(&new, Utc::now()).unwrap();
    assert_eq!(wait.policy, ResumePolicy::RequiresLocalApproval);
    assert_eq!(wait.policy.as_policy_arg(), PolicyArg::Full);
    assert_eq!(wait.state, State::NeedsAttention);
    assert!(
        db.claim_quota_wait("wait", wait.generation, Utc::now())
            .unwrap()
            .is_none()
    );
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    let policy: String = conn
        .query_row("SELECT policy FROM quota_waits", [], |r| r.get(0))
        .unwrap();
    assert_eq!(policy, "requires_local_approval");
    assert!(
        conn.execute("UPDATE quota_waits SET policy = 'full'", [])
            .is_err()
    );
    assert_eq!(
        ResumePolicy::from(PolicyArg::ReadOnly).as_policy_arg(),
        PolicyArg::ReadOnly
    );
}

#[test]
fn invalid_checkpoint_progress_is_rejected_or_requires_attention() {
    let (_tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    db.claim_quota_wait("wait", 0, now).unwrap();
    db.advance_quota_wait("wait", 1, State::Running, None, now)
        .unwrap();
    let mut progress = QuotaWaitProgress {
        checkpoint_seq: 1,
        next_engine_index: 0,
        reset_at: None,
        next_attempt_at: None,
    };
    assert!(
        db.advance_quota_wait("wait", 2, State::Waiting, Some(&progress), now)
            .is_err()
    );
    assert_eq!(db.quota_wait("wait").unwrap().unwrap().generation, 2);
    progress.checkpoint_seq = 99;
    assert_eq!(
        db.advance_quota_wait("wait", 2, State::Waiting, Some(&progress), now)
            .unwrap()
            .unwrap()
            .state,
        State::NeedsAttention
    );
    assert_eq!(db.quota_wait("wait").unwrap().unwrap().checkpoint_seq, 2);
}

#[test]
fn session_and_goal_must_belong_to_same_chat_and_engine() {
    let (tmp, db, new) = fixture();
    let now = Utc::now();
    db.create_quota_wait(&new, now).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
    conn.execute(
        "UPDATE engine_sessions SET engine = 'codex' WHERE id = 'session'",
        [],
    )
    .unwrap();
    assert!(db.claim_quota_wait("wait", 0, now).unwrap().is_none());
    assert_eq!(
        db.quota_wait("wait").unwrap().unwrap().state,
        State::NeedsAttention
    );
}

#[test]
fn inbox_recovery_preserves_only_valid_durable_quota_wait_and_original_recipient() {
    use ultron_core::db::telegram::{DeliveryState, InboxState};
    for corrupt in [false, true] {
        let (tmp, db, mut new) = fixture();
        let ev = event(
            "telegram-user",
            "user_text",
            json!({"type":"user_text", "text":"Telegram hedefi"}),
        );
        let TelegramAcceptance::Accepted(stored) = db
            .accept_telegram_turn(&TelegramTurnAcceptance {
                bot_id: 100,
                update_id: 1,
                user_id: 101,
                private_chat_id: 101,
                logical_turn_id: "telegram-turn",
                event: &ev,
            })
            .unwrap()
        else {
            panic!("ilk kabul gerekli")
        };
        db.start_telegram_inbox(100, 1).unwrap();
        new.logical_turn_id = "telegram-turn".into();
        new.user_event_seq = stored.seq;
        new.checkpoint_seq = stored.seq;
        db.create_quota_wait(&new, Utc::now()).unwrap();
        if corrupt {
            let conn = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
            conn.execute("DELETE FROM events WHERE seq = ?1", [stored.seq])
                .unwrap();
        }
        drop(db);
        let db = Db::open(&tmp.path().join("store.db")).unwrap();
        assert_eq!(db.recover_telegram_inbox().unwrap(), usize::from(corrupt));
        let origin = db.telegram_turn_origin("telegram-turn").unwrap().unwrap();
        assert_eq!(
            origin.delivery_state,
            if corrupt {
                DeliveryState::NeedsAttention
            } else {
                DeliveryState::Pending
            }
        );
        assert_eq!(
            db.telegram_inbox_entry(100, 1).unwrap().unwrap().state,
            if corrupt {
                InboxState::NeedsAttention
            } else {
                InboxState::Running
            }
        );
        if !corrupt {
            let now = Utc::now();
            let claimed = db.claim_quota_wait("wait", 0, now).unwrap().unwrap();
            db.advance_quota_wait("wait", claimed.generation, State::Running, None, now)
                .unwrap();
            let final_event = event(
                "final",
                "assistant_text",
                json!({"type":"assistant_text", "text":"Bitti"}),
            );
            let final_event = db.append_event(&final_event).unwrap().unwrap();
            assert!(
                db.complete_telegram_turn("telegram-turn", final_event.seq)
                    .unwrap()
            );
            assert_eq!(
                db.telegram_turn_origin("telegram-turn")
                    .unwrap()
                    .unwrap()
                    .private_chat_id,
                101
            );
        }
    }
}
