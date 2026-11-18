//! Hafıza kasası, makbuz, öneri, getirme ve IPC sözleşmeleri (Aşama 5a, Task 1).
//!
//! Tasarım ilkeleri:
//! - Kimlik, revision, hash ve yol değerleri geçerli olmadan **inşa edilemez**; serde çözümlemesi
//!   de aynı doğrulamadan geçer.
//! - Hata değerleri (`ContractError`) hiçbir zaman girdi içeriğini taşımaz.
//! - İçerik taşıyan tiplerin `Debug` çıktısı gövde/özet/alıntı basmaz.
//! - Kaynak yetkisi istemciden gelmez: istemci yalnız bir `request_id` ve (varsa) sohbet
//!   referansı gönderir; makbuz yolu/hash'i daemon üretir.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------------------------
// Sınırlar
// ---------------------------------------------------------------------------------------------

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_FRONT_MATTER_BYTES: usize = 16 * 1024;
pub const MAX_FRONT_MATTER_DEPTH: usize = 6;
pub const MAX_FRONT_MATTER_NODES: usize = 1024;
pub const MAX_FRONT_MATTER_SCALAR_BYTES: usize = 4096;
pub const MAX_FRONT_MATTER_KEY_CHARS: usize = 64;
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_SUMMARY_CHARS: usize = 500;
pub const MAX_TAGS: usize = 16;
pub const MAX_TAG_CHARS: usize = 48;
pub const MAX_ALIASES: usize = 16;
pub const MAX_ALIAS_CHARS: usize = 100;
pub const MAX_PROJECT_CHARS: usize = 128;
pub const MAX_FACTS: usize = 64;
pub const MAX_EXTRA_KEYS: usize = 32;
pub const MAX_RECEIPTS_PER_ITEM: usize = 32;
pub const MAX_DETAIL_RECEIPTS: usize = 16;
pub const MAX_RECEIPT_EXCERPT_CHARS: usize = 2000;
pub const MAX_LOCATOR_BYTES: usize = 260;
pub const MAX_CAPTURE_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_CANDIDATE_BODY_BYTES: usize = 16 * 1024;
pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_SNIPPET_CHARS: usize = 1000;
pub const MAX_CURSOR_BYTES: usize = 256;
pub const MAX_ID_TOKEN_CHARS: usize = 128;
pub const MAX_REQUEST_ID_CHARS: usize = 64;
pub const DEFAULT_PAGE_LIMIT: u32 = 20;
/// Liste/arama sayfası üst sınırı; en kötü durum DTO boyutu testle 1 MiB altında doğrulanır.
pub const MAX_PAGE_LIMIT: u32 = 50;
/// Öneri sayfaları aday gövdesi taşıdığı için daha dardır.
pub const MAX_SUGGESTION_PAGE_LIMIT: u32 = 10;
pub const MAX_SHADOW_REPORT_DAYS: u32 = 365;
/// Bir olay kaynağı/sohbet referansının kapsayabileceği en çok olay sayısı (aralık dahil).
pub const MAX_CHAT_REF_EVENTS: i64 = 10_000;
/// Çerçeve üst sınırı ile aynı değer (`ipc::MAX_LINE_BYTES`); burada bağımsızca sabitlenir.
pub const FRAME_BUDGET_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Hata
// ---------------------------------------------------------------------------------------------

/// Kapalı, içerik taşımayan sözleşme hata kümesi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractError {
    InvalidId,
    InvalidRevision,
    InvalidHash,
    InvalidSeq,
    InvalidLocator,
    InvalidText,
    InvalidField,
    TooLarge,
    TooDeep,
    UnknownSchemaVersion,
    MalformedFrontMatter,
    YamlAlias,
    YamlTag,
    YamlUnsupported,
    LimitExceeded,
    /// Aynı `request_id` farklı içerik/kaynak parmak iziyle yeniden kullanıldı.
    RequestConflict,
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ContractError::InvalidId => "geçersiz kimlik",
            ContractError::InvalidRevision => "geçersiz revision",
            ContractError::InvalidHash => "geçersiz hash",
            ContractError::InvalidSeq => "geçersiz olay aralığı",
            ContractError::InvalidLocator => "geçersiz göreli yol",
            ContractError::InvalidText => "geçersiz metin",
            ContractError::InvalidField => "geçersiz alan",
            ContractError::TooLarge => "içerik sınırı aşıldı",
            ContractError::TooDeep => "yapı çok derin",
            ContractError::UnknownSchemaVersion => "bilinmeyen şema sürümü",
            ContractError::MalformedFrontMatter => "ön bilgi çözümlenemedi",
            ContractError::YamlAlias => "YAML takma adı (alias/anchor) desteklenmez",
            ContractError::YamlTag => "YAML etiketi (tag) desteklenmez",
            ContractError::YamlUnsupported => "desteklenmeyen YAML sözdizimi",
            ContractError::LimitExceeded => "sayı sınırı aşıldı",
            ContractError::RequestConflict => "aynı request_id farklı içerikle kullanıldı",
        })
    }
}

impl std::error::Error for ContractError {}

pub type ContractResult<T> = Result<T, ContractError>;

// ---------------------------------------------------------------------------------------------
// Doğrulama yardımcıları
// ---------------------------------------------------------------------------------------------

fn is_line_break_like(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}')
}

/// Tek satırlık metin: kontrol karakteri yok, en çok `max_chars` karakter.
pub fn check_line(s: &str, max_chars: usize, allow_empty: bool) -> ContractResult<()> {
    if (!allow_empty && s.trim().is_empty()) || s.chars().count() > max_chars {
        return Err(ContractError::InvalidText);
    }
    if s.chars().any(is_line_break_like) {
        return Err(ContractError::InvalidText);
    }
    Ok(())
}

/// Çok satırlı metin: `\n`, `\r`, `\t` dışında kontrol karakteri yok. JSON kaçışı en kötü
/// durumda bayt başına 2 bayt üretir; böylece çerçeve bütçesi öngörülebilir kalır.
pub fn check_text(s: &str, max_bytes: usize, allow_empty: bool) -> ContractResult<()> {
    if s.len() > max_bytes {
        return Err(ContractError::TooLarge);
    }
    if !allow_empty && s.trim().is_empty() {
        return Err(ContractError::InvalidText);
    }
    if s.chars().any(|c| (c.is_control() && !matches!(c, '\n' | '\r' | '\t')) || matches!(c, '\u{feff}' | '\u{fffe}' | '\u{ffff}')) {
        return Err(ContractError::InvalidText);
    }
    Ok(())
}

fn valid_token(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// İstemci tarafından seçilen idempotency anahtarı.
pub fn validate_request_id(s: &str) -> ContractResult<()> {
    if valid_token(s, MAX_REQUEST_ID_CHARS) { Ok(()) } else { Err(ContractError::InvalidId) }
}

/// Ultron sohbet kimliği (yalnız güvenli karakterler).
pub fn validate_chat_id(s: &str) -> ContractResult<()> {
    if valid_token(s, MAX_ID_TOKEN_CHARS) { Ok(()) } else { Err(ContractError::InvalidId) }
}

pub fn validate_project_name(s: &str) -> ContractResult<()> {
    check_line(s, MAX_PROJECT_CHARS, false)
}

/// Opak sayfalama imleci.
pub fn validate_cursor(s: &str) -> ContractResult<()> {
    if s.is_empty() || s.len() > MAX_CURSOR_BYTES || !s.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ContractError::InvalidField);
    }
    Ok(())
}

