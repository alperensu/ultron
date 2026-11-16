use std::path::{Path, PathBuf};

use ultron_core::import::{ImportState, import_claude_history, mask_secrets};
use ultron_core::ipc::ProjectInfo;
use ultron_core::{Db, StoredEvent};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_history")
}

fn open(dir: &Path) -> Db {
    Db::open(&dir.join("ultron.db")).unwrap()
}

fn texts(evs: &[StoredEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| e.payload["text"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn import_masks_secrets() {
    let positives = [
        "sk-ant-api03-abcdefghijklmnop",
        "sk-abcdefghijklmnopqrstuvwx",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "gho_abcdefghijklmnopqrstuvwxyz0123456789",
        "github_pat_11ABCDEFG0abcdefghijkl_mnop",
        "AKIAABCDEFGHIJKLMNOP",
        "xoxb-1234567890-abcdefghij",
        "AIzaSyA1234567890abcdefghijklmnopqrstuv",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2ln",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\ndef\n-----END RSA PRIVATE KEY-----",
        "https://user:hunter2@example.com/x",
        "password=hunter2",
        "api_token=abc123",
        "secret=\"has space\"",
        "--password=syntheticflag",
        "(token=syntheticpunctuation)",
        "?token=syntheticquery&x=ok",
    ];
    for p in positives {
        let (out, n) = mask_secrets(&format!("onceki {p} sonraki"));
        assert!(n >= 1, "maskelenmedi: {p}");
        assert!(out.contains("[MASKED]"), "{p} -> {out}");
        assert!(out.contains("onceki") && out.contains("sonraki"), "{out}");
    }
    let pem = "-----BEGIN PRIVATE KEY-----\nAAA\n-----END PRIVATE KEY-----";
    assert!(!mask_secrets(pem).0.contains("AAA"));
    assert!(
        !mask_secrets("https://user:hunter2@example.com")
            .0
            .contains("hunter2")
    );

    let negatives = [
        "sk-short",
        "ask-abcdefghijklmnopqrstuvwxyz",
        "ghp_kisa",
        "github_pat_kisa",
        "AKIA123",
        "xoxz-1234567890-abcdefghij",
        "AIzaKisa",
        "eyJhbGciOiJIUzI1NiJ9 tek parca",
        "-----BEGIN CERTIFICATE-----",
        "http://example.com:8080/yol",
        "user@example.com",
        "tokenizer ve token_count=5 sayilir",
        "password nedir",
        "secrets are fun",
    ];
    for n in negatives {
        let (out, c) = mask_secrets(n);
        assert_eq!((out.as_str(), c), (n, 0), "yanlis pozitif: {n}");
    }

    // Uçtan uca: fixture'daki token=... veritabanına düz yazılmaz.
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let mut st = ImportState::load(&db).unwrap();
    let rep = import_claude_history(&db, &fixtures(), &mut st).unwrap();
    assert!(rep.masked >= 1);
    let evs = db.events_for_chat("claude-sess-aaa", None).unwrap();
    assert!(texts(&evs).iter().all(|t| !t.contains("abc123secretvalue")));
}

#[test]
fn import_skips_malformed_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let mut st = ImportState::load(&db).unwrap();
    let rep = import_claude_history(&db, &fixtures(), &mut st).unwrap();
    assert_eq!(rep.files, 2);
    assert_eq!(rep.chats, 2);
    assert_eq!(rep.skipped, 1);
    // aaa: kullanıcı + 2 asistan; bbb: 2 mesaj. thinking ve tool_result alınmaz.
    assert_eq!(rep.messages, 5);
    let evs = db.events_for_chat("claude-sess-aaa", None).unwrap();
    let t = texts(&evs);
    let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["user_text", "assistant_text", "assistant_text"]);
    assert!(t[1].contains("Dosyaya bakiyorum."));
    assert!(t[1].contains("[araç: Read"));
    assert!(
        t.iter()
            .all(|x| !x.contains("gizli dusunce") && !x.contains("fn main"))
    );
    assert_eq!(evs[0].seq, 1);
    assert_eq!(evs[2].seq, 3);
    let hits = db.search_messages(None, "depoyu", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn import_is_incremental_and_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let f = root.join("s1.jsonl");
    let l1 = r#"{"type":"user","sessionId":"s1","cwd":"C:\\X","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"bir"}}"#;
    let l2 = r#"{"type":"assistant","sessionId":"s1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"iki"}]}}"#;
    std::fs::write(&f, format!("{l1}\n")).unwrap();

    let mut st = ImportState::load(&db).unwrap();
    let r1 = import_claude_history(&db, &root, &mut st).unwrap();
    assert_eq!((r1.chats, r1.messages), (1, 1));
    let r2 = import_claude_history(&db, &root, &mut st).unwrap();
    assert_eq!((r2.chats, r2.messages), (0, 0));

    // Yarım yazılmış son satır işlenmez; tamamlanınca alınır.
    let half = &l2[..20];
    std::fs::write(&f, format!("{l1}\n{half}")).unwrap();
    let r3 = import_claude_history(&db, &root, &mut st).unwrap();
    assert_eq!(r3.messages, 0);
    std::fs::write(&f, format!("{l1}\n{l2}\n")).unwrap();
    let r4 = import_claude_history(&db, &root, &mut st).unwrap();
    assert_eq!((r4.chats, r4.messages), (0, 1));

    // Durum kaybolsa bile (sıfır ofset) mesajlar çoğalmaz.
    let mut st2 = ImportState::default();
    let r5 = import_claude_history(&db, &root, &mut st2).unwrap();
    assert_eq!(r5.messages, 0);
    assert_eq!(db.events_for_chat("claude-s1", None).unwrap().len(), 2);

    // Kalıcı durum: yeniden yükleme ofseti korur.
    let mut st3 = ImportState::load(&db).unwrap();
    let r6 = import_claude_history(&db, &root, &mut st3).unwrap();
    assert_eq!(r6.messages, 0);
}

#[test]
fn import_binds_project_by_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    db.upsert_project(&ProjectInfo {
        id: "alpha".into(),
        name: "Alpha".into(),
        path: "c:/work/alpha/".into(),
        aliases: vec![],
        description: None,
        last_active_at: None,
    })
    .unwrap();
    let mut st = ImportState::load(&db).unwrap();
    import_claude_history(&db, &fixtures(), &mut st).unwrap();
    let a = db.get_chat("claude-sess-aaa").unwrap().unwrap();
    let b = db.get_chat("claude-sess-bbb").unwrap().unwrap();
    assert_eq!(a.project_id.as_deref(), Some("alpha"));
    assert_eq!(b.project_id, None);
    // Sohbetler listelenebilir kalır (motor alanı geçersiz değer taşımaz).
    assert_eq!(db.list_chats(None, false).unwrap().len(), 2);
    assert!(a.title.starts_with("Merhaba"));
}

fn write_records(file: &Path, records: &[serde_json::Value]) {
    let text = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(file, text).unwrap();
}

fn message(text: &str) -> serde_json::Value {
    serde_json::json!({"type":"user", "message":{"content":text}})
}

#[test]
fn pem_split_across_blocks_is_masked() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let record = serde_json::json!({"type":"assistant", "message":{"content":[
        {"type":"text", "text":"before\n-----BEGIN PRIVATE KEY-----"},
        {"type":"text", "text":"syntheticpemvalue\n-----END PRIVATE KEY-----\nafter"}
    ]}});
    write_records(&root.join("pem.jsonl"), &[record]);
    let report = import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    assert_eq!(report.masked, 1);
    let text = texts(&db.events_for_chat("claude-pem", None).unwrap()).remove(0);
    assert_eq!(text, "before\n[MASKED]\nafter");
    assert!(db.search_messages(None, "syntheticpemvalue", 10).unwrap().is_empty());
}

