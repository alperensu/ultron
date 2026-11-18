//! Hafıza durum depoları: öneri kararı (ilk karar kazanır), iş kiralama (lease/generation,
//! sınırlı deneme) ve işlem idempotency'si. Bütün veri sentetiktir.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ultron_core::Db;
use ultron_core::db::memory::{
    MEMORY_JOB_MAX_ATTEMPTS, OperationAdvance, OperationBegin, ReceiptRecord, SuggestionDecision,
    SuggestionDecisionRequest, SuggestionInsert,
};
use ultron_core::memory::{
    Audience, ClaimStatus, ContractError, DecisionSource, MemoryId, MemoryJobError, MemoryJobStatus,
    MemoryOperation, MemoryOperationKind, MemoryOperationState, MemorySuggestion, MemoryType,
    OperationSource, Receipt, Revision, Sha256Hash, SourceRef, SuggestionCandidate,
    SuggestionStatus, SuggestionTarget,
};

fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("memory.db")).unwrap();
    (dir, db)
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn receipt(seed: &str) -> ReceiptRecord {
    let hash = Sha256Hash::of(seed.as_bytes());
    ReceiptRecord {
        relative_path: Receipt::relative_path(&hash),
        hash,
        source: SourceRef::Event { chat_id: "chat-1".into(), seq_from: 1, seq_to: 2 },
        claim_status: ClaimStatus::UserAuthored,
        excerpt: "Sentetik alıntı".into(),
        captured_at: at("2026-11-18T09:00:00Z"),
        evidence_available: true,
    }
}

fn suggestion(turn: &str, body: &str) -> MemorySuggestion {
    let candidate = SuggestionCandidate {
        target: SuggestionTarget::New,
        memory_type: MemoryType::User,
        title: "Aday başlığı".into(),
        summary: "Aday özeti".into(),
        body: body.into(),
        tags: vec![],
        project: None,
    };
    let receipts = vec![receipt(&format!("sug-{turn}-{body}")).hash];
    let candidate_hash = candidate.candidate_hash(&receipts).unwrap();
    MemorySuggestion {
        id: MemoryId::new(),
        chat_id: "chat-1".into(),
        logical_turn_id: turn.into(),
        end_seq: 5,
        candidate,
        candidate_hash,
        receipts,
        status: SuggestionStatus::Pending,
        created_at: at("2026-11-18T09:00:00Z"),
        decided_at: None,
        decided_by: None,
    }
}

fn decision<'a>(
    s: &'a MemorySuggestion,
    to: SuggestionStatus,
    by: DecisionSource,
    request_id: &'a str,
    request_hash: &'a Sha256Hash,
) -> SuggestionDecisionRequest<'a> {
    SuggestionDecisionRequest {
        id: &s.id,
        to,
        by,
        request_id,
        request_hash,
        candidate_hash: &s.candidate_hash,
        accepted_item_id: (to == SuggestionStatus::Accepted).then_some(&s.id),
    }
}

