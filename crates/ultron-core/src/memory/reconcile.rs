//! Dış değişiklik uzlaştırması: sınırlı tam envanter ve güvenli delta'lar (Aşama 5a, Task 2).
//!
//! `Vault::scan` salt okunurdur: kullanıcı dosyalarını asla değiştirmez/silmez. Her dosya
//! içerik hash'i (SHA-256) ile karşılaştırılır; boyut/zaman damgası kullanılmaz. Çözümlenemeyen,
//! sır içeren ya da çakışan dosyaların ham içeriği hiçbir delta'da dönmez; yalnız güvenli bir
//! kod ve Türkçe kullanıcı mesajı döner.
//!
//! Delta türleri: `New`, `Changed`, `Moved`, `Deleted`, `Restored` (temiz içerik taşır),
//! `Conflict` (çözümlenebilir ama kullanıcı kararı gerekir), `Quarantined` (geçersiz/okunamaz).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use super::contracts::{
    ContractError, FrontMatter, MemoryId, Sha256Hash, check_text, parse_note, split_note,
};
use super::receipts::ReceiptIssue;
use super::vault::{MAX_NOTE_FILE_BYTES, NOTE_DIRS, Vault, VaultError, VaultResult};

/// Çağıranın (DB) bildiği önceki envanter satırı.
#[derive(Debug, Clone)]
pub struct KnownEntry {
    pub locator: String,
    pub content_hash: Sha256Hash,
    pub id: Option<MemoryId>,
}

/// Tarama bütçeleri. Herhangi biri aşılırsa rapor `truncated` olur ve (kapsam dışı kalan
/// yollar için) `Deleted` üretilmez.
#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    /// Taranacak en çok not dosyası.
    pub max_files: usize,
    pub max_depth: usize,
    /// Tek dizinde okunacak en çok girdi (akış halinde sayılır).
    pub max_entries_per_dir: usize,
    pub max_dirs: usize,
    /// Toplam okunacak bayt.
    pub max_total_bytes: u64,
    /// Döndürülecek en çok delta; fazlası sonraki taramaya kalır.
    pub max_deltas: usize,
    pub time_budget: Option<Duration>,
    /// Değişmemiş notların makbuz zincirini de (varlık + hash) denetle.
    pub verify_unchanged_receipts: bool,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits {
            max_files: 5000,
            max_depth: 6,
            max_entries_per_dir: 5000,
            max_dirs: 2000,
            max_total_bytes: 128 * 1024 * 1024,
            max_deltas: 2000,
            time_budget: Some(Duration::from_secs(30)),
            verify_unchanged_receipts: true,
        }
    }
}

/// İçerik taşımayan, kapalı hata kodu kümesi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeCode {
    InvalidYaml,
    YamlAlias,
    YamlTag,
    UnknownSchema,
    InvalidField,
    TooLarge,
    TooDeep,
    NotUtf8,
    Reparse,
    InvalidLocator,
    PrivateRemote,
    RemoteWithoutVisibility,
    DuplicateId,
    IdMismatch,
    SecretDetected,
    Unreadable,
}