#[test]
fn yaml_literal_block_masked() {
    let (text, count) = mask_secrets("config:\n  password: |\n    syntheticliteral\n    secondline\n  public: visible\n");
    assert!(!text.contains("syntheticliteral") && !text.contains("secondline"), "{text}");
    assert!(text.contains("public: visible"));
    assert!(count >= 1);
}

#[test]
fn yaml_folded_block_masked() {
    let (text, count) = mask_secrets("'api_token': >- # folded\n  syntheticfolded\n\n  secondline\npublic: visible\n");
    assert!(!text.contains("syntheticfolded") && !text.contains("secondline"), "{text}");
    assert!(text.contains("public: visible"));
    assert!(count >= 1);
}

#[test]
fn yaml_blocks_are_absent_from_db_and_fts() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    write_records(&root.join("yaml.jsonl"), &[
        message("password: |\n  syntheticliteral\n  syntheticcontinuation\npublic: visible"),
        message("token: >\n  syntheticfolded\n  syntheticcontinuation\npublic: visible"),
    ]);
    import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("ultron.db")).unwrap();
    for (table, column) in [("events", "payload_json"), ("messages_fts", "text")] {
        let count: i64 = conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE {column} LIKE '%synthetic%'"),
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 0, "{table}");
    }
    for raw in ["syntheticliteral", "syntheticfolded", "syntheticcontinuation"] {
        assert!(db.search_messages(None, raw, 10).unwrap().is_empty());
    }
}

