use chrono::Utc;
use serde_json::json;
use ultron_core::db::{Db, StoredEvent};
use ultron_core::handoff::build_handoff;
use ultron_core::workstate::WorkState;

const HISTORY_HINT: &str = "[GEÇMİŞ]";

/// Sohbeti açar, `messages` sayısı kadar sentetik mesaj ve bir çalışma durumu yazar.
/// Mesajlar `user_text` / `assistant_text` dönüşümlüdür; her biri `MSG-<no>-` ile başlar.
fn seeded_db(dir: &std::path::Path, messages: usize, msg_chars: usize) -> Db {
    let db = Db::open(&dir.join("ultron.db")).unwrap();
    db.insert_chat("chat-1", None, "test", Utc::now()).unwrap();
    for i in 1..=messages {
        let (kind, role_tag) = if i % 2 == 1 {
            ("user_text", "U")
        } else {
            ("assistant_text", "A")
        };
        let body = "ab".repeat(msg_chars / 2);
        let text = format!("MSG-{i:02}-{role_tag}-{body}");
        db.insert_event(&StoredEvent {
            id: format!("m{i}"),
            chat_id: "chat-1".into(),
            session_id: None,
            seq: i as i64,
            ts: Utc::now(),
            kind: kind.into(),
            payload: json!({"type": kind, "text": text}),
        })
        .unwrap();
    }
    let state = WorkState {
        goal: Some("Test hedefi".into()),
        next_step: Some("Sonraki test".into()),
        ..WorkState::default()
    };
    db.put_work_state("chat-1", &serde_json::to_value(&state).unwrap())
        .unwrap();
    db
}

fn position(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("metin içinde yok: {needle}"))
}

#[test]
fn pack_order_is_stable_first() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), 12, 60);
    let pack = build_handoff(&db, "chat-1", 100_000, 6, None, None).unwrap();
    assert!(!pack.truncated);
    assert!(pack.text.starts_with("[ULTRON DEVİR]"));
    let order = [
        position(&pack.text, "[ULTRON DEVİR]"),
        position(&pack.text, "[ÇALIŞMA DURUMU]"),
        position(&pack.text, "[ESKİ MESAJLAR ÖZETİ]"),
        position(&pack.text, "[SON MESAJLAR]"),
        position(&pack.text, HISTORY_HINT),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "sıra bozuk: {order:?}");
    assert!(pack.text.contains("Test hedefi"));
    let names: Vec<&str> = pack.sections.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "header",
            "work_state",
            "older_summary",
            "recent_messages",
            "history_hint"
        ]
    );
}

#[test]
fn pack_respects_budget_and_marks_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), 20, 300);
    let pack = build_handoff(&db, "chat-1", 2000, 6, None, None).unwrap();
    assert!(pack.text.chars().count() <= 2000, "bütçe aşıldı: {}", pack.text.chars().count());
    assert!(pack.truncated);
    assert!(pack.text.contains("[KIRPILDI: "));
}

#[test]
fn pack_never_drops_work_state_or_last_two_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), 20, 300);
    let pack = build_handoff(&db, "chat-1", 1200, 6, None, None).unwrap();
    assert!(pack.truncated);
    assert!(pack.text.chars().count() <= 1200);
    assert!(pack.text.contains("Test hedefi"));
    assert!(pack.text.contains("Sonraki test"));
    assert!(pack.text.contains("MSG-19-U-"));
    assert!(pack.text.contains("MSG-20-A-"));
}

#[test]
fn pack_includes_external_changes_section() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), 3, 40);
    let external = "src/main.rs dosyası sohbet dışında değişti";
    let pack = build_handoff(&db, "chat-1", 100_000, 6, Some(external), None).unwrap();
    assert!(pack.text.contains("[SON OTURUMDAN BERİ DEĞİŞENLER]"));
    assert!(pack.text.contains(external));
    assert!(
        position(&pack.text, "[ÇALIŞMA DURUMU]")
            < position(&pack.text, "[SON OTURUMDAN BERİ DEĞİŞENLER]")
    );
    assert!(
        position(&pack.text, "[SON OTURUMDAN BERİ DEĞİŞENLER]")
            < position(&pack.text, "[SON MESAJLAR]")
    );
    assert!(pack.sections.iter().any(|(n, _)| n == "external_changes"));
}

#[test]
fn pack_names_project_instruction_files() {
    let tmp = tempfile::tempdir().unwrap();
    let db = seeded_db(tmp.path(), 4, 40);
    let project = tmp.path().join("proje");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("AGENTS.md"), "GIZLI-ICERIK-1").unwrap();
    std::fs::write(project.join("CLAUDE.md"), "GIZLI-ICERIK-2").unwrap();
    let pack = build_handoff(&db, "chat-1", 100_000, 6, None, Some(&project)).unwrap();
    let header = position(&pack.text, "[ULTRON DEVİR]");
    let instr = position(&pack.text, "[PROJE TALİMATLARI]");
    let state = position(&pack.text, "[ÇALIŞMA DURUMU]");
    assert!(header < instr && instr < state);
    assert!(pack.text.contains(&project.join("AGENTS.md").display().to_string()));
    assert!(pack.text.contains(&project.join("CLAUDE.md").display().to_string()));
    assert!(!pack.text.contains("GIZLI-ICERIK"));
    let empty = tmp.path().join("bos");
    std::fs::create_dir_all(&empty).unwrap();
    let pack = build_handoff(&db, "chat-1", 100_000, 6, None, Some(&empty)).unwrap();
    assert!(!pack.text.contains("[PROJE TALİMATLARI]"));
}

#[test]
fn pack_coalesces_streamed_fragments() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("ultron.db")).unwrap();
    db.insert_chat("chat-1", None, "test", Utc::now()).unwrap();
    let fragments = ["Bir ", "iki ", "üç ", "dört ", "beş"];
    let mut events: Vec<(&str, &str)> = fragments.iter().map(|f| ("assistant_text", *f)).collect();
    events.push(("user_text", "son soru"));
    for (i, (kind, text)) in events.into_iter().enumerate() {
        db.insert_event(&StoredEvent {
            id: format!("f{i}"),
            chat_id: "chat-1".into(),
            session_id: None,
            seq: i as i64 + 1,
            ts: Utc::now(),
            kind: kind.into(),
            payload: json!({"type": kind, "text": text}),
        })
        .unwrap();
    }
    let pack = build_handoff(&db, "chat-1", 100_000, 6, None, None).unwrap();
    let assistant_lines = pack.text.lines().filter(|l| l.starts_with("asistan: ")).count();
    assert_eq!(assistant_lines, 1, "{}", pack.text);
    assert!(pack.text.contains("asistan: Bir iki üç dört beş\n"), "{}", pack.text);
    assert!(pack.text.contains("kullanıcı: son soru\n"), "{}", pack.text);
}
