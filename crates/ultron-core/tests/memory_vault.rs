//! Markdown kasa: güvenli yollar, atomik yazma, CAS, maskeleme, çökme matrisi (Aşama 5a, Task 2).
//! Yalnız sentetik veri ve geçici dizinler.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use tempfile::TempDir;
use ultron_core::Paths;
use ultron_core::memory::vault::{
    CommitKind, FaultAction, FaultPoint, NewNote, ReceiptSource, ReceiptSpec, Vault, VaultError,
};
use ultron_core::memory::{
    ClaimStatus, MemoryId, MemoryPatch, MemoryType, Revision, Sensitivity, Sha256Hash, Visibility,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 18, 9, 0, 0).unwrap()
}

struct Env {
    _tmp: TempDir,
    paths: Paths,
    root: PathBuf,
    tmp: PathBuf,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let paths = Paths::from_root(base.join("ultron"));
    let root = base.join("brain");
    Env { tmp: base, _tmp: tmp, paths, root }
}

fn open(e: &Env) -> Vault {
    Vault::open(&e.root, &e.paths).unwrap()
}

fn snap_spec(text: &str) -> ReceiptSpec {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    ReceiptSpec {
        source: ReceiptSource::Snapshot { id: format!("req-{n}-{}", std::process::id()), text: text.to_string() },
        excerpt: text.chars().take(80).collect(),
        claim_status: ClaimStatus::UserAuthored,
        harness_verified: false,
    }
}

fn new_note(id: &str, sensitivity: Sensitivity) -> NewNote {
    NewNote {
        id: Some(MemoryId::parse(id).unwrap()),
        memory_type: MemoryType::User,
        inbox: false,
        title: "Türkçe başlık İĞÜŞÖÇ ığüşöç".into(),
        summary: "Kısa özet".into(),
        body: "Gövde: çalışma saatleri sabah dokuzdur.\n".into(),
        project: None,
        tags: vec!["ornek".into()],
        aliases: vec![],
        facts: BTreeMap::new(),
        sensitivity,
        visibility: None,
        review_at: None,
    }
}

const ID1: &str = "11111111-1111-4111-8111-111111111111";
const ID2: &str = "22222222-2222-4222-8222-222222222222";

fn secret() -> String {
    format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2")
}

fn junction(link: &Path, target: &Path) {
    let st = Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
    assert!(st.status.success(), "mklink /J başarısız");
}

fn files_under(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------------------------
// Kök doğrulaması
// ---------------------------------------------------------------------------------------------

#[test]
fn open_rejects_unsafe_roots() {
    let e = env();
    // göreli
    assert!(matches!(Vault::open(Path::new("brain"), &e.paths), Err(VaultError::InvalidRoot(_))));
    // sürücü kökü
    assert!(matches!(Vault::open(Path::new("C:\\"), &e.paths), Err(VaultError::InvalidRoot(_))));
    // UNC / aygıt
    assert!(matches!(Vault::open(Path::new(r"\\server\share\x"), &e.paths), Err(VaultError::InvalidRoot(_))));
    assert!(matches!(Vault::open(Path::new(r"\\.\C:\x"), &e.paths), Err(VaultError::InvalidRoot(_))));
    // çalışma zamanı yollarıyla çakışma
    for bad in [
        e.paths.root.clone(),
        e.paths.root.join("state"),
        e.paths.root.join("state").join("sub"),
        e.paths.run.clone(),
        e.paths.transcripts.join("x"),
        e.paths.root.join("trash"),
        e.paths.root.parent().unwrap().to_path_buf(), // üst dizin
    ] {
        assert!(matches!(Vault::open(&bad, &e.paths), Err(VaultError::InvalidRoot(_))), "{bad:?}");
    }
}

#[test]
fn open_overlap_check_is_case_insensitive() {
    let e = env();
    std::fs::create_dir_all(&e.paths.run).unwrap();
    let upper = PathBuf::from(e.paths.run.to_string_lossy().to_uppercase());
    assert!(matches!(Vault::open(&upper.join("kasa"), &e.paths), Err(VaultError::InvalidRoot(_))));
}

#[test]
fn open_rejects_junction_root() {
    let e = env();
    let real = e.tmp.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = e.tmp.join("link");
    junction(&link, &real);
    assert!(matches!(Vault::open(&link, &e.paths), Err(VaultError::Reparse)));
}

#[test]
fn open_allows_default_vault_under_runtime_root() {
    let e = env();
    Vault::open(&e.paths.default_vault, &e.paths).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Roundtrip, varsayılanlar, maskeleme
// ---------------------------------------------------------------------------------------------

#[test]
fn create_roundtrips_turkish_and_derives_name_from_id() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("Kullanıcı: sabah dokuz."), now()).unwrap();
    assert_eq!(out.locator, format!("me/{ID1}.md"));
    assert_eq!(out.kind, CommitKind::Created);
    assert_eq!(out.revision.get(), 1);
    let read = v.read_note(&out.locator).unwrap();
    assert_eq!(read.front.title, "Türkçe başlık İĞÜŞÖÇ ığüşöç");
    assert!(read.body.contains("çalışma saatleri"));
    assert_eq!(read.content_hash, out.content_hash);
    assert_eq!(read.front.receipts, vec![out.receipt_hash.clone()]);
    assert!(!out.operation_id.is_empty());
}

