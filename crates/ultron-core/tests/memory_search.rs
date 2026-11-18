//! Hafıza araması: Türkçe katlama, literal sorgu, sınırlar, sayfalama ve kitle matrisi.
//! Bütün veri sentetiktir.

use chrono::{DateTime, Utc};
use ultron_core::Db;
use ultron_core::db::memory::{MemoryItemRecord, ReceiptRecord};
use ultron_core::memory::{
    Audience, ClaimStatus, ContractError, IndexState, MemoryId, MemoryType, Receipt, Revision,
    Sensitivity, Sha256Hash, SourceRef, Visibility,
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

fn item_with(id: &MemoryId, path: &str, title: &str, summary: &str, body: &str) -> MemoryItemRecord {
    MemoryItemRecord {
        id: id.clone(),
        relative_path: path.into(),
        memory_type: MemoryType::User,
        project: None,
        visibility: Visibility::Private,
        remote_allowed: false,
        sensitivity: Sensitivity::Normal,
        title: title.into(),
        summary: summary.into(),
        body: body.into(),
        tags: vec![],
        revision: Revision::new(1).unwrap(),
        content_hash: Sha256Hash::of(format!("{path}\n{title}\n{summary}\n{body}").as_bytes()),
        created_at: at("2026-11-18T09:00:00Z"),
        updated_at: at("2026-11-18T09:00:00Z"),
        review_at: None,
        index_state: IndexState::Indexed,
        trashed_at: None,
        receipts: vec![receipt(&format!("{path}-{body}"))],
    }
}

fn item(id: &MemoryId, path: &str, title: &str, body: &str) -> MemoryItemRecord {
    item_with(id, path, title, &format!("Özet: {title}"), body)
}

fn hit_ids(db: &Db, query: &str, aud: Audience, project: Option<&str>) -> Vec<String> {
    let mut ids: Vec<String> = db
        .memory_search(query, aud, project, Some(50), None)
        .unwrap()
        .items
        .into_iter()
        .map(|h| h.item.id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn turkish_letters_fold_in_both_directions() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/tr.md", "İstanbul Şişli ğüneş", "Işık ve ÇAY")).unwrap();
    for q in ["istanbul", "ISTANBUL", "ıstanbul", "Şişli", "sisli", "GUNES", "ışık", "isik", "çay", "cay"] {
        assert_eq!(
            hit_ids(&db, q, Audience::Desktop, None),
            vec![id.as_str().to_string()],
            "sorgu: {q}"
        );
    }
}

#[test]
fn hostile_queries_are_literal_and_never_panic() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Alfa", "alpha zeta")).unwrap();

    // Operatör ve SQL metni sözcük olarak aranır; FTS/SQL olarak çalışmaz.
    for q in [
        "",
        "   ",
        "\"",
        "*",
        "NEAR(alpha zeta)",
        "alpha OR zeta",
        "alpha AND",
        "title:alpha",
        "'; DROP TABLE memory_items; --",
        "😀",
        "-",
        "^alpha",
        "\u{0307}\u{0307}",
    ] {
        let page = db.memory_search(q, Audience::Desktop, None, None, None);
        assert!(page.is_ok(), "sorgu hata verdi: {q:?}");
    }
    assert!(hit_ids(&db, "", Audience::Desktop, None).is_empty());
    assert!(hit_ids(&db, "😀", Audience::Desktop, None).is_empty());
    assert!(hit_ids(&db, "alpha OR zeta", Audience::Desktop, None).is_empty());
    assert!(hit_ids(&db, "NEAR(alpha zeta)", Audience::Desktop, None).is_empty());
    assert!(hit_ids(&db, "alpha AND", Audience::Desktop, None).is_empty());
    assert!(hit_ids(&db, "*", Audience::Desktop, None).is_empty());
    assert_eq!(hit_ids(&db, "alpha zeta", Audience::Desktop, None), vec![id.as_str().to_string()]);
    // Tablo bozulmadı.
    assert!(db.memory_get(&id, Audience::Desktop).unwrap().is_some());
}

#[test]
fn overlong_query_is_rejected_with_a_contract_error() {
    let (_d, db) = open();
    let long = "x".repeat(300);
    let err = db.memory_search(&long, Audience::Desktop, None, None, None).unwrap_err();
    assert!(err.downcast_ref::<ContractError>().is_some(), "sözleşme hatası bekleniyordu");
}

#[test]
fn limit_is_clamped_to_one_through_fifty() {
    let (_d, db) = open();
    for n in 0..60 {
        let id = MemoryId::new();
        db.memory_upsert_item(&item(&id, &format!("me/{n}.md"), "Not", "ortak kelime")).unwrap();
    }
    let big = db.memory_search("ortak", Audience::Desktop, None, Some(1000), None).unwrap();
    assert_eq!(big.items.len(), 50);
    assert!(big.next_cursor.is_some());
    let zero = db.memory_search("ortak", Audience::Desktop, None, Some(0), None).unwrap();
    assert_eq!(zero.items.len(), 1);
    let default = db.memory_search("ortak", Audience::Desktop, None, None, None).unwrap();
    assert_eq!(default.items.len(), 20);
}

#[test]
fn paging_is_stable_and_covers_every_hit_once() {
    let (_d, db) = open();
    for n in 0..7 {
        let id = MemoryId::new();
        db.memory_upsert_item(&item(&id, &format!("me/{n}.md"), "Not", "ortak kelime")).unwrap();
    }
    let full: Vec<String> = db
        .memory_search("ortak", Audience::Desktop, None, Some(50), None)
        .unwrap()
        .items
        .into_iter()
        .map(|h| h.item.id.as_str().to_string())
        .collect();
    assert_eq!(full.len(), 7);

    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = db
            .memory_search("ortak", Audience::Desktop, None, Some(3), cursor.as_deref())
            .unwrap();
        paged.extend(page.items.into_iter().map(|h| h.item.id.as_str().to_string()));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, full);
}