#[test]
fn equal_size_rewrite_after_first_line_is_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("equal.jsonl");
    let first = serde_json::json!({"type":"system", "cwd":"synthetic/project"});
    write_records(&file, &[first.clone(), message("old")]);
    import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    let size = std::fs::metadata(&file).unwrap().len();
    write_records(&file, &[first, message("new")]);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), size);
    let report = import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    assert_eq!(report.messages, 1);
    assert_eq!(texts(&db.events_for_chat("claude-equal", None).unwrap()), ["old", "new"]);
}

#[test]
fn legacy_checkpoint_without_prefix_hash_does_not_duplicate_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    write_records(&root.join("legacy.jsonl"), &[message("original")]);
    import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("ultron.db")).unwrap();
    conn.execute("UPDATE import_state SET prefix_hash = NULL", []).unwrap();
    let report = import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    assert_eq!(report.messages, 0);
    assert_eq!(db.events_for_chat("claude-legacy", None).unwrap().len(), 1);
}

#[test]
fn import_after_live_events_loses_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    db.insert_chat("claude-live", None, "", chrono::Utc::now()).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("ultron.db")).unwrap();
    // Deterministically interleave a live event after the first imported event.
    conn.execute_batch(r#"
        CREATE TRIGGER live_interleave AFTER INSERT ON events
        WHEN NEW.chat_id = 'claude-live' AND NEW.seq = 1
        BEGIN
            INSERT INTO events(id, chat_id, seq, ts, kind, payload_json)
            VALUES ('live-event', NEW.chat_id, 2, NEW.ts, 'user_text', '{"text":"live"}');
        END;
    "#).unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    write_records(&root.join("live.jsonl"), &[message("first"), message("second"), message("third")]);
    let report = import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    assert_eq!(report.messages, 3);
    let events = db.events_for_chat("claude-live", None).unwrap();
    assert_eq!(texts(&events), ["first", "live", "second", "third"]);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert_eq!(import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap().messages, 0);
}

#[test]
fn rewritten_file_imports_new_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("rewrite.jsonl");
    // Equal-size rewrites must be detected even when the offset equals the size.
    write_records(&file, &[message("old")]);
    import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    write_records(&file, &[message("new")]);
    let report = import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    assert_eq!(report.messages, 1);
    assert_eq!(texts(&db.events_for_chat("claude-rewrite", None).unwrap()), ["old", "new"]);

    let original = serde_json::json!({"type":"user", "uuid":"source-1", "message":{"content":"uuid message"}});
    write_records(&file, &[original.clone(), message("long padding message")]);
    import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    write_records(&file, &[original, message("short")]);
    let report = import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    assert_eq!(report.messages, 1, "source UUID survives a new generation");
    let events = db.events_for_chat("claude-rewrite", None).unwrap();
    assert_eq!(texts(&events).iter().filter(|t| *t == "uuid message").count(), 1);
}

#[test]
fn cwd_from_metadata_binds_project() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    for id in ["alpha", "user-choice"] {
        db.upsert_project(&ProjectInfo { id: id.into(), name: id.into(), path: format!("c:/work/{id}"), aliases: vec![], description: None, last_active_at: None }).unwrap();
    }
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let metadata = serde_json::json!({"type":"user", "isMeta":true, "cwd":"C:\\Work\\Alpha\\"});
    let mut state = ImportState::default();
    write_records(&root.join("before.jsonl"), &[metadata.clone(), message("before")]);
    write_records(&root.join("after.jsonl"), &[message("after")]);
    import_claude_history(&db, &root, &mut state).unwrap();
    assert_eq!(db.get_chat("claude-before").unwrap().unwrap().project_id.as_deref(), Some("alpha"));
    write_records(&root.join("after.jsonl"), &[message("after"), metadata.clone()]);
    import_claude_history(&db, &root, &mut state).unwrap();
    assert_eq!(db.get_chat("claude-after").unwrap().unwrap().project_id.as_deref(), Some("alpha"));
    db.update_chat("claude-after", None, None, None, Some(Some("user-choice"))).unwrap();
    write_records(&root.join("after.jsonl"), &[message("after"), metadata.clone(), metadata]);
    import_claude_history(&db, &root, &mut state).unwrap();
    assert_eq!(db.get_chat("claude-after").unwrap().unwrap().project_id.as_deref(), Some("user-choice"));
}

