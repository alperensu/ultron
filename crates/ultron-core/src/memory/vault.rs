//! Markdown kasa: güvenli okuma, atomik yazma, revision CAS ve Ultron çöpüne silme (Aşama 5a, Task 2).
//!
//! # Dosya düzeni
//! ```text
//! <kasa>/me|projects|home|tools|people|inbox/<id>.md   not (YAML ön bilgi + gövde)
//! <kasa>/receipts/<sha256>.json                         değişmez, kanonik JSON makbuz
//! <kasa>/receipts/sources/<istek-kimliği>.md            değişmez, maskelenmiş kaynak görüntüsü
//! <kasa>/**/.<ad>.<işlem>.ultron-tmp                    yarım yazım; `sweep_temps` temizler
//! ```
//! Not dosya adı doğrulanmış kimlikten türetilir (`<uuid>.md`), başlıktan değil.
//!
//! # Yazım sırası (her revision)
//! 1. Bellekte: maskeleme, ön bilgi/gövde/makbuz kurulumu ve doğrulama (diske dokunmaz).
//! 2. Kaynak görüntüsü (varsa) ve makbuz: temp + sync + değiştirmesiz taşıma; mevcut aynı hash
//!    farklı içerikse `Tampered` (üzerine yazılmaz).
//! 3. Not: aynı dizinde temp + `sync_all`, sonra atomik taşıma (yeni not: değiştirmesiz;
//!    güncelleme: değiştirmeli, önce içerik hash'i yeniden doğrulanır).
//! 4. `CommitOutcome` (işlem kimliği ile) döner; DB indeksi çağıranındır.
//!
//! # Kalan yarış (TOCTOU) sınırı
//! Yol bileşenleri `symlink_metadata` ile, açılan dosya tutamağı ise `FILE_FLAG_OPEN_REPARSE_POINT`
//! ve `GetFinalPathNameByHandle` ile denetlenir; ancak denetim ile ardından gelen taşıma
//! arasında aynı Windows hesabındaki başka bir süreç bir üst dizini bağlantıyla değiştirebilir.
//! Bu OS izolasyonu iddiası değildir. Güncellemede yarış penceresi "hash yeniden doğrulama,
//! sonra rename" arasındaki mikro aralıktır; bu aralıktaki dış düzenleme ezilir, ancak ezilen
//! sürümün hash'i `CommitOutcome::previous_hash` ile döner. Tek yazıcı kilidi çağırana aittir.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::contracts::{
    ClaimStatus, ContractError, FrontMatter, MAX_BODY_BYTES, MAX_CAPTURE_TEXT_BYTES,
    MAX_FRONT_MATTER_BYTES, MAX_RECEIPTS_PER_ITEM, MemoryId, MemoryPatch, MemoryType, Receipt,
    Revision, SCHEMA_VERSION, Sensitivity, Sha256Hash, SourceRef, Visibility,
    check_visibility_pair, default_visibility_for, parse_note, render_note, validate_locator,
    validate_request_id,
};
use crate::Paths;
use crate::trash::Trash;

/// Not dosyalarının yaşadığı üst dizinler (`receipts/` ayrıdır; `daily/` 5b'dir).
pub const NOTE_DIRS: [&str; 6] = ["me", "projects", "home", "tools", "people", "inbox"];
const ALLOWED_TOPS: [&str; 7] = ["me", "projects", "home", "tools", "people", "inbox", "receipts"];
pub const TEMP_SUFFIX: &str = ".ultron-tmp";
/// Okunacak bir not dosyasının üst sınırı.
pub const MAX_NOTE_FILE_BYTES: u64 = (MAX_BODY_BYTES + MAX_FRONT_MATTER_BYTES + 64) as u64;

// ---------------------------------------------------------------------------------------------
// Hata, hata enjeksiyonu
// ---------------------------------------------------------------------------------------------

/// İçerik/yol taşımayan kasa hataları.
#[derive(Debug)]
pub enum VaultError {
    Contract(ContractError),
    InvalidRoot(&'static str),
    /// Yol kasa dışına çıkıyor ya da geçersiz konumda.
    PathEscape,
    /// Sembolik bağlantı, junction veya başka reparse noktası.
    Reparse,
    /// Birden fazla sert bağlantısı olan not dosyası yazılamaz.
    HardLink,
    NotFound,
    AlreadyExists,
    NotAFile,
    TooLarge,
    NotUtf8,
    /// `expected_revision`/içerik hash'i güncel dosyayla uyuşmuyor (son yazan kazanmaz).
    Conflict { current_revision: Option<Revision> },
    /// Mevcut makbuz/kaynak dosyası beklenen içerikle uyuşmuyor; üzerine yazılmadı.
    Tampered,
    /// Bu işlem için yetki yok (ör. yetkisiz `Verified`).
    Unauthorized,
    Io(io::ErrorKind),
    /// Test/hata enjeksiyonu "çökme" noktası.
    Injected(FaultPoint),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VaultError::Contract(e) => write!(f, "sözleşme hatası: {e}"),
            VaultError::InvalidRoot(why) => write!(f, "geçersiz kasa kökü: {why}"),
            VaultError::PathEscape => f.write_str("yol kasa dışına çıkıyor"),
            VaultError::Reparse => f.write_str("sembolik bağlantı/junction (reparse) reddedildi"),
            VaultError::HardLink => f.write_str("sert bağlantılı not dosyası yazılamaz"),
            VaultError::NotFound => f.write_str("dosya bulunamadı"),
            VaultError::AlreadyExists => f.write_str("dosya zaten var"),
            VaultError::NotAFile => f.write_str("düz dosya değil"),
            VaultError::TooLarge => f.write_str("dosya boyut sınırını aşıyor"),
            VaultError::NotUtf8 => f.write_str("dosya UTF-8 değil"),
            VaultError::Conflict { .. } => f.write_str("revision/içerik çakışması"),
            VaultError::Tampered => f.write_str("makbuz/kaynak dosyası beklenen içerikle uyuşmuyor"),
            VaultError::Unauthorized => f.write_str("bu işlem için yetki yok"),
            VaultError::Io(kind) => write!(f, "G/Ç hatası: {kind}"),
            VaultError::Injected(p) => write!(f, "enjekte edilen hata: {p:?}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<ContractError> for VaultError {
    fn from(e: ContractError) -> Self {
        VaultError::Contract(e)
    }
}

impl From<io::Error> for VaultError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => VaultError::NotFound,
            io::ErrorKind::AlreadyExists => VaultError::AlreadyExists,
            kind => VaultError::Io(kind),
        }
    }
}

