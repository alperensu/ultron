//! Aşama 5a Task 1: v14 hafıza şeması (v13'ten ekleyici yükseltme, rollback, kısıtlar).

use std::path::Path;

use rusqlite::{Connection, params};
use ultron_core::Db;

const V14_TABLES: [&str; 8] = [
    "memory_items",
    "memory_receipts",
    "memory_item_receipts",
    "memory_suggestions",
    "memory_jobs",
    "memory_operations",
    "memory_changes",
    "approval_shadow_scores",
];

const OLD_TABLES: [&str; 14] = [
    "projects", "chats", "engine_sessions", "events", "messages_fts", "work_state", "import_state",
    "approvals", "approval_outbox", "project_notes", "quota_waits", "telegram_bindings", "telegram_offsets", "telegram_chat_bindings",
];

fn conn(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    c
}

fn version(c: &Connection) -> i64 {
    c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap()
}

fn table_exists(c: &Connection, name: &str) -> bool {
    c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [name], |r| r.get::<_, i64>(0)).unwrap() > 0
}

fn schema_of(c: &Connection, name: &str) -> Option<String> {
    c.query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [name], |r| r.get(0)).ok()
}

/// Tam şemalı yeni bir DB'den v14 nesnelerini kaldırıp `user_version = 13` yaparak v13 durumunu kurar.
fn make_v13(path: &Path) {
    drop(Db::open(path).unwrap());
    let c = conn(path);
    c.execute_batch(
        "DROP TABLE memory_fts; DROP TABLE memory_item_receipts; DROP TABLE memory_receipts;
         DROP TABLE memory_items; DROP TABLE memory_suggestions; DROP TABLE memory_jobs;
         DROP TABLE memory_operations; DROP TABLE memory_changes; DROP TABLE approval_shadow_scores;
         PRAGMA user_version = 13;",
    )
    .unwrap();
    assert_eq!(version(&c), 13);
}

fn seed_legacy(path: &Path) {
    let c = conn(path);
    c.execute("INSERT INTO chats(id, title, created_at) VALUES ('eski-sohbet', 'Türkçe başlık', '2026-01-01T00:00:00Z')", []).unwrap();
    c.execute(
        "INSERT INTO events(id, chat_id, seq, ts, kind, payload_json)
         VALUES ('e1', 'eski-sohbet', 1, '2026-01-01T00:00:00Z', 'user_text', '{\"type\":\"user_text\",\"text\":\"merhaba dünya\"}')",
        [],
    )
    .unwrap();
    c.execute("INSERT INTO messages_fts(chat_id, event_id, text) VALUES ('eski-sohbet', 'e1', 'merhaba dünya')", []).unwrap();
    c.execute(
        "INSERT INTO approvals(id, chat_id, engine, category, reason, action_json, created_at)
         VALUES ('onay-1', 'eski-sohbet', 'claude', '\"SystemConfig\"', 'sentetik', '{}', '2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
}

fn legacy_snapshot(path: &Path) -> Vec<(String, Option<String>)> {
    let c = conn(path);
    OLD_TABLES.iter().filter(|t| table_exists(&c, t)).map(|t| ((*t).to_string(), schema_of(&c, t))).collect()
}

#[test]
fn fresh_database_gets_all_v14_tables_and_reopens_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state").join("fresh.db");
    drop(Db::open(&path).unwrap());
    let c = conn(&path);
    assert_eq!(version(&c), 14);
    for table in V14_TABLES.iter().chain(["memory_fts"].iter()) {
        assert!(table_exists(&c, table), "{table}");
    }
    let fts = schema_of(&c, "memory_fts").unwrap().to_lowercase();
    assert!(fts.contains("fts5") && fts.contains("unicode61 remove_diacritics 2") && fts.contains("item_id unindexed"), "{fts}");
    drop(c);
    drop(Db::open(&path).unwrap());
    drop(Db::open(&path).unwrap());
    assert_eq!(version(&conn(&path)), 14);
}

#[test]
fn v13_database_upgrades_additively_without_touching_old_data() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v13.db");
    make_v13(&path);
    seed_legacy(&path);
    let before = legacy_snapshot(&path);
    {
        let c = conn(&path);
        for table in V14_TABLES {
            assert!(!table_exists(&c, table), "{table} v13'te olmamalı");
        }
    }
    drop(Db::open(&path).unwrap());
    let c = conn(&path);
    assert_eq!(version(&c), 14);
    assert_eq!(legacy_snapshot(&path), before, "eski tabloların şeması değişmemeli");
    for table in V14_TABLES {
        assert!(table_exists(&c, table), "{table}");
    }
    assert_eq!(c.query_row("SELECT title FROM chats WHERE id = 'eski-sohbet'", [], |r| r.get::<_, String>(0)).unwrap(), "Türkçe başlık");
    assert_eq!(
        c.query_row("SELECT payload_json FROM events WHERE id = 'e1'", [], |r| r.get::<_, String>(0)).unwrap(),
        "{\"type\":\"user_text\",\"text\":\"merhaba dünya\"}"
    );
    assert_eq!(c.query_row("SELECT status FROM approvals WHERE id = 'onay-1'", [], |r| r.get::<_, String>(0)).unwrap(), "pending");
    // messages_fts kayıtları ve araması aynı.
    assert_eq!(
        c.query_row("SELECT event_id FROM messages_fts WHERE messages_fts MATCH 'dunya'", [], |r| r.get::<_, String>(0)).unwrap(),
        "e1"
    );
    // Yeni tablolar boş.
    for table in V14_TABLES {
        let n: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "{table}");
    }
}