#[test]
fn suggestion_insert_is_deduplicated_per_turn_and_candidate() {
    let (_d, db) = open();
    let first = suggestion("turn-1", "aynı aday");
    assert_eq!(db.memory_insert_suggestion(&first).unwrap(), SuggestionInsert::Inserted(first.id.clone()));

    // Aynı tur ve aynı aday: yeni satır yok, mevcut kimlik döner.
    let twin = suggestion("turn-1", "aynı aday");
    assert_eq!(db.memory_insert_suggestion(&twin).unwrap(), SuggestionInsert::Existing(first.id.clone()));

    // Aynı aday başka turda ayrı bir öneridir.
    let other_turn = suggestion("turn-2", "aynı aday");
    assert_eq!(
        db.memory_insert_suggestion(&other_turn).unwrap(),
        SuggestionInsert::Inserted(other_turn.id.clone())
    );

    let stored = db.memory_get_suggestion(&first.id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(stored.status, SuggestionStatus::Pending);
    assert_eq!(stored.candidate, first.candidate);
}

#[test]
fn first_decision_wins_and_same_request_replays() {
    let (_d, db) = open();
    let s = suggestion("turn-1", "karar");
    db.memory_insert_suggestion(&s).unwrap();
    let h1 = Sha256Hash::of(b"istek-1");
    let h2 = Sha256Hash::of(b"istek-2");

    let accept = decision(&s, SuggestionStatus::Accepted, DecisionSource::Desktop, "req-1", &h1);
    assert_eq!(db.memory_resolve_suggestion(&accept, Audience::Desktop).unwrap(), SuggestionDecision::Decided(SuggestionStatus::Accepted));
    // Ağ tekrarı / çift tık: aynı sonuç.
    assert_eq!(db.memory_resolve_suggestion(&accept, Audience::Desktop).unwrap(), SuggestionDecision::Replayed(SuggestionStatus::Accepted));
    // Çift onay farklı istekle: ilk karar kalır.
    let reject = decision(&s, SuggestionStatus::Rejected, DecisionSource::Telegram, "req-2", &h2);
    assert_eq!(
        db.memory_resolve_suggestion(&reject, Audience::Desktop).unwrap(),
        SuggestionDecision::AlreadyDecided(SuggestionStatus::Accepted)
    );

    let stored = db.memory_get_suggestion(&s.id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(stored.status, SuggestionStatus::Accepted);
    assert_eq!(stored.decided_by, Some(DecisionSource::Desktop));
    assert!(stored.decided_at.is_some());
}

#[test]
fn same_request_id_with_different_hash_is_a_request_conflict() {
    let (_d, db) = open();
    let s = suggestion("turn-1", "çakışma");
    db.memory_insert_suggestion(&s).unwrap();
    let h1 = Sha256Hash::of(b"istek-a");
    let h2 = Sha256Hash::of(b"istek-b");
    db.memory_resolve_suggestion(&decision(&s, SuggestionStatus::Accepted, DecisionSource::Desktop, "req-x", &h1), Audience::Desktop)
        .unwrap();
    let err = db
        .memory_resolve_suggestion(&decision(&s, SuggestionStatus::Accepted, DecisionSource::Desktop, "req-x", &h2), Audience::Desktop)
        .unwrap_err();
    assert_eq!(err.downcast_ref::<ContractError>(), Some(&ContractError::RequestConflict));
}

#[test]
fn decision_with_a_different_candidate_hash_is_refused_and_leaves_pending() {
    let (_d, db) = open();
    let s = suggestion("turn-1", "eski aday");
    db.memory_insert_suggestion(&s).unwrap();
    let other = suggestion("turn-9", "baska aday");
    let h = Sha256Hash::of(b"istek");
    let req = SuggestionDecisionRequest {
        id: &s.id,
        to: SuggestionStatus::Accepted,
        by: DecisionSource::Desktop,
        request_id: "req-hash",
        request_hash: &h,
        candidate_hash: &other.candidate_hash,
        accepted_item_id: Some(&s.id),
    };
    let err = db.memory_resolve_suggestion(&req, Audience::Desktop).unwrap_err();
    assert_eq!(err.downcast_ref::<ContractError>(), Some(&ContractError::InvalidHash));
    assert_eq!(db.memory_get_suggestion(&s.id, Audience::Desktop).unwrap().unwrap().status, SuggestionStatus::Pending);

    let missing = MemoryId::new();
    let req = SuggestionDecisionRequest { id: &missing, ..req };
    assert_eq!(db.memory_resolve_suggestion(&req, Audience::Desktop).unwrap(), SuggestionDecision::NotFound);
}

#[test]
fn enqueue_is_unique_per_turn_and_end_seq() {
    let (_d, db) = open();
    assert!(db.memory_enqueue_job("chat-1", "turn-1", 5).unwrap());
    assert!(!db.memory_enqueue_job("chat-1", "turn-1", 5).unwrap());
    assert!(db.memory_enqueue_job("chat-1", "turn-1", 6).unwrap());
    let job = db.memory_get_job("chat-1", "turn-1", 5).unwrap().unwrap();
    assert_eq!(job.status, MemoryJobStatus::Queued);
    assert_eq!(job.attempt, 0);
}

#[test]
fn concurrent_claimers_never_receive_the_same_job() {
    let (_d, db) = open();
    let db = Arc::new(db);
    for i in 0..20 {
        assert!(db.memory_enqueue_job("chat-1", &format!("turn-{i}"), 1).unwrap());
    }
    let now = at("2026-11-18T09:00:00Z");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Some(claim) = db.memory_claim_job(now, Duration::seconds(60)).unwrap() {
                got.push(claim.id);
            }
            got
        }));
    }
    let mut all: Vec<i64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    all.sort_unstable();
    let unique = all.len();
    all.dedup();
    assert_eq!(unique, 20);
    assert_eq!(all.len(), 20, "bir iş iki kez kiralandı");
}

#[test]
fn second_claimer_is_refused_while_lease_is_live() {
    let (_d, db) = open();
    db.memory_enqueue_job("chat-1", "turn-1", 1).unwrap();
    let now = at("2026-11-18T09:00:00Z");
    let first = db.memory_claim_job(now, Duration::seconds(60)).unwrap().unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.attempt, 1);
    assert!(db.memory_claim_job(now, Duration::seconds(60)).unwrap().is_none());
}