impl SafeCode {
    pub fn as_str(self) -> &'static str {
        match self {
            SafeCode::InvalidYaml => "invalid_yaml",
            SafeCode::YamlAlias => "yaml_alias",
            SafeCode::YamlTag => "yaml_tag",
            SafeCode::UnknownSchema => "unknown_schema",
            SafeCode::InvalidField => "invalid_field",
            SafeCode::TooLarge => "too_large",
            SafeCode::TooDeep => "too_deep",
            SafeCode::NotUtf8 => "not_utf8",
            SafeCode::Reparse => "reparse",
            SafeCode::InvalidLocator => "invalid_locator",
            SafeCode::PrivateRemote => "private_remote",
            SafeCode::RemoteWithoutVisibility => "remote_without_visibility",
            SafeCode::DuplicateId => "duplicate_id",
            SafeCode::IdMismatch => "id_mismatch",
            SafeCode::SecretDetected => "secret_detected",
            SafeCode::Unreadable => "unreadable",
        }
    }

    /// Kullanıcıya gösterilecek Türkçe mesaj.
    pub fn message_tr(self) -> &'static str {
        match self {
            SafeCode::InvalidYaml => "Notun ön bilgisi (YAML) çözümlenemedi; not indekslenmedi ve dosyaya dokunulmadı.",
            SafeCode::YamlAlias => "Ön bilgide YAML takma adı/çapası var; desteklenmediği için not indekslenmedi.",
            SafeCode::YamlTag => "Ön bilgide YAML etiketi var; desteklenmediği için not indekslenmedi.",
            SafeCode::UnknownSchema => "Notun şema sürümü bilinmiyor; not indekslenmedi.",
            SafeCode::InvalidField => "Ön bilgide geçersiz bir alan veya tür var; not indekslenmedi.",
            SafeCode::TooLarge => "Not boyut sınırını aşıyor; indekslenmedi.",
            SafeCode::TooDeep => "Ön bilgi yapısı çok derin; not indekslenmedi.",
            SafeCode::NotUtf8 => "Dosya geçerli UTF-8 değil; not indekslenmedi.",
            SafeCode::Reparse => "Sembolik bağlantı veya junction bulundu; güvenlik nedeniyle izlenmedi.",
            SafeCode::InvalidLocator => "Dosya adı güvenli bir kasa yolu değil; indekslenmedi.",
            SafeCode::PrivateRemote => "Not hem özel (visibility: private) hem uzaktan erişime açık (remote_allowed: true) görünüyor; bu birleşim geçersiz, not indekslenmedi.",
            SafeCode::RemoteWithoutVisibility => "Notta `remote_allowed: true` var ancak `visibility` yok. Uzaktan erişime izin vermek için `visibility` alanını açıkça `shared` veya `project` olarak ayarlayın; aksi halde not özel kalır ve indekslenmedi.",
            SafeCode::DuplicateId => "Aynı kimliğe sahip birden fazla not var; otomatik birleştirilmedi. Yinelenen dosyalardan birini düzeltin veya taşıyın.",
            SafeCode::IdMismatch => "Dosyanın kimliği bu konumda kayıtlı kimlikle uyuşmuyor; kimlik değişmez, çakışma olarak işaretlendi.",
            SafeCode::SecretDetected => "Notta gizli bilgi benzeri bir desen bulundu; içerik indekse alınmadı. Gizli bilgiyi dosyadan kaldırın.",
            SafeCode::Unreadable => "Dosya okunamadı; indekslenmedi.",
        }
    }
}

fn code_of(e: ContractError) -> SafeCode {
    match e {
        ContractError::YamlAlias => SafeCode::YamlAlias,
        ContractError::YamlTag => SafeCode::YamlTag,
        ContractError::UnknownSchemaVersion => SafeCode::UnknownSchema,
        ContractError::TooLarge | ContractError::LimitExceeded => SafeCode::TooLarge,
        ContractError::TooDeep => SafeCode::TooDeep,
        ContractError::InvalidField | ContractError::InvalidId | ContractError::InvalidRevision
        | ContractError::InvalidHash | ContractError::InvalidLocator | ContractError::InvalidSeq => {
            SafeCode::InvalidField
        }
        _ => SafeCode::InvalidYaml,
    }
}

/// İndekse verilebilecek temiz not (ayrıştırılmış, sırsız, çakışmasız).
#[derive(Clone)]
pub struct CleanNote {
    pub locator: String,
    pub front: FrontMatter,
    pub body: String,
    pub content_hash: Sha256Hash,
    /// Makbuz zinciri sorunları (eksik/tahrif); not yine de görünür, sorun işaretlidir.
    pub receipt_issues: Vec<(Sha256Hash, ReceiptIssue)>,
}