#[test]
fn failed_v14_migration_rolls_back_completely_and_can_be_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollback.db");
    make_v13(&path);
    seed_legacy(&path);
    // v14'ün en son oluşturduğu tablonun adıyla çakışan bir tablo: migrasyon sonda patlar.
    conn(&path).execute_batch("CREATE TABLE approval_shadow_scores (cakisma TEXT);").unwrap();
    let before = legacy_snapshot(&path);

    assert!(Db::open(&path).is_err(), "çakışan tablo migrasyonu başarısız etmeli");
    let c = conn(&path);
    assert_eq!(version(&c), 13, "başarısız migrasyon sürümü ilerletmemeli");
    for table in V14_TABLES.iter().filter(|t| **t != "approval_shadow_scores").chain(["memory_fts"].iter()) {
        assert!(!table_exists(&c, table), "{table} geri alınmalı");
    }
    assert_eq!(legacy_snapshot(&path), before);
    assert_eq!(c.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    // Engel kalkınca aynı DB yükseltilir.
    c.execute_batch("DROP TABLE approval_shadow_scores;").unwrap();
    drop(c);
    drop(Db::open(&path).unwrap());
    assert_eq!(version(&conn(&path)), 14);
}

fn fresh(tmp: &tempfile::TempDir) -> Connection {
    let path = tmp.path().join("kisit.db");
    drop(Db::open(&path).unwrap());
    conn(&path)
}

const HASH_A: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
const HASH_B: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

fn insert_item(c: &Connection, id: &str, path: &str, visibility: &str) -> rusqlite::Result<usize> {
    c.execute(
        "INSERT INTO memory_items(id, relative_path, memory_type, visibility, title, revision, content_hash, created_at, updated_at)
         VALUES (?1, ?2, 'user', ?3, 'Başlık', 1, ?4, '2026-11-18T09:00:00Z', '2026-11-18T09:00:00Z')",
        params![id, path, visibility, HASH_A],
    )
}

#[test]
fn item_defaults_are_private_and_remote_denied_and_constraints_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    insert_item(&c, "i1", "inbox/i1.md", "private").unwrap();
    let (vis, remote, sens, state): (String, i64, String, String) = c
        .query_row("SELECT visibility, remote_allowed, sensitivity, index_state FROM memory_items WHERE id = 'i1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    assert_eq!((vis.as_str(), remote, sens.as_str(), state.as_str()), ("private", 0, "personal", "indexed"));
    assert!(insert_item(&c, "i2", "inbox/i1.md", "private").is_err(), "relative_path benzersiz");
    assert!(insert_item(&c, "i3", "inbox/i3.md", "public").is_err(), "görünürlük kümesi kapalı");
    assert!(c.execute("UPDATE memory_items SET revision = 0 WHERE id = 'i1'", []).is_err());
    assert!(c.execute("UPDATE memory_items SET content_hash = 'md5:x' WHERE id = 'i1'", []).is_err());
    assert!(c.execute("UPDATE memory_items SET memory_type = 'banana' WHERE id = 'i1'", []).is_err());
    assert!(c.execute("UPDATE memory_items SET remote_allowed = 2 WHERE id = 'i1'", []).is_err());
    assert!(c.execute("UPDATE memory_items SET index_state = 'weird' WHERE id = 'i1'", []).is_err());
}