#[test]
fn expired_lease_is_reclaimed_and_old_generation_is_fenced() {
    let (_d, db) = open();
    db.memory_enqueue_job("chat-1", "turn-1", 1).unwrap();
    let t0 = at("2026-11-18T09:00:00Z");
    let old = db.memory_claim_job(t0, Duration::seconds(10)).unwrap().unwrap();

    let later = t0 + Duration::seconds(20);
    let new = db.memory_claim_job(later, Duration::seconds(10)).unwrap().unwrap();
    assert_eq!(new.id, old.id);
    assert_eq!(new.generation, 2);
    assert_eq!(new.attempt, 2);

    // Eski sahip artık tamamlayamaz ve başarısızlık bildiremez.
    assert!(!db.memory_complete_job(old.id, old.generation).unwrap());
    assert!(db.memory_fail_job(old.id, old.generation, MemoryJobError::Timeout).unwrap().is_none());
    // Yeni sahip tamamlayabilir.
    assert!(db.memory_complete_job(new.id, new.generation).unwrap());
    let job = db.memory_get_job("chat-1", "turn-1", 1).unwrap().unwrap();
    assert_eq!(job.status, MemoryJobStatus::Done);
    assert!(job.lease_until.is_none());
}

#[test]
fn expired_leases_are_bounded_by_max_attempts() {
    let (_d, db) = open();
    db.memory_enqueue_job("chat-1", "turn-1", 1).unwrap();
    let t0 = at("2026-11-18T09:00:00Z");
    let mut last = None;
    for step in 0..MEMORY_JOB_MAX_ATTEMPTS {
        let claim = db
            .memory_claim_job(t0 + Duration::seconds(i64::from(step) * 20), Duration::seconds(10))
            .unwrap()
            .expect("deneme hakkı varken iş alınabilmeli");
        assert_eq!(claim.attempt, step + 1);
        last = Some(claim);
    }
    assert!(last.is_some());
    // Son deneme de süresi dolunca iş başarısız olur ve yeniden kiralanmaz.
    let exhausted = db
        .memory_claim_job(t0 + Duration::seconds(200), Duration::seconds(10))
        .unwrap();
    assert!(exhausted.is_none());
    let job = db.memory_get_job("chat-1", "turn-1", 1).unwrap().unwrap();
    assert_eq!(job.status, MemoryJobStatus::Failed);
    assert_eq!(job.error, Some(MemoryJobError::Timeout));
}

#[test]
fn failures_requeue_until_the_attempt_budget_is_spent() {
    let (_d, db) = open();
    db.memory_enqueue_job("chat-1", "turn-1", 1).unwrap();
    let now = at("2026-11-18T09:00:00Z");
    let mut status = None;
    for _ in 0..MEMORY_JOB_MAX_ATTEMPTS {
        let claim = db.memory_claim_job(now, Duration::seconds(60)).unwrap().unwrap();
        status = db.memory_fail_job(claim.id, claim.generation, MemoryJobError::EngineUnavailable).unwrap();
    }
    assert_eq!(status, Some(MemoryJobStatus::Failed));
    assert!(db.memory_claim_job(now, Duration::seconds(60)).unwrap().is_none());
    let job = db.memory_get_job("chat-1", "turn-1", 1).unwrap().unwrap();
    assert_eq!(job.error, Some(MemoryJobError::EngineUnavailable));
}

fn operation(request_id: &str, body: &str) -> MemoryOperation {
    MemoryOperation {
        request_id: request_id.into(),
        request_hash: Sha256Hash::of(body.as_bytes()),
        source: OperationSource::Desktop,
        kind: MemoryOperationKind::Capture,
        target_id: None,
        expected_revision: None,
        state: MemoryOperationState::Pending,
        result_revision: None,
        content_hash: None,
        receipt_hash: None,
    }
}

#[test]
fn operation_replays_same_request_and_conflicts_on_different_payload() {
    let (_d, db) = open();
    let op = operation("op-1", "yük-a");
    assert_eq!(db.memory_begin_operation(&op).unwrap(), OperationBegin::Started);
    match db.memory_begin_operation(&op).unwrap() {
        OperationBegin::Replay(existing) => assert_eq!(existing.request_hash, op.request_hash),
        other => panic!("beklenen replay, gelen: {other:?}"),
    }
    let other = operation("op-1", "yük-b");
    let err = db.memory_begin_operation(&other).unwrap_err();
    assert_eq!(err.downcast_ref::<ContractError>(), Some(&ContractError::RequestConflict));
}

