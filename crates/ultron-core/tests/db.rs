use chrono::Utc;
use serde_json::json;
use ultron_core::{Db, StoredEvent};

fn event(id: &str, chat_id: &str, seq: i64, kind: &str, payload: serde_json::Value) -> StoredEvent {
    StoredEvent {
        id: id.to_string(),
        chat_id: chat_id.to_string(),
        session_id: None,
        seq,
        ts: Utc::now(),
        kind: kind.to_string(),
        payload,
    }
}

fn open_with_chat(dir: &std::path::Path) -> Db {
    let db = Db::open(&dir.join("state").join("ultron.db")).unwrap();
    db.insert_chat("chat-1", None, "test", Utc::now()).unwrap();
    db
}

#[test]
fn open_sets_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("ultron.db");
    let _db = Db::open(&path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn event_insert_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    let ev = event("e1", "chat-1", 1, "user_text", json!({"text": "merhaba"}));
    assert!(db.insert_event(&ev).unwrap());
    assert!(!db.insert_event(&ev).unwrap());
    assert_eq!(db.events_for_chat("chat-1", None).unwrap().len(), 1);
}

#[test]
fn sequence_collision_is_not_an_id_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    db.insert_event(&event("a", "chat-1", 1, "user_text", json!({"text":"first"}))).unwrap();
    assert!(db.insert_event(&event("b", "chat-1", 1, "user_text", json!({"text":"second"}))).is_err());
    assert!(db.search_messages(None, "second", 10).unwrap().is_empty());
}

#[test]
fn events_survive_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    db.insert_event(&event("e1", "chat-1", 1, "user_text", json!({"text": "a"})))
        .unwrap();
    drop(db);
    let db = Db::open(&tmp.path().join("state").join("ultron.db")).unwrap();
    let events = db.events_for_chat("chat-1", None).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, "e1");
}

#[test]
fn fts_finds_turkish_text() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    db.insert_event(&event(
        "e1",
        "chat-1",
        1,
        "user_text",
        json!({"text": "Ödeme ekranında buton kayıyor"}),
    ))
    .unwrap();
    let hits = db.search_messages(None, "ödeme", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].event_id, "e1");
}

#[test]
fn replay_is_paged() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    let batch: Vec<StoredEvent> = (1..=1200)
        .map(|seq| event(&format!("e{seq}"), "chat-1", seq, "user_text", json!({"text": "x"})))
        .collect();
    for ev in &batch {
        db.insert_event(ev).unwrap();
    }
    let mut seen: Vec<i64> = Vec::new();
    let mut after = None;
    loop {
        let page = db.events_for_chat_page("chat-1", after, 500).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 500);
        after = page.last().map(|e| e.seq);
        seen.extend(page.iter().map(|e| e.seq));
    }
    assert_eq!(seen, (1..=1200).collect::<Vec<i64>>());
}

#[test]
fn last_event_returns_highest_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    assert!(db.last_event("chat-1").unwrap().is_none());
    assert!(!db.has_events("chat-1").unwrap());
    for seq in [1, 3, 2] {
        db.insert_event(&event(
            &format!("e{seq}"),
            "chat-1",
            seq,
            "user_text",
            json!({"type": "user_text", "text": format!("m{seq}")}),
        ))
        .unwrap();
    }
    let last = db.last_event("chat-1").unwrap().unwrap();
    assert_eq!(last.seq, 3);
    assert_eq!(last.id, "e3");
    assert!(db.has_events("chat-1").unwrap());
}

#[test]
fn events_tail_returns_last_n_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_with_chat(tmp.path());
    for seq in 1..=10 {
        db.insert_event(&event(
            &format!("e{seq}"),
            "chat-1",
            seq,
            "user_text",
            json!({"type": "user_text", "text": format!("m{seq}")}),
        ))
        .unwrap();
    }
    let tail: Vec<i64> = db
        .events_tail("chat-1", 4)
        .unwrap()
        .into_iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(tail, vec![7, 8, 9, 10]);
    let all = db.events_tail("chat-1", 100).unwrap();
    assert_eq!(all.len(), 10);
    assert_eq!(all[0].seq, 1);
}
