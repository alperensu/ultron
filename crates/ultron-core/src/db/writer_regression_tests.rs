use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static WRITER_BLOCKED: AtomicBool = AtomicBool::new(false);

fn signal_busy(_: i32) -> bool {
    WRITER_BLOCKED.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1));
    true
}

#[test]
fn live_append_and_import_interleave_without_loss() {
    for import_first in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("chat", None, "", Utc::now()).unwrap();
        db.conn().unwrap().busy_handler(Some(signal_busy)).unwrap();
        let mut holder = Connection::open(&paths.db).unwrap();
        let tx = holder.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        WRITER_BLOCKED.store(false, Ordering::SeqCst);
        let event = StoredEvent {
            id: "imported".into(), chat_id: "chat".into(), session_id: None,
            seq: 0, ts: Utc::now(), kind: "user_text".into(),
            payload: serde_json::json!({"text":"imported"}),
        };
        let worker_db = db.clone();
        let worker_event = event.clone();
        let worker = std::thread::spawn(move || {
            if import_first {
                crate::eventlog::EventLog::new(worker_db, paths).append(
                    "chat", None, crate::events::EngineEvent::UserText { text: "live".into() },
                ).map(|_| ())
            } else {
                worker_db.commit_import_record(
                    "source", &ImportCheckpoint::default(), Some(&worker_event),
                ).map(|_| ())
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !WRITER_BLOCKED.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let blocked = WRITER_BLOCKED.load(Ordering::SeqCst);
        if import_first {
            Db::append_event_tx(&tx, &event).unwrap();
        } else {
            let mut live = event.clone();
            live.id = "live".into();
            live.payload = serde_json::json!({"text":"live"});
            Db::append_event_tx(&tx, &live).unwrap();
        }
        tx.commit().unwrap();
        let result = worker.join().unwrap();
        assert!(blocked, "worker never reached the SQLite writer lock");
        result.unwrap();
        let events = db.events_for_chat("chat", None).unwrap();
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(events[0].payload["text"], if import_first { "imported" } else { "live" });
        for text in ["imported", "live"] {
            assert_eq!(db.search_messages(None, text, 10).unwrap().len(), 1);
        }
        assert!(!db.commit_import_record("source", &ImportCheckpoint::default(), Some(&event)).unwrap());
        assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 2);
    }
}