#[test]
fn structured_secrets_are_masked_before_storage_and_fts() {
    let positives = [
        r#"{"password":"syntheticpassword"}"#,
        r#"'token': 'synthetictoken'"#,
        "api_key: syntheticapikey",
        r#"{"password":"prefix\"syntheticescaped"}"#,
        r#"'password': 'prefix\'syntheticsingle'"#,
        r#"api_key="syntheticquotedkey""#,
    ];
    for text in positives {
        let (masked, count) = mask_secrets(text);
        assert_eq!(count, 1, "{text}");
        assert!(!masked.contains("synthetic"), "{masked}");
    }
    for text in ["token_count: 5", "tokenizer: abc", "password_hint: abc", "api_keys: abc", "password: ", "password policy", r#"{"description":"token example"}"#] {
        assert_eq!(mask_secrets(text), (text.into(), 0), "{text}");
    }
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    write_records(&root.join("secrets.jsonl"), &positives.iter().map(|t| message(t)).collect::<Vec<_>>());
    let report = import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    assert_eq!(report.masked, positives.len());
    assert!(texts(&db.events_for_chat("claude-secrets", None).unwrap()).iter().all(|t| !t.contains("synthetic")));
    let conn = rusqlite::Connection::open(tmp.path().join("ultron.db")).unwrap();
    for table in ["events", "messages_fts"] {
        let column = if table == "events" { "payload_json" } else { "text" };
        let leaked: i64 = conn.query_row(&format!("SELECT count(*) FROM {table} WHERE {column} LIKE '%synthetic%'"), [], |r| r.get(0)).unwrap();
        assert_eq!(leaked, 0, "{table}");
    }
    for raw in ["syntheticpassword", "synthetictoken", "syntheticapikey", "syntheticescaped", "syntheticsingle", "syntheticquotedkey"] {
        assert!(db.search_messages(None, raw, 10).unwrap().is_empty());
    }
}

#[test]
fn mask_before_truncate() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let arg = format!("{}https://u:syntheticpass@host/path", "x".repeat(65));
    let record = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"tool_use", "name":"Fetch", "input":{"url":arg}}]}});
    write_records(&root.join("tool.jsonl"), &[record]);
    let report = import_claude_history(&db, &root, &mut ImportState::default()).unwrap();
    assert_eq!(report.masked, 1);
    let text = texts(&db.events_for_chat("claude-tool", None).unwrap()).remove(0);
    assert!(!text.contains("u:synt"), "{text}");
    assert!(db.search_messages(None, "synt", 10).unwrap().is_empty());
}

#[test]
fn failed_checkpoint_rolls_back_message_and_fts() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    write_records(&root.join("atomic.jsonl"), &[message("atomicmessage")]);
    let conn = rusqlite::Connection::open(tmp.path().join("ultron.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_checkpoint BEFORE INSERT ON import_state
         WHEN NEW.byte_offset > 0 BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;",
    ).unwrap();
    let mut state = ImportState::default();
    assert!(import_claude_history(&db, &root, &mut state).is_err());
    assert!(db.events_for_chat("claude-atomic", None).unwrap().is_empty());
    assert!(db.search_messages(None, "atomicmessage", 10).unwrap().is_empty());
    assert!(db.import_offsets().unwrap().is_empty());
    conn.execute_batch("DROP TRIGGER reject_checkpoint").unwrap();
    assert_eq!(import_claude_history(&db, &root, &mut state).unwrap().messages, 1);
    assert_eq!(db.search_messages(None, "atomicmessage", 10).unwrap().len(), 1);
}

#[test]
fn metadata_cwd_survives_restart_before_first_message() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open(tmp.path());
    db.upsert_project(&ProjectInfo {
        id: "alpha".into(), name: "Alpha".into(), path: "c:/work/alpha".into(),
        aliases: vec![], description: None, last_active_at: None,
    }).unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("metadata.jsonl");
    let metadata = serde_json::json!({"type":"system", "cwd":"C:\\Work\\Alpha"});
    write_records(&file, std::slice::from_ref(&metadata));
    assert_eq!(import_claude_history(&db, &root, &mut ImportState::default()).unwrap().chats, 0);
    write_records(&file, &[metadata, message("first")]);
    import_claude_history(&db, &root, &mut ImportState::load(&db).unwrap()).unwrap();
    assert_eq!(db.get_chat("claude-metadata").unwrap().unwrap().project_id.as_deref(), Some("alpha"));
}