pub fn validate_page_limit(limit: Option<u32>, max: u32) -> ContractResult<()> {
    match limit {
        Some(n) if n == 0 || n > max => Err(ContractError::LimitExceeded),
        _ => Ok(()),
    }
}

fn validate_tags(tags: &[String]) -> ContractResult<()> {
    if tags.len() > MAX_TAGS {
        return Err(ContractError::LimitExceeded);
    }
    for tag in tags {
        check_line(tag, MAX_TAG_CHARS, false)?;
    }
    Ok(())
}

/// Kasa köküne göre göreli yol: `/` ayracı; mutlak/UNC/sürücü/ADS/aygıt adı/`..`/ters eğik çizgi yok.
pub fn validate_locator(s: &str) -> ContractResult<()> {
    if s.is_empty() || s.len() > MAX_LOCATOR_BYTES || s.starts_with('/') {
        return Err(ContractError::InvalidLocator);
    }
    if s.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(ContractError::InvalidLocator);
    }
    for component in s.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(ContractError::InvalidLocator);
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(ContractError::InvalidLocator);
        }
        // Windows taban adı sonundaki boşluk/noktaları yok sayar ("NUL .txt" aygıttır) ve
        // üst simge rakamları (COM¹²³, LPT¹²³) da ayrılmış sayar.
        let stem = component.split('.').next().unwrap_or("").trim_end().to_uppercase();
        let digit_suffix = |prefix: &str| {
            stem.strip_prefix(prefix).is_some_and(|rest| {
                let mut chars = rest.chars();
                matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_digit() || matches!(c, '\u{b9}' | '\u{b2}' | '\u{b3}'))
            })
        };
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
            || digit_suffix("COM")
            || digit_suffix("LPT");
        if reserved {
            return Err(ContractError::InvalidLocator);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Değer tipleri: kimlik, revision, hash
// ---------------------------------------------------------------------------------------------

/// Küçük harfli, tire ayraçlı UUID.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MemoryId(String);

impl MemoryId {
    pub fn new() -> Self {
        MemoryId(uuid::Uuid::new_v4().to_string())
    }
    pub fn parse(s: &str) -> ContractResult<Self> {
        let parsed = uuid::Uuid::parse_str(s).map_err(|_| ContractError::InvalidId)?;
        if parsed.hyphenated().to_string() != s {
            return Err(ContractError::InvalidId);
        }
        Ok(MemoryId(s.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<String> for MemoryId {
    type Error = ContractError;
    fn try_from(value: String) -> ContractResult<Self> {
        MemoryId::parse(&value)
    }
}

impl From<MemoryId> for String {
    fn from(id: MemoryId) -> String {
        id.0
    }
}

impl fmt::Debug for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MemoryId({})", self.0)
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 1 ile `i64::MAX` arası içerik sürümü.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Revision(u64);

impl Revision {
    pub const FIRST: Revision = Revision(1);
    pub fn new(n: u64) -> ContractResult<Self> {
        if n == 0 || n > i64::MAX as u64 {
            return Err(ContractError::InvalidRevision);
        }
        Ok(Revision(n))
    }
    pub fn get(self) -> u64 {
        self.0
    }
    pub fn next(self) -> ContractResult<Self> {
        Revision::new(self.0 + 1)
    }
}

impl TryFrom<u64> for Revision {
    type Error = ContractError;
    fn try_from(value: u64) -> ContractResult<Self> {
        Revision::new(value)
    }
}

impl From<Revision> for u64 {
    fn from(r: Revision) -> u64 {
        r.0
    }
}

/// `sha256:<64 küçük harfli hex>`; makbuz, içerik ve parmak izi hash'i.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hash(String);

impl Sha256Hash {
    pub const PREFIX: &'static str = "sha256:";

    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(64);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        Sha256Hash(hex)
    }

    /// Yalnız `sha256:`-önekli biçimi kabul eder.
    pub fn parse(s: &str) -> ContractResult<Self> {
        let hex = s.strip_prefix(Self::PREFIX).ok_or(ContractError::InvalidHash)?;
        Self::from_hex(hex)
    }

    /// Önek olmadan 64 küçük harfli hex (makbuz dosya adı gibi).
    pub fn from_hex(hex: &str) -> ContractResult<Self> {
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(ContractError::InvalidHash);
        }
        Ok(Sha256Hash(hex.to_string()))
    }

    pub fn hex(&self) -> &str {
        &self.0
    }

    pub fn prefixed(&self) -> String {
        format!("{}{}", Self::PREFIX, self.0)
    }
}

impl TryFrom<String> for Sha256Hash {
    type Error = ContractError;
    fn try_from(value: String) -> ContractResult<Self> {
        Sha256Hash::parse(&value)
    }
}

impl From<Sha256Hash> for String {
    fn from(h: Sha256Hash) -> String {
        h.prefixed()
    }
}

impl fmt::Debug for Sha256Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Hash({}{})", Self::PREFIX, self.0)
    }
}

// ---------------------------------------------------------------------------------------------
// Sınıflandırmalar
// ---------------------------------------------------------------------------------------------

/// Spec türleri; hızlı yakalama `inbox/` altında `User` notudur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    User,
    Project,
    Home,
    Tool,
    Person,
    Rule,
    Decision,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Project,
    Shared,
}