#[test]
fn response_byte_budget_pages_long_snippets_without_losing_hits() {
    let (_d, db) = open();
    let long_summary = format!("ortak {}", "x".repeat(490));
    for n in 0..50 {
        let id = MemoryId::new();
        db.memory_upsert_item(&item_with(&id, &format!("me/{n}.md"), "Not", &long_summary, "ortak")).unwrap();
    }
    let first = db.memory_search("ortak", Audience::Desktop, None, Some(50), None).unwrap();
    assert!(first.items.len() < 50, "bayt bütçesi ilk sayfada uygulanmadı");
    assert!(first.next_cursor.is_some());

    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = db
            .memory_search("ortak", Audience::Desktop, None, Some(50), cursor.as_deref())
            .unwrap();
        for hit in page.items {
            assert!(seen.insert(hit.item.id.as_str().to_string()), "sonuç iki kez döndü");
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 50);
}

#[test]
fn list_and_get_follow_the_same_audience_rules() {
    let (_d, db) = open();
    let id = MemoryId::new();
    db.memory_upsert_item(&item(&id, "me/a.md", "Not", "kelime")).unwrap();
    assert!(db.memory_get(&id, Audience::Telegram).unwrap().is_none());
    assert!(db.memory_get(&id, Audience::Engine).unwrap().is_none());
    assert!(db.memory_list(Audience::Telegram, None).unwrap().items.is_empty());
    assert_eq!(db.memory_list(Audience::Desktop, None).unwrap().items.len(), 1);
}

