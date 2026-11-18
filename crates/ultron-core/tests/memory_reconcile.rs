//! Dış değişiklik uzlaştırması: hash taraması, güvenli delta'lar, çöp/restore (Aşama 5a, Task 2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use tempfile::TempDir;
use ultron_core::Paths;
use ultron_core::memory::reconcile::{Delta, KnownEntry, SafeCode, ScanLimits};
use ultron_core::memory::vault::{NewNote, ReceiptSource, ReceiptSpec, Vault};
use ultron_core::memory::{ClaimStatus, MemoryId, MemoryType, Sensitivity};
use ultron_core::trash::Trash;

const VALID: &str = include_str!("fixtures/memory-vault/valid_note.md");
const BLOCK: &str = include_str!("fixtures/memory-vault/block_scalar.md");
const ANCHOR: &str = include_str!("fixtures/memory-vault/anchor.md");
const REMOTE_NO_VIS: &str = include_str!("fixtures/memory-vault/remote_no_visibility.md");
const PRIVATE_REMOTE: &str = include_str!("fixtures/memory-vault/private_remote.md");

const ID_VALID: &str = "11111111-1111-4111-8111-111111111111";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 18, 9, 0, 0).unwrap()
}

struct Env {
    _tmp: TempDir,
    tmp: PathBuf,
    paths: Paths,
    root: PathBuf,
    vault: Vault,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let paths = Paths::from_root(base.join("ultron"));
    let root = base.join("brain");
    let vault = Vault::open(&root, &paths).unwrap();
    Env { _tmp: tmp, tmp: base, paths, root, vault }
}

fn put(e: &Env, rel: &str, text: &str) -> PathBuf {
    let p = e.root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

fn scan(e: &Env, known: &[KnownEntry]) -> ultron_core::memory::reconcile::ScanReport {
    e.vault.scan(known, &ScanLimits::default()).unwrap()
}

fn find<'a>(deltas: &'a [Delta], rel: &str) -> &'a Delta {
    deltas.iter().find(|d| d.locator() == rel).unwrap_or_else(|| panic!("delta yok: {rel}"))
}

fn junction(link: &Path, target: &Path) {
    let st = Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
    assert!(st.status.success());
}

#[test]
fn new_valid_note_is_clean_and_listed_in_inventory() {
    let e = env();
    put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let r = scan(&e, &[]);
    let Delta::New { note } = find(&r.deltas, &format!("me/{ID_VALID}.md")) else { panic!() };
    assert!(note.body.contains("ığüşöç"));
    assert_eq!(r.inventory.len(), 1);
    // ikinci tarama: değişiklik yok
    let r2 = scan(&e, &r.inventory);
    assert!(r2.deltas.is_empty(), "{:?}", r2.deltas);
}