/// Eksikse en kısıtlayıcı varsayılan (`Personal`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Normal,
    #[default]
    Personal,
    Finance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Claimed,
    /// Yalnız harness atar; model atayamaz.
    Verified,
    UserAuthored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Indexed,
    Pending,
    Stale,
    Conflict,
    Quarantined,
    Inactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionStatus {
    Pending,
    Accepted,
    Rejected,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    Desktop,
    Telegram,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryChangeKind {
    Created,
    Updated,
    Trashed,
    Restored,
    ExternalChange,
    Deactivated,
    SuggestionCreated,
    SuggestionResolved,
    IndexRebuilt,
}

/// Olay akışında gösterilen indeks durumu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIndexStatus {
    Ready,
    Rebuilding,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryJobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Ham sağlayıcı metni yerine güvenli iş hata kodu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryJobError {
    Timeout,
    EngineUnavailable,
    InvalidOutput,
    StoreUnavailable,
    Cancelled,
    Quota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOperationKind {
    Capture,
    Update,
    Delete,
    AcceptSuggestion,
    Reconcile,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOperationState {
    Pending,
    FileCommitted,
    Completed,
    Failed,
}

// ---------------------------------------------------------------------------------------------
// Kaynak ve makbuz
// ---------------------------------------------------------------------------------------------

/// Bir kaydın kaynağı. Örnekler sentetiktir; gerçek kimlikler daemon tarafından üretilir.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceRef {
    /// Ultron sohbet olay aralığı (Telegram kimliği değil).
    Event { chat_id: String, seq_from: i64, seq_to: i64 },
    /// Kasa köküne göre göreli, değiştirilemez kaynak görüntüsü.
    File { locator: String, content_hash: Sha256Hash },
    /// İçe aktarılmış kayıt; kaynak yoksa "kanıt yok" gösterilir, sahte olay üretilmez.
    Import { importer: String, source_hash: Sha256Hash, record_key: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SourceRefWire {
    Event { chat_id: String, seq_from: i64, seq_to: i64 },
    File { locator: String, content_hash: Sha256Hash },
    Import { importer: String, source_hash: Sha256Hash, record_key: String },
}

impl<'de> Deserialize<'de> for SourceRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = match SourceRefWire::deserialize(deserializer)? {
            SourceRefWire::Event { chat_id, seq_from, seq_to } => SourceRef::Event { chat_id, seq_from, seq_to },
            SourceRefWire::File { locator, content_hash } => SourceRef::File { locator, content_hash },
            SourceRefWire::Import { importer, source_hash, record_key } => {
                SourceRef::Import { importer, source_hash, record_key }
            }
        };
        source.validate().map_err(serde::de::Error::custom)?;
        Ok(source)
    }
}

impl SourceRef {
    pub fn validate(&self) -> ContractResult<()> {
        match self {
            SourceRef::Event { chat_id, seq_from, seq_to } => {
                validate_chat_id(chat_id)?;
                if *seq_from < 1 || seq_to < seq_from || seq_to - seq_from >= MAX_CHAT_REF_EVENTS {
                    return Err(ContractError::InvalidSeq);
                }
                Ok(())
            }
            SourceRef::File { locator, .. } => validate_locator(locator),
            SourceRef::Import { importer, record_key, .. } => {
                if !valid_token(importer, 64) {
                    return Err(ContractError::InvalidId);
                }
                check_line(record_key, 256, false)
            }
        }
    }
}

impl fmt::Debug for SourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceRef::Event { seq_from, seq_to, .. } => {
                write!(f, "SourceRef::Event {{ chat_id: <gizli>, seq: {seq_from}..={seq_to} }}")
            }
            SourceRef::File { content_hash, .. } => {
                write!(f, "SourceRef::File {{ locator: <gizli>, content_hash: {content_hash:?} }}")
            }
            SourceRef::Import { importer, .. } => write!(f, "SourceRef::Import {{ importer: {importer} }}"),
        }
    }
}

pub const RECEIPT_VERSION: u32 = 1;

/// `receipts/<sha256>.json` içeriği. Hash, maskelenmiş kanonik JSON üzerinden hesaplanır.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub source: SourceRef,
    /// Maskelenmiş alıntı; özgün sır içeren içerik saklanmaz.
    pub excerpt: String,
    pub captured_at: DateTime<Utc>,
    pub claim_status: ClaimStatus,
}

impl Receipt {
    /// Alıntıyı `mask_secrets` ile maskeler ve sınırlar, sonra doğrular.
    pub fn new(
        source: SourceRef,
        excerpt: &str,
        captured_at: DateTime<Utc>,
        claim_status: ClaimStatus,
    ) -> ContractResult<Self> {
        let masked = crate::import::mask_secrets(excerpt).0;
        let bounded: String = masked.chars().take(MAX_RECEIPT_EXCERPT_CHARS).collect();
        let receipt = Receipt { version: RECEIPT_VERSION, source, excerpt: bounded, captured_at, claim_status };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> ContractResult<()> {
        if self.version != RECEIPT_VERSION {
            return Err(ContractError::UnknownSchemaVersion);
        }
        self.source.validate()?;
        check_text(&self.excerpt, MAX_RECEIPT_EXCERPT_CHARS * 4, true)?;
        if self.excerpt.chars().count() > MAX_RECEIPT_EXCERPT_CHARS {
            return Err(ContractError::TooLarge);
        }
        // Yazılmış makbuz maskelenmiş olmalı; kalan sır deseni varsa reddedilir.
        if crate::import::mask_secrets(&self.excerpt).0 != self.excerpt {
            return Err(ContractError::InvalidText);
        }
        Ok(())
    }

    /// Alan sırası bildirim sırasıdır; aynı içerik her zaman aynı bayt dizisini üretir.
    pub fn canonical_json(&self) -> ContractResult<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| ContractError::InvalidField)
    }

    pub fn hash(&self) -> ContractResult<Sha256Hash> {
        Ok(Sha256Hash::of(&self.canonical_json()?))
    }

    /// Diskten okunan makbuz baytlarını çözer; kanonik biçim ve dosya adındaki hash eşleşmelidir.
    pub fn from_canonical_json(bytes: &[u8], expected: &Sha256Hash) -> ContractResult<Self> {
        if bytes.len() > MAX_RECEIPT_EXCERPT_CHARS * 4 + 4096 {
            return Err(ContractError::TooLarge);
        }
        let receipt: Receipt = serde_json::from_slice(bytes).map_err(|_| ContractError::InvalidField)?;
        receipt.validate()?;
        if receipt.canonical_json()? != bytes || &Sha256Hash::of(bytes) != expected {
            return Err(ContractError::InvalidHash);
        }
        Ok(receipt)
    }

    /// Makbuzun kasa içindeki göreli yolu.
    pub fn relative_path(hash: &Sha256Hash) -> String {
        format!("receipts/{}.json", hash.hex())
    }
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receipt")
            .field("source", &self.source)
            .field("claim_status", &self.claim_status)
            .field("excerpt_chars", &self.excerpt.chars().count())
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// YAML ön bilgi
// ---------------------------------------------------------------------------------------------

/// Not ön bilgisi. Bilinmeyen üst düzey anahtarlar `extra` içinde kayıpsız korunur.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontMatter {
    pub schema_version: u32,
    pub id: MemoryId,
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: Revision,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub remote_allowed: bool,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    #[serde(default)]
    pub review_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub facts: BTreeMap<String, Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub receipts: Vec<Sha256Hash>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

const KNOWN_KEYS: [&str; 17] = [
    "schema_version", "id", "type", "title", "created_at", "updated_at", "revision", "summary",
    "project", "visibility", "remote_allowed", "sensitivity", "review_at", "aliases", "facts",
    "tags", "receipts",
];

impl FrontMatter {
    pub fn validate(&self) -> ContractResult<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::UnknownSchemaVersion);
        }
        check_line(&self.title, MAX_TITLE_CHARS, false)?;
        check_text(&self.summary, MAX_SUMMARY_CHARS * 4, true)?;
        if self.summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(ContractError::TooLarge);
        }
        if self.updated_at < self.created_at {
            return Err(ContractError::InvalidField);
        }
        check_visibility_pair(self.visibility, self.remote_allowed)?;
        if self.extra.keys().any(|key| KNOWN_KEYS.contains(&key.as_str())) {
            return Err(ContractError::InvalidField);
        }
        if let Some(project) = &self.project {
            validate_project_name(project)?;
        }
        validate_tags(&self.tags)?;
        if self.aliases.len() > MAX_ALIASES {
            return Err(ContractError::LimitExceeded);
        }
        for alias in &self.aliases {
            check_line(alias, MAX_ALIAS_CHARS, false)?;
        }
        if self.facts.len() > MAX_FACTS || self.extra.len() > MAX_EXTRA_KEYS || self.receipts.len() > MAX_RECEIPTS_PER_ITEM {
            return Err(ContractError::LimitExceeded);
        }
        let mut nodes = 0usize;
        for (key, value) in self.facts.iter().chain(self.extra.iter()) {
            check_key(key)?;
            check_value_limits(value, 3, &mut nodes)?;
        }
        Ok(())
    }
}