impl fmt::Debug for CleanNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CleanNote")
            .field("id", &self.front.id)
            .field("content_hash", &self.content_hash)
            .field("receipt_issues", &self.receipt_issues.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum Delta {
    New { note: CleanNote },
    Changed { previous_hash: Sha256Hash, note: CleanNote },
    Moved { from: String, note: CleanNote, content_changed: bool },
    Deleted { locator: String, id: Option<MemoryId>, last_hash: Sha256Hash },
    /// Ultron çöpünden geri yüklenen not.
    Restored { note: CleanNote },
    Conflict { locator: String, id: Option<MemoryId>, code: SafeCode, content_hash: Option<Sha256Hash> },
    /// Değişmemiş bir notun makbuz zincirinde eksik/tahrif var.
    ReceiptIssues { locator: String, id: Option<MemoryId>, issues: Vec<(Sha256Hash, ReceiptIssue)> },
    Quarantined { locator: String, code: SafeCode, content_hash: Option<Sha256Hash> },
}

impl Delta {
    pub fn locator(&self) -> &str {
        match self {
            Delta::New { note } | Delta::Changed { note, .. } | Delta::Moved { note, .. } | Delta::Restored { note } => {
                &note.locator
            }
            Delta::Deleted { locator, .. }
            | Delta::Conflict { locator, .. }
            | Delta::Quarantined { locator, .. }
            | Delta::ReceiptIssues { locator, .. } => locator,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub deltas: Vec<Delta>,
    /// Temiz notların yeni envanteri (çağıran sonraki `known` olarak saklayabilir).
    pub inventory: Vec<KnownEntry>,
    pub files_seen: usize,
    /// Envanter tam değil (bütçe aşıldı, dizin okunamadı/derin/reparse). Eksik kapsamda `Deleted`
    /// ve güvenilir `Moved` üretilmez.
    pub truncated: bool,
    pub skipped_non_note: usize,
    pub skipped_invalid_names: usize,
}

enum Parsed {
    Clean(Box<CleanNote>),
    Bad(Box<Delta>),
}

struct Item {
    locator: String,
    hash: Option<Sha256Hash>,
    id: Option<MemoryId>,
    parsed: Option<Parsed>,
    unchanged: bool,
}

/// Ham ön bilgi satırlarına bakarak yalnız sınıflandırma yapar; metni dışarı vermez.
fn classify_visibility(text: &str) -> Option<SafeCode> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (yaml, _) = split_note(text).ok()?;
    let mut remote_true = false;
    let mut vis: Option<String> = None;
    for line in yaml.lines() {
        if let Some(v) = line.strip_prefix("remote_allowed:") {
            remote_true = v.trim().trim_matches(['"', '\'']).eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("visibility:") {
            vis = Some(v.trim().trim_matches(['"', '\'']).to_ascii_lowercase());
        }
    }
    match (remote_true, vis.as_deref()) {
        (true, None) => Some(SafeCode::RemoteWithoutVisibility),
        (true, Some("private")) => Some(SafeCode::PrivateRemote),
        _ => None,
    }
}

fn classify(
    vault: &Vault,
    cache: &mut HashMap<Sha256Hash, Option<ReceiptIssue>>,
    locator: &str,
    bytes: &[u8],
) -> (Sha256Hash, Parsed) {
    let hash = Sha256Hash::of(bytes);
    let bad = |code: SafeCode, conflict: bool| {
        let delta = if conflict {
            Delta::Conflict { locator: locator.to_string(), id: None, code, content_hash: Some(hash.clone()) }
        } else {
            Delta::Quarantined { locator: locator.to_string(), code, content_hash: Some(hash.clone()) }
        };
        Parsed::Bad(Box::new(delta))
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (hash.clone(), bad(SafeCode::NotUtf8, false));
    };
    match parse_note(text) {
        Err(e) => {
            if let Some(code) = classify_visibility(text) {
                return (hash.clone(), bad(code, code == SafeCode::RemoteWithoutVisibility));
            }
            let _ = check_text; // yalnız belge amaçlı: ham metin sınıflandırmadan dışarı çıkmaz
            (hash.clone(), bad(code_of(e), false))
        }
        Ok((front, body)) => {
            if crate::import::mask_secrets(text).0 != text {
                return (hash.clone(), bad(SafeCode::SecretDetected, true));
            }
            let receipt_issues = vault.verify_receipts_cached(&front, cache);
            let note = CleanNote { locator: locator.to_string(), front, body, content_hash: hash.clone(), receipt_issues };
            (hash, Parsed::Clean(Box::new(note)))
        }
    }
}

fn under_prefix(key: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| key == p || key.starts_with(&format!("{p}/")))
}

impl Vault {
    /// Sınırlı tam envanter; `known` ile karşılaştırıp delta üretir. Salt okunur.
    pub fn scan(&self, known: &[KnownEntry], limits: &ScanLimits) -> VaultResult<ScanReport> {
        let started = Instant::now();
        let over_time = || limits.time_budget.is_some_and(|b| started.elapsed() > b);
        let mut report = ScanReport::default();
        let mut items: Vec<Item> = Vec::new();
        let mut present: BTreeSet<String> = BTreeSet::new();
        // Tamamlanamayan yol önekleri (küçük harf); altındaki bilinen notlar "silindi" sayılmaz.
        let mut incomplete: Vec<String> = Vec::new();
        let mut global_incomplete = false;
        let by_loc: HashMap<String, &KnownEntry> = known.iter().map(|k| (k.locator.to_lowercase(), k)).collect();
        let mut receipt_cache: HashMap<Sha256Hash, Option<ReceiptIssue>> = HashMap::new();

        // 1) Numaralandırma (sıralı, sınırlı, akış).
        let mut stack: Vec<(String, usize)> = NOTE_DIRS.iter().rev().map(|d| (d.to_string(), 0)).collect();
        let mut dirs_visited = 0usize;
        'walk: while let Some((dir, depth)) = stack.pop() {
            if over_time() || dirs_visited >= limits.max_dirs {
                global_incomplete = true;
                break;
            }
            dirs_visited += 1;
            let (entries, cut) = match self.read_dir_limited(&dir, limits.max_entries_per_dir) {
                Ok(r) => r,
                Err(VaultError::Reparse) => {
                    report.deltas.push(Delta::Quarantined { locator: dir.clone(), code: SafeCode::Reparse, content_hash: None });
                    incomplete.push(dir.to_lowercase());
                    continue;
                }
                Err(_) => {
                    incomplete.push(dir.to_lowercase());
                    continue;
                }
            };
            if cut {
                incomplete.push(dir.to_lowercase());
            }
            let mut subdirs = Vec::new();
            for e in entries {
                let locator = format!("{dir}/{}", e.name);
                if super::contracts::validate_locator(&locator).is_err() {
                    report.skipped_invalid_names += 1;
                    continue;
                }
                if e.reparse {
                    // Reparse girdi "mevcut" sayılır (silinmiş değil), yalnız karantinaya alınır.
                    if e.is_dir {
                        incomplete.push(locator.to_lowercase());
                    } else {
                        present.insert(locator.to_lowercase());
                    }
                    report.deltas.push(Delta::Quarantined { locator, code: SafeCode::Reparse, content_hash: None });
                    continue;
                }
                if e.is_dir {
                    if depth < limits.max_depth {
                        subdirs.push((locator, depth + 1));
                    } else {
                        incomplete.push(locator.to_lowercase());
                    }
                    continue;
                }
                if !e.name.ends_with(".md") {
                    report.skipped_non_note += 1;
                    continue;
                }
                if report.files_seen >= limits.max_files {
                    global_incomplete = true;
                    break 'walk;
                }
                report.files_seen += 1;
                present.insert(locator.to_lowercase());
                if e.size > MAX_NOTE_FILE_BYTES {
                    report.deltas.push(Delta::Quarantined { locator: locator.clone(), code: SafeCode::TooLarge, content_hash: None });
                    continue;
                }
                items.push(Item { locator, hash: None, id: None, parsed: None, unchanged: false });
            }
            for s in subdirs.into_iter().rev() {
                stack.push(s);
            }
        }

        // 2) Okuma + hash + (değiştiyse) çözümleme; bayt/süre bütçesi.
        let mut bytes_total = 0u64;
        let mut bad_deltas = Vec::new();
        for item in &mut items {
            if over_time() || bytes_total >= limits.max_total_bytes {
                global_incomplete = true;
                break;
            }
            match self.read_checked(&item.locator, MAX_NOTE_FILE_BYTES) {
                Ok((bytes, _)) => {
                    bytes_total += bytes.len() as u64;
                    let hash = Sha256Hash::of(&bytes);
                    if let Some(k) = by_loc.get(&item.locator.to_lowercase())
                        && k.content_hash == hash
                        && k.id.is_some()
                    {
                        item.hash = Some(hash);
                        item.id = k.id.clone();
                        item.unchanged = true;
                        if limits.verify_unchanged_receipts
                            && let Ok(text) = std::str::from_utf8(&bytes)
                            && let Ok((front, _)) = parse_note(text)
                        {
                            let issues = self.verify_receipts_cached(&front, &mut receipt_cache);
                            if !issues.is_empty() {
                                bad_deltas.push(Delta::ReceiptIssues {
                                    locator: item.locator.clone(),
                                    id: item.id.clone(),
                                    issues,
                                });
                            }
                        }
                        continue;
                    }
                    let (hash, parsed) = classify(self, &mut receipt_cache, &item.locator, &bytes);
                    item.hash = Some(hash);
                    match parsed {
                        Parsed::Clean(n) => {
                            item.id = Some(n.front.id.clone());
                            item.parsed = Some(Parsed::Clean(n));
                        }
                        Parsed::Bad(d) => item.parsed = Some(Parsed::Bad(d)),
                    }
                }
                Err(VaultError::Reparse) => bad_deltas.push(Delta::Quarantined {
                    locator: item.locator.clone(),
                    code: SafeCode::Reparse,
                    content_hash: None,
                }),
                Err(VaultError::TooLarge) => bad_deltas.push(Delta::Quarantined {
                    locator: item.locator.clone(),
                    code: SafeCode::TooLarge,
                    content_hash: None,
                }),
                Err(_) => bad_deltas.push(Delta::Quarantined {
                    locator: item.locator.clone(),
                    code: SafeCode::Unreadable,
                    content_hash: None,
                }),
            }
        }
        report.deltas.extend(bad_deltas);
        report.truncated = global_incomplete || !incomplete.is_empty();

        // 3) Yinelenen kimlikler.
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, it) in items.iter().enumerate() {
            if let Some(id) = &it.id {
                groups.entry(id.as_str().to_string()).or_default().push(i);
            }
        }
        let mut dup: BTreeSet<usize> = BTreeSet::new();
        for idxs in groups.values().filter(|v| v.len() > 1) {
            dup.extend(idxs.iter().copied());
        }
        let known_by_id: HashMap<&str, &KnownEntry> =
            known.iter().filter_map(|k| k.id.as_ref().map(|id| (id.as_str(), k))).collect();
        let mut moved_from: BTreeSet<String> = BTreeSet::new();

        // 4) Delta üretimi.
        for (i, it) in items.iter().enumerate() {
            let Some(hash) = it.hash.clone() else { continue };
            if dup.contains(&i) {
                report.deltas.push(Delta::Conflict {
                    locator: it.locator.clone(),
                    id: it.id.clone(),
                    code: SafeCode::DuplicateId,
                    content_hash: Some(hash),
                });
                continue;
            }
            if let Some(id) = &it.id {
                report.inventory.push(KnownEntry { locator: it.locator.clone(), content_hash: hash.clone(), id: Some(id.clone()) });
            }
            if it.unchanged {
                continue;
            }
            match it.parsed.as_ref() {
                Some(Parsed::Bad(d)) => report.deltas.push((**d).clone()),
                Some(Parsed::Clean(note)) => {
                    let note = (**note).clone();
                    if let Some(k) = by_loc.get(&it.locator.to_lowercase()) {
                        if k.id.as_ref().is_some_and(|kid| kid != &note.front.id) {
                            report.inventory.pop();
                            report.deltas.push(Delta::Conflict {
                                locator: it.locator.clone(),
                                id: k.id.clone(),
                                code: SafeCode::IdMismatch,
                                content_hash: Some(hash),
                            });
                        } else {
                            report.deltas.push(Delta::Changed { previous_hash: k.content_hash.clone(), note });
                        }
                    } else if let Some(k) = known_by_id.get(note.front.id.as_str()) {
                        // Eksik envanterde eski konumun gerçekten yok olduğu bilinemez: Moved yerine
                        // yinelenen kimlik çakışması (kimlik çiftlenmesin).
                        if report.truncated || present.contains(&k.locator.to_lowercase()) {
                            report.inventory.pop();
                            report.deltas.push(Delta::Conflict {
                                locator: it.locator.clone(),
                                id: Some(note.front.id.clone()),
                                code: SafeCode::DuplicateId,
                                content_hash: Some(hash),
                            });
                        } else {
                            moved_from.insert(k.locator.to_lowercase());
                            let content_changed = k.content_hash != note.content_hash;
                            report.deltas.push(Delta::Moved { from: k.locator.clone(), note, content_changed });
                        }
                    } else {
                        report.deltas.push(Delta::New { note });
                    }
                }
                None => {}
            }
        }

        // 5) Silinenler: yalnız tam numaralandırılmış kapsamda.
        if !global_incomplete {
            for k in known {
                let key = k.locator.to_lowercase();
                if !present.contains(&key) && !moved_from.contains(&key) && !under_prefix(&key, &incomplete) {
                    report.deltas.push(Delta::Deleted {
                        locator: k.locator.clone(),
                        id: k.id.clone(),
                        last_hash: k.content_hash.clone(),
                    });
                }
            }
        }
        report.deltas.sort_by(|a, b| a.locator().cmp(b.locator()));
        report.inventory.sort_by(|a, b| a.locator.cmp(&b.locator));
        // Delta tavanı: taşan delta'ların envanter satırları da çıkarılır (çağıran indekslemediği
        // bir notu "biliniyor" saymasın).
        if report.deltas.len() > limits.max_deltas {
            let dropped = report.deltas.split_off(limits.max_deltas);
            let gone: BTreeSet<String> = dropped.iter().map(|d| d.locator().to_lowercase()).collect();
            report.inventory.retain(|k| !gone.contains(&k.locator.to_lowercase()));
            report.truncated = true;
        }
        Ok(report)
    }

    /// Ultron çöpünden geri yüklenen notun delta'sı: temiz ise `Restored`, değilse
    /// `Conflict`/`Quarantined`. `known`, geri yüklenen konumdaki eski kaydı içermemelidir.
    pub fn restore_delta(&self, restored_locator: &str, known: &[KnownEntry]) -> VaultResult<Delta> {
        let known: Vec<KnownEntry> =
            known.iter().filter(|k| !k.locator.eq_ignore_ascii_case(restored_locator)).cloned().collect();
        let report = self.scan(&known, &ScanLimits::default())?;
        for d in report.deltas {
            if !d.locator().eq_ignore_ascii_case(restored_locator) {
                continue;
            }
            return Ok(match d {
                Delta::New { note } | Delta::Changed { note, .. } | Delta::Moved { note, .. } => Delta::Restored { note },
                other => other,
            });
        }
        Err(VaultError::NotFound)
    }
}
