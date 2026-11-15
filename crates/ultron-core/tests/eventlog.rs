use std::sync::Arc;

use chrono::Utc;
use ultron_core::eventlog::excerpt;
use ultron_core::events::EngineEvent;
use ultron_core::{Db, EventLog, Paths};

fn setup() -> (tempfile::TempDir, Paths, Arc<Db>) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db).unwrap());
    db.insert_chat("chat-1", None, "test", Utc::now()).unwrap();
    (tmp, paths, db)
}

fn assistant(text: &str) -> EngineEvent {
    EngineEvent::AssistantText {
        text: text.to_string(),
    }
}

#[test]
fn append_assigns_monotonic_seq() {
    let (_tmp, paths, db) = setup();
    let log = EventLog::new(db, paths);
    let a = log.append("chat-1", None, assistant("bir")).unwrap();
    let b = log.append("chat-1", None, assistant("iki")).unwrap();
    let c = log.append("chat-1", None, assistant("üç")).unwrap();
    assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
}

#[test]
fn append_writes_transcript_line() {
    let (_tmp, paths, db) = setup();
    let log = EventLog::new(db, paths.clone());
    log.append("chat-1", None, assistant("merhaba")).unwrap();
    let text = std::fs::read_to_string(paths.transcripts.join("chat-1.jsonl")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["type"], "assistant_text");
}

#[tokio::test]
async fn subscribers_receive_events() {
    let (_tmp, paths, db) = setup();
    let log = EventLog::new(db, paths);
    let mut rx = log.subscribe();
    let stored = log.append("chat-1", None, assistant("selam")).unwrap();
    let got = rx.recv().await.unwrap();
    assert_eq!(got.id, stored.id);
}

#[test]
fn excerpt_marks_truncation() {
    let long = "a".repeat(10_000);
    let out = excerpt(&long, 4000);
    assert!(out.chars().count() <= 4100);
    assert!(out.contains("kırpıldı"));

    let short = "b".repeat(100);
    assert_eq!(excerpt(&short, 4000), short);
}