impl fmt::Debug for FrontMatter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrontMatter")
            .field("id", &self.id)
            .field("type", &self.memory_type)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

fn check_key(key: &str) -> ContractResult<()> {
    check_line(key, MAX_FRONT_MATTER_KEY_CHARS, false)
}

fn check_value_limits(value: &Value, depth: usize, nodes: &mut usize) -> ContractResult<()> {
    *nodes += 1;
    if *nodes > MAX_FRONT_MATTER_NODES {
        return Err(ContractError::TooLarge);
    }
    if depth > MAX_FRONT_MATTER_DEPTH {
        return Err(ContractError::TooDeep);
    }
    match value {
        Value::String(s) => {
            if s.len() > MAX_FRONT_MATTER_SCALAR_BYTES {
                return Err(ContractError::TooLarge);
            }
        }
        Value::Array(items) => {
            for item in items {
                check_value_limits(item, depth + 1, nodes)?;
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                check_key(key)?;
                check_value_limits(item, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// YAML ön bilgi metnini (sınırlayıcılar olmadan) çözer ve doğrular.
pub fn parse_front_matter(yaml: &str) -> ContractResult<FrontMatter> {
    if yaml.len() > MAX_FRONT_MATTER_BYTES {
        return Err(ContractError::TooLarge);
    }
    if yaml.chars().any(|c| (c.is_control() && !matches!(c, '\n' | '\r' | '\t')) || matches!(c, '\u{feff}' | '\u{fffe}' | '\u{ffff}' | '\u{2028}' | '\u{2029}')) {
        return Err(ContractError::InvalidText);
    }
    // libyaml `\r`, NEL, LS ve PS'yi de satır sonu sayar; tarayıcı yalnız `\n`'yi tanır. Bu
    // yüzden `\r\n` dışındaki çıplak `\r` ayrıştırıcıya girmeden reddedilir (NEL zaten kontrol
    // karakteri olarak yukarıda reddedilir).
    let bytes = yaml.as_bytes();
    if bytes.iter().enumerate().any(|(i, b)| *b == b'\r' && bytes.get(i + 1) != Some(&b'\n')) {
        return Err(ContractError::InvalidText);
    }
    // Belge işaretçileri (`---`, `...`) ve yönergeler (`%YAML`, `%TAG`) desteklenmez: bu satırlardan
    // sonraki düğümler tarayıcının düğüm-başı izini atlatabilir.
    if yaml.split('\n').any(|line| {
        let t = line.trim_start_matches([' ', '\t']);
        t.starts_with("---") || t.starts_with("...") || t.starts_with('%')
    }) {
        return Err(ContractError::YamlUnsupported);
    }
    // Olay düzeyinde ayrıştırma: çapa, takma ad ve etiket değer üretimine hiç ulaşmaz.
    let value = yaml_events_to_json(yaml)?;
    let Value::Object(map) = &value else {
        return Err(ContractError::MalformedFrontMatter);
    };
    if map.get("schema_version").and_then(Value::as_u64) != Some(u64::from(SCHEMA_VERSION)) {
        return Err(ContractError::UnknownSchemaVersion);
    }
    let mut nodes = 0usize;
    check_value_limits(&value, 1, &mut nodes)?;
    let front: FrontMatter = serde_json::from_value(value).map_err(|_| ContractError::InvalidField)?;
    front.validate()?;
    Ok(front)
}

/// Düz (plain) YAML skaleri için çekirdek şema tür çözümü: null, bool, tamsayı, kayan nokta ya da
/// metin. `.nan`/`.inf` reddedilir (JSON'a kayıpsız çevrilemez).
fn resolve_plain_scalar(s: &str) -> ContractResult<Value> {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF" => {
            return Err(ContractError::YamlUnsupported);
        }
        _ => {}
    }
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits(unsigned) {
        if let Ok(i) = s.parse::<i64>() {
            return Ok(Value::from(i));
        }
        if let Ok(u) = s.parse::<u64>() {
            return Ok(Value::from(u));
        }
        return float_value(s);
    }
    if let Some(h) = s.strip_prefix("0x")
        && !h.is_empty()
        && h.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return u64::from_str_radix(h, 16).map(Value::from).map_err(|_| ContractError::YamlUnsupported);
    }
    if let Some(o) = s.strip_prefix("0o")
        && !o.is_empty()
        && o.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return u64::from_str_radix(o, 8).map(Value::from).map_err(|_| ContractError::YamlUnsupported);
    }
    // Kayan nokta: [-+]? ( .d+ | d+ (. d*)? ) ([eE] [-+]? d+)?
    let (mant, exp) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let mant_ok = match mant.split_once('.') {
        Some((int, frac)) => (digits(int) && frac.bytes().all(|b| b.is_ascii_digit())) || (int.is_empty() && digits(frac)),
        None => digits(mant),
    };
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if mant_ok && exp_ok {
        return float_value(s);
    }
    Ok(Value::String(s.to_string()))
}

fn float_value(s: &str) -> ContractResult<Value> {
    let f: f64 = s.parse().map_err(|_| ContractError::YamlUnsupported)?;
    serde_json::Number::from_f64(f).map(Value::Number).ok_or(ContractError::YamlUnsupported)
}

/// Ayrıştırıcı olaylarından kurulan kısmi koleksiyon.
enum Frame {
    Seq(Vec<Value>),
    Map { map: serde_json::Map<String, Value>, key: Option<String> },
}

/// Tamamlanan değeri üst çerçeveye (ya da köke) yerleştirir.
fn place_value(stack: &mut [Frame], root: &mut Option<Value>, value: Value) -> ContractResult<()> {
    match stack.last_mut() {
        None => {
            if root.replace(value).is_some() {
                return Err(ContractError::YamlUnsupported);
            }
        }
        Some(Frame::Seq(items)) => items.push(value),
        Some(Frame::Map { map, key }) => {
            let k = key.take().ok_or(ContractError::YamlUnsupported)?;
            if map.insert(k, value).is_some() {
                return Err(ContractError::MalformedFrontMatter);
            }
        }
    }
    Ok(())
}

/// Ön bilgiyi saf Rust `yaml-rust2` olay API'siyle okur ve doğrudan `serde_json::Value` kurar.
/// Olaylar burada yürünür: `Alias` olayı, sıfırdan farklı çapa kimliği ve her etiket reddedilir
/// (alias genişletmesi/etiket çözümü hiçbir yerde çalışmaz). Çok belgeli akış, blok skaler
/// (`|`, `>`), metin olmayan anahtar, yinelenen anahtar, aşırı derinlik/düğüm reddedilir.
/// Tırnaklı skalerler her zaman metindir; çok satırlı tırnaklı/düz skalerler ayrıştırıcının
/// katlama kurallarıyla kabul edilir (artık yalnızca bir skalerdir, güvenlidir).
fn yaml_events_to_json(yaml: &str) -> ContractResult<Value> {
    use yaml_rust2::parser::{Event, Parser};
    use yaml_rust2::scanner::TScalarStyle;

    let mut parser = Parser::new_from_str(yaml);
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<Value> = None;
    let mut nodes = 0usize;
    let mut docs = 0usize;

    loop {
        let (event, _) = parser.next_token().map_err(|e| {
            // Tanımsız çapaya başvuru (`*x`) ayrıştırıcıda hata olarak biter; özünde bir takma addır.
            let info = e.info();
            if info.contains("unknown anchor") {
                ContractError::YamlAlias
            } else if info.contains("recursion limit") {
                ContractError::TooDeep
            } else {
                ContractError::MalformedFrontMatter
            }
        })?;
        let is_seq = matches!(event, Event::SequenceStart(..));
        match event {
            Event::Nothing | Event::StreamStart | Event::DocumentEnd => {}
            Event::StreamEnd => break,
            Event::DocumentStart => {
                docs += 1;
                if docs > 1 {
                    return Err(ContractError::YamlUnsupported);
                }
            }
            Event::Alias(_) => return Err(ContractError::YamlAlias),
            Event::Scalar(text, style, anchor, tag) => {
                if anchor != 0 {
                    return Err(ContractError::YamlAlias);
                }
                if tag.is_some() {
                    return Err(ContractError::YamlTag);
                }
                nodes += 1;
                if nodes > MAX_FRONT_MATTER_NODES {
                    return Err(ContractError::TooLarge);
                }
                if text.len() > MAX_FRONT_MATTER_SCALAR_BYTES {
                    return Err(ContractError::TooLarge);
                }
                let value = match style {
                    TScalarStyle::Plain => resolve_plain_scalar(&text)?,
                    TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => Value::String(text),
                    TScalarStyle::Literal | TScalarStyle::Folded => return Err(ContractError::YamlUnsupported),
                };
                // Eşleme anahtarı beklenen konumda yalnızca metin anahtar kabul edilir.
                if let Some(Frame::Map { key, .. }) = stack.last_mut()
                    && key.is_none()
                {
                    let Value::String(k) = value else {
                        return Err(ContractError::YamlUnsupported);
                    };
                    *key = Some(k);
                    continue;
                }
                place_value(&mut stack, &mut root, value)?;
            }
            Event::SequenceStart(anchor, tag) | Event::MappingStart(anchor, tag) => {
                if anchor != 0 {
                    return Err(ContractError::YamlAlias);
                }
                if tag.is_some() {
                    return Err(ContractError::YamlTag);
                }
                nodes += 1;
                if nodes > MAX_FRONT_MATTER_NODES {
                    return Err(ContractError::TooLarge);
                }
                // Eşleme anahtarı konumundaki koleksiyon (karmaşık anahtar) desteklenmez.
                if matches!(stack.last(), Some(Frame::Map { key: None, .. })) {
                    return Err(ContractError::YamlUnsupported);
                }
                if stack.len() >= MAX_FRONT_MATTER_DEPTH {
                    return Err(ContractError::TooDeep);
                }
                stack.push(if is_seq {
                    Frame::Seq(Vec::new())
                } else {
                    Frame::Map { map: serde_json::Map::new(), key: None }
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let value = match stack.pop() {
                    Some(Frame::Seq(items)) => Value::Array(items),
                    Some(Frame::Map { map, key: None }) => Value::Object(map),
                    _ => return Err(ContractError::MalformedFrontMatter),
                };
                place_value(&mut stack, &mut root, value)?;
            }
        }
    }
    if !stack.is_empty() {
        return Err(ContractError::MalformedFrontMatter);
    }
    root.ok_or(ContractError::MalformedFrontMatter)
}

/// `visibility=private` dışarı açılamaz: `remote_allowed=true` ile birlikte geçersizdir.
pub fn check_visibility_pair(visibility: Visibility, remote_allowed: bool) -> ContractResult<()> {
    if visibility == Visibility::Private && remote_allowed {
        return Err(ContractError::InvalidField);
    }
    Ok(())
}

/// Yeni not varsayılanları (kullanıcı sonradan açıkça değiştirebilir): `normal` duyarlılık
/// `shared` + `remote_allowed`; `personal`/`finance` `private` + `remote_allowed=false`.
pub fn default_visibility_for(sensitivity: Sensitivity) -> (Visibility, bool) {
    match sensitivity {
        Sensitivity::Normal => (Visibility::Shared, true),
        Sensitivity::Personal | Sensitivity::Finance => (Visibility::Private, false),
    }
}

fn json_for_yaml(value: &Value) -> ContractResult<String> {
    let json = serde_json::to_string(value).map_err(|_| ContractError::InvalidField)?;
    // YAML okuyucusunun reddettiği/satır sonu saydığı karakterleri JSON kaçışına çevir.
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if is_line_break_like(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn render_key(key: &str) -> ContractResult<String> {
    let plain = !key.is_empty()
        && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        && !matches!(key.to_ascii_lowercase().as_str(), "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n");
    if plain { Ok(key.to_string()) } else { json_for_yaml(&Value::String(key.to_string())) }
}

/// Ön bilgiyi `anahtar: <JSON akış değeri>` satırları olarak yazar. JSON, YAML'ın alt kümesidir;
/// böylece blok skaler/çok satırlı tırnak üretilmez ve yazılan çıktı `parse_front_matter` ile
/// kayıpsız geri okunur. Bilinen alanlar sabit sırada, ekstra anahtarlar alfabetiktir.
pub fn render_front_matter(front: &FrontMatter) -> ContractResult<String> {
    front.validate()?;
    let Value::Object(map) = serde_json::to_value(front).map_err(|_| ContractError::InvalidField)? else {
        return Err(ContractError::InvalidField);
    };
    let mut out = String::new();
    for key in KNOWN_KEYS.iter().copied().chain(front.extra.keys().map(String::as_str)) {
        if let Some(value) = map.get(key) {
            out.push_str(&render_key(key)?);
            out.push_str(": ");
            out.push_str(&json_for_yaml(value)?);
            out.push('\n');
        }
    }
    if out.len() > MAX_FRONT_MATTER_BYTES {
        return Err(ContractError::TooLarge);
    }
    Ok(out)
}

/// `---` ile başlayan notu (ön bilgi, gövde) olarak böler; BOM yok sayılır.
pub fn split_note(text: &str) -> ContractResult<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut offset = 0usize;
    let mut front_start = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        match front_start {
            None => {
                if trimmed != "---" {
                    return Err(ContractError::MalformedFrontMatter);
                }
                front_start = Some(offset + line.len());
            }
            Some(start) => {
                if trimmed == "---" {
                    return Ok((&text[start..offset], &text[offset + line.len()..]));
                }
                if offset + line.len() - start > MAX_FRONT_MATTER_BYTES {
                    return Err(ContractError::TooLarge);
                }
            }
        }
        offset += line.len();
    }
    Err(ContractError::MalformedFrontMatter)
}

/// Not metnini ön bilgi ve gövde olarak çözer.
pub fn parse_note(text: &str) -> ContractResult<(FrontMatter, String)> {
    if text.len() > MAX_BODY_BYTES + MAX_FRONT_MATTER_BYTES + 16 {
        return Err(ContractError::TooLarge);
    }
    let (yaml, body) = split_note(text)?;
    let front = parse_front_matter(yaml)?;
    check_text(body, MAX_BODY_BYTES, true)?;
    Ok((front, body.to_string()))
}

pub fn render_note(front: &FrontMatter, body: &str) -> ContractResult<String> {
    check_text(body, MAX_BODY_BYTES, true)?;
    Ok(format!("---\n{}---\n{}", render_front_matter(front)?, body))
}

// ---------------------------------------------------------------------------------------------
// Öneri, iş, işlem, değişim kayıtları
// ---------------------------------------------------------------------------------------------

/// Adayın yeni not mu yoksa mevcut notun değişikliği mi olduğu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SuggestionTarget {
    New,
    Update { id: MemoryId, expected_revision: Revision },
}

/// Model çıktısından türeyen, henüz kabul edilmemiş aday. Maskelenmiş olmalıdır.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionCandidate {
    pub target: SuggestionTarget,
    pub memory_type: MemoryType,
    pub title: String,
    pub summary: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
}

impl SuggestionCandidate {
    pub fn validate(&self) -> ContractResult<()> {
        check_line(&self.title, MAX_TITLE_CHARS, false)?;
        check_text(&self.summary, MAX_SUMMARY_CHARS * 4, true)?;
        if self.summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(ContractError::TooLarge);
        }
        check_text(&self.body, MAX_CANDIDATE_BODY_BYTES, false)?;
        validate_tags(&self.tags)?;
        if let Some(project) = &self.project {
            validate_project_name(project)?;
        }
        // Aday maskelenmiş olmalı; kalan sır deseni varsa reddedilir.
        for text in [&self.title, &self.summary, &self.body] {
            if crate::import::mask_secrets(text).0 != *text {
                return Err(ContractError::InvalidText);
            }
        }
        Ok(())
    }

    /// Kullanıcının gördüğü adayın ve bağlanacak makbuz listesinin (sıralı, tekil) kanonik
    /// hash'i; kabul bu hash'e bağlanır, makbuz listesi sonradan değişemez.
    pub fn candidate_hash(&self, receipts: &[Sha256Hash]) -> ContractResult<Sha256Hash> {
        self.validate()?;
        let mut sorted: Vec<&Sha256Hash> = receipts.iter().collect();
        sorted.sort();
        sorted.dedup();
        let bytes = serde_json::to_vec(&serde_json::json!({"candidate": self, "receipts": sorted}))
            .map_err(|_| ContractError::InvalidField)?;
        Ok(Sha256Hash::of(&bytes))
    }

    /// Telegram yalnız yeni not ekleyen adayı çözebilir.
    pub fn modifies_existing(&self) -> bool {
        matches!(self.target, SuggestionTarget::Update { .. })
    }
}