#[test]
fn new_note_uses_default_visibility_for_sensitivity() {
    let e = env();
    let v = open(&e);
    let a = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let b = v.create_note(new_note(ID2, Sensitivity::Normal), snap_spec("b"), now()).unwrap();
    let a = v.read_note(&a.locator).unwrap().front;
    let b = v.read_note(&b.locator).unwrap().front;
    assert_eq!((a.visibility, a.remote_allowed), (Visibility::Private, false));
    assert_eq!((b.visibility, b.remote_allowed), (Visibility::Shared, true));
}

#[test]
fn inbox_and_type_dirs() {
    let e = env();
    let v = open(&e);
    let mut n = new_note(ID1, Sensitivity::Personal);
    n.inbox = true;
    assert_eq!(v.create_note(n, snap_spec("x"), now()).unwrap().locator, format!("inbox/{ID1}.md"));
    let mut n = new_note(ID2, Sensitivity::Normal);
    n.memory_type = MemoryType::Project;
    assert_eq!(v.create_note(n, snap_spec("y"), now()).unwrap().locator, format!("projects/{ID2}.md"));
}

#[test]
fn secrets_are_masked_before_any_disk_write() {
    let e = env();
    let v = open(&e);
    let s = secret();
    let mut n = new_note(ID1, Sensitivity::Personal);
    n.title = format!("Anahtar {s}");
    n.summary = format!("özet {s}");
    n.body = format!("gövde {s}\n");
    n.tags = vec![format!("t-{s}")];
    n.facts.insert("k".into(), serde_json::json!(format!("v {s}")));
    let mut spec = snap_spec(&format!("kaynak {s}"));
    spec.excerpt = format!("alıntı {s}");
    let out = v.create_note(n, spec, now()).unwrap();
    for rel in files_under(&e.root) {
        let bytes = std::fs::read(e.root.join(&rel)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&s), "sır diskte: {rel}");
    }
    assert!(v.read_note(&out.locator).unwrap().body.contains("[MASKED]"));
}

#[test]
fn secrets_in_update_patch_are_masked() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let s = secret();
    let patch = MemoryPatch { body: Some(format!("yeni {s}")), title: Some(format!("T {s}")), ..Default::default() };
    v.update_note(&out.locator, out.revision, &out.content_hash, &patch, snap_spec(&format!("k {s}")), now()).unwrap();
    for rel in files_under(&e.root) {
        let bytes = std::fs::read(e.root.join(&rel)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&s), "sır diskte: {rel}");
    }
}

