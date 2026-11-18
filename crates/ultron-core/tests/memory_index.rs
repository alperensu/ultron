//! Hafıza indeksi: upsert/CAS, pasifleştirme, tam rebuild ve değişim cursor'ı (sentetik veri).

use chrono::{DateTime, Utc};
use ultron_core::Db;
use ultron_core::db::memory::{
    DeactivateOutcome, MemoryItemRecord, ReceiptRecord, UpsertOutcome,
};
use ultron_core::memory::{
    Audience, ClaimStatus, IndexState, MemoryChangeKind, MemoryId, MemoryType, Receipt,
    Revision, Sensitivity, Sha256Hash, SourceRef, SuggestionCandidate, SuggestionStatus,
    SuggestionTarget, Visibility, MemorySuggestion,
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

fn item(id: &MemoryId, path: &str, title: &str, body: &str, rev: u64) -> MemoryItemRecord {
    MemoryItemRecord {
        id: id.clone(),
        relative_path: path.into(),
        memory_type: MemoryType::User,
        project: None,
        visibility: Visibility::Private,
        remote_allowed: false,
        sensitivity: Sensitivity::Personal,
        title: title.into(),
        summary: format!("Özet: {title}"),
        body: body.into(),
        tags: vec!["sentetik".into()],
        revision: Revision::new(rev).unwrap(),
        content_hash: Sha256Hash::of(format!("{rev}\n{title}\n{body}").as_bytes()),
        created_at: at("2026-11-18T09:00:00Z"),
        updated_at: at("2026-11-18T09:00:00Z"),
        review_at: None,
        index_state: IndexState::Indexed,
        trashed_at: None,
        receipts: vec![receipt(&format!("{}-{rev}-{body}", id.as_str()))],
    }
}

fn suggestion(turn: &str, body: &str) -> MemorySuggestion {
    let candidate = SuggestionCandidate {
        target: SuggestionTarget::New,
        memory_type: MemoryType::User,
        title: "Aday başlığı".into(),
        summary: "Aday özeti".into(),
        body: body.into(),
        tags: vec!["sentetik".into()],
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

fn ids(db: &Db, query: &str) -> Vec<String> {
    db.memory_search(query, Audience::Desktop, None, None, None)
        .unwrap()
        .items
        .into_iter()
        .map(|h| h.item.id.as_str().to_string())
        .collect()
}

#[test]
fn upsert_is_idempotent_and_logs_one_created_change() {
    let (_d, db) = open();
    let id = MemoryId::new();
    let it = item(&id, "me/a.md", "Sentetik başlık", "gövde metni", 1);
    assert_eq!(db.memory_upsert_item(&it).unwrap(), UpsertOutcome::Inserted);
    assert_eq!(db.memory_upsert_item(&it).unwrap(), UpsertOutcome::Unchanged);
    let changes = db.memory_changes_since(Audience::Desktop, 0, None).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, MemoryChangeKind::Created);
    assert_eq!(changes[0].item_id.as_ref(), Some(&id));
}

#[test]
fn stale_revision_is_rejected_by_cas() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Yeni", "ikinci sürüm", 2)).unwrap();
    let stale = item(&id, "me/a.md", "Eski", "birinci sürüm", 1);
    assert_eq!(
        db.memory_upsert_item(&stale).unwrap(),
        UpsertOutcome::StaleRevision { current: Revision::new(2).unwrap() }
    );
    let detail = db.memory_get(&id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(detail.item.revision, Revision::new(2).unwrap());
    assert_eq!(detail.body, "ikinci sürüm");
}

#[test]
fn same_revision_with_other_hash_is_a_conflict() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Başlık", "birinci", 1)).unwrap();
    let other = item(&id, "me/a.md", "Başlık", "farklı gövde", 1);
    assert_eq!(db.memory_upsert_item(&other).unwrap(), UpsertOutcome::HashMismatch);
}

#[test]
fn update_replaces_the_fts_content() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Not", "eskikelime", 1)).unwrap();
    assert_eq!(ids(&db, "eskikelime").len(), 1);
    assert_eq!(db.memory_upsert_item(&item(&id, "me/a.md", "Not", "yenikelime", 2)).unwrap(), UpsertOutcome::Updated);
    assert!(ids(&db, "eskikelime").is_empty());
    assert_eq!(ids(&db, "yenikelime"), vec![id.as_str().to_string()]);
}