impl fmt::Debug for SuggestionCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuggestionCandidate")
            .field("target", &self.target)
            .field("type", &self.memory_type)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySuggestion {
    pub id: MemoryId,
    pub chat_id: String,
    pub logical_turn_id: String,
    pub end_seq: i64,
    pub candidate: SuggestionCandidate,
    pub candidate_hash: Sha256Hash,
    pub receipts: Vec<Sha256Hash>,
    pub status: SuggestionStatus,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<DecisionSource>,
}

impl MemorySuggestion {
    pub fn validate(&self) -> ContractResult<()> {
        validate_chat_id(&self.chat_id)?;
        check_line(&self.logical_turn_id, MAX_ID_TOKEN_CHARS, false)?;
        if self.end_seq < 1 {
            return Err(ContractError::InvalidSeq);
        }
        if self.candidate.candidate_hash(&self.receipts)? != self.candidate_hash || self.receipts.len() > MAX_RECEIPTS_PER_ITEM {
            return Err(ContractError::InvalidHash);
        }
        let decided = self.status != SuggestionStatus::Pending;
        if decided != (self.decided_at.is_some() && self.decided_by.is_some())
            || (!decided && (self.decided_at.is_some() || self.decided_by.is_some()))
        {
            return Err(ContractError::InvalidField);
        }
        Ok(())
    }
}