#[test]
fn verified_claim_requires_harness_authority() {
    let e = env();
    let v = open(&e);
    let mut spec = snap_spec("a");
    spec.claim_status = ClaimStatus::Verified;
    assert!(matches!(v.create_note(new_note(ID1, Sensitivity::Personal), spec.clone(), now()), Err(VaultError::Unauthorized)));
    assert!(files_under(&e.root).is_empty());
    spec.harness_verified = true;
    v.create_note(new_note(ID1, Sensitivity::Personal), spec, now()).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Yol güvenliği
// ---------------------------------------------------------------------------------------------

#[test]
fn read_rejects_hostile_locators() {
    let e = env();
    let v = open(&e);
    std::fs::write(e.tmp.join("outside.md"), "x").unwrap();
    for bad in [
        "../outside.md",
        "me/../../outside.md",
        "/abs.md",
        "C:/x.md",
        "C:\\x.md",
        "me\\x.md",
        "//server/share/x.md",
        r"\\.\C:\x.md",
        "me/a:b.md",
        "me/note.md:stream",
        "me/NUL.md",
        "me/COM1.md",
        "me/x.md.",
        "daily/x.md",
        "unknown/x.md",
        "",
    ] {
        assert!(v.read_note(bad).is_err(), "kabul edildi: {bad:?}");
    }
}

#[test]
fn junction_directory_blocks_write_and_read() {
    let e = env();
    std::fs::create_dir_all(&e.root).unwrap();
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "gizli").unwrap();
    let v = open(&e);
    junction(&e.root.join("me"), &outside);
    let r = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now());
    assert!(matches!(r, Err(VaultError::Reparse)), "{r:?}");
    assert_eq!(files_under(&outside), vec!["secret.md".to_string()]);
    assert!(matches!(v.read_note("me/secret.md"), Err(VaultError::Reparse)));
}

#[test]
fn symlink_or_junction_receipts_dir_is_rejected() {
    let e = env();
    std::fs::create_dir_all(&e.root).unwrap();
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let v = open(&e);
    junction(&e.root.join("receipts"), &outside);
    let r = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now());
    assert!(matches!(r, Err(VaultError::Reparse)), "{r:?}");
    assert!(files_under(&outside).is_empty());
    assert!(!e.root.join("me").join(format!("{ID1}.md")).exists());
}

#[test]
fn hardlinked_note_is_not_written() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let alias = e.tmp.join("alias.md");
    std::fs::hard_link(e.root.join("me").join(format!("{ID1}.md")), &alias).unwrap();
    let before = std::fs::read(&alias).unwrap();
    let patch = MemoryPatch { body: Some("değişti".into()), ..Default::default() };
    let r = v.update_note(&out.locator, out.revision, &out.content_hash, &patch, snap_spec("b"), now());
    assert!(matches!(r, Err(VaultError::HardLink)), "{r:?}");
    assert_eq!(std::fs::read(&alias).unwrap(), before);
    assert_eq!(files_under(&e.root).iter().filter(|f| f.ends_with(".ultron-tmp")).count(), 0);
}

// ---------------------------------------------------------------------------------------------
// CAS ve mevcut dosya koruması
// ---------------------------------------------------------------------------------------------

#[test]
fn revision_cas_conflict_and_success() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let p = MemoryPatch { body: Some("birinci\n".into()), ..Default::default() };
    let u = v.update_note(&out.locator, out.revision, &out.content_hash, &p, snap_spec("b"), now()).unwrap();
    assert_eq!(u.revision.get(), 2);
    assert_eq!(u.previous_hash.as_ref(), Some(&out.content_hash));
    assert_eq!(u.kind, CommitKind::Updated);
    let p2 = MemoryPatch { body: Some("ikinci\n".into()), ..Default::default() };
    let r = v.update_note(&out.locator, out.revision, &out.content_hash, &p2, snap_spec("c"), now());
    assert!(matches!(r, Err(VaultError::Conflict { current_revision: Some(rev) }) if rev.get() == 2), "{r:?}");
    let read = v.read_note(&out.locator).unwrap();
    assert_eq!(read.body, "birinci\n");
    // her revision kendi makbuzunu ekler
    assert_eq!(read.front.receipts.len(), 2);
}

#[test]
fn create_never_overwrites_existing_user_file() {
    let e = env();
    let v = open(&e);
    std::fs::create_dir_all(e.root.join("me")).unwrap();
    let target = e.root.join("me").join(format!("{ID1}.md"));
    std::fs::write(&target, "kullanıcının elle yazdığı dosya").unwrap();
    let r = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now());
    assert!(matches!(r, Err(VaultError::AlreadyExists)), "{r:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "kullanıcının elle yazdığı dosya");
}