#[test]
fn receipts_require_real_event_ranges_and_item_links_are_unique() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let ins = |hash: &str, kind: &str, locator: Option<&str>, chat: Option<&str>, from: Option<i64>, to: Option<i64>| {
        c.execute(
            "INSERT INTO memory_receipts(hash, relative_path, source_kind, source_locator, chat_id, seq_from, seq_to, claim_status, captured_at)
             VALUES (?1, 'receipts/x.json', ?2, ?3, ?4, ?5, ?6, 'claimed', '2026-11-18T09:00:00Z')",
            params![hash, kind, locator, chat, from, to],
        )
    };
    ins(HASH_A, "event", None, Some("sohbet"), Some(1), Some(2)).unwrap();
    assert!(ins(HASH_B, "event", None, None, None, None).is_err(), "olay makbuzu sohbet/seq ister");
    assert!(ins(HASH_B, "event", None, Some("sohbet"), Some(3), Some(2)).is_err(), "seq_to >= seq_from");
    assert!(ins(HASH_B, "event", None, Some("sohbet"), Some(0), Some(2)).is_err());
    assert!(ins(HASH_B, "file", None, None, None, None).is_err(), "dosya makbuzu locator ister");
    ins(HASH_B, "file", Some("receipts/sources/a.md"), None, None, None).unwrap();
    assert!(ins("sha256:kisa", "file", Some("a"), None, None, None).is_err());
    assert!(ins(&HASH_A.replace("sha256:", "sha1:  "), "file", Some("a"), None, None, None).is_err());

    insert_item(&c, "i1", "inbox/i1.md", "private").unwrap();
    c.execute("INSERT INTO memory_item_receipts(item_id, revision, receipt_hash) VALUES ('i1', 1, ?1)", [HASH_A]).unwrap();
    assert!(c.execute("INSERT INTO memory_item_receipts(item_id, revision, receipt_hash) VALUES ('i1', 1, ?1)", [HASH_A]).is_err());
    assert!(c.execute("INSERT INTO memory_item_receipts(item_id, revision, receipt_hash) VALUES ('i1', 2, 'sha256:yok')", []).is_err(), "makbuz FK");
    assert!(c.execute("INSERT INTO memory_item_receipts(item_id, revision, receipt_hash) VALUES ('yok', 1, ?1)", [HASH_B]).is_err(), "öğe FK");
    // Öğe silinince bağlantılar düşer, makbuzlar kalır.
    c.execute("DELETE FROM memory_items WHERE id = 'i1'", []).unwrap();
    assert_eq!(c.query_row("SELECT COUNT(*) FROM memory_item_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(c.query_row("SELECT COUNT(*) FROM memory_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
}

#[test]
fn suggestions_jobs_operations_and_changes_enforce_state_machines() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let sug = |id: &str, hash: &str| {
        c.execute(
            "INSERT INTO memory_suggestions(id, chat_id, logical_turn_id, end_seq, candidate_json, candidate_hash, dedupe_key, created_at)
             VALUES (?1, 'sohbet', 'tur', 3, '{}', ?2, 'k', '2026-11-18T09:00:00Z')",
            params![id, hash],
        )
    };
    sug("s1", HASH_A).unwrap();
    assert!(sug("s2", HASH_A).is_err(), "aynı tur+aday tek satır");
    sug("s3", HASH_B).unwrap();
    // Terminal karar zaman ve kaynak ister; bekleyen karar alanı taşıyamaz.
    assert!(c.execute("UPDATE memory_suggestions SET status = 'accepted' WHERE id = 's1'", []).is_err());
    assert!(c.execute("UPDATE memory_suggestions SET decided_by = 'desktop' WHERE id = 's1'", []).is_err());
    assert!(c.execute("UPDATE memory_suggestions SET status = 'maybe' WHERE id = 's1'", []).is_err());
    assert!(c.execute("UPDATE memory_suggestions SET status = 'accepted', decided_at = 'x', decided_by = 'robot' WHERE id = 's1'", []).is_err());
    c.execute("UPDATE memory_suggestions SET status = 'accepted', decided_at = '2026-11-18T09:01:00Z', decided_by = 'telegram' WHERE id = 's1'", []).unwrap();

    let job = |chat: &str, end: i64| {
        c.execute(
            "INSERT INTO memory_jobs(chat_id, logical_turn_id, end_seq, created_at, updated_at) VALUES (?1, 't', ?2, 'a', 'a')",
            params![chat, end],
        )
    };
    job("c", 1).unwrap();
    assert!(job("c", 1).is_err(), "(chat, tur, end_seq) benzersiz");
    job("c", 2).unwrap();
    assert!(c.execute("UPDATE memory_jobs SET status = 'zombie' WHERE chat_id = 'c'", []).is_err());
    assert!(c.execute("UPDATE memory_jobs SET error_code = 'ham sağlayıcı metni' WHERE chat_id = 'c'", []).is_err(), "kapalı hata kodu kümesi");
    c.execute("UPDATE memory_jobs SET status = 'failed', error_code = 'quota' WHERE end_seq = 1", []).unwrap();

    let op = |req: &str| {
        c.execute(
            "INSERT INTO memory_operations(request_id, request_hash, source, kind, created_at, updated_at)
             VALUES (?1, ?2, 'desktop', 'capture', 'a', 'a')",
            params![req, HASH_A],
        )
    };
    op("r1").unwrap();
    assert!(op("r1").is_err(), "request_id idempotency anahtarı benzersiz");
    assert!(c.execute("UPDATE memory_operations SET state = 'half' WHERE request_id = 'r1'", []).is_err());
    c.execute("UPDATE memory_operations SET state = 'file_committed' WHERE request_id = 'r1'", []).unwrap();

    c.execute("INSERT INTO memory_changes(item_id, revision, kind, created_at) VALUES ('i', 1, 'created', 'a')", []).unwrap();
    c.execute("INSERT INTO memory_changes(item_id, revision, kind, created_at) VALUES ('i', 2, 'external_change', 'a')", []).unwrap();
    assert!(c.execute("INSERT INTO memory_changes(kind, created_at) VALUES ('rm_rf', 'a')", []).is_err());
    let cursors: Vec<i64> = c.prepare("SELECT cursor FROM memory_changes ORDER BY cursor").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(cursors.len(), 2);
    assert!(cursors[0] < cursors[1], "cursor artan");
}