#[test]
fn invalid_notes_are_quarantined_or_conflict_and_raw_files_untouched() {
    let e = env();
    let cases: [(&str, &str); 4] = [
        ("me/block.md", BLOCK),
        ("me/anchor.md", ANCHOR),
        ("me/remote.md", REMOTE_NO_VIS),
        ("me/privrem.md", PRIVATE_REMOTE),
    ];
    for (rel, text) in cases {
        put(&e, rel, text);
    }
    put(&e, "me/binary.md", "x");
    std::fs::write(e.root.join("me/binary.md"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
    put(&e, "me/noyaml.md", "ön bilgi yok\n");
    let r = scan(&e, &[]);

    assert!(matches!(find(&r.deltas, "me/block.md"), Delta::Quarantined { .. }));
    assert!(matches!(find(&r.deltas, "me/anchor.md"), Delta::Quarantined { code: SafeCode::YamlAlias, .. }));
    match find(&r.deltas, "me/remote.md") {
        Delta::Conflict { code: SafeCode::RemoteWithoutVisibility, .. } => {
            let msg = SafeCode::RemoteWithoutVisibility.message_tr();
            assert!(msg.contains("visibility") && msg.contains("shared") && msg.contains("project"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(find(&r.deltas, "me/privrem.md"), Delta::Quarantined { code: SafeCode::PrivateRemote, .. }));
    assert!(matches!(find(&r.deltas, "me/binary.md"), Delta::Quarantined { code: SafeCode::NotUtf8, .. }));
    assert!(matches!(find(&r.deltas, "me/noyaml.md"), Delta::Quarantined { .. }));
    // hiçbiri indekse girmez, ham dosyalar bire bir aynı kalır
    assert!(r.inventory.is_empty());
    for (rel, text) in cases {
        assert_eq!(std::fs::read_to_string(e.root.join(rel)).unwrap(), text);
    }
    // delta'lar ham içerik taşımaz
    let dump = format!("{:?}", r.deltas);
    assert!(!dump.contains("satir bir") && !dump.contains("test örneğidir"));
}

#[test]
fn secret_in_hand_written_note_is_conflict_without_content() {
    let e = env();
    let secret = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2");
    let text = VALID.replace("Bu içerik", &format!("anahtar {secret} Bu içerik"));
    put(&e, &format!("me/{ID_VALID}.md"), &text);
    let r = scan(&e, &[]);
    assert!(matches!(find(&r.deltas, &format!("me/{ID_VALID}.md")), Delta::Conflict { code: SafeCode::SecretDetected, .. }));
    assert!(r.inventory.is_empty());
    assert!(!format!("{:?}", r.deltas).contains(&secret));
    assert!(std::fs::read_to_string(e.root.join(format!("me/{ID_VALID}.md"))).unwrap().contains(&secret));
}

#[test]
fn hash_scan_catches_same_length_same_mtime_edit() {
    let e = env();
    let p = put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let first = scan(&e, &[]);
    let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
    let edited = VALID.replace("Bu içerik", "Bu içeriK");
    assert_eq!(edited.len(), VALID.len());
    std::fs::write(&p, &edited).unwrap();
    std::fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len() as usize, VALID.len());
    let r = scan(&e, &first.inventory);
    assert!(matches!(find(&r.deltas, &format!("me/{ID_VALID}.md")), Delta::Changed { .. }), "{:?}", r.deltas);
}

#[test]
fn moved_and_deleted_notes() {
    let e = env();
    let p = put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let first = scan(&e, &[]);
    // taşıma: kimlik aynı, yeni konum
    std::fs::create_dir_all(e.root.join("projects")).unwrap();
    let moved_to = e.root.join("projects").join("tasindi.md");
    std::fs::rename(&p, &moved_to).unwrap();
    let r = scan(&e, &first.inventory);
    match find(&r.deltas, "projects/tasindi.md") {
        Delta::Moved { from, content_changed, note } => {
            assert_eq!(from, &format!("me/{ID_VALID}.md"));
            assert!(!content_changed);
            assert_eq!(note.front.id.as_str(), ID_VALID);
        }
        other => panic!("{other:?}"),
    }
    assert!(!r.deltas.iter().any(|d| matches!(d, Delta::Deleted { .. })));
    // dış silme
    std::fs::remove_file(&moved_to).unwrap();
    let r2 = scan(&e, &r.inventory);
    assert!(matches!(find(&r2.deltas, "projects/tasindi.md"), Delta::Deleted { .. }));
}

#[test]
fn duplicate_ids_conflict_and_are_not_merged() {
    let e = env();
    put(&e, &format!("me/{ID_VALID}.md"), VALID);
    put(&e, "projects/kopya.md", VALID);
    let r = scan(&e, &[]);
    assert!(matches!(find(&r.deltas, &format!("me/{ID_VALID}.md")), Delta::Conflict { code: SafeCode::DuplicateId, .. }));
    assert!(matches!(find(&r.deltas, "projects/kopya.md"), Delta::Conflict { code: SafeCode::DuplicateId, .. }));
    assert!(r.inventory.is_empty());
}

#[test]
fn id_changed_in_place_is_conflict() {
    let e = env();
    let p = put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let first = scan(&e, &[]);
    std::fs::write(&p, VALID.replace(ID_VALID, "99999999-9999-4999-8999-999999999999")).unwrap();
    let r = scan(&e, &first.inventory);
    assert!(matches!(find(&r.deltas, &format!("me/{ID_VALID}.md")), Delta::Conflict { code: SafeCode::IdMismatch, .. }));
}

#[test]
fn scan_is_bounded_and_does_not_report_deletes_when_truncated() {
    let e = env();
    put(&e, &format!("me/{ID_VALID}.md"), VALID);
    put(&e, "me/bozuk1.md", "x");
    put(&e, "me/bozuk2.md", "y");
    let known = vec![KnownEntry {
        locator: "me/eskidenvar.md".into(),
        content_hash: ultron_core::memory::Sha256Hash::of(b"eski"),
        id: Some(MemoryId::new()),
    }];
    let r = e.vault.scan(&known, &ScanLimits { max_files: 1, ..ScanLimits::default() }).unwrap();
    assert!(r.truncated);
    assert_eq!(r.files_seen, 1);
    assert!(!r.deltas.iter().any(|d| matches!(d, Delta::Deleted { .. })));
    let full = scan(&e, &known);
    assert!(!full.truncated);
    assert!(matches!(find(&full.deltas, "me/eskidenvar.md"), Delta::Deleted { .. }));
}

#[test]
fn non_notes_temp_files_are_ignored_and_junction_is_quarantined() {
    let e = env();
    put(&e, "me/not.txt", "yok say");
    put(&e, "me/.x.abc.ultron-tmp", "---\nyarım");
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("dis.md"), VALID).unwrap();
    junction(&e.root.join("projects"), &outside);
    let r = scan(&e, &[]);
    assert!(matches!(find(&r.deltas, "projects"), Delta::Quarantined { code: SafeCode::Reparse, .. }));
    assert!(!r.deltas.iter().any(|d| d.locator().contains("dis.md")));
    assert!(r.inventory.is_empty());
    assert_eq!(r.skipped_non_note, 2); // .txt + .ultron-tmp
}

fn receipt_spec(id: &str) -> ReceiptSpec {
    ReceiptSpec {
        source: ReceiptSource::Snapshot { id: id.into(), text: "kaynak".into() },
        excerpt: "kaynak".into(),
        claim_status: ClaimStatus::UserAuthored,
        harness_verified: false,
    }
}

#[test]
fn adopt_external_edit_bumps_revision_adds_receipt_and_snapshot() {
    let e = env();
    let p = put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let first = scan(&e, &[]);
    std::fs::write(&p, VALID.replace("Bu içerik", "Elle düzenlenmiş içerik")).unwrap();
    let r = scan(&e, &first.inventory);
    let Delta::Changed { note, previous_hash } = find(&r.deltas, &format!("me/{ID_VALID}.md")) else { panic!() };
    assert_eq!(previous_hash, &first.inventory[0].content_hash);
    let out = e.vault.adopt_external(&note.locator, Some((note.front.revision, previous_hash.clone())), now()).unwrap().unwrap();
    assert_eq!(out.revision.get(), 2);
    let read = e.vault.read_note(&note.locator).unwrap();
    assert!(read.body.contains("Elle düzenlenmiş"));
    assert_eq!(read.front.receipts.len(), 1);
    assert!(e.vault.verify_receipts(&read.front).is_empty());
    // adopt sonrası tarama yalnız Changed (yeni hash) görür; Ultron yazımı dış değişiklik sayılmaz
    // çağıran envanteri günceller:
    let known = vec![KnownEntry { locator: note.locator.clone(), content_hash: out.content_hash.clone(), id: Some(out.id.clone()) }];
    assert!(scan(&e, &known).deltas.is_empty());
}

#[test]
fn adopt_refuses_unclean_files_and_leaves_them_untouched() {
    let e = env();
    for (rel, text) in [("me/a.md", BLOCK), ("me/b.md", REMOTE_NO_VIS), ("me/c.md", PRIVATE_REMOTE)] {
        let p = put(&e, rel, text);
        assert!(e.vault.adopt_external(rel, None, now()).is_err(), "{rel}");
        assert_eq!(std::fs::read_to_string(p).unwrap(), text);
    }
    assert!(!e.root.join("receipts").exists() || std::fs::read_dir(e.root.join("receipts")).unwrap().next().is_none());
}

#[test]
fn ultron_delete_goes_to_trash_and_restore_yields_delta() {
    let e = env();
    let n = NewNote {
        id: Some(MemoryId::parse(ID_VALID).unwrap()),
        memory_type: MemoryType::User,
        inbox: false,
        title: "Silinecek".into(),
        summary: String::new(),
        body: "gövde\n".into(),
        project: None,
        tags: vec![],
        aliases: vec![],
        facts: BTreeMap::new(),
        sensitivity: Sensitivity::Personal,
        visibility: None,
        review_at: None,
    };
    let out = e.vault.create_note(n, receipt_spec("req-1"), now()).unwrap();
    let known = vec![KnownEntry { locator: out.locator.clone(), content_hash: out.content_hash.clone(), id: Some(out.id.clone()) }];
    let before = std::fs::read(e.root.join(&out.locator)).unwrap();

    let trash = Trash::new(&e.paths);
    let del = e.vault.delete_to_trash(&trash, &out.locator, None).unwrap();
    assert!(!e.root.join(&out.locator).exists());
    assert_eq!(trash.list().unwrap().len(), 1);
    assert_eq!(del.last_hash, out.content_hash);
    let r = scan(&e, &known);
    assert!(matches!(find(&r.deltas, &out.locator), Delta::Deleted { .. }));

    let rel = e.vault.restore_from_trash(&trash, &del.trash_id).unwrap();
    assert_eq!(rel, out.locator);
    assert_eq!(std::fs::read(e.root.join(&rel)).unwrap(), before);
    let d = e.vault.restore_delta(&rel, &known).unwrap();
    assert!(matches!(d, Delta::Restored { .. }), "{d:?}");
    assert!(trash.list().unwrap().is_empty());
}

#[test]
fn restore_rejects_entries_from_outside_the_vault() {
    let e = env();
    let outside = e.tmp.join("baska.md");
    std::fs::write(&outside, "x").unwrap();
    let trash = Trash::new(&e.paths);
    let entry = trash.put(&outside, None).unwrap();
    assert!(e.vault.restore_from_trash(&trash, &entry.id).is_err());
    assert_eq!(trash.list().unwrap().len(), 1);
}

#[test]
fn delete_refuses_hostile_locators() {
    let e = env();
    let trash = Trash::new(&e.paths);
    std::fs::write(e.tmp.join("dis.md"), "x").unwrap();
    for bad in ["../dis.md", "receipts/x.md", "me/yok.md"] {
        assert!(e.vault.delete_to_trash(&trash, bad, None).is_err(), "{bad}");
    }
    assert!(e.tmp.join("dis.md").exists());
}

// ---------------------------------------------------------------------------------------------
// Düzeltme turu: numaralandırma hatası "silindi" üretmez, bütçeler, makbuz denetimi
// ---------------------------------------------------------------------------------------------

fn mk_known(locator: &str, id: &str) -> KnownEntry {
    KnownEntry {
        locator: locator.into(),
        content_hash: ultron_core::memory::Sha256Hash::of(locator.as_bytes()),
        id: Some(MemoryId::parse(id).unwrap()),
    }
}

fn mk_note(e: &Env, id: &str) -> ultron_core::memory::vault::CommitOutcome {
    let n = NewNote {
        id: Some(MemoryId::parse(id).unwrap()),
        memory_type: MemoryType::User,
        inbox: false,
        title: "Not".into(),
        summary: String::new(),
        body: "gövde\n".into(),
        project: None,
        tags: vec![],
        aliases: vec![],
        facts: BTreeMap::new(),
        sensitivity: Sensitivity::Personal,
        visibility: None,
        review_at: None,
    };
    e.vault.create_note(n, receipt_spec(&format!("req-{id}")), now()).unwrap()
}

const ID_B: &str = "66666666-6666-4666-8666-666666666666";
const ID_C: &str = "77777777-7777-4777-8777-777777777777";

#[test]
fn junction_dir_marks_prefix_incomplete_and_never_yields_deleted_under_it() {
    let e = env();
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    junction(&e.root.join("projects"), &outside);
    std::fs::create_dir_all(e.root.join("home")).unwrap();
    let known = vec![
        mk_known("projects/a.md", ID_B),
        mk_known("projects/alt/b.md", ID_C),
        mk_known("home/gone.md", "88888888-8888-4888-8888-888888888888"),
    ];
    let r = scan(&e, &known);
    assert!(r.truncated);
    assert!(matches!(find(&r.deltas, "projects"), Delta::Quarantined { code: SafeCode::Reparse, .. }));
    assert!(!r.deltas.iter().any(|d| matches!(d, Delta::Deleted { locator, .. } if locator.starts_with("projects"))));
    // tam numaralandırılmış kapsamda silme yine raporlanır
    assert!(matches!(find(&r.deltas, "home/gone.md"), Delta::Deleted { .. }));
}

#[test]
fn depth_cut_and_entry_cut_suppress_deleted_under_prefix() {
    let e = env();
    put(&e, "me/a/b/derin.md", "x");
    put(&e, "inbox/x1.md", "x");
    put(&e, "inbox/x2.md", "x");
    let known = vec![mk_known("me/a/b/derin.md", ID_B), mk_known("inbox/eski.md", ID_C)];
    let r = e.vault.scan(&known, &ScanLimits { max_depth: 1, max_entries_per_dir: 1, ..ScanLimits::default() }).unwrap();
    assert!(r.truncated);
    assert!(!r.deltas.iter().any(|d| matches!(d, Delta::Deleted { .. })), "{:?}", r.deltas);
}

#[test]
fn truncated_scan_never_classifies_duplicate_as_moved() {
    let e = env();
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    junction(&e.root.join("people"), &outside);
    put(&e, "home/kopya.md", &VALID.replace(ID_VALID, ID_B));
    let known = vec![mk_known("people/asil.md", ID_B)];
    let r = scan(&e, &known);
    assert!(r.truncated);
    assert!(matches!(find(&r.deltas, "home/kopya.md"), Delta::Conflict { code: SafeCode::DuplicateId, .. }), "{:?}", r.deltas);
    assert!(r.inventory.is_empty());
}

#[test]
fn byte_budget_and_delta_cap_truncate_without_false_deletes_or_inventory() {
    let e = env();
    put(&e, "me/a.md", &VALID.replace(ID_VALID, ID_B));
    put(&e, "me/b.md", &VALID.replace(ID_VALID, ID_C));
    let known = vec![mk_known("me/yok.md", "88888888-8888-4888-8888-888888888888")];
    let r = e.vault.scan(&known, &ScanLimits { max_total_bytes: 1, ..ScanLimits::default() }).unwrap();
    assert!(r.truncated);
    assert!(!r.deltas.iter().any(|d| matches!(d, Delta::Deleted { .. })));
    let r = e.vault.scan(&[], &ScanLimits { max_deltas: 1, ..ScanLimits::default() }).unwrap();
    assert!(r.truncated);
    assert_eq!(r.deltas.len(), 1);
    assert_eq!(r.inventory.len(), 1, "taşan delta'nın envanter satırı kalmamalı");
    assert_eq!(r.inventory[0].locator, r.deltas[0].locator());
}

#[test]
fn unchanged_note_with_missing_or_tampered_receipt_is_reported() {
    let e = env();
    let out = mk_note(&e, ID_B);
    let known = vec![KnownEntry { locator: out.locator.clone(), content_hash: out.content_hash.clone(), id: Some(out.id.clone()) }];
    assert!(scan(&e, &known).deltas.is_empty());
    let rpath = e.root.join("receipts").join(format!("{}.json", out.receipt_hash.hex()));
    std::fs::write(&rpath, b"{}").unwrap();
    let r = scan(&e, &known);
    assert!(matches!(find(&r.deltas, &out.locator), Delta::ReceiptIssues { .. }), "{:?}", r.deltas);
    std::fs::remove_file(&rpath).unwrap();
    let r = scan(&e, &known);
    assert!(matches!(find(&r.deltas, &out.locator), Delta::ReceiptIssues { issues, .. }
        if issues[0].1 == ultron_core::memory::receipts::ReceiptIssue::Missing));
    let off = e.vault.scan(&known, &ScanLimits { verify_unchanged_receipts: false, ..ScanLimits::default() }).unwrap();
    assert!(off.deltas.is_empty());
}

#[test]
fn adopt_is_idempotent_when_known_hash_matches_disk() {
    let e = env();
    let p = put(&e, &format!("me/{ID_VALID}.md"), VALID);
    let first = scan(&e, &[]);
    std::fs::write(&p, VALID.replace("Bu içerik", "Düzenlendi")).unwrap();
    let rev = ultron_core::memory::Revision::new(1).unwrap();
    let loc = format!("me/{ID_VALID}.md");
    let out = e.vault.adopt_external(&loc, Some((rev, first.inventory[0].content_hash.clone())), now()).unwrap().unwrap();
    assert_eq!(out.revision.get(), 2);
    // çağıran artık yeni hash'i biliyor: ikinci çağrı değişiklik yapmaz
    let again = e.vault.adopt_external(&loc, Some((out.revision, out.content_hash.clone())), now()).unwrap();
    assert!(again.is_none());
    assert_eq!(e.vault.read_note(&loc).unwrap().front.revision.get(), 2);
}

#[test]
fn restore_validates_target_before_restoring() {
    let e = env();
    let trash = Trash::new(&e.paths);
    let bad = put(&e, "receipts/sentetik.json", "{}");
    let entry = trash.put(&bad, None).unwrap();
    assert!(e.vault.restore_from_trash(&trash, &entry.id).is_err());
    assert!(!bad.exists(), "doğrulama başarısızken geri yükleme yapılmamalı");
    assert_eq!(trash.list().unwrap().len(), 1);
    let txt = put(&e, "me/duz.txt", "x");
    let entry2 = trash.put(&txt, None).unwrap();
    assert!(e.vault.restore_from_trash(&trash, &entry2.id).is_err());
    assert!(!txt.exists());
    assert_eq!(trash.list().unwrap().len(), 2);
}

#[test]
fn restore_failure_after_restore_puts_file_back_into_trash() {
    let e = env();
    let trash = Trash::new(&e.paths);
    // Ön doğrulama geçer (konum <= 260 bayt) ama ad çakışınca trash'in eklediği ek ("... (geri
    // yüklendi 1)") konumu sınırın üstüne çıkarır: geri yükleme SONRASI doğrulama başarısız olur.
    let rel = format!("me/{}.md", "a".repeat(242));
    assert!(rel.len() <= 260 && rel.len() + 19 > 260);
    let note = put(&e, &rel, "birinci
");
    let entry = trash.put(&note, None).unwrap();
    std::fs::write(&note, "yeni
").unwrap();
    assert!(e.vault.restore_from_trash(&trash, &entry.id).is_err());
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "yeni
", "dolu dosyaya dokunulmaz");
    let count = std::fs::read_dir(note.parent().unwrap()).unwrap().count();
    assert_eq!(count, 1, "kasada artık dosya kalmamalı");
    let listed = trash.list().unwrap();
    assert_eq!(listed.len(), 1, "geri alınan dosya çöp kaydında kalmalı");
    // Engel kalkınca kayıt açık hedefle geri yüklenebilir.
    std::fs::remove_file(&note).unwrap();
    trash.restore(&listed[0].id, Some(&note)).unwrap();
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "birinci
");
}

#[test]
fn default_scan_limits_have_a_time_budget() {
    assert_eq!(ScanLimits::default().time_budget, Some(std::time::Duration::from_secs(30)));
}