pub type VaultResult<T> = Result<T, VaultError>;

/// Yazım hattındaki enjeksiyon noktaları (nokta "ulaşıldı" anlamındadır).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeSnapshot,
    AfterSnapshot,
    AfterReceipt,
    AfterTempCreate,
    AfterTempPartial,
    AfterTempSync,
    AfterRename,
}

/// Enjeksiyon kararı: `Crash` süreç ölümünü taklit eder (temizlik yok); `Io` sıradan bir hata
/// (disk dolu, izin) taklit eder ve temp dosya temizlenir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    Proceed,
    Crash,
    Io(io::ErrorKind),
}

pub type FaultHook = Arc<dyn Fn(FaultPoint) -> FaultAction + Send + Sync>;

// ---------------------------------------------------------------------------------------------
// Windows yardımcıları
// ---------------------------------------------------------------------------------------------

mod sys {
    use super::*;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    pub struct Info {
        pub attributes: u32,
        pub links: u32,
    }

    pub fn info(file: &File) -> io::Result<Info> {
        // SAFETY: tutamak `file` tarafından açık tutuluyor; yapı sıfırlanmış ve boyutu doğru.
        let mut raw: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut raw) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Info { attributes: raw.dwFileAttributes, links: raw.nNumberOfLinks })
    }

    pub fn final_path(file: &File) -> io::Result<String> {
        let mut buf = vec![0u16; 1024];
        loop {
            // SAFETY: `buf` yazılabilir ve uzunluğu bildirilen kadar.
            let n = unsafe {
                GetFinalPathNameByHandleW(file.as_raw_handle() as HANDLE, buf.as_mut_ptr(), buf.len() as u32, 0)
            } as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                return Ok(String::from_utf16_lossy(&buf[..n]));
            }
            buf.resize(n + 1, 0);
        }
    }

    /// Atomik taşıma; `replace=false` ise hedef varsa `AlreadyExists` (değiştirmez).
    pub fn move_file(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        let wide = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let (a, b) = (wide(from), wide(to));
        let flags = MOVEFILE_WRITE_THROUGH | if replace { MOVEFILE_REPLACE_EXISTING } else { 0 };
        // SAFETY: iki NUL sonlu geçerli UTF-16 tamponu.
        let ok = unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), flags) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // ERROR_ALREADY_EXISTS (183) / ERROR_FILE_EXISTS (80)
            if matches!(err.raw_os_error(), Some(183) | Some(80)) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            return Err(err);
        }
        Ok(())
    }
}

fn is_reparse_attrs(attrs: u32) -> bool {
    attrs & sys::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Karşılaştırma için normalleştirilmiş (öneksiz, küçük harfli, `/` ayraçlı) yol.
fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// `inner`, `outer` ile aynı ya da altında mı (bileşen sınırında, büyük/küçük harf duyarsız)?
pub fn path_within(outer: &Path, inner: &Path) -> bool {
    let (o, i) = (norm_path(outer), norm_path(inner));
    i == o || i.starts_with(&format!("{o}/"))
}

fn overlaps(a: &Path, b: &Path) -> bool {
    path_within(a, b) || path_within(b, a)
}

/// Var olan en yakın üst dizini kanonikleştirir, kalanı ekler (henüz oluşmamış yollar için).
fn canonical_or_abs(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.clone();
    loop {
        if let Ok(c) = fs::canonicalize(&cur) {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent;
            }
            _ => return abs,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Veri tipleri
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NoteRead {
    pub locator: String,
    pub front: FrontMatter,
    pub body: String,
    pub content_hash: Sha256Hash,
}

/// Makbuz için kaynak belirtimi.
#[derive(Clone)]
pub enum ReceiptSource {
    /// Çağıranın verdiği kaynak. `File` ise dosya var ve hash'i eşleşmelidir.
    Existing(SourceRef),
    /// Maskelenip `receipts/sources/<id>.md` olarak değişmez yazılacak kaynak görüntüsü.
    Snapshot { id: String, text: String },
}

#[derive(Clone)]
pub struct ReceiptSpec {
    pub source: ReceiptSource,
    pub excerpt: String,
    pub claim_status: ClaimStatus,
    /// `Verified` yalnız harness atayabilir; model yolundan `true` verilmez.
    pub harness_verified: bool,
}

#[derive(Clone)]
pub struct NewNote {
    /// Verilmezse üretilir. Dosya adı bu kimlikten türetilir.
    pub id: Option<MemoryId>,
    pub memory_type: MemoryType,
    /// `true`: `inbox/` (hızlı yakalama), aksi halde türün dizini.
    pub inbox: bool,
    pub title: String,
    pub summary: String,
    pub body: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub facts: BTreeMap<String, Value>,
    pub sensitivity: Sensitivity,
    /// `None`: `default_visibility_for(sensitivity)`.
    pub visibility: Option<(Visibility, bool)>,
    pub review_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    Created,
    Updated,
    /// Kullanıcının dış düzenlemesi revision/makbuzla uzlaştırıldı.
    Adopted,
}

/// Disk commit sonucu: DB indeks adımı için. Not gövdesi taşımaz.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub operation_id: String,
    pub kind: CommitKind,
    pub locator: String,
    pub id: MemoryId,
    pub revision: Revision,
    pub content_hash: Sha256Hash,
    pub previous_hash: Option<Sha256Hash>,
    pub receipt_hash: Sha256Hash,
    pub receipt_created: bool,
    /// Maskeleme girdi metinlerinden en az birini değiştirdi (içerik kullanıcının verdiğinden farklı).
    pub masked: bool,
}

#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    pub trash_id: String,
    pub locator: String,
    pub id: Option<MemoryId>,
    pub last_hash: Sha256Hash,
}

#[derive(Debug, Clone)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub reparse: bool,
    pub size: u64,
}