impl fmt::Debug for MemorySuggestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySuggestion")
            .field("id", &self.id)
            .field("status", &self.status)
            .field("candidate", &self.candidate)
            .finish_non_exhaustive()
    }
}

/// Kalıcı özet+aday işi; `(chat_id, logical_turn_id, end_seq)` benzersizdir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryJob {
    pub chat_id: String,
    pub logical_turn_id: String,
    pub end_seq: i64,
    pub status: MemoryJobStatus,
    pub attempt: u32,
    pub lease_until: Option<DateTime<Utc>>,
    pub generation: i64,
    pub error: Option<MemoryJobError>,
}

/// İşlemi başlatan taraf; `request_id` ile birlikte idempotency kapsamını belirler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationSource {
    Desktop,
    Telegram,
    Engine,
    System,
}

/// İsteğin parmak izi: tür + kaynak + yük. Bu fonksiyon MASKELEMEZ; çağıran, zaten maskelenmiş
/// kanonik yükü vermelidir (hedef kimlik ve `expected_revision` dahil, yükün parçası olarak).
/// Aynı `request_id` yalnız aynı parmak iziyle tekrarlanabilir.
pub fn request_fingerprint<T: Serialize>(
    kind: MemoryOperationKind,
    source: OperationSource,
    payload: &T,
) -> ContractResult<Sha256Hash> {
    let bytes = serde_json::to_vec(&serde_json::json!({"kind": kind, "source": source, "payload": payload}))
        .map_err(|_| ContractError::InvalidField)?;
    Ok(Sha256Hash::of(&bytes))
}

/// Dosya/DB uzlaştırması için idempotent işlem kaydı.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryOperation {
    pub request_id: String,
    pub request_hash: Sha256Hash,
    pub source: OperationSource,
    pub kind: MemoryOperationKind,
    pub target_id: Option<MemoryId>,
    pub expected_revision: Option<Revision>,
    pub state: MemoryOperationState,
    pub result_revision: Option<Revision>,
    pub content_hash: Option<Sha256Hash>,
    pub receipt_hash: Option<Sha256Hash>,
}

impl MemoryOperation {
    /// Aynı `request_id` ile gelen yeni isteğin parmak izi kayıtlıyla eşleşmezse
    /// `RequestConflict`; eşleşirse çağıran ilk sonucu döndürür (yeniden yürütmez).
    pub fn check_replay(&self, request_hash: &Sha256Hash) -> ContractResult<()> {
        if &self.request_hash == request_hash { Ok(()) } else { Err(ContractError::RequestConflict) }
    }
}