#[test]
fn external_edit_during_commit_is_conflict_not_overwrite() {
    let e = env();
    let v0 = open(&e);
    let out = v0.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let target = e.root.join("me").join(format!("{ID1}.md"));
    let t2 = target.clone();
    let hook: Arc<dyn Fn(FaultPoint) -> FaultAction + Send + Sync> = Arc::new(move |p| {
        if p == FaultPoint::AfterTempSync {
            let mut s = std::fs::read_to_string(&t2).unwrap();
            s.push_str("kullanıcı araya girdi\n");
            std::fs::write(&t2, s).unwrap();
        }
        FaultAction::Proceed
    });
    let v = open(&e).with_fault_hook(hook);
    let p = MemoryPatch { body: Some("model değişikliği\n".into()), ..Default::default() };
    let r = v.update_note(&out.locator, out.revision, &out.content_hash, &p, snap_spec("b"), now());
    assert!(matches!(r, Err(VaultError::Conflict { .. })), "{r:?}");
    assert!(std::fs::read_to_string(&target).unwrap().contains("kullanıcı araya girdi"));
    assert!(!files_under(&e.root).iter().any(|f| f.ends_with(".ultron-tmp")), "temp temizlenmedi");
}

#[test]
fn unparseable_note_cannot_be_updated_and_stays_untouched() {
    let e = env();
    let v = open(&e);
    std::fs::create_dir_all(e.root.join("me")).unwrap();
    let target = e.root.join("me").join(format!("{ID1}.md"));
    std::fs::write(&target, "---\nbozuk: [\n---\ngövde").unwrap();
    let p = MemoryPatch { body: Some("x".into()), ..Default::default() };
    let r = v.update_note(&format!("me/{ID1}.md"), Revision::new(1).unwrap(), &Sha256Hash::of(b""), &p, snap_spec("a"), now());
    assert!(r.is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "---\nbozuk: [\n---\ngövde");
}

// ---------------------------------------------------------------------------------------------
// Çökme matrisi
// ---------------------------------------------------------------------------------------------

const POINTS: [FaultPoint; 7] = [
    FaultPoint::BeforeSnapshot,
    FaultPoint::AfterSnapshot,
    FaultPoint::AfterReceipt,
    FaultPoint::AfterTempCreate,
    FaultPoint::AfterTempPartial,
    FaultPoint::AfterTempSync,
    FaultPoint::AfterRename,
];

fn hook_at(point: FaultPoint, action: FaultAction) -> Arc<dyn Fn(FaultPoint) -> FaultAction + Send + Sync> {
    Arc::new(move |p| if p == point { action } else { FaultAction::Proceed })
}

fn temps(root: &Path) -> Vec<String> {
    files_under(root).into_iter().filter(|f| f.ends_with(".ultron-tmp")).collect()
}

#[test]
fn crash_matrix_create() {
    for point in POINTS {
        let e = env();
        let v = open(&e).with_fault_hook(hook_at(point, FaultAction::Crash));
        let r = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("kaynak"), now());
        assert!(matches!(r, Err(VaultError::Injected(p)) if p == point), "{point:?}: {r:?}");
        let note = e.root.join("me").join(format!("{ID1}.md"));
        let clean = open(&e);
        if point == FaultPoint::AfterRename {
            // not kalıcı ve tam; yalnız sonuç kayboldu
            assert!(clean.read_note(&format!("me/{ID1}.md")).is_ok());
        } else {
            assert!(!note.exists(), "{point:?}: yarım not görünür");
        }
        // temp kalıntısı yalnız temp aşamalarında olur ve sweep ile temizlenir
        let has_temp = !temps(&e.root).is_empty();
        let expect_temp = matches!(point, FaultPoint::AfterTempCreate | FaultPoint::AfterTempPartial | FaultPoint::AfterTempSync);
        assert_eq!(has_temp, expect_temp, "{point:?}");
        assert_eq!(clean.sweep_temps().unwrap(), usize::from(expect_temp));
        // makbuz/kaynak yalnız yazıldıkları noktadan sonra var
        let receipts = files_under(&e.root).into_iter().filter(|f| f.starts_with("receipts/")).count();
        let expected = match point {
            FaultPoint::BeforeSnapshot => 0,
            FaultPoint::AfterSnapshot => 1,
            _ => 2,
        };
        assert_eq!(receipts, expected, "{point:?}");
        // kurtarma: yeniden deneme (AfterRename dışında) başarılı olur
        if point != FaultPoint::AfterRename {
            let again = clean.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("kaynak"), now());
            assert!(again.is_ok(), "{point:?}: {again:?}");
        }
    }
}

