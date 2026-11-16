use std::path::Path;

use ultron_core::Db;
use ultron_core::ipc::{ProjectInfo, RouteResult};
use ultron_core::projects::{import_catalog, parse_catalog};
use ultron_core::routing::route;

fn project(db: &Db, id: &str, name: &str, path: &Path, aliases: &[&str]) {
    db.upsert_project(&ProjectInfo {
        id: id.into(),
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        aliases: aliases.iter().map(|s| (*s).into()).collect(),
        description: None,
        last_active_at: None,
    })
    .unwrap();
}

#[test]
fn catalog_parses_and_skips_bad_lines() {
    let seeds = parse_catalog(include_str!("fixtures/catalog/projects.md"));
    assert_eq!(seeds.len(), 3);
    assert_eq!(seeds[0].name, "Ödeme");
    assert_eq!(seeds[0].path, "workspace/odeme");
    assert_eq!(seeds[0].description, "Ödeme akışı");
    assert_eq!(seeds[1].name, "Atölye");
    assert_eq!(seeds[2].name, "İletişim");
}

#[test]
fn route_explicit_name_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    project(&db, "a", "Ödeme", &tmp.path().join("a"), &[]);
    project(&db, "b", "Atölye", &tmp.path().join("b"), &[]);
    let r = route(&db, "Ödeme akışını düzelt", Some("b")).unwrap();
    assert_eq!(r.project_id.as_deref(), Some("a"));
    assert_eq!(r.confidence, 0.9);
    assert!(!r.reason.is_empty());
    assert!(r.alternatives.is_empty());
}

#[test]
fn route_ambiguous_returns_alternatives() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    project(&db, "a", "Ödeme", &tmp.path().join("a"), &["tahsilat"]);
    project(&db, "b", "Atölye", &tmp.path().join("b"), &[]);
    let r = route(&db, "Ödeme ve tahsilat; Atölye ile bağla", None).unwrap();
    assert_eq!(r.project_id.as_deref(), Some("a"));
    assert_eq!(r.alternatives, ["b"]);
    assert_eq!(r.confidence, 0.9);
    let tied = route(&db, "Atölye ve Ödeme", None).unwrap();
    assert_eq!(tied.project_id.as_deref(), Some("a"));
    assert_eq!(tied.alternatives, ["b"]);
}

#[test]
fn route_falls_back_to_last_active() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    project(&db, "a", "Ödeme", &tmp.path().join("a"), &[]);
    let r = route(&db, "devam edelim", Some("a")).unwrap();
    assert_eq!(r.project_id.as_deref(), Some("a"));
    assert_eq!(r.confidence, 0.5);
    assert_eq!(route(&db, "devam", None).unwrap().project_id, None);
    let stale = route(&db, "devam", Some("silinmis")).unwrap();
    assert_eq!(stale.project_id, None);
    assert_eq!(stale.confidence, 0.0);
}

#[test]
fn route_turkish_case_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    project(
        &db,
        "a",
        "ödeme",
        &tmp.path().join("a"),
        &["iletişim", "ışık"],
    );
    for text in ["ÖDEME", "İLETİŞİM", "IŞIK", "(ödeme)"] {
        assert_eq!(
            route(&db, text, None).unwrap().project_id.as_deref(),
            Some("a"),
            "{text}"
        );
    }
    for text in ["ödemeler", "önödeme", "x_ödeme", "ödeme2"] {
        assert_eq!(route(&db, text, None).unwrap().project_id, None, "{text}");
    }
}

#[test]
fn route_matches_folder_alias_and_top_level_entries_only() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    let root = tmp.path().join("sentetik-klasor");
    std::fs::create_dir_all(root.join("bilesenler")).unwrap();
    std::fs::write(root.join("deneme.rs"), "sentetik").unwrap();
    std::fs::write(root.join("bilesenler/gizli-alt-dosya.rs"), "sentetik").unwrap();
    project(&db, "a", "Ödeme", &root, &["kasa"]);
    for text in ["sentetik-klasor", "KASA", "deneme.rs", "bilesenler"] {
        assert_eq!(
            route(&db, text, None).unwrap().project_id.as_deref(),
            Some("a"),
            "{text}"
        );
    }
    for text in ["gizli-alt-dosya.rs", "deneme.rsbackup", "kasalar"] {
        assert_eq!(route(&db, text, None).unwrap().project_id, None, "{text}");
    }
    project(&db, "b", "Yok", &tmp.path().join("bulunamayan"), &[]);
    assert_eq!(
        route(&db, "Yok", None).unwrap().project_id.as_deref(),
        Some("b")
    );
}

#[test]
fn catalog_import_is_idempotent_and_preserves_activity_and_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("db")).unwrap();
    let md = "- **Ödeme** — `workspace/odeme` — İlk açıklama";
    let first = import_catalog(&db, md).unwrap();
    assert_eq!((first.imported, first.updated), (1, 0));
    let mut p = db.list_projects().unwrap().remove(0);
    p.aliases = vec!["kasa".into()];
    p.last_active_at = Some("2026-11-16T09:00:00Z".into());
    db.upsert_project(&p).unwrap();
    let next = import_catalog(&db, &md.replace("İlk", "Yeni")).unwrap();
    assert_eq!((next.imported, next.updated), (0, 1));
    let projects = db.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, p.id);
    assert_eq!(projects[0].aliases, p.aliases);
    assert_eq!(projects[0].last_active_at, p.last_active_at);
    assert_eq!(projects[0].description.as_deref(), Some("Yeni açıklama"));
}

#[test]
fn route_result_round_trips_over_ipc() {
    let r = RouteResult {
        project_id: Some("p1".into()),
        confidence: 0.9,
        reason: "açık eşleşme".into(),
        alternatives: vec!["p2".into()],
    };
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(serde_json::from_value::<RouteResult>(json).unwrap(), r);
}