// ---------------------------------------------------------------------------------------------
// Vault
// ---------------------------------------------------------------------------------------------

pub struct Vault {
    root: PathBuf,
    hook: Option<FaultHook>,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault { .. }")
    }
}

fn mask(s: &str) -> String {
    crate::import::mask_secrets(s).0
}

/// Maskelemenin içeriği değiştirip değiştirmediğini izler.
#[derive(Default)]
struct Masker {
    changed: bool,
}

impl Masker {
    fn s(&mut self, x: &str) -> String {
        let m = mask(x);
        if m != x {
            self.changed = true;
        }
        m
    }

    fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.s(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (self.s(k), self.value(v))).collect()),
            other => other.clone(),
        }
    }

    fn map(&mut self, m: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        m.iter().map(|(k, v)| (self.s(k), self.value(v))).collect()
    }
}

/// Makbuz kaynağındaki serbest metinleri (içe aktarma kayıt anahtarı) maskeler.
fn mask_source(src: &SourceRef, mk: &mut Masker) -> SourceRef {
    match src {
        SourceRef::Import { importer, source_hash, record_key } => SourceRef::Import {
            importer: importer.clone(),
            source_hash: source_hash.clone(),
            record_key: mk.s(record_key),
        },
        other => other.clone(),
    }
}

/// Ultron temp dosya adı deseni: `.<ad>.<12 küçük hex>.ultron-tmp`.
fn is_our_temp_name(name: &str) -> bool {
    let Some(inner) = name.strip_prefix('.').and_then(|n| n.strip_suffix(TEMP_SUFFIX)) else {
        return false;
    };
    let Some((stem, op)) = inner.rsplit_once('.') else { return false };
    !stem.is_empty() && op.len() == 12 && op.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn type_dir(t: MemoryType) -> &'static str {
    match t {
        MemoryType::User | MemoryType::Rule => "me",
        MemoryType::Project | MemoryType::Decision => "projects",
        MemoryType::Home => "home",
        MemoryType::Tool => "tools",
        MemoryType::Person => "people",
    }
}