#[test]
fn operation_recovery_lists_uncompleted_work_and_rejects_invalid_transitions() {
    let (_d, db) = open();
    db.memory_begin_operation(&operation("op-2", "yük")).unwrap();
    let content = Sha256Hash::of(b"icerik");
    let rev = Revision::new(1).unwrap();

    // Dosya commit oldu, DB'ye yazılmadan çökme simülasyonu: recovery bu işlemi görür.
    assert!(db
        .memory_advance_operation(&OperationAdvance {
            request_id: "op-2",
            to: MemoryOperationState::FileCommitted,
            result_revision: Some(rev),
            content_hash: Some(&content),
            receipt_hash: None,
            error_code: None,
        })
        .unwrap());
    let pending = db.memory_pending_operations().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, MemoryOperationState::FileCommitted);
    assert_eq!(pending[0].content_hash, Some(content.clone()));

    // Uzlaştırma tamamlar; tamamlanmış işlem tekrar açılamaz ve listede kalmaz.
    let done = OperationAdvance {
        request_id: "op-2",
        to: MemoryOperationState::Completed,
        result_revision: None,
        content_hash: None,
        receipt_hash: None,
        error_code: None,
    };
    assert!(db.memory_advance_operation(&done).unwrap());
    assert!(!db.memory_advance_operation(&OperationAdvance { to: MemoryOperationState::Failed, ..done }).unwrap());
    assert!(db.memory_pending_operations().unwrap().is_empty());
}

#[test]
fn operation_unknown_request_does_not_advance() {
    let (_d, db) = open();
    let adv = OperationAdvance {
        request_id: "yok",
        to: MemoryOperationState::Completed,
        result_revision: None,
        content_hash: None,
        receipt_hash: None,
        error_code: None,
    };
    assert!(!db.memory_advance_operation(&adv).unwrap());
}

#[test]
fn suggestion_decision_is_recorded_with_its_source() {
    let (_d, db) = open();
    let s = suggestion("turn-3", "kaynak");
    db.memory_insert_suggestion(&s).unwrap();
    let h = Sha256Hash::of(b"telegram-istek");
    db.memory_resolve_suggestion(&decision(&s, SuggestionStatus::Rejected, DecisionSource::Telegram, "req-t", &h), Audience::Desktop)
        .unwrap();
    let stored = db.memory_get_suggestion(&s.id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(stored.decided_by, Some(DecisionSource::Telegram));
}

#[test]
fn engine_cannot_touch_suggestions_and_decision_consistency_is_checked() {
    let (_d, db) = open();
    let s = suggestion("turn-aud", "gizli aday");
    db.memory_insert_suggestion(&s).unwrap();
    assert!(db.memory_get_suggestion(&s.id, Audience::Engine).is_err());
    assert!(db.memory_get_suggestion(&s.id, Audience::Telegram).unwrap().is_some());
    let h = Sha256Hash::of(b"i");
    let ok = decision(&s, SuggestionStatus::Rejected, DecisionSource::Desktop, "req-e", &h);
    assert!(db.memory_resolve_suggestion(&ok, Audience::Engine).is_err());
    let bad1 = SuggestionDecisionRequest { accepted_item_id: Some(&s.id), ..ok };
    assert!(db.memory_resolve_suggestion(&bad1, Audience::Desktop).is_err());
    let bad2 = SuggestionDecisionRequest { to: SuggestionStatus::Accepted, ..ok };
    assert!(db.memory_resolve_suggestion(&bad2, Audience::Desktop).is_err());
    assert_eq!(
        db.memory_get_suggestion(&s.id, Audience::Desktop).unwrap().unwrap().status,
        SuggestionStatus::Pending
    );
}

#[test]
fn completed_to_completed_is_distinguishable() {
    use ultron_core::db::memory::AdvanceOutcome;
    let (_d, db) = open();
    let op = operation("req-done", "g");
    db.memory_begin_operation(&op).unwrap();
    let adv = |to| OperationAdvance {
        request_id: "req-done",
        to,
        result_revision: None,
        content_hash: None,
        receipt_hash: None,
        error_code: None,
    };
    assert_eq!(
        db.memory_advance_operation_outcome(&adv(MemoryOperationState::Completed)).unwrap(),
        AdvanceOutcome::Advanced
    );
    assert_eq!(
        db.memory_advance_operation_outcome(&adv(MemoryOperationState::Completed)).unwrap(),
        AdvanceOutcome::AlreadyAtState
    );
    assert_eq!(
        db.memory_advance_operation_outcome(&adv(MemoryOperationState::Pending)).unwrap(),
        AdvanceOutcome::Rejected
    );
    assert!(!db.memory_advance_operation(&adv(MemoryOperationState::Completed)).unwrap());
}