/// Her öğe için beklenen görünürlük, `Audience::admits` + indeks/çöp koşulu ile hesaplanır ve
/// SQL'in döndürdüğü sonuçla karşılaştırılır (arama, liste, getirme).
#[test]
fn audience_matrix_matches_contract_admits() {
    let (_d, db) = open();
    let projects: [Option<&str>; 3] = [None, Some("p1"), Some("p2")];
    // (id, visibility, remote, item_project, visible_by_state)
    let mut specs: Vec<(MemoryId, Visibility, bool, Option<&str>, bool)> = Vec::new();
    let combos = [
        (Visibility::Private, vec![false]),
        (Visibility::Project, vec![false, true]),
        (Visibility::Shared, vec![false, true]),
    ];
    let mut n = 0;
    for (vis, remotes) in combos {
        for remote in remotes {
            for project in projects {
                let id = MemoryId::new();
                let mut it = item(&id, &format!("me/m{n}.md"), "Matris", "ortak matris");
                n += 1;
                it.visibility = vis;
                it.remote_allowed = remote;
                it.project = project.map(str::to_string);
                db.memory_upsert_item(&it).unwrap();
                specs.push((id, vis, remote, project, true));
            }
        }
    }
    // Çöp ve karantina: kitle koşulları uysa bile gizli.
    let trashed = MemoryId::new();
    let mut t = item(&trashed, "me/trash.md", "Çöp", "ortak matris");
    t.visibility = Visibility::Shared;
    t.remote_allowed = true;
    t.trashed_at = Some(at("2026-11-18T10:00:00Z"));
    db.memory_upsert_item(&t).unwrap();
    specs.push((trashed, Visibility::Shared, true, None, false));

    let quarantined = MemoryId::new();
    let mut q = item(&quarantined, "me/quar.md", "Karantina", "ortak matris");
    q.visibility = Visibility::Shared;
    q.remote_allowed = true;
    q.index_state = IndexState::Quarantined;
    db.memory_upsert_item(&q).unwrap();
    specs.push((quarantined, Visibility::Shared, true, None, false));

    let scopes: [Option<&str>; 3] = [None, Some("p1"), Some("p2")];
    for aud in [Audience::Desktop, Audience::Engine, Audience::Telegram] {
        for scope in scopes {
            let expected = {
                let mut v: Vec<String> = specs
                    .iter()
                    .filter(|(_, vis, remote, project, visible)| {
                        if !visible {
                            return false;
                        }
                        match aud {
                            // Masaüstü her şeyi görür; proje verilirse yalnız o proje ve projesizler.
                            Audience::Desktop => {
                                scope.is_none() || project.is_none() || *project == scope
                            }
                            _ => aud.admits(*vis, *remote, *project, scope),
                        }
                    })
                    .map(|(id, ..)| id.as_str().to_string())
                    .collect();
                v.sort();
                v
            };
            assert_eq!(hit_ids(&db, "ortak", aud, scope), expected, "arama {aud:?} kapsam {scope:?}");
        }

        // Liste ve getirme proje kapsamı almaz (kapsam = None).
        let mut expected_none: Vec<String> = specs
            .iter()
            .filter(|(_, vis, remote, project, visible)| {
                *visible && (aud == Audience::Desktop || aud.admits(*vis, *remote, *project, None))
            })
            .map(|(id, ..)| id.as_str().to_string())
            .collect();
        expected_none.sort();
        let mut listed: Vec<String> = db
            .memory_list(aud, None)
            .unwrap()
            .items
            .into_iter()
            .map(|v| v.id.as_str().to_string())
            .collect();
        listed.sort();
        assert_eq!(listed, expected_none, "liste {aud:?}");

        for (id, vis, remote, project, visible) in &specs {
            let allowed = *visible && (aud == Audience::Desktop || aud.admits(*vis, *remote, *project, None));
            let got = db.memory_get(id, aud).unwrap().is_some();
            assert_eq!(got, allowed, "getirme {aud:?} {}", id.as_str());
        }
    }
}

#[test]
fn cursor_is_opaque_and_bound_to_query_audience_and_project() {
    let (_d, db) = open();
    for n in 0..5 {
        let id = MemoryId::new();
        db.memory_upsert_item(&item(&id, &format!("me/{n}.md"), "Not", "ortak kelime")).unwrap();
    }
    let page = db.memory_search("ortak", Audience::Desktop, None, Some(2), None).unwrap();
    let cursor = page.next_cursor.clone().unwrap();
    // Skor (ondalik nokta/eksi isareti) veya kimlik tasimaz.
    assert!(cursor.starts_with('o') && !cursor.contains('-') && !cursor.contains("mem"), "{cursor}");
    assert!(db.memory_search("ortak", Audience::Desktop, None, Some(2), Some(&cursor)).is_ok());
    // Baska sorgu / kitle / proje ve bozuk imlecler reddedilir.
    assert!(db.memory_search("baska", Audience::Desktop, None, Some(2), Some(&cursor)).is_err());
    assert!(db.memory_search("ortak", Audience::Telegram, None, Some(2), Some(&cursor)).is_err());
    assert!(db.memory_search("ortak", Audience::Desktop, Some("p"), Some(2), Some(&cursor)).is_err());
    for bad in ["1.5|x", "o9.0000000000000000", "o.abc", "oabc.def", "o99999999999.x", "zzz"] {
        assert!(db.memory_search("ortak", Audience::Desktop, None, Some(2), Some(bad)).is_err(), "{bad}");
    }
}