#[test]
fn deactivate_requires_current_revision_and_hides_the_item() {
    let (_d, db) = open();
    let id = MemoryId::new();
    let first = item(&id, "me/a.md", "Silinecek", "silinecekkelime", 1);
    db.memory_upsert_item(&first).unwrap();
    let rev1 = Revision::new(1).unwrap();
    let rev2 = Revision::new(2).unwrap();

    assert_eq!(
        db.memory_deactivate_item(&id, rev2, &first.content_hash).unwrap(),
        DeactivateOutcome::StaleRevision { current: rev1 }
    );
    let wrong_hash = Sha256Hash::of(b"baska");
    assert_eq!(
        db.memory_deactivate_item(&id, rev1, &wrong_hash).unwrap(),
        DeactivateOutcome::HashMismatch
    );
    assert_eq!(
        db.memory_deactivate_item(&id, rev1, &first.content_hash).unwrap(),
        DeactivateOutcome::Deactivated
    );
    assert_eq!(
        db.memory_deactivate_item(&id, rev1, &first.content_hash).unwrap(),
        DeactivateOutcome::AlreadyInactive
    );
    assert!(db.memory_get(&id, Audience::Desktop).unwrap().is_none());
    assert!(ids(&db, "silinecekkelime").is_empty());
    assert!(db.memory_list(Audience::Desktop, None).unwrap().items.is_empty());
}

#[test]
fn rebuild_restores_index_and_keeps_suggestions_and_jobs() {
    let (_d, db) = open();
    let a_id = MemoryId::new();
    let b_id = MemoryId::new();
    let a = item(&a_id, "me/a.md", "Alfa", "alfakelime", 1);
    let b = item(&b_id, "me/b.md", "Beta", "betakelime", 1);
    db.memory_upsert_item(&a).unwrap();
    db.memory_upsert_item(&b).unwrap();

    let sug = suggestion("turn-1", "aday gövdesi");
    db.memory_insert_suggestion(&sug).unwrap();
    assert!(db.memory_enqueue_job("chat-1", "turn-1", 5).unwrap());

    // Kayıp indeks: yalnız a yeniden kurulur; b indekste yoktur ama kasa kaydı (deltası) değildir.
    assert_eq!(db.memory_rebuild_index(vec![a.clone()]).unwrap(), 1);
    assert_eq!(ids(&db, "alfakelime"), vec![a_id.as_str().to_string()]);
    assert!(ids(&db, "betakelime").is_empty());

    // Öneri ve iş kuyruğu rebuild'den etkilenmez.
    let kept = db.memory_get_suggestion(&sug.id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(kept.status, SuggestionStatus::Pending);
    let job = db.memory_get_job("chat-1", "turn-1", 5).unwrap().unwrap();
    assert_eq!(job.status, ultron_core::memory::MemoryJobStatus::Queued);

    // Tam rebuild aynı kabul edilmiş notları bulur.
    assert_eq!(db.memory_rebuild_index(vec![a.clone(), b.clone()]).unwrap(), 2);
    assert_eq!(ids(&db, "alfakelime").len(), 1);
    assert_eq!(ids(&db, "betakelime").len(), 1);
}

#[test]
fn rebuild_tolerates_duplicate_delta_and_rejects_path_collisions() {
    let (_d, db) = open();
    let id = MemoryId::new();
    let a = item(&id, "me/a.md", "Not", "tekrar", 1);
    assert_eq!(db.memory_rebuild_index(vec![a.clone(), a.clone()]).unwrap(), 1);
    assert_eq!(ids(&db, "tekrar").len(), 1);

    // Farklı kimlik aynı yolu (büyük/küçük harf fark etmeden) kullanamaz.
    let other = item(&MemoryId::new(), "ME/A.md", "Başka", "baska", 1);
    assert_eq!(db.memory_upsert_item(&other).unwrap(), UpsertOutcome::PathConflict);
}

#[test]
fn get_exposes_receipts_of_the_current_revision() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Makbuzlu", "makbuzkelime", 1)).unwrap();
    let detail = db.memory_get(&id, Audience::Desktop).unwrap().unwrap();
    assert_eq!(detail.item.receipt_count, 1);
    assert_eq!(detail.receipts.len(), 1);
    assert!(detail.receipts[0].evidence_available);
    assert!(matches!(detail.receipts[0].source, SourceRef::Event { .. }));
}

#[test]
fn changes_since_pages_with_a_cursor() {
    let (_d, db) = open();
    for n in 0..3 {
        let id = MemoryId::new();
        db.memory_upsert_item(&item(&id, &format!("me/{n}.md"), "Not", "degisim", 1)).unwrap();
    }
    let first = db.memory_changes_since(Audience::Desktop, 0, Some(2)).unwrap();
    assert_eq!(first.len(), 2);
    let rest = db.memory_changes_since(Audience::Desktop, first[1].cursor, Some(10)).unwrap();
    assert_eq!(rest.len(), 1);
    assert!(rest[0].cursor > first[1].cursor);
}

