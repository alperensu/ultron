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