#[test]
fn shadow_scores_are_bounded_unique_per_approval_and_carry_no_decision() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    c.execute(
        "INSERT INTO approvals(id, chat_id, engine, category, reason, action_json, created_at)
         VALUES ('onay-1', NULL, 'claude', '\"SystemConfig\"', 'sentetik', '{}', '2026-11-18T09:00:00Z')",
        [],
    )
    .unwrap();
    let score = |approval: &str, p: f64| {
        c.execute(
            "INSERT INTO approval_shadow_scores(approval_id, model, prompt_version, p_safe, measured_at, duration_ms, action_fingerprint)
             VALUES (?1, 'jeb:4b', 'shadow-b-v1', ?2, '2026-11-18T09:00:01Z', 12, ?3)",
            params![approval, p, HASH_A],
        )
    };
    score("onay-1", 0.5).unwrap();
    assert!(score("onay-1", 0.6).is_err(), "onay başına tek puan");
    assert!(score("yok", 0.5).is_err(), "FK: mevcut onay");
    c.execute("DELETE FROM approval_shadow_scores", []).unwrap();
    for bad in [-0.1, 1.1] {
        assert!(score("onay-1", bad).is_err(), "{bad}");
    }
    score("onay-1", 0.0).unwrap();
    // Karar/eşik sütunu yoktur.
    let columns: Vec<String> = c.prepare("PRAGMA table_info(approval_shadow_scores)").unwrap()
        .query_map([], |r| r.get::<_, String>(1)).unwrap().collect::<Result<_, _>>().unwrap();
    for forbidden in ["decision", "approve", "approved", "threshold", "status", "resolved_by"] {
        assert!(!columns.iter().any(|name| name == forbidden), "{forbidden}");
    }
    // Onay silinirse puan da silinir (kalıcı onay satırı kaybolursa yetim kalmaz).
    c.execute("DELETE FROM approvals WHERE id = 'onay-1'", []).unwrap();
    assert_eq!(c.query_row("SELECT COUNT(*) FROM approval_shadow_scores", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn memory_fts_matches_turkish_text_without_touching_messages_fts() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let messages_before = schema_of(&c, "messages_fts");
    c.execute(
        "INSERT INTO memory_fts(item_id, title, summary, body, tags) VALUES ('i1', 'Çalışma düzeni', 'Şirket İşleri', 'Ağaç dalında ığdır', 'türkçe')",
        [],
    )
    .unwrap();
    let find = |q: &str| -> Vec<String> {
        c.prepare("SELECT item_id FROM memory_fts WHERE memory_fts MATCH ?1 ORDER BY bm25(memory_fts)").unwrap()
            .query_map([q], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(find("\"calısma\""), vec!["i1"], "aksan duyarsız (ç -> c); noktasız ı korunur");
    // Bilinen davranış: unicode61 `ı` (noktasız i) harfini `i`ye katlamaz; Türkçe i/ı eşlemesi
    // sorgu tarafında Task 3'te ele alınmalıdır. Şema düzeyinde burada sabitlenir.
    assert!(find("\"calisma\"").is_empty());
    assert_eq!(find("\"isleri\""), vec!["i1"], "İ -> i");
    assert_eq!(find("\"sirket\""), vec!["i1"], "Ş -> s");
    assert_eq!(find("\"agac\""), vec!["i1"]);
    assert_eq!(find("\"turkce\""), vec!["i1"]);
    assert!(find("\"yokkelime\"").is_empty());
    // item_id dizinlenmez.
    assert!(find("\"i1\"").is_empty());
    assert_eq!(schema_of(&c, "messages_fts"), messages_before);
    assert!(c.query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get::<_, i64>(0)).unwrap() == 0);
}

// ---------------------------------------------------------------------------------------------
// Düzeltme turu: şema sertleştirmesi
// ---------------------------------------------------------------------------------------------

#[test]
fn hash_checks_are_case_sensitive_and_primary_keys_reject_null() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let upper = "SHA256:0000000000000000000000000000000000000000000000000000000000000001";
    let upper_hex = "sha256:000000000000000000000000000000000000000000000000000000000000000A";
    let ins = |hash: &str| {
        c.execute(
            "INSERT INTO memory_receipts(hash, relative_path, source_kind, source_locator, claim_status, captured_at)
             VALUES (?1, 'receipts/x.json', 'file', 'a.md', 'claimed', 'a')",
            [hash],
        )
    };
    assert!(ins(upper).is_err(), "büyük harfli önek reddedilmeli");
    assert!(ins(upper_hex).is_err(), "büyük harfli hex reddedilmeli");
    assert!(ins("sha256:000000000000000000000000000000000000000000000000000000000000000g").is_err());
    ins(HASH_A).unwrap();
    // NULL birincil anahtar (TEXT PK) reddedilir.
    assert!(c.execute("INSERT INTO memory_receipts(hash, relative_path, source_kind, source_locator, claim_status, captured_at)
        VALUES (NULL, 'r', 'file', 'a', 'claimed', 'a')", []).is_err());
    assert!(c.execute("INSERT INTO memory_items(id, relative_path, memory_type, title, revision, content_hash, created_at, updated_at)
        VALUES (NULL, 'inbox/n.md', 'user', 't', 1, ?1, 'a', 'a')", [HASH_A]).is_err());
    assert!(c.execute("INSERT INTO memory_suggestions(id, chat_id, logical_turn_id, end_seq, candidate_json, candidate_hash, dedupe_key, created_at)
        VALUES (NULL, 'c', 't', 1, '{}', ?1, 'k', 'a')", [HASH_A]).is_err());
    insert_item(&c, "i1", "inbox/a.md", "private").unwrap();
    assert!(c.execute("UPDATE memory_items SET content_hash = ?1 WHERE id = 'i1'", [upper]).is_err());
}

#[test]
fn relative_paths_are_unique_case_insensitively() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    insert_item(&c, "i1", "Notes/A.md", "private").unwrap();
    assert!(insert_item(&c, "i2", "notes/a.md", "private").is_err(), "NTFS büyük/küçük harf duyarsız");
    insert_item(&c, "i3", "notes/b.md", "private").unwrap();
}

#[test]
fn private_items_cannot_be_remote_allowed_in_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    insert_item(&c, "i1", "inbox/a.md", "private").unwrap();
    assert!(c.execute("UPDATE memory_items SET remote_allowed = 1 WHERE id = 'i1'", []).is_err());
    c.execute("UPDATE memory_items SET visibility = 'shared', remote_allowed = 1 WHERE id = 'i1'", []).unwrap();
    assert!(c.execute("UPDATE memory_items SET visibility = 'private' WHERE id = 'i1'", []).is_err());
}

#[test]
fn operations_bind_request_hash_source_and_safe_error_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let op = |req: &str, hash: Option<&str>, source: &str| {
        c.execute(
            "INSERT INTO memory_operations(request_id, request_hash, source, kind, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'capture', 'a', 'a')",
            params![req, hash, source],
        )
    };
    op("r1", Some(HASH_A), "desktop").unwrap();
    assert!(op("r2", None, "desktop").is_err(), "request_hash zorunlu");
    assert!(op("r3", Some("sha256:kisa"), "desktop").is_err());
    assert!(op("r4", Some(&HASH_A.replace("sha256", "SHA256")), "desktop").is_err());
    assert!(op("r5", Some(HASH_A), "robot").is_err(), "kaynak kapalı küme");
    for source in ["telegram", "engine", "system"] {
        op(&format!("ok-{source}"), Some(HASH_B), source).unwrap();
    }
    // Aynı request_id (aynı ya da farklı hash) ikinci satır olamaz: çakışma uygulama katmanında
    // `check_replay` ile ayrıştırılır, DB aynı kimliği ikinci kez yazdırmaz.
    assert!(op("r1", Some(HASH_B), "desktop").is_err());
    for bad in ["Ham hata: C:\\x", "", "a b", "UPPER", &"x".repeat(49)] {
        assert!(c.execute("UPDATE memory_operations SET error_code = ?1 WHERE request_id = 'r1'", [bad]).is_err(), "{bad:?}");
    }
    c.execute("UPDATE memory_operations SET error_code = 'store_unavailable' WHERE request_id = 'r1'", []).unwrap();
    assert!(c.execute("UPDATE memory_operations SET content_hash = 'md5:x' WHERE request_id = 'r1'", []).is_err());
    assert!(c.execute("UPDATE memory_operations SET receipt_hash = ?1 WHERE request_id = 'r1'", [&HASH_A.to_uppercase()]).is_err());
    c.execute("UPDATE memory_operations SET content_hash = ?1, receipt_hash = ?1 WHERE request_id = 'r1'", [HASH_A]).unwrap();
}

#[test]
fn suggestion_targets_and_decision_requests_are_paired() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fresh(&tmp);
    let ins = |id: &str, hash: &str, target: Option<&str>, rev: Option<i64>| {
        c.execute(
            "INSERT INTO memory_suggestions(id, chat_id, logical_turn_id, end_seq, candidate_json, candidate_hash,
                target_id, target_revision, dedupe_key, created_at)
             VALUES (?1, 'c', 't', 1, '{}', ?2, ?3, ?4, 'k', 'a')",
            params![id, hash, target, rev],
        )
    };
    assert!(ins("s1", HASH_A, Some("hedef"), None).is_err(), "hedef id revision'sız");
    assert!(ins("s2", HASH_A, None, Some(2)).is_err(), "revision hedefsiz");
    ins("s3", HASH_A, Some("hedef"), Some(2)).unwrap();
    ins("s4", HASH_B, None, None).unwrap();
    // Karar kimliği ve parmak izi birlikte yazılır.
    assert!(c.execute("UPDATE memory_suggestions SET decision_request_id = 'r' WHERE id = 's4'", []).is_err());
    assert!(c.execute("UPDATE memory_suggestions SET decision_request_hash = ?1 WHERE id = 's4'", [HASH_A]).is_err());
    assert!(c.execute("UPDATE memory_suggestions SET decision_request_id = 'r', decision_request_hash = 'SHA256:x' WHERE id = 's4'", []).is_err());
    c.execute("UPDATE memory_suggestions SET decision_request_id = 'r', decision_request_hash = ?1 WHERE id = 's4'", [HASH_A]).unwrap();
}