fn with_vis(mut it: MemoryItemRecord, vis: Visibility, remote: bool) -> MemoryItemRecord {
    it.visibility = vis;
    it.remote_allowed = remote;
    it
}

#[test]
fn same_revision_metadata_drift_updates_row_fts_and_logs_change() {
    let (_d, db) = open();
    let id = MemoryId::new();
    let open_item = with_vis(item(&id, "me/a.md", "Sentetik", "ortakkelime", 1), Visibility::Shared, true);
    assert_eq!(db.memory_upsert_item(&open_item).unwrap(), UpsertOutcome::Inserted);
    assert_eq!(db.memory_search("ortakkelime", Audience::Telegram, None, None, None).unwrap().items.len(), 1);

    // Gorunurluk daralmasi ayni revizyon+hash ile gelse de uygulanir.
    let tight = with_vis(open_item.clone(), Visibility::Private, false);
    assert_eq!(db.memory_upsert_item(&tight).unwrap(), UpsertOutcome::Updated);
    assert!(db.memory_search("ortakkelime", Audience::Telegram, None, None, None).unwrap().items.is_empty());
    assert_eq!(db.memory_search("ortakkelime", Audience::Desktop, None, None, None).unwrap().items.len(), 1);
    assert_eq!(db.memory_upsert_item(&tight).unwrap(), UpsertOutcome::Unchanged);

    // Pasiflestirilmis ogenin ayni rev/hash ile donusu yeniden aranabilir olur.
    assert_eq!(
        db.memory_deactivate_item(&id, tight.revision, &tight.content_hash).unwrap(),
        DeactivateOutcome::Deactivated
    );
    assert!(ids(&db, "ortakkelime").is_empty());
    assert_eq!(db.memory_upsert_item(&tight).unwrap(), UpsertOutcome::Updated);
    assert_eq!(ids(&db, "ortakkelime").len(), 1);
    let kinds: Vec<_> =
        db.memory_changes_since(Audience::Desktop, 0, None).unwrap().into_iter().map(|c| c.kind).collect();
    assert_eq!(kinds.last(), Some(&MemoryChangeKind::Restored));
    assert_eq!(kinds.len(), 4);
}

#[test]
fn changes_since_is_desktop_only() {
    let (_d, db) = open();
    for aud in [Audience::Engine, Audience::Telegram] {
        assert!(db.memory_changes_since(aud, 0, None).is_err());
    }
}

#[test]
fn relative_path_conflict_is_a_typed_outcome() {
    let (_d, db) = open();
    let (a, b) = (MemoryId::new(), MemoryId::new());
    db.memory_upsert_item(&item(&a, "me/x.md", "A", "govde", 1)).unwrap();
    assert_eq!(db.memory_upsert_item(&item(&b, "ME/X.md", "B", "govde", 1)).unwrap(), UpsertOutcome::PathConflict);
    assert!(db.memory_get(&b, Audience::Desktop).unwrap().is_none());
}

#[test]
fn combining_marks_fold_in_index_and_query() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/c.md", "Kafe", "cafe\u{301}s menusu", 1)).unwrap();
    assert_eq!(ids(&db, "cafes").len(), 1);
    assert_eq!(ids(&db, "cafe\u{301}s").len(), 1);
}

#[test]
fn receipt_evidence_upgrades_from_missing_to_present() {
    let (_d, db) = open();
    let id = MemoryId::new();
    let mut it = item(&id, "me/r.md", "R", "govde", 1);
    it.receipts[0].evidence_available = false;
    db.memory_upsert_item(&it).unwrap();
    let state = |db: &Db| db.memory_get(&id, Audience::Desktop).unwrap().unwrap().receipts[0].evidence_available;
    assert!(!state(&db));
    it.receipts[0].evidence_available = true;
    db.memory_upsert_item(&it).unwrap();
    assert!(state(&db));
}

#[test]
fn rebuild_many_items_and_fts_rowids_stay_consistent() {
    let (_d, db) = open();
    let recs: Vec<_> = (0..300)
        .map(|n| item(&MemoryId::new(), &format!("me/{n}.md"), "Toplu", &format!("ortak{n}"), 1))
        .collect();
    assert_eq!(db.memory_rebuild_index(recs.clone()).unwrap(), 300);
    assert_eq!(ids(&db, "ortak7").len(), 1);
    // Guncelleme sonrasi eski metin kalmaz (rowid ile silindi).
    let mut up = recs[7].clone();
    up.revision = Revision::new(2).unwrap();
    up.body = "yenimetin".into();
    up.content_hash = Sha256Hash::of(b"yeni");
    up.receipts = vec![receipt("yeni-7")];
    db.memory_upsert_item(&up).unwrap();
    assert!(ids(&db, "ortak7").is_empty());
    assert_eq!(ids(&db, "yenimetin").len(), 1);
}
