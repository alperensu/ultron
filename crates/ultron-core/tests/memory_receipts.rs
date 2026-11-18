//! Makbuz ve kaynak görüntüsü: değişmezlik, idempotans, tahrif tespiti (Aşama 5a, Task 2).

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use tempfile::TempDir;
use ultron_core::Paths;
use ultron_core::memory::receipts::ReceiptIssue;
use ultron_core::memory::vault::{NewNote, ReceiptSource, ReceiptSpec, Vault, VaultError};
use ultron_core::memory::{
    ClaimStatus, MemoryId, MemoryPatch, MemoryType, Receipt, Sensitivity, Sha256Hash, SourceRef,
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 18, 9, 0, 0).unwrap()
}

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    vault: Vault,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let paths = Paths::from_root(base.join("ultron"));
    let root = base.join("brain");
    let vault = Vault::open(&root, &paths).unwrap();
    Env { _tmp: tmp, root, vault }
}

fn spec(id: &str, text: &str) -> ReceiptSpec {
    ReceiptSpec {
        source: ReceiptSource::Snapshot { id: id.into(), text: text.into() },
        excerpt: text.into(),
        claim_status: ClaimStatus::UserAuthored,
        harness_verified: false,
    }
}

fn note(id: &str) -> NewNote {
    NewNote {
        id: Some(MemoryId::parse(id).unwrap()),
        memory_type: MemoryType::User,
        inbox: false,
        title: "Makbuz testi".into(),
        summary: String::new(),
        body: "gövde\n".into(),
        project: None,
        tags: vec![],
        aliases: vec![],
        facts: BTreeMap::new(),
        sensitivity: Sensitivity::Personal,
        visibility: None,
        review_at: None,
    }
}

const ID1: &str = "11111111-1111-4111-8111-111111111111";
const ID2: &str = "22222222-2222-4222-8222-222222222222";

#[test]
fn every_revision_has_readable_source_snapshot_and_receipt() {
    let e = env();
    let out = e.vault.create_note(note(ID1), spec("req-1", "Kullanıcı yazdı: kahve severim."), now()).unwrap();
    let p = MemoryPatch { body: Some("güncel\n".into()), ..Default::default() };
    let up = e.vault.update_note(&out.locator, out.revision, &out.content_hash, &p, spec("req-2", "Düzeltme: çay."), now()).unwrap();
    let front = e.vault.read_note(&out.locator).unwrap().front;
    assert_eq!(front.receipts, vec![out.receipt_hash.clone(), up.receipt_hash.clone()]);
    for h in &front.receipts {
        let r = e.vault.read_receipt(h).unwrap();
        let SourceRef::File { locator, content_hash } = &r.source else { panic!("File bekleniyordu") };
        let text = e.vault.read_source_snapshot(locator, content_hash).unwrap();
        assert!(text.contains("Kullanıcı") || text.contains("Düzeltme"));
    }
    assert!(e.vault.verify_receipts(&front).is_empty());
}

#[test]
fn receipt_file_is_canonical_json_named_by_its_hash() {
    let e = env();
    let out = e.vault.create_note(note(ID1), spec("req-1", "metin"), now()).unwrap();
    let path = e.root.join("receipts").join(format!("{}.json", out.receipt_hash.hex()));
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(Sha256Hash::of(&bytes), out.receipt_hash);
    let r = Receipt::from_canonical_json(&bytes, &out.receipt_hash).unwrap();
    assert_eq!(r.claim_status, ClaimStatus::UserAuthored);
    assert!(String::from_utf8(bytes).unwrap().starts_with("{\"version\":1,"));
}

#[test]
fn same_receipt_is_idempotent() {
    let e = env();
    let a = e.vault.create_note(note(ID1), spec("req-1", "aynı"), now()).unwrap();
    // aynı kaynak/alıntı/zaman, başka not: makbuz ve kaynak yeniden kullanılır
    let b = e.vault.create_note(note(ID2), spec("req-1", "aynı"), now()).unwrap();
    assert_eq!(a.receipt_hash, b.receipt_hash);
    assert!(a.receipt_created);
    assert!(!b.receipt_created);
}