#[test]
fn crash_matrix_update_keeps_old_or_new_never_partial() {
    for point in POINTS {
        let e = env();
        let v0 = open(&e);
        let out = v0.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
        let before = std::fs::read(e.root.join("me").join(format!("{ID1}.md"))).unwrap();
        let v = open(&e).with_fault_hook(hook_at(point, FaultAction::Crash));
        let p = MemoryPatch { body: Some("yeni gövde\n".into()), ..Default::default() };
        let r = v.update_note(&out.locator, out.revision, &out.content_hash, &p, snap_spec("b"), now());
        assert!(matches!(r, Err(VaultError::Injected(_))), "{point:?}");
        let clean = open(&e);
        let read = clean.read_note(&out.locator).unwrap(); // her durumda ayrıştırılabilir
        if point == FaultPoint::AfterRename {
            assert_eq!(read.front.revision.get(), 2);
        } else {
            assert_eq!(std::fs::read(e.root.join("me").join(format!("{ID1}.md"))).unwrap(), before, "{point:?}");
        }
        clean.sweep_temps().unwrap();
        assert!(temps(&e.root).is_empty());
    }
}

#[test]
fn io_errors_return_error_clean_temp_and_keep_user_file() {
    for kind in [std::io::ErrorKind::StorageFull, std::io::ErrorKind::PermissionDenied] {
        for point in [FaultPoint::AfterTempCreate, FaultPoint::AfterTempPartial, FaultPoint::AfterTempSync] {
            let e = env();
            let out = open(&e).create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
            let target = e.root.join("me").join(format!("{ID1}.md"));
            let before = std::fs::read(&target).unwrap();
            let v = open(&e).with_fault_hook(hook_at(point, FaultAction::Io(kind)));
            let p = MemoryPatch { body: Some("yeni\n".into()), ..Default::default() };
            let r = v.update_note(&out.locator, out.revision, &out.content_hash, &p, snap_spec("b"), now());
            assert!(matches!(r, Err(VaultError::Io(k)) if k == kind), "{kind:?}/{point:?}: {r:?}");
            assert_eq!(std::fs::read(&target).unwrap(), before);
            assert!(temps(&e.root).is_empty(), "temp kaldı: {kind:?}/{point:?}");
            // yeni not için de sahte başarı yok
            let r = v.create_note(new_note(ID2, Sensitivity::Personal), snap_spec("c"), now());
            assert!(r.is_err());
            assert!(!e.root.join("me").join(format!("{ID2}.md")).exists());
        }
    }
}

#[test]
fn leftover_temp_files_never_look_like_notes() {
    let e = env();
    let v = open(&e);
    std::fs::create_dir_all(e.root.join("me")).unwrap();
    std::fs::write(e.root.join("me").join(".x.0123456789ab.ultron-tmp"), "---\nyarım").unwrap();
    assert_eq!(v.sweep_temps().unwrap(), 1);
}

// ---------------------------------------------------------------------------------------------
// Düzeltme turu: son-render maskeleme, hash CAS, makbuz kaynağı, süpürme, açılış sırası
// ---------------------------------------------------------------------------------------------

fn all_files_masked_clean(root: &Path) {
    for rel in files_under(root) {
        let text = String::from_utf8_lossy(&std::fs::read(root.join(&rel)).unwrap()).to_string();
        assert_eq!(ultron_core::import::mask_secrets(&text).0, text, "maskesiz desen diskte: {rel}");
    }
}

#[test]
fn combined_key_value_pattern_never_reaches_disk() {
    let e = env();
    let v = open(&e);
    let mut n = new_note(ID1, Sensitivity::Personal);
    n.facts.insert("password".into(), serde_json::json!("hunter2hunter2"));
    n.facts.insert("api_key".into(), serde_json::json!("abcdef0123456789abcdef"));
    n.tags = vec!["token".into()];
    n.summary = "password: hunter2hunter2".into();
    let r = v.create_note(n, snap_spec("secret: hunter2hunter2"), now());
    if r.is_err() {
        assert!(files_under(&e.root).is_empty(), "hata sonrası disk değişti");
    }
    all_files_masked_clean(&e.root);
}