impl Vault {
    /// Kasayı açar (yoksa oluşturur). `runtime` Ultron çalışma zamanı yollarıdır; kasa bunlarla
    /// çakışamaz. Kök kanonikleştirilir; sürücü kökü, UNC/aygıt yolu ve reparse noktası reddedilir.
    ///
    /// Çakışma kuralı: kasa; çalışma zamanı kökünün (`root`) kendisi ya da üstü olamaz; `state/`
    /// (db), `run/`, `transcripts/`, `fixtures-private/` ve `trash/` ile aynı olamaz, bunların
    /// üstü ya da altı olamaz. Kasa `Paths::root` altında bunlardan ayrı bir dizin olabilir
    /// (`default_vault = root/vault` test kurulumu böyle çalışır).
    pub fn open(root: &Path, runtime: &Paths) -> VaultResult<Vault> {
        if !root.is_absolute() {
            return Err(VaultError::InvalidRoot("mutlak yol gerekli"));
        }
        if let Some(Component::Prefix(p)) = root.components().next()
            && !matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        {
            return Err(VaultError::InvalidRoot("UNC/aygıt yolu"));
        }
        if let Ok(meta) = fs::symlink_metadata(root)
            && is_reparse_attrs(meta.file_attributes())
        {
            return Err(VaultError::Reparse);
        }
        Self::check_overlap(&canonical_or_abs(root), runtime)?;
        fs::create_dir_all(root)?;
        let canon = fs::canonicalize(root)?;
        match canon.components().next() {
            Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::VerbatimDisk(_) | Prefix::Disk(_)) => {}
            _ => return Err(VaultError::InvalidRoot("UNC/aygıt yolu")),
        }
        if !canon.components().any(|c| matches!(c, Component::Normal(_))) {
            return Err(VaultError::InvalidRoot("sürücü kökü"));
        }
        Self::check_overlap(&canon, runtime)?;
        Ok(Vault { root: canon, hook: None })
    }

    fn check_overlap(canon: &Path, runtime: &Paths) -> VaultResult<()> {
        let runtime_root = canonical_or_abs(&runtime.root);
        if path_within(canon, &runtime_root) {
            return Err(VaultError::InvalidRoot("Ultron çalışma zamanı köküyle çakışıyor"));
        }
        let forbidden = [
            runtime.db.parent().map(Path::to_path_buf).unwrap_or_else(|| runtime.root.join("state")),
            runtime.db.clone(),
            runtime.run.clone(),
            runtime.transcripts.clone(),
            runtime.fixtures_private.clone(),
            runtime.root.join("trash"),
        ];
        for f in forbidden {
            if overlaps(canon, &canonical_or_abs(&f)) {
                return Err(VaultError::InvalidRoot("Ultron çalışma zamanı yollarıyla çakışıyor"));
            }
        }
        Ok(())
    }

    /// Hata enjeksiyonu (testler). Üretimde kullanılmaz.
    pub fn with_fault_hook(mut self, hook: FaultHook) -> Vault {
        self.hook = Some(hook);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn fault(&self, point: FaultPoint) -> VaultResult<()> {
        match self.hook.as_ref().map(|h| h(point)).unwrap_or(FaultAction::Proceed) {
            FaultAction::Proceed => Ok(()),
            FaultAction::Crash => Err(VaultError::Injected(point)),
            FaultAction::Io(kind) => Err(VaultError::Io(kind)),
        }
    }

    // ---- yol çözümleme --------------------------------------------------------------------

    /// Göreli yolu doğrular ve mevcut her bileşende reparse noktası olmadığını denetler.
    pub(crate) fn locate(&self, locator: &str) -> VaultResult<PathBuf> {
        validate_locator(locator)?;
        let top = locator.split('/').next().unwrap_or("");
        if !ALLOWED_TOPS.contains(&top) {
            return Err(VaultError::PathEscape);
        }
        let mut cur = self.root.clone();
        let comps: Vec<&str> = locator.split('/').collect();
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(m) => {
                    if is_reparse_attrs(m.file_attributes()) {
                        return Err(VaultError::Reparse);
                    }
                    if i + 1 < comps.len() && !m.is_dir() {
                        return Err(VaultError::PathEscape);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(cur)
    }

    /// Doğrulanmış dizin yolunu (`me`, `receipts/sources`...) bileşen bileşen oluşturur.
    fn ensure_dir(&self, rel_dir: &str) -> VaultResult<PathBuf> {
        let mut cur = self.root.clone();
        for c in rel_dir.split('/') {
            cur.push(c);
            loop {
                match fs::symlink_metadata(&cur) {
                    Ok(m) => {
                        if is_reparse_attrs(m.file_attributes()) {
                            return Err(VaultError::Reparse);
                        }
                        if !m.is_dir() {
                            return Err(VaultError::PathEscape);
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&cur) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    },
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(cur)
    }

    /// Açık tutamak üzerinden denetim: reparse/dizin değil, kasa içinde kalıyor.
    fn check_handle(&self, file: &File) -> VaultResult<sys::Info> {
        let info = sys::info(file)?;
        if is_reparse_attrs(info.attributes) {
            return Err(VaultError::Reparse);
        }
        if info.attributes & sys::FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(VaultError::NotAFile);
        }
        let fp = sys::final_path(file)?;
        if !path_within(&self.root, Path::new(&fp)) {
            return Err(VaultError::PathEscape);
        }
        Ok(info)
    }

    fn open_read(&self, path: &Path) -> VaultResult<(File, sys::Info)> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(sys::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let info = self.check_handle(&file)?;
        Ok((file, info))
    }

    /// Sınırlı okuma; sert bağlantı sayısını da döner.
    pub(crate) fn read_checked(&self, locator: &str, max: u64) -> VaultResult<(Vec<u8>, u32)> {
        let path = self.locate(locator)?;
        let (file, info) = self.open_read(&path)?;
        let mut buf = Vec::new();
        file.take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            return Err(VaultError::TooLarge);
        }
        Ok((buf, info.links))
    }

    /// Akış halinde en çok `max_entries` girdi okur; sınır aşıldıysa ikinci değer `true`.
    pub(crate) fn read_dir_limited(&self, rel_dir: &str, max_entries: usize) -> VaultResult<(Vec<DirEntryInfo>, bool)> {
        let path = self.locate(rel_dir)?;
        let rd = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for (n, entry) in rd.enumerate() {
            if n >= max_entries {
                out.sort_by(|a: &DirEntryInfo, b| a.name.cmp(&b.name));
                return Ok((out, true));
            }
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            let meta = entry.metadata()?;
            out.push(DirEntryInfo {
                name,
                is_dir: meta.is_dir(),
                reparse: is_reparse_attrs(meta.file_attributes()),
                size: meta.len(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((out, false))
    }

    // ---- okuma ------------------------------------------------------------------------------

    /// Notu okur ve kesin biçimde çözer. Bozuk not için `Contract` hatası döner; içerik taşımaz.
    pub fn read_note(&self, locator: &str) -> VaultResult<NoteRead> {
        self.read_note_raw(locator).map(|(n, _)| n)
    }

    /// `read_note` + ham metin (sır denetimi için; dışarı verilmez).
    fn read_note_raw(&self, locator: &str) -> VaultResult<(NoteRead, String)> {
        if !locator.ends_with(".md") || locator.starts_with("receipts/") {
            return Err(VaultError::Contract(ContractError::InvalidLocator));
        }
        let (bytes, _) = self.read_checked(locator, MAX_NOTE_FILE_BYTES)?;
        let hash = Sha256Hash::of(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| VaultError::NotUtf8)?;
        let (front, body) = parse_note(&text)?;
        Ok((NoteRead { locator: locator.to_string(), front, body, content_hash: hash }, text))
    }

    // ---- düşük seviye atomik yazım ----------------------------------------------------------

    fn temp_path(dir: &Path, stem: &str, op: &str) -> PathBuf {
        dir.join(format!(".{stem}.{op}{TEMP_SUFFIX}"))
    }

    /// Temp dosyayı yazar ve sync eder. Hata (enjekte `Io` dahil) halinde temp silinir; `Crash`
    /// halinde olduğu gibi bırakılır.
    fn write_temp(&self, dir: &Path, stem: &str, op: &str, bytes: &[u8], inject: bool) -> VaultResult<PathBuf> {
        let tmp = Self::temp_path(dir, stem, op);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(sys::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&tmp)?;
        let result = (|| -> VaultResult<()> {
            self.check_handle(&file)?;
            if inject {
                self.fault(FaultPoint::AfterTempCreate)?;
            }
            let half = bytes.len() / 2;
            file.write_all(&bytes[..half])?;
            if inject {
                self.fault(FaultPoint::AfterTempPartial)?;
            }
            file.write_all(&bytes[half..])?;
            file.flush()?;
            file.sync_all()?;
            if inject {
                self.fault(FaultPoint::AfterTempSync)?;
            }
            Ok(())
        })();
        drop(file);
        match result {
            Ok(()) => Ok(tmp),
            Err(VaultError::Injected(p)) => Err(VaultError::Injected(p)),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Değişmez dosya: yoksa atomik oluşturur; varsa içerik bire bir aynı olmalı (idempotent),
    /// farklıysa `Tampered`. Dönen `bool`: yeni oluşturuldu mu.
    pub(crate) fn write_immutable(&self, locator: &str, bytes: &[u8], op: &str) -> VaultResult<bool> {
        let path = self.locate(locator)?;
        let (dir_rel, name) = locator.rsplit_once('/').ok_or(VaultError::PathEscape)?;
        if fs::symlink_metadata(&path).is_ok() {
            return self.verify_existing(locator, bytes, op).map(|()| false);
        }
        let dir = self.ensure_dir(dir_rel)?;
        let tmp = self.write_temp(&dir, name, op, bytes, false)?;
        match sys::move_file(&tmp, &path, false) {
            Ok(()) => Ok(true),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                if e.kind() == io::ErrorKind::AlreadyExists {
                    self.verify_existing(locator, bytes, op).map(|()| false)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Mevcut dosya beklenenle aynıysa Ok. Farklıysa `Tampered`; içerik-adresli makbuz dosyası
    /// (`receipts/<hash>.json`) ek olarak `receipts/quarantine/` altına taşınır (üzerine yazılmaz,
    /// işlem yine hata döner; yeniden denemede yuva boştur). Kaynak görüntüleri istek kimliğiyle
    /// adreslendiği için taşınmaz (meşru ilk görüntü korunur).
    fn verify_existing(&self, locator: &str, expected: &[u8], op: &str) -> VaultResult<()> {
        let mismatch = match self.read_checked(locator, expected.len() as u64) {
            Ok((existing, _)) if existing == expected => return Ok(()),
            Ok(_) | Err(VaultError::TooLarge) => true,
            Err(e) => return Err(e),
        };
        if mismatch
            && !locator.starts_with("receipts/sources/")
            && let (Ok(src), Ok(dir)) = (self.locate(locator), self.ensure_dir("receipts/quarantine"))
        {
            let name = locator.rsplit('/').next().unwrap_or("x");
            let _ = sys::move_file(&src, &dir.join(format!("{name}.{op}.bad")), false);
        }
        Err(VaultError::Tampered)
    }

    /// Yarım kalmış `*.ultron-tmp` dosyalarını siler (açılışta, tek yazıcı kilidi altında).
    pub fn sweep_temps(&self) -> VaultResult<usize> {
        fn walk(vault: &Vault, rel: &str, depth: usize, n: &mut usize) {
            if depth > 8 {
                return;
            }
            // Okunamayan/reparse dizinler atlanır; süpürme durmaz.
            let Ok((entries, _)) = vault.read_dir_limited(rel, 100_000) else { return };
            for e in entries {
                if e.reparse {
                    continue;
                }
                let child = format!("{rel}/{}", e.name);
                if e.is_dir {
                    walk(vault, &child, depth + 1, n);
                } else if is_our_temp_name(&e.name)
                    && let Ok(path) = vault.locate_temp(&child)
                    && fs::remove_file(path).is_ok()
                {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        for top in ALLOWED_TOPS {
            walk(self, top, 0, &mut n);
        }
        Ok(n)
    }

    /// Temp dosya adları `validate_locator` kuralına uymaz (baştaki nokta serbest ama son nokta
    /// yok; burada yalnız bileşen/reparse denetimi gerekir).
    fn locate_temp(&self, rel: &str) -> VaultResult<PathBuf> {
        if rel.contains("..") || rel.contains('\\') || rel.contains(':') {
            return Err(VaultError::PathEscape);
        }
        let mut cur = self.root.clone();
        for c in rel.split('/') {
            cur.push(c);
            let m = fs::symlink_metadata(&cur)?;
            if is_reparse_attrs(m.file_attributes()) {
                return Err(VaultError::Reparse);
            }
        }
        Ok(cur)
    }

    // ---- not yazımı -------------------------------------------------------------------------

    /// Yeni not oluşturur. Mevcut dosyanın üzerine asla yazmaz.
    pub fn create_note(&self, new: NewNote, receipt: ReceiptSpec, now: DateTime<Utc>) -> VaultResult<CommitOutcome> {
        let id = new.id.clone().unwrap_or_default();
        let dir = if new.inbox { "inbox" } else { type_dir(new.memory_type) };
        let locator = format!("{dir}/{}.md", id.as_str());
        let (visibility, remote_allowed) = match new.visibility {
            Some(pair) => {
                check_visibility_pair(pair.0, pair.1)?;
                pair
            }
            None => default_visibility_for(new.sensitivity),
        };
        let mut mk = Masker::default();
        let front = FrontMatter {
            schema_version: SCHEMA_VERSION,
            id,
            memory_type: new.memory_type,
            title: mk.s(&new.title),
            created_at: now,
            updated_at: now,
            revision: Revision::new(1)?,
            summary: mk.s(&new.summary),
            project: new.project.as_deref().map(|p| mk.s(p)),
            visibility,
            remote_allowed,
            sensitivity: new.sensitivity,
            review_at: new.review_at,
            aliases: new.aliases.iter().map(|a| mk.s(a)).collect(),
            facts: mk.map(&new.facts),
            tags: new.tags.iter().map(|t| mk.s(t)).collect(),
            receipts: Vec::new(),
            extra: BTreeMap::new(),
        };
        let body = mk.s(&new.body);
        self.commit(&locator, front, body, None, receipt, now, CommitKind::Created, mk.changed)
    }

    /// Revision + içerik-hash CAS ile güncelleme. `expected_hash`, çağıranın bildiği (indeksteki)
    /// dosya içerik hash'idir; henüz taranmamış bir dış düzenleme varsa `Conflict` döner.
    /// Mevcut dosya sır deseni içeriyorsa (elle yazılmış, uzlaştırılmamış) güncelleme reddedilir.
    pub fn update_note(
        &self,
        locator: &str,
        expected_revision: Revision,
        expected_hash: &Sha256Hash,
        patch: &MemoryPatch,
        receipt: ReceiptSpec,
        now: DateTime<Utc>,
    ) -> VaultResult<CommitOutcome> {
        patch.validate()?;
        let (current, text) = self.read_note_raw(locator)?;
        if mask(&text) != text {
            return Err(VaultError::Contract(ContractError::InvalidText));
        }
        if &current.content_hash != expected_hash || current.front.revision != expected_revision {
            return Err(VaultError::Conflict { current_revision: Some(current.front.revision) });
        }
        let mut mk = Masker::default();
        let mut front = current.front.clone();
        let mut body = current.body.clone();
        if let Some(t) = &patch.title {
            front.title = mk.s(t);
        }
        if let Some(s) = &patch.summary {
            front.summary = mk.s(s);
        }
        if let Some(b) = &patch.body {
            body = mk.s(b);
        }
        if let Some(tags) = &patch.tags {
            front.tags = tags.iter().map(|t| mk.s(t)).collect();
        }
        if let Some(p) = &patch.project {
            front.project = p.as_deref().map(|x| mk.s(x));
        }
        if let Some(s) = patch.sensitivity {
            front.sensitivity = s;
        }
        if let Some(v) = patch.visibility {
            front.visibility = v;
            if v == Visibility::Private && patch.remote_allowed.is_none() {
                front.remote_allowed = false;
            }
        }
        if let Some(r) = patch.remote_allowed {
            front.remote_allowed = r;
        }
        check_visibility_pair(front.visibility, front.remote_allowed)?;
        // Tüm alanlar yeniden maskelenir (mevcut dosya temiz olsa da tutarlılık için).
        front.title = mk.s(&front.title);
        front.summary = mk.s(&front.summary);
        front.project = front.project.take().map(|x| mk.s(&x));
        front.aliases = front.aliases.iter().map(|a| mk.s(a)).collect();
        front.tags = front.tags.iter().map(|t| mk.s(t)).collect();
        front.facts = mk.map(&front.facts);
        front.extra = mk.map(&front.extra);
        body = mk.s(&body);
        front.revision = current.front.revision.next()?;
        self.commit(locator, front, body, Some(current.content_hash), receipt, now, CommitKind::Updated, mk.changed)
    }

    /// Kullanıcının dış düzenlemesini yeni revision + makbuzla uzlaştırır. `known`: çağıranın
    /// bildiği (revision, içerik hash'i); diskteki hash bununla aynıysa yapılacak bir şey yoktur
    /// ve `Ok(None)` döner (idempotent). Dosya temiz (ayrıştırılabilir, sırsız) olmalıdır; aksi
    /// halde hata döner ve dosyaya dokunulmaz. Not: uzlaştırma dosyayı kanonik biçimde yeniden
    /// yazar (BOM, yorumlar ve ön bilgi biçimi kaybolur; gövde ve bilinen/serbest alanlar korunur).
    pub fn adopt_external(
        &self,
        locator: &str,
        known: Option<(Revision, Sha256Hash)>,
        now: DateTime<Utc>,
    ) -> VaultResult<Option<CommitOutcome>> {
        let (read, text) = self.read_note_raw(locator)?;
        if let Some((_, h)) = &known
            && h == &read.content_hash
        {
            return Ok(None);
        }
        if mask(&text) != text {
            return Err(VaultError::Contract(ContractError::InvalidText));
        }
        let hash = read.content_hash.clone();
        let mut front = read.front;
        let body_in = read.body;
        let base = known.as_ref().map_or(front.revision.get(), |(k, _)| k.get().max(front.revision.get()));
        front.revision = Revision::new(base)?.next()?;
        let snapshot_id = format!("ext-{}", &hash.hex()[..32]);
        let receipt = ReceiptSpec {
            source: ReceiptSource::Snapshot { id: snapshot_id, text },
            excerpt: body_in.chars().take(300).collect(),
            claim_status: ClaimStatus::UserAuthored,
            harness_verified: false,
        };
        self.commit(locator, front, body_in, Some(hash), receipt, now, CommitKind::Adopted, false).map(Some)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &self,
        locator: &str,
        mut front: FrontMatter,
        body: String,
        expected_hash: Option<Sha256Hash>,
        spec: ReceiptSpec,
        now: DateTime<Utc>,
        kind: CommitKind,
        masked_in: bool,
    ) -> VaultResult<CommitOutcome> {
        if spec.claim_status == ClaimStatus::Verified && !spec.harness_verified {
            return Err(VaultError::Unauthorized);
        }
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let op12 = &operation_id[..12];
        let path = self.locate(locator)?;
        let (dir_rel, name) = locator.rsplit_once('/').ok_or(VaultError::PathEscape)?;
        let stem = name.strip_suffix(".md").unwrap_or(name);
        let mut mk = Masker { changed: masked_in };

        // 1) Bellekte hazırlık: kaynak, makbuz, not. Diske dokunmaz.
        let (source, snapshot) = match &spec.source {
            ReceiptSource::Existing(src) => {
                src.validate()?;
                if let SourceRef::File { locator: l, .. } = src
                    && !(l.starts_with("receipts/sources/") && l.ends_with(".md"))
                {
                    return Err(VaultError::Contract(ContractError::InvalidLocator));
                }
                (mask_source(src, &mut mk), None)
            }
            ReceiptSource::Snapshot { id, text } => {
                validate_request_id(id)?;
                if text.len() > MAX_CAPTURE_TEXT_BYTES {
                    return Err(VaultError::TooLarge);
                }
                let masked = mk.s(text);
                let snap_locator = format!("receipts/sources/{id}.md");
                let hash = Sha256Hash::of(masked.as_bytes());
                (SourceRef::File { locator: snap_locator.clone(), content_hash: hash }, Some((snap_locator, masked)))
            }
        };
        mk.s(&spec.excerpt);
        let receipt = Receipt::new(source, &spec.excerpt, now, spec.claim_status)?;
        let receipt_hash = receipt.hash()?;
        let receipt_bytes = receipt.canonical_json()?;
        if !front.receipts.contains(&receipt_hash) {
            front.receipts.push(receipt_hash.clone());
        }
        if front.receipts.len() > MAX_RECEIPTS_PER_ITEM {
            // En eski bağlantılar listeden düşer; makbuz dosyaları değişmeden diskte kalır.
            let excess = front.receipts.len() - MAX_RECEIPTS_PER_ITEM;
            front.receipts.drain(..excess);
        }
        front.updated_at = now.max(front.created_at);
        let rendered = render_note(&front, &body)?;
        // Son güvence: alanlar tek tek maskelense de birleşik metin (ör. `anahtar: değer`)
        // sır deseni oluşturabilir; diske hiçbir şey yazmadan reddedilir.
        if mask(&rendered) != rendered {
            return Err(VaultError::Contract(ContractError::InvalidText));
        }
        let note_bytes = rendered.into_bytes();
        if note_bytes.len() as u64 > MAX_NOTE_FILE_BYTES {
            return Err(VaultError::TooLarge);
        }
        let content_hash = Sha256Hash::of(&note_bytes);
        if expected_hash.is_none() && fs::symlink_metadata(&path).is_ok() {
            return Err(VaultError::AlreadyExists);
        }
        // Mevcut File kaynağı gerçekten okunabilir ve hash'i doğru olmalı.
        if let ReceiptSource::Existing(SourceRef::File { locator: l, content_hash: h }) = &spec.source {
            let (b, _) = self.read_checked(l, MAX_CAPTURE_TEXT_BYTES as u64 * 2)?;
            if &Sha256Hash::of(&b) != h {
                return Err(VaultError::Tampered);
            }
        }

        // 2) Kaynak görüntüsü + makbuz.
        self.fault(FaultPoint::BeforeSnapshot)?;
        if let Some((snap_locator, text)) = &snapshot {
            self.write_immutable(snap_locator, text.as_bytes(), op12)?;
        }
        self.fault(FaultPoint::AfterSnapshot)?;
        let receipt_created =
            self.write_immutable(&Receipt::relative_path(&receipt_hash), &receipt_bytes, op12)?;
        self.fault(FaultPoint::AfterReceipt)?;

        // 3) Not: temp + sync, sonra atomik taşıma.
        let dir = self.ensure_dir(dir_rel)?;
        let tmp = self.write_temp(&dir, stem, op12, &note_bytes, true)?;
        let finish = (|| -> VaultResult<()> {
            if let Some(expected) = &expected_hash {
                // Yarış penceresini daralt: taşımadan hemen önce hedef hâlâ okunan sürüm mü?
                let path = self.locate(locator)?;
                let (file, info) = self.open_read(&path)?;
                if info.links > 1 {
                    return Err(VaultError::HardLink);
                }
                let mut buf = Vec::new();
                file.take(MAX_NOTE_FILE_BYTES + 1).read_to_end(&mut buf)?;
                if &Sha256Hash::of(&buf) != expected {
                    return Err(VaultError::Conflict { current_revision: None });
                }
            }
            let path = self.locate(locator)?;
            sys::move_file(&tmp, &path, expected_hash.is_some()).map_err(VaultError::from)
        })();
        if let Err(e) = finish {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        self.fault(FaultPoint::AfterRename)?;
        Ok(CommitOutcome {
            operation_id,
            kind,
            locator: locator.to_string(),
            id: front.id.clone(),
            revision: front.revision,
            content_hash,
            previous_hash: expected_hash,
            receipt_hash,
            receipt_created,
            masked: mk.changed,
        })
    }

    // ---- silme/geri yükleme -----------------------------------------------------------------

    /// Notu Ultron çöpüne taşır. `Trash::put` başarısızsa dosya yerinde kalır.
    pub fn delete_to_trash(
        &self,
        trash: &Trash,
        locator: &str,
        chat_id: Option<&str>,
    ) -> VaultResult<DeleteOutcome> {
        if !locator.ends_with(".md") || locator.starts_with("receipts/") {
            return Err(VaultError::Contract(ContractError::InvalidLocator));
        }
        let path = self.locate(locator)?;
        let (bytes, _) = self.read_checked(locator, MAX_NOTE_FILE_BYTES)?;
        let last_hash = Sha256Hash::of(&bytes);
        let id = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|t| parse_note(t).ok())
            .map(|(f, _)| f.id);
        let entry = trash.put(&path, chat_id).map_err(|_| VaultError::Io(io::ErrorKind::Other))?;
        Ok(DeleteOutcome { trash_id: entry.id, locator: locator.to_string(), id, last_hash })
    }

    /// Bu kasadan silinmiş bir girdiyi özgün konumuna (doluysa Trash'in ek adıyla) geri yükler ve
    /// göreli yolu döner. Başka konumdan silinmiş girdiler reddedilir. Çağıran, dönen yolu
    /// `scan` ile yeniden indeksler (restore bir delta üretir).
    pub fn restore_from_trash(&self, trash: &Trash, trash_id: &str) -> VaultResult<String> {
        let entry = trash
            .list()
            .map_err(|_| VaultError::Io(io::ErrorKind::Other))?
            .into_iter()
            .find(|e| e.id == trash_id)
            .ok_or(VaultError::NotFound)?;
        if !path_within(&self.root, &entry.original_path) || entry.is_dir {
            return Err(VaultError::PathEscape);
        }
        // Geri yüklemeden ÖNCE hedef konum doğrulanır: yalnız not dizinleri ve `.md`.
        let rel = self.rel_of(&entry.original_path)?;
        validate_locator(&rel)?;
        let top = rel.split('/').next().unwrap_or("");
        if !NOTE_DIRS.contains(&top) || !rel.ends_with(".md") {
            return Err(VaultError::PathEscape);
        }
        // Özgün konumun üst bileşenleri bu arada bağlantıya çevrilmiş olmasın.
        self.locate(&rel)?;
        let restored = trash
            .restore(trash_id, None)
            .map_err(|_| VaultError::Io(io::ErrorKind::Other))?;
        // Geri yükleme sonrası doğrulama başarısız olursa dosya çöp kutusuna geri konur; böylece
        // kayıt kaybolmaz ve işlem yeniden denenebilir. (Ad çakışmasında trash `... (geri
        // yüklendi N)` eki ekler; bu `.md` ile bitmediğinden reddedilir ve geri alınır.)
        let verified = (|| -> VaultResult<String> {
            let rel = self.rel_of(&fs::canonicalize(&restored)?)?;
            validate_locator(&rel)?;
            let top = rel.split('/').next().unwrap_or("");
            if !NOTE_DIRS.contains(&top) || !rel.ends_with(".md") {
                return Err(VaultError::PathEscape);
            }
            Ok(rel)
        })();
        if verified.is_err() {
            let _ = trash.put(&restored, None);
        }
        verified
    }

    /// Kasa içindeki mutlak yolun göreli (`/` ayraçlı, gerçek yazımlı) karşılığı.
    fn rel_of(&self, abs: &Path) -> VaultResult<String> {
        if !path_within(&self.root, abs) {
            return Err(VaultError::PathEscape);
        }
        let root_n = self.root.components().count();
        let rel: Vec<String> = abs
            .components()
            .skip(root_n)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rel.is_empty() {
            return Err(VaultError::PathEscape);
        }
        Ok(rel.join("/"))
    }
}