/// Global kasa değişimi; sahte `chat_id` üretmeden UI'nın cursor'dan delta yenilemesini sağlar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryChange {
    pub cursor: i64,
    pub item_id: Option<MemoryId>,
    pub suggestion_id: Option<MemoryId>,
    pub revision: Option<Revision>,
    pub kind: MemoryChangeKind,
}

// ---------------------------------------------------------------------------------------------
// Gölge onay puanı
// ---------------------------------------------------------------------------------------------

/// Yerel modelin P(güvenli) ölçümü. Yalnız günlük/kayıt içindir; karar veya eşik alanı yoktur.
/// İstemci puan yazamaz: bu tipi taşıyan hiçbir `Request` varyantı yoktur.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalShadowScore {
    pub approval_id: String,
    pub model: String,
    pub prompt_version: String,
    pub p_safe: f64,
    pub measured_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub action_fingerprint: Sha256Hash,
}

impl ApprovalShadowScore {
    pub fn validate(&self) -> ContractResult<()> {
        check_line(&self.approval_id, MAX_ID_TOKEN_CHARS, false)?;
        check_line(&self.model, 128, false)?;
        check_line(&self.prompt_version, 64, false)?;
        if !self.p_safe.is_finite() || !(0.0..=1.0).contains(&self.p_safe) {
            return Err(ContractError::InvalidField);
        }
        Ok(())
    }
}

/// `ultron approvals shadow-report` / masaüstü sonucu. `agree`: model güvenli (p >= 0.5) dedi ve
/// kullanıcı onayladı ya da güvensiz dedi ve reddetti. `false_safe`: model güvenli dedi, kullanıcı
/// reddetti. Hesap Task 7/8'e aittir; burada yalnız tel biçimi sabittir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowReport {
    pub since: DateTime<Utc>,
    pub total_asks: u64,
    pub scored: u64,
    pub resolved_scored: u64,
    pub agree: u64,
    pub disagree: u64,
    pub false_safe: u64,
    pub false_unsafe: u64,
    pub mean_p_safe: Option<f64>,
    pub models: Vec<String>,
    pub prompt_versions: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Getirme sözleşmesi
// ---------------------------------------------------------------------------------------------

/// Getirmeyi isteyen taraf. Bütün filtreler snippet çıkarılmadan ve sıralanmadan önce uygulanır.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Desktop,
    Engine,
    Telegram,
}

impl Audience {
    /// Bir öğenin bu hedef kitleye açılıp açılmadığı. İstemci/model kitleyi veya projeyi seçemez;
    /// daemon bunları tur belirteci/izinli kimlikten çözer.
    ///
    /// - `Desktop`: yerel kullanıcı her şeyi görür.
    /// - `Engine`/`Telegram` (verinin makine dışına çıkabildiği kitleler): `visibility != Private`
    ///   VE `remote_allowed` şarttır (koordinatör kararı 2026-11-18).
    ///   Ayrıca öğenin projesi varsa kapsam projesi aynı olmalı; `Visibility::Project` projesiz
    ///   öğe bozuk sayılır ve açılmaz.
    pub fn admits(
        self,
        visibility: Visibility,
        remote_allowed: bool,
        item_project: Option<&str>,
        scope_project: Option<&str>,
    ) -> bool {
        if self == Audience::Desktop {
            return true;
        }
        // Dışarı çıkabilen kitleler: `private` hiçbir zaman açılmaz ve `remote_allowed` şarttır.
        if visibility == Visibility::Private || !remote_allowed {
            return false;
        }
        match (item_project, visibility) {
            (Some(item), _) => scope_project == Some(item),
            (None, Visibility::Project) => false,
            (None, _) => true,
        }
    }
}

/// Bütçe: kesin tokenizer yok; UTF-8 bayt sayısı token üst tahmini, ayrıca karakter sınırı.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    pub max_tokens: u32,
    pub max_chars: u32,
}

/// Token üst tahmini (tüm çit/atıf dahil UTF-8 bayt sayısı); `chars/4` kesin hesap değildir.
pub fn token_upper_bound(text: &str) -> u32 {
    u32::try_from(text.len()).unwrap_or(u32::MAX)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContextItem {
    pub item_id: MemoryId,
    pub revision: Revision,
    pub title: String,
    pub summary: String,
    pub snippet: String,
    pub receipts: Vec<Sha256Hash>,
    pub index_state: IndexState,
    pub truncated: bool,
}

impl fmt::Debug for MemoryContextItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryContextItem")
            .field("item_id", &self.item_id)
            .field("revision", &self.revision)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

/// `retrieve(...)` ve `memory_search` çıktısı; güvenilmeyen veri çitine konacak ham maddedir.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryContext {
    pub items: Vec<MemoryContextItem>,
    pub truncated: bool,
    pub budget: MemoryBudget,
    pub used_chars: u32,
    pub token_upper_bound: u32,
}

impl fmt::Debug for MemoryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryContext")
            .field("items", &self.items.len())
            .field("truncated", &self.truncated)
            .field("used_chars", &self.used_chars)
            .finish()
    }
}

/// Dar motor aracı (`memory_search` / `memory_get`) girdileri; kitle/proje/sohbet içermez.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryToolRequest {
    MemorySearch { query: String, limit: Option<u32> },
    MemoryGet { id: MemoryId },
}

impl MemoryToolRequest {
    pub fn validate(&self) -> ContractResult<()> {
        match self {
            MemoryToolRequest::MemorySearch { query, limit } => {
                validate_query(query)?;
                validate_page_limit(*limit, 20)
            }
            MemoryToolRequest::MemoryGet { .. } => Ok(()),
        }
    }
}

impl fmt::Debug for MemoryToolRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryToolRequest::MemorySearch { limit, .. } => {
                write!(f, "MemorySearch {{ query: <gizli>, limit: {limit:?} }}")
            }
            MemoryToolRequest::MemoryGet { id } => write!(f, "MemoryGet {{ id: {id} }}"),
        }
    }
}

pub fn validate_query(q: &str) -> ContractResult<()> {
    check_line(q, MAX_QUERY_CHARS, false)
}

// ---------------------------------------------------------------------------------------------
// IPC DTO'ları
// ---------------------------------------------------------------------------------------------

/// Sohbete bağlı yakalamada istemcinin gösterdiği sohbet referansı. Daemon aralığın DB'de
/// gerçekten var olduğunu doğrular; istemci serbest kaynak yolu veya Telegram kimliği gönderemez.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRef {
    pub chat_id: String,
    pub seq_from: i64,
    pub seq_to: i64,
}