#[test]
fn update_rejects_note_that_already_contains_secret() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let path = e.root.join("me").join(format!("{ID1}.md"));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("elle eklenen {}\n", secret()));
    std::fs::write(&path, &text).unwrap();
    let hash = Sha256Hash::of(text.as_bytes());
    let p = MemoryPatch { title: Some("Yeni".into()), ..Default::default() };
    let r = v.update_note(&out.locator, out.revision, &hash, &p, snap_spec("b"), now());
    assert!(matches!(r, Err(VaultError::Contract(_))), "{r:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert!(temps(&e.root).is_empty());
}

#[test]
fn update_requires_matching_content_hash_even_if_revision_matches() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let path = e.root.join("me").join(format!("{ID1}.md"));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("kullanıcı henüz taranmamış satır ekledi\n");
    std::fs::write(&path, &text).unwrap();
    let p = MemoryPatch { body: Some("model\n".into()), ..Default::default() };
    let r = v.update_note(&out.locator, out.revision, &out.content_hash, &p, snap_spec("b"), now());
    assert!(matches!(r, Err(VaultError::Conflict { .. })), "{r:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn commit_outcome_reports_masking() {
    let e = env();
    let v = open(&e);
    let clean = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("temiz"), now()).unwrap();
    assert!(!clean.masked);
    let mut n = new_note(ID2, Sensitivity::Personal);
    n.body = format!("gövde {}\n", secret());
    let dirty = v.create_note(n, snap_spec("temiz"), now()).unwrap();
    assert!(dirty.masked);
}

#[test]
fn existing_file_source_must_live_under_receipts_sources() {
    let e = env();
    let v = open(&e);
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), snap_spec("a"), now()).unwrap();
    let h = Sha256Hash::of(&std::fs::read(e.root.join("me").join(format!("{ID1}.md"))).unwrap());
    let spec = ReceiptSpec {
        source: ReceiptSource::Existing(ultron_core::memory::SourceRef::File { locator: out.locator.clone(), content_hash: h }),
        excerpt: "x".into(),
        claim_status: ClaimStatus::Claimed,
        harness_verified: false,
    };
    assert!(v.create_note(new_note(ID2, Sensitivity::Personal), spec, now()).is_err());
    assert!(!e.root.join("me").join(format!("{ID2}.md")).exists());
}

#[test]
fn import_record_key_is_masked_in_receipt() {
    let e = env();
    let v = open(&e);
    let s = secret();
    let spec = ReceiptSpec {
        source: ReceiptSource::Existing(ultron_core::memory::SourceRef::Import {
            importer: "claude_history".into(),
            source_hash: Sha256Hash::of(b"kaynak"),
            record_key: format!("kayit-{s}"),
        }),
        excerpt: "x".into(),
        claim_status: ClaimStatus::Claimed,
        harness_verified: false,
    };
    let out = v.create_note(new_note(ID1, Sensitivity::Personal), spec, now()).unwrap();
    assert!(out.masked);
    all_files_masked_clean(&e.root);
    for rel in files_under(&e.root) {
        assert!(!std::fs::read_to_string(e.root.join(&rel)).unwrap().contains(&s));
    }
}

#[test]
fn sweep_only_removes_our_temp_pattern_and_survives_bad_dirs() {
    let e = env();
    let v = open(&e);
    std::fs::create_dir_all(e.root.join("me")).unwrap();
    std::fs::write(e.root.join("me").join("benim.ultron-tmp"), "kullanıcı dosyası").unwrap();
    std::fs::write(e.root.join("me").join(".x.zzzzzzzzzzzz.ultron-tmp"), "desen dışı").unwrap();
    std::fs::write(e.root.join("me").join(".x.0123456789ab.ultron-tmp"), "bizim").unwrap();
    let outside = e.tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(".y.0123456789ab.ultron-tmp"), "dışarıda").unwrap();
    junction(&e.root.join("projects"), &outside);
    assert_eq!(v.sweep_temps().unwrap(), 1);
    assert!(e.root.join("me").join("benim.ultron-tmp").exists());
    assert!(e.root.join("me").join(".x.zzzzzzzzzzzz.ultron-tmp").exists());
    assert!(outside.join(".y.0123456789ab.ultron-tmp").exists());
}

#[test]
fn open_rejects_overlap_before_creating_any_directory() {
    let e = env();
    let bad = e.paths.run.join("kasa");
    assert!(matches!(Vault::open(&bad, &e.paths), Err(VaultError::InvalidRoot(_))));
    assert!(!e.paths.run.exists());
}