#[test]
fn existing_receipt_with_different_content_is_never_overwritten() {
    let e = env();
    let a = e.vault.create_note(note(ID1), spec("req-1", "aynı"), now()).unwrap();
    let path = e.root.join("receipts").join(format!("{}.json", a.receipt_hash.hex()));
    std::fs::write(&path, b"{\"tahrif\":true}").unwrap();
    let r = e.vault.create_note(note(ID2), spec("req-1", "aynı"), now());
    assert!(matches!(r, Err(VaultError::Tampered)), "{r:?}");
    // bozuk dosya karantinaya taşındı (üzerine yazılmadı), not yazılmadı
    assert!(!path.exists());
    let q: Vec<_> = std::fs::read_dir(e.root.join("receipts/quarantine")).unwrap().flatten().collect();
    assert_eq!(q.len(), 1);
    assert_eq!(std::fs::read(q[0].path()).unwrap(), b"{\"tahrif\":true}");
    assert!(!e.root.join("me").join(format!("{ID2}.md")).exists());
    // yuva boşaldı: yeniden deneme başarılı ve doğru makbuzu yazar
    let again = e.vault.create_note(note(ID2), spec("req-1", "aynı"), now()).unwrap();
    assert!(again.receipt_created);
    assert!(e.vault.read_receipt(&a.receipt_hash).is_ok());
}

#[test]
fn snapshot_id_reuse_with_different_text_is_rejected() {
    let e = env();
    e.vault.create_note(note(ID1), spec("req-1", "ilk metin"), now()).unwrap();
    let r = e.vault.create_note(note(ID2), spec("req-1", "başka metin"), now());
    assert!(matches!(r, Err(VaultError::Tampered)), "{r:?}");
    let snap = std::fs::read_to_string(e.root.join("receipts/sources/req-1.md")).unwrap();
    assert_eq!(snap, "ilk metin");
}

#[test]
fn tampered_missing_receipts_and_sources_are_reported() {
    let e = env();
    let out = e.vault.create_note(note(ID1), spec("req-1", "metin"), now()).unwrap();
    let front = e.vault.read_note(&out.locator).unwrap().front;
    let rpath = e.root.join("receipts").join(format!("{}.json", out.receipt_hash.hex()));
    let spath = e.root.join("receipts/sources/req-1.md");

    std::fs::write(&spath, "tahrif").unwrap();
    assert_eq!(e.vault.verify_receipts(&front)[0].1, ReceiptIssue::SourceTampered);
    std::fs::remove_file(&spath).unwrap();
    assert_eq!(e.vault.verify_receipts(&front)[0].1, ReceiptIssue::SourceMissing);

    let original = std::fs::read(&rpath).unwrap();
    std::fs::write(&rpath, b"{}").unwrap();
    assert_eq!(e.vault.verify_receipts(&front)[0].1, ReceiptIssue::Tampered);
    std::fs::remove_file(&rpath).unwrap();
    assert_eq!(e.vault.verify_receipts(&front)[0].1, ReceiptIssue::Missing);
    std::fs::write(&rpath, original).unwrap();
}

#[test]
fn existing_file_source_must_match_its_hash() {
    let e = env();
    let out = e.vault.create_note(note(ID1), spec("req-1", "metin"), now()).unwrap();
    let receipt = e.vault.read_receipt(&out.receipt_hash).unwrap();
    let SourceRef::File { locator, content_hash } = receipt.source else { panic!() };
    // doğru hash: kabul
    let ok = ReceiptSpec {
        source: ReceiptSource::Existing(SourceRef::File { locator: locator.clone(), content_hash }),
        excerpt: "e".into(),
        claim_status: ClaimStatus::Claimed,
        harness_verified: false,
    };
    e.vault.create_note(note(ID2), ok, now()).unwrap();
    // yanlış hash: ret, not yok
    let bad = ReceiptSpec {
        source: ReceiptSource::Existing(SourceRef::File { locator, content_hash: Sha256Hash::of(b"baska") }),
        excerpt: "e".into(),
        claim_status: ClaimStatus::Claimed,
        harness_verified: false,
    };
    let id3 = "33333333-3333-4333-8333-333333333333";
    assert!(matches!(e.vault.create_note(note(id3), bad, now()), Err(VaultError::Tampered)));
    assert!(!e.root.join("me").join(format!("{id3}.md")).exists());
}

#[test]
fn event_source_receipt_carries_masked_excerpt_only() {
    let e = env();
    let secret = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2");
    let s = ReceiptSpec {
        source: ReceiptSource::Existing(SourceRef::Event { chat_id: "chat-1".into(), seq_from: 1, seq_to: 3 }),
        excerpt: format!("söz {secret}"),
        claim_status: ClaimStatus::Claimed,
        harness_verified: false,
    };
    let out = e.vault.create_note(note(ID1), s, now()).unwrap();
    let r = e.vault.read_receipt(&out.receipt_hash).unwrap();
    assert!(!r.excerpt.contains(&secret));
    assert!(r.excerpt.contains("[MASKED]"));
}

#[test]
fn receipt_reads_reject_traversal_in_snapshot_locator() {
    let e = env();
    assert!(e.vault.read_source_snapshot("me/x.md", &Sha256Hash::of(b"")).is_err());
    assert!(e.vault.read_source_snapshot("receipts/sources/../../x.md", &Sha256Hash::of(b"")).is_err());
}