impl ChatRef {
    pub fn validate(&self) -> ContractResult<()> {
        SourceRef::Event { chat_id: self.chat_id.clone(), seq_from: self.seq_from, seq_to: self.seq_to }.validate()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCaptureRequest {
    pub request_id: String,
    pub text: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub chat_ref: Option<ChatRef>,
}

impl MemoryCaptureRequest {
    pub fn validate(&self) -> ContractResult<()> {
        validate_request_id(&self.request_id)?;
        check_text(&self.text, MAX_CAPTURE_TEXT_BYTES, false)?;
        if let Some(title) = &self.title {
            check_line(title, MAX_TITLE_CHARS, false)?;
        }
        if let Some(project) = &self.project {
            validate_project_name(project)?;
        }
        validate_tags(&self.tags)?;
        if let Some(chat_ref) = &self.chat_ref {
            chat_ref.validate()?;
        }
        Ok(())
    }
}

impl fmt::Debug for MemoryCaptureRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryCaptureRequest")
            .field("request_id", &self.request_id)
            .field("text_bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Kısmi düzenleme. `None`: dokunma; `project: Some(None)`: projeden ayır.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub project: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<Sensitivity>,
}

impl MemoryPatch {
    pub fn is_empty(&self) -> bool {
        self == &MemoryPatch::default()
    }

    pub fn validate(&self) -> ContractResult<()> {
        if self.is_empty() {
            return Err(ContractError::InvalidField);
        }
        if let Some(title) = &self.title {
            check_line(title, MAX_TITLE_CHARS, false)?;
        }
        if let Some(summary) = &self.summary {
            check_text(summary, MAX_SUMMARY_CHARS * 4, true)?;
            if summary.chars().count() > MAX_SUMMARY_CHARS {
                return Err(ContractError::TooLarge);
            }
        }
        if let Some(body) = &self.body {
            check_text(body, MAX_BODY_BYTES, true)?;
        }
        if let Some(tags) = &self.tags {
            validate_tags(tags)?;
        }
        if let Some(Some(project)) = &self.project {
            validate_project_name(project)?;
        }
        // Yama içindeki çelişki; mevcut notla birleşik durum uygulama sırasında
        // `check_visibility_pair` ile yeniden denetlenmelidir.
        if let (Some(Visibility::Private), Some(true)) = (self.visibility, self.remote_allowed) {
            return Err(ContractError::InvalidField);
        }
        Ok(())
    }
}

impl fmt::Debug for MemoryPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryPatch")
            .field("title", &self.title.is_some())
            .field("summary", &self.summary.is_some())
            .field("body", &self.body.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpdateRequest {
    pub request_id: String,
    pub id: MemoryId,
    pub expected_revision: Revision,
    pub patch: MemoryPatch,
}

impl MemoryUpdateRequest {
    pub fn validate(&self) -> ContractResult<()> {
        validate_request_id(&self.request_id)?;
        self.patch.validate()
    }
}

/// Liste/arama/ayrıntıda gösterilen not özeti. Gövde yoktur.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItemView {
    pub id: MemoryId,
    pub memory_type: MemoryType,
    pub title: String,
    pub summary: String,
    pub project: Option<String>,
    pub visibility: Visibility,
    pub remote_allowed: bool,
    pub sensitivity: Sensitivity,
    pub revision: Revision,
    pub content_hash: Sha256Hash,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub review_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub index_state: IndexState,
    pub receipt_count: u32,
}

impl fmt::Debug for MemoryItemView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryItemView")
            .field("id", &self.id)
            .field("revision", &self.revision)
            .field("index_state", &self.index_state)
            .finish_non_exhaustive()
    }
}

/// Kullanıcıya gösterilen makbuz; kaynak "kanıt yok" olabilir (`evidence_available = false`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptView {
    pub hash: Sha256Hash,
    pub source: SourceRef,
    pub claim_status: ClaimStatus,
    pub captured_at: DateTime<Utc>,
    pub excerpt: String,
    pub evidence_available: bool,
}

impl fmt::Debug for ReceiptView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptView")
            .field("hash", &self.hash)
            .field("claim_status", &self.claim_status)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItemDetail {
    pub item: MemoryItemView,
    pub body: String,
    pub receipts: Vec<ReceiptView>,
}

impl fmt::Debug for MemoryItemDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryItemDetail")
            .field("item", &self.item)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchHit {
    pub item: MemoryItemView,
    pub snippet: String,
}

impl fmt::Debug for MemorySearchHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySearchHit").field("item", &self.item).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRefreshReport {
    pub scanned: u32,
    pub changed: u32,
    pub conflicts: u32,
    pub quarantined: u32,
    pub index_status: MemoryIndexStatus,
}

// ---------------------------------------------------------------------------------------------
// Yetki matrisi
// ---------------------------------------------------------------------------------------------

/// Çağıran kimlik sınıfı. Sınıf bağlantıdan/tur belirtecinden daemon tarafından çözülür.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCaller {
    /// `require_approver` geçmiş yerel masaüstü/CLI.
    Desktop,
    /// İzinli Telegram kullanıcısı (uygulama katmanı üçlü kimlik filtresini yeniden denetler).
    Telegram,
    /// Motor requester'ı; yalnız canlı tur belirteciyle okuyabilir.
    Engine { live_turn_token: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    List,
    Get,
    Search,
    Capture,
    Update,
    Delete,
    ListSuggestions,
    /// `modifies_existing`: aday mevcut bir notu değiştiriyorsa Telegram'a kapalıdır.
    ResolveSuggestion { modifies_existing: bool },
    Refresh,
    ShadowReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzDenied {
    NotAllowed,
    TokenRequired,
    UseDesktop,
}

impl fmt::Display for AuthzDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AuthzDenied::NotAllowed => "bu çağıran için hafıza işlemi yetkisiz",
            AuthzDenied::TokenRequired => "canlı tur belirteci gerekli",
            AuthzDenied::UseDesktop => "bu işlem yalnız masaüstünden yapılabilir",
        })
    }
}

impl std::error::Error for AuthzDenied {}

/// Yetki matrisi (tek kaynak). Başarıda arama/getirme için kullanılacak kitleyi döndürür.
///
/// | İşlem                         | Desktop | Telegram              | Motor (canlı belirteç) |
/// | ----------------------------- | ------- | --------------------- | ---------------------- |
/// | List/Update/Delete/Refresh    | evet    | hayır                 | hayır                  |
/// | ListSuggestions/ShadowReport  | evet    | hayır                 | hayır                  |
/// | Get                           | evet    | hayır                 | evet (kitle filtreli)  |
/// | Search                        | evet    | evet (remote_allowed) | evet (kitle filtreli)  |
/// | Capture                       | evet    | evet (`/not`)         | hayır                  |
/// | ResolveSuggestion (yeni not)  | evet    | evet                  | hayır                  |
/// | ResolveSuggestion (değişiklik)| evet    | masaüstüne yönlendir  | hayır                  |
pub fn authorize(caller: MemoryCaller, op: MemoryOp) -> Result<Audience, AuthzDenied> {
    match caller {
        MemoryCaller::Desktop => Ok(Audience::Desktop),
        MemoryCaller::Telegram => match op {
            MemoryOp::Search | MemoryOp::Capture | MemoryOp::ResolveSuggestion { modifies_existing: false } => {
                Ok(Audience::Telegram)
            }
            MemoryOp::ResolveSuggestion { modifies_existing: true } => Err(AuthzDenied::UseDesktop),
            _ => Err(AuthzDenied::NotAllowed),
        },
        MemoryCaller::Engine { live_turn_token } => match op {
            MemoryOp::Search | MemoryOp::Get => {
                if live_turn_token { Ok(Audience::Engine) } else { Err(AuthzDenied::TokenRequired) }
            }
            _ => Err(AuthzDenied::NotAllowed),
        },
    }
}
