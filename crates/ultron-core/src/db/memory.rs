//! Hafıza indeksi, arama ve kalıcı durum depoları (Aşama 5a Task 3).
//!
//! Kabul edilmiş hafızanın doğruluk kaynağı Markdown kasasıdır; `memory_items`, `memory_fts` ve
//! `memory_item_receipts` yeniden üretilebilir önbelleklerdir. Öneri, iş ve işlem tabloları
//! kalıcı durumdur; indeks yeniden kurulumu bunlara dokunmaz. Bütün sorgular parametrelidir.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{Db, parse_ts};
use crate::memory::search::{fold_text, fts_query};
use crate::memory::{
    Audience, ClaimStatus, ContractError, ContractResult, DecisionSource, DEFAULT_PAGE_LIMIT,
    IndexState, MAX_BODY_BYTES, MAX_DETAIL_RECEIPTS, MAX_ID_TOKEN_CHARS, MAX_PAGE_LIMIT,
    MAX_RECEIPT_EXCERPT_CHARS, MAX_RECEIPTS_PER_ITEM, MAX_SUMMARY_CHARS,
    MAX_TAG_CHARS, MAX_TAGS, MAX_TITLE_CHARS, MemoryChange, MemoryChangeKind, MemoryId,
    MemoryItemDetail, MemoryItemView, MemoryJob, MemoryJobError, MemoryJobStatus, MemoryOperation,
    MemoryOperationState, MemoryPage, MemorySearchHit, MemorySuggestion, MemoryType, Receipt,
    ReceiptView, Revision, Sensitivity, Sha256Hash, SourceRef, SuggestionCandidate,
    SuggestionStatus, SuggestionTarget, Visibility, check_line, check_text,
    check_visibility_pair, validate_chat_id, validate_cursor, validate_locator, validate_query,
    validate_project_name, validate_request_id,
};

/// Bir iş için en fazla deneme; süresi dolan kiralama bu sınırdan sonra başarısız sayılır.
pub const MEMORY_JOB_MAX_ATTEMPTS: u32 = 3;
/// Tek arama yanıtının toplam metin bayt bütçesi (en az bir sonuç her zaman döner).
pub const SEARCH_RESPONSE_BYTES: usize = 32 * 1024;
const SNIPPET_CHARS: usize = 240;

/// Yalnız kabul edilmiş, indekslenmiş, çöpe taşınmamış öğeler görünür. Sütun sırası
/// [`ItemRow::from_row`] ile birebir eşleşir (0..=16).
const ITEM_COLS: &str = "i.id, i.memory_type, i.project, i.visibility, i.remote_allowed, \
    i.sensitivity, i.title, i.summary, i.body, i.tags_json, i.revision, i.content_hash, \
    i.created_at, i.updated_at, i.review_at, i.index_state, \
    (SELECT COUNT(*) FROM memory_item_receipts r WHERE r.item_id = i.id AND r.revision = i.revision)";

const UPSERT_ITEM_SQL: &str = "INSERT INTO memory_items (id, relative_path, memory_type, project, \
    visibility, remote_allowed, sensitivity, title, summary, body, tags_json, revision, \
    content_hash, created_at, updated_at, review_at, index_state, trashed_at) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) \
    ON CONFLICT(id) DO UPDATE SET relative_path = excluded.relative_path, \
    memory_type = excluded.memory_type, project = excluded.project, \
    visibility = excluded.visibility, remote_allowed = excluded.remote_allowed, \
    sensitivity = excluded.sensitivity, title = excluded.title, summary = excluded.summary, \
    body = excluded.body, tags_json = excluded.tags_json, revision = excluded.revision, \
    content_hash = excluded.content_hash, created_at = excluded.created_at, \
    updated_at = excluded.updated_at, review_at = excluded.review_at, \
    index_state = excluded.index_state, trashed_at = excluded.trashed_at";

/// Kasadan doğrulanmış tek bir not revizyonu (indeks deltası). Makbuzlar en az bir tane olmalı.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItemRecord {
    pub id: MemoryId,
    pub relative_path: String,
    pub memory_type: MemoryType,
    pub project: Option<String>,
    pub visibility: Visibility,
    pub remote_allowed: bool,
    pub sensitivity: Sensitivity,
    pub title: String,
    pub summary: String,
    pub body: String,
    pub tags: Vec<String>,
    pub revision: Revision,
    pub content_hash: Sha256Hash,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub review_at: Option<DateTime<Utc>>,
    pub index_state: IndexState,
    pub trashed_at: Option<DateTime<Utc>>,
    pub receipts: Vec<ReceiptRecord>,
}

impl MemoryItemRecord {
    pub fn validate(&self) -> ContractResult<()> {
        validate_locator(&self.relative_path)?;
        check_line(&self.title, MAX_TITLE_CHARS, false)?;
        check_text(&self.summary, MAX_SUMMARY_CHARS * 4, true)?;
        if self.summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(ContractError::TooLarge);
        }
        check_text(&self.body, MAX_BODY_BYTES, true)?;
        if self.tags.len() > MAX_TAGS {
            return Err(ContractError::LimitExceeded);
        }
        for tag in &self.tags {
            check_line(tag, MAX_TAG_CHARS, false)?;
        }
        if let Some(project) = &self.project {
            validate_project_name(project)?;
        }
        check_visibility_pair(self.visibility, self.remote_allowed)?;
        if self.updated_at < self.created_at {
            return Err(ContractError::InvalidField);
        }
        if self.receipts.is_empty() {
            return Err(ContractError::InvalidField);
        }
        if self.receipts.len() > MAX_RECEIPTS_PER_ITEM {
            return Err(ContractError::LimitExceeded);
        }
        for receipt in &self.receipts {
            receipt.validate()?;
        }
        Ok(())
    }
}

/// Kabul edilmiş revizyona bağlı makbuz (`memory_receipts` satırı).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptRecord {
    pub hash: Sha256Hash,
    pub relative_path: String,
    pub source: SourceRef,
    pub claim_status: ClaimStatus,
    pub excerpt: String,
    pub captured_at: DateTime<Utc>,
    pub evidence_available: bool,
}

impl ReceiptRecord {
    pub fn validate(&self) -> ContractResult<()> {
        if self.relative_path != Receipt::relative_path(&self.hash) {
            return Err(ContractError::InvalidLocator);
        }
        self.source.validate()?;
        check_text(&self.excerpt, MAX_RECEIPT_EXCERPT_CHARS * 4, true)?;
        if self.excerpt.chars().count() > MAX_RECEIPT_EXCERPT_CHARS {
            return Err(ContractError::TooLarge);
        }
        Ok(())
    }
}

/// Upsert sonucu. `StaleRevision` ve `HashMismatch` hata değil, CAS/çakışma kararıdır.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    /// Aynı `(id, revision, content_hash)` ve aynı dizin durumu/yol/görünürlük/proje/hassasiyet:
    /// yeniden uygulama yok, tekrar değişim kaydı yok. Bunlardan biri farklıysa satır ve FTS
    /// güncellenir (`Updated`); görünürlük daralması her zaman uygulanır.
    Unchanged,
    /// Daha eski revizyon; mevcut revizyon korunur.
    StaleRevision { current: Revision },
    /// Aynı revizyon, farklı içerik hash'i: çakışma.
    HashMismatch,
    /// `relative_path` başka bir öğeye ait (UNIQUE NOCASE); hiçbir şey yazılmadı.
    PathConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivateOutcome {
    Deactivated,
    AlreadyInactive,
    NotFound,
    StaleRevision { current: Revision },
    HashMismatch,
}

/// Arama/listede dönen tek öğenin ham satırı.
struct ItemRow {
    id: String,
    memory_type: String,
    project: Option<String>,
    visibility: String,
    remote_allowed: bool,
    sensitivity: String,
    title: String,
    summary: String,
    body: String,
    tags_json: String,
    revision: i64,
    content_hash: String,
    created_at: String,
    updated_at: String,
    review_at: Option<String>,
    index_state: String,
    receipt_count: i64,
}

impl ItemRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
        Ok(ItemRow {
            id: r.get(0)?,
            memory_type: r.get(1)?,
            project: r.get(2)?,
            visibility: r.get(3)?,
            remote_allowed: r.get(4)?,
            sensitivity: r.get(5)?,
            title: r.get(6)?,
            summary: r.get(7)?,
            body: r.get(8)?,
            tags_json: r.get(9)?,
            revision: r.get(10)?,
            content_hash: r.get(11)?,
            created_at: r.get(12)?,
            updated_at: r.get(13)?,
            review_at: r.get(14)?,
            index_state: r.get(15)?,
            receipt_count: r.get(16)?,
        })
    }
}

type ReceiptRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
    String,
    String,
    String,
);

type JobRow = (String, i64, Option<String>, i64, Option<String>);

type SuggestionRow = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// İş talebi sonucu: başarılı kiralama, sahiplenilen iş üzerindeki jeton (`generation`) ile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryJobClaim {
    pub id: i64,
    pub chat_id: String,
    pub logical_turn_id: String,
    pub end_seq: i64,
    pub generation: i64,
    pub attempt: u32,
}

/// Sınırlı kuyruğa alma sonucu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEnqueue {
    Queued,
    Duplicate,
    Full,
}

/// Öneri ekleme sonucu: aynı tur+aday daha önce varsa mevcut kimlik döner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionInsert {
    Inserted(MemoryId),
    Existing(MemoryId),
}

/// Öneri karar isteği. `candidate_hash` kullanıcının gördüğü adayla eşleşmelidir.
#[derive(Debug, Clone, Copy)]
pub struct SuggestionDecisionRequest<'a> {
    pub id: &'a MemoryId,
    pub to: SuggestionStatus,
    pub by: DecisionSource,
    pub request_id: &'a str,
    pub request_hash: &'a Sha256Hash,
    pub candidate_hash: &'a Sha256Hash,
    pub accepted_item_id: Option<&'a MemoryId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionDecision {
    NotFound,
    /// Bu çağrı kararı yazdı.
    Decided(SuggestionStatus),
    /// Aynı `request_id` ve aynı parmak izi: ilk sonuç aynen döner.
    Replayed(SuggestionStatus),
    /// Farklı istek, ilk karar kazandı.
    AlreadyDecided(SuggestionStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationBegin {
    Started,
    /// Aynı `request_id` ve parmak izi: kayıtlı işlem (yeniden yürütülmez).
    Replay(MemoryOperation),
}

/// İşlem durumunu ilerletme sonucu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceOutcome {
    Advanced,
    /// İşlem zaten istenen durumda (`Completed -> Completed`): idempotent yeniden çağrı.
    AlreadyAtState,
    /// Bilinmeyen işlem veya geçersiz geçiş.
    Rejected,
}

/// İşlem durumunu ilerletme isteği. Geçersiz geçişler reddedilir (`AdvanceOutcome::Rejected`).
#[derive(Debug, Clone, Copy)]
pub struct OperationAdvance<'a> {
    pub request_id: &'a str,
    pub to: MemoryOperationState,
    pub result_revision: Option<Revision>,
    pub content_hash: Option<&'a Sha256Hash>,
    pub receipt_hash: Option<&'a Sha256Hash>,
    pub error_code: Option<&'a str>,
}

/// Arama, liste ve getirme için yetki kısıtı. Tablo alias'ı `i` olmalıdır.
/// `project_param` bağlı parametrenin adıdır (`Option<&str>`).
///
/// Uzak kitleler (Engine/Telegram) için [`Audience::admits`] ile aynı koşul SQL'e taşınır:
/// `visibility != private`, `remote_allowed` ve proje kapsamı. Masaüstü her öğeyi görür; proje
/// parametresi verilirse yalnız o projeyi ve projesizleri filtreler.
fn audience_sql(audience: Audience, project_param: &str) -> String {
    match audience {
        Audience::Desktop => {
            format!("({project_param} IS NULL OR i.project IS NULL OR i.project = {project_param})")
        }
        Audience::Engine | Audience::Telegram => format!(
            "(i.visibility <> 'private' AND i.remote_allowed = 1 \
             AND ((i.project IS NULL AND i.visibility <> 'project') OR i.project = {project_param}))"
        ),
    }
}

impl Db {
    /// Tek bir kabul edilmiş revizyonu indeksler. Tek transaction: `memory_items`,
    /// `memory_receipts`, `memory_item_receipts`, `memory_fts` ve `memory_changes`.
    pub fn memory_upsert_item(&self, item: &MemoryItemRecord) -> Result<UpsertOutcome> {
        item.validate()?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = upsert_item_tx(&tx, item, false)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Mevcut revizyonu pasifleştirir (dış silme veya çöp). Revizyon CAS'ı zorunludur.
    pub fn memory_deactivate_item(
        &self,
        id: &MemoryId,
        revision: Revision,
        content_hash: &Sha256Hash,
    ) -> Result<DeactivateOutcome> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT revision, content_hash, index_state FROM memory_items WHERE id = ?1",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((rev, hash, state)) = row else {
            return Ok(DeactivateOutcome::NotFound);
        };
        let current = revision_of(rev)?;
        if current != revision {
            return Ok(DeactivateOutcome::StaleRevision { current });
        }
        if hash != content_hash.prefixed() {
            return Ok(DeactivateOutcome::HashMismatch);
        }
        if state == "inactive" {
            return Ok(DeactivateOutcome::AlreadyInactive);
        }
        tx.execute(
            "UPDATE memory_items SET index_state = 'inactive' WHERE id = ?1",
            [id.as_str()],
        )?;
        // FTS satırının rowid'si `memory_items.rowid` ile aynıdır (bkz. `upsert_item_tx`).
        tx.execute(
            "DELETE FROM memory_fts WHERE rowid = (SELECT rowid FROM memory_items WHERE id = ?1)",
            [id.as_str()],
        )?;
        append_change_tx(&tx, Some(id.as_str()), None, Some(revision), MemoryChangeKind::Deactivated)?;
        tx.commit()?;
        Ok(DeactivateOutcome::Deactivated)
    }

    /// Hafıza indeksini sıfırdan kurar. Yalnız `memory_items`, `memory_fts` ve
    /// `memory_item_receipts` temizlenir ve verilen deltalarla doldurulur; öneri, iş ve işlem
    /// tabloları aynen kalır. Hata olursa transaction geri alınır.
    pub fn memory_rebuild_index<I>(&self, items: I) -> Result<usize>
    where
        I: IntoIterator<Item = MemoryItemRecord>,
    {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "DELETE FROM memory_item_receipts; DELETE FROM memory_fts; DELETE FROM memory_items;",
        )?;
        let mut inserted = 0usize;
        for item in items {
            item.validate()?;
            match upsert_item_tx(&tx, &item, true)? {
                UpsertOutcome::Inserted => inserted += 1,
                UpsertOutcome::Unchanged => {}
                other => return Err(anyhow!("yeniden kurulumda beklenmeyen sonuç: {other:?}")),
            }
        }
        append_change_tx(&tx, None, None, None, MemoryChangeKind::IndexRebuilt)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Sayfalı BM25 araması. Sayfalama ofset tabanlıdır: sayfalar arasında indeks değişirse
    /// sonuçlar kayabilir (kopya/atlama); imleç başka sorgu/kitle/proje ile kullanılamaz. Filtreler (iz, durum, kitle, proje) sıralama ve snippet'ten önce
    /// SQL'de uygulanır; gizli kayıtların sayısı hiçbir yerde döndürülmez.
    /// Sorgu sözcükleri literal kabul edilir; boş veya yalnız noktalama içeren sorgu boş sayfa döner.
    pub fn memory_search(
        &self,
        query: &str,
        audience: Audience,
        project: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<MemoryPage<MemorySearchHit>> {
        // Boş/yalnız boşluklu sorgu hata değildir: boş sayfa döner.
        if !query.trim().is_empty() {
            validate_query(query)?;
        }
        if let Some(c) = cursor {
            validate_cursor(c)?;
        }
        let limit = clamp_limit(limit) as usize;
        let Some(expr) = fts_query(query) else {
            return Ok(MemoryPage { items: Vec::new(), next_cursor: None });
        };
        let binding = cursor_binding(query, audience, project);
        let offset = match cursor {
            Some(c) => parse_search_cursor(c, &binding)?,
            None => 0,
        };
        let sql = format!(
            "WITH hits AS (
                SELECT i.id AS hid, bm25(memory_fts) AS score
                FROM memory_fts JOIN memory_items i ON i.id = memory_fts.item_id
                WHERE memory_fts MATCH ?1 AND i.index_state = 'indexed' AND i.trashed_at IS NULL
                  AND {aud}
            )
            SELECT {ITEM_COLS}
            FROM hits h JOIN memory_items i ON i.id = h.hid
            ORDER BY h.score, i.id
            LIMIT ?2 OFFSET ?3",
            aud = audience_sql(audience, "?4"),
        );
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let fetched = stmt
            .query_map(
                params![expr, (limit + 1) as i64, offset as i64, project],
                ItemRow::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let total = fetched.len();
        let mut items = Vec::new();
        let mut used = 0usize;
        for row in fetched {
            if items.len() >= limit {
                break;
            }
            let (view, body) = row_to_view(row)?;
            let snippet = snippet_of(&view.summary, &body);
            let cost = view.title.len() + view.summary.len() + snippet.len();
            if !items.is_empty() && used + cost > SEARCH_RESPONSE_BYTES {
                break;
            }
            used += cost;
            items.push(MemorySearchHit { item: view, snippet });
        }
        // İmleç opaktır: yalnız konum ofseti ve sorgu+kitle+proje bağlayıcısı taşır; bm25 skoru
        // veya kimlik taşımaz (skor gizli satırlara bağlı IDF sızdırırdı).
        let next_cursor = if total > items.len() && !items.is_empty() {
            Some(format!("o{}.{binding}", offset + items.len()))
        } else {
            None
        };
        Ok(MemoryPage { items, next_cursor })
    }

    /// Kitleye açık öğeleri güncellik sırasıyla listeler (`updated_at` azalan, `id` artan).
    pub fn memory_list(
        &self,
        audience: Audience,
        cursor: Option<&str>,
    ) -> Result<MemoryPage<MemoryItemView>> {
        let (after_ts, after_id) = match cursor {
            Some(c) => {
                validate_cursor(c)?;
                let (ts, id) = c.split_once('|').ok_or(ContractError::InvalidField)?;
                let ts = ts_string(&parse_ts(ts)?);
                MemoryId::parse(id)?;
                (Some(ts), Some(id.to_string()))
            }
            None => (None, None),
        };
        let limit = clamp_limit(None) as usize;
        let sql = format!(
            "SELECT {ITEM_COLS} FROM memory_items i
             WHERE i.index_state = 'indexed' AND i.trashed_at IS NULL AND {aud}
               AND (?1 IS NULL OR i.updated_at < ?1 OR (i.updated_at = ?1 AND i.id > ?2))
             ORDER BY i.updated_at DESC, i.id ASC
             LIMIT ?3",
            aud = audience_sql(audience, "?4"),
        );
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(
                params![after_ts, after_id, (limit + 1) as i64, Option::<&str>::None],
                ItemRow::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = match rows.last() {
            Some(last) if has_more => Some(format!("{}|{}", last.updated_at, last.id)),
            _ => None,
        };
        let items = rows
            .into_iter()
            .map(|r| row_to_view(r).map(|(view, _)| view))
            .collect::<Result<Vec<_>>>()?;
        Ok(MemoryPage { items, next_cursor })
    }

    /// Kimliğe göre ayrıntı. Kitle göremiyorsa veya kayıt gizliyse `None` döner (varlık sızmaz).
    pub fn memory_get(&self, id: &MemoryId, audience: Audience) -> Result<Option<MemoryItemDetail>> {
        let conn = self.conn()?;
        let sql = format!(
            "SELECT {ITEM_COLS} FROM memory_items i
             WHERE i.id = ?1 AND i.index_state = 'indexed' AND i.trashed_at IS NULL AND {aud}",
            aud = audience_sql(audience, "?2"),
        );
        let row = conn
            .query_row(&sql, params![id.as_str(), Option::<&str>::None], ItemRow::from_row)
            .optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (item, body) = row_to_view(row)?;
        let receipts = receipts_for(&conn, id.as_str(), item.revision)?;
        Ok(Some(MemoryItemDetail { item, body, receipts }))
    }

    /// Artan cursor sırasıyla değişimler. `cursor` son görülen değişimin kimliğidir (0 = baştan).
    ///
    /// Yalnız masaüstü: değişim günlüğü gizli/çöpteki öğelerin kimliklerini ve olaylarını içerir.
    /// Engine/Telegram için hata döner (varlık sızmaz).
    pub fn memory_changes_since(
        &self,
        audience: Audience,
        cursor: i64,
        limit: Option<u32>,
    ) -> Result<Vec<MemoryChange>> {
        if audience != Audience::Desktop {
            return Err(anyhow::Error::from(ContractError::InvalidField));
        }
        let limit = clamp_limit(limit) as i64;
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT cursor, item_id, suggestion_id, revision, kind FROM memory_changes
             WHERE cursor > ?1 ORDER BY cursor LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![cursor, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(cursor, item, suggestion, rev, kind)| {
                Ok(MemoryChange {
                    cursor,
                    item_id: item.as_deref().map(MemoryId::parse).transpose()?,
                    suggestion_id: suggestion.as_deref().map(MemoryId::parse).transpose()?,
                    revision: rev.map(revision_of).transpose()?,
                    kind: from_wire(&kind)?,
                })
            })
            .collect()
    }

    /// Öneriyi `pending` olarak ekler. Aynı `(chat, tur, aday hash)` varsa yeni satır açmaz.
    pub fn memory_insert_suggestion(&self, s: &MemorySuggestion) -> Result<SuggestionInsert> {
        s.validate()?;
        if s.status != SuggestionStatus::Pending || s.decided_at.is_some() || s.decided_by.is_some() {
            return Err(anyhow::Error::from(ContractError::InvalidField));
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM memory_suggestions
                 WHERE chat_id = ?1 AND logical_turn_id = ?2 AND candidate_hash = ?3",
                params![s.chat_id, s.logical_turn_id, s.candidate_hash.prefixed()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(SuggestionInsert::Existing(MemoryId::parse(&id)?));
        }
        let (target_id, target_rev) = match &s.candidate.target {
            SuggestionTarget::New => (None, None),
            SuggestionTarget::Update { id, expected_revision } => {
                (Some(id.as_str().to_string()), Some(expected_revision.get() as i64))
            }
        };
        let receipts: Vec<String> = s.receipts.iter().map(Sha256Hash::prefixed).collect();
        let dedupe_key = format!("{}:{}:{}", s.chat_id, s.logical_turn_id, s.candidate_hash.prefixed());
        tx.execute(
            "INSERT INTO memory_suggestions (id, chat_id, logical_turn_id, end_seq, candidate_json,
             candidate_hash, target_id, target_revision, receipts_json, dedupe_key, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)",
            params![
                s.id.as_str(),
                s.chat_id,
                s.logical_turn_id,
                s.end_seq,
                serde_json::to_string(&s.candidate)?,
                s.candidate_hash.prefixed(),
                target_id,
                target_rev,
                serde_json::to_string(&receipts)?,
                dedupe_key,
                ts_string(&s.created_at),
            ],
        )?;
        append_change_tx(&tx, None, Some(s.id.as_str()), None, MemoryChangeKind::SuggestionCreated)?;
        tx.commit()?;
        Ok(SuggestionInsert::Inserted(s.id.clone()))
    }

    /// Öneriyi okur ve kanıt bütünlüğünü (`validate`) yeniden doğrular.
    ///
    /// Yalnız masaüstü ve Telegram; Engine reddedilir (aday metni gizli olabilir).
    pub fn memory_get_suggestion(
        &self,
        id: &MemoryId,
        audience: Audience,
    ) -> Result<Option<MemorySuggestion>> {
        suggestion_audience_check(audience)?;
        let conn = self.conn()?;
        let row: Option<SuggestionRow> = conn
            .query_row(
                "SELECT chat_id, logical_turn_id, end_seq, candidate_json, candidate_hash,
                 receipts_json, status, created_at, decided_at, decided_by
                 FROM memory_suggestions WHERE id = ?1",
                [id.as_str()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((chat_id, turn, end_seq, cand, hash, receipts, status, created, decided_at, decided_by)) = row
        else {
            return Ok(None);
        };
        let receipt_strings: Vec<String> = serde_json::from_str(&receipts)?;
        let s = MemorySuggestion {
            id: id.clone(),
            chat_id,
            logical_turn_id: turn,
            end_seq,
            candidate: serde_json::from_str::<SuggestionCandidate>(&cand)?,
            candidate_hash: Sha256Hash::parse(&hash)?,
            receipts: receipt_strings
                .iter()
                .map(|h| Sha256Hash::parse(h))
                .collect::<ContractResult<Vec<_>>>()?,
            status: from_wire(&status)?,
            created_at: parse_ts(&created)?,
            decided_at: decided_at.as_deref().map(parse_ts).transpose()?,
            decided_by: decided_by.as_deref().map(from_wire::<DecisionSource>).transpose()?,
        };
        s.validate()?;
        Ok(Some(s))
    }

    /// Öneri kararı. İlk karar kazanır; aynı `request_id` + aynı parmak izi ilk sonucu döndürür;
    /// aynı `request_id` farklı parmak iziyle gelirse `RequestConflict` hatası verir.
    ///
    /// Yalnız masaüstü ve Telegram; Engine reddedilir. (Telegram'ın yalnız yeni-aday kararı
    /// verebilmesi kuralı Task 10'a aittir.) `Accepted` için `accepted_item_id` zorunlu,
    /// diğer kararlar için yasaktır.
    pub fn memory_resolve_suggestion(
        &self,
        req: &SuggestionDecisionRequest<'_>,
        audience: Audience,
    ) -> Result<SuggestionDecision> {
        suggestion_audience_check(audience)?;
        validate_request_id(req.request_id)?;
        if req.accepted_item_id.is_some() != (req.to == SuggestionStatus::Accepted) {
            return Err(anyhow::Error::from(ContractError::InvalidField));
        }
        if req.to == SuggestionStatus::Pending {
            return Err(anyhow::Error::from(ContractError::InvalidField));
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT status, candidate_hash, decision_request_id, decision_request_hash
                 FROM memory_suggestions WHERE id = ?1",
                [req.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((status, cand_hash, dec_req, dec_hash)) = row else {
            return Ok(SuggestionDecision::NotFound);
        };
        if cand_hash != req.candidate_hash.prefixed() {
            return Err(anyhow::Error::from(ContractError::InvalidHash));
        }
        let status: SuggestionStatus = from_wire(&status)?;
        if status == SuggestionStatus::Pending {
            tx.execute(
                "UPDATE memory_suggestions SET status = ?2, decided_at = ?3, decided_by = ?4,
                 decision_request_id = ?5, decision_request_hash = ?6, accepted_item_id = ?7
                 WHERE id = ?1 AND status = 'pending'",
                params![
                    req.id.as_str(),
                    to_wire(&req.to)?,
                    ts_string(&Utc::now()),
                    to_wire(&req.by)?,
                    req.request_id,
                    req.request_hash.prefixed(),
                    req.accepted_item_id.map(|i| i.as_str().to_string()),
                ],
            )?;
            append_change_tx(&tx, None, Some(req.id.as_str()), None, MemoryChangeKind::SuggestionResolved)?;
            tx.commit()?;
            return Ok(SuggestionDecision::Decided(req.to));
        }
        if dec_req.as_deref() == Some(req.request_id) {
            let hash = req.request_hash.prefixed();
            if dec_hash.as_deref() == Some(hash.as_str()) {
                return Ok(SuggestionDecision::Replayed(status));
            }
            return Err(anyhow::Error::from(ContractError::RequestConflict));
        }
        Ok(SuggestionDecision::AlreadyDecided(status))
    }

    /// Sınırlı kuyruğa alma: tek IMMEDIATE işlemde yineleme kontrolü, açık (`queued`+`running`)
    /// iş sayımı ve ekleme. `max_open` dolduysa `Full` döner ve satır eklenmez.
    pub fn memory_enqueue_job_bounded(
        &self,
        chat_id: &str,
        logical_turn_id: &str,
        end_seq: i64,
        max_open: usize,
    ) -> Result<JobEnqueue> {
        validate_chat_id(chat_id)?;
        check_line(logical_turn_id, MAX_ID_TOKEN_CHARS, false)?;
        if end_seq < 1 {
            return Err(anyhow::Error::from(ContractError::InvalidSeq));
        }
        let now = ts_string(&Utc::now());
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_jobs WHERE chat_id = ?1 AND logical_turn_id = ?2 AND end_seq = ?3)",
            params![chat_id, logical_turn_id, end_seq],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(JobEnqueue::Duplicate);
        }
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memory_jobs WHERE status IN ('queued', 'running')",
            [],
            |r| r.get(0),
        )?;
        if usize::try_from(open).unwrap_or(usize::MAX) >= max_open {
            return Ok(JobEnqueue::Full);
        }
        tx.execute(
            "INSERT INTO memory_jobs (chat_id, logical_turn_id, end_seq, status, attempt,
             generation, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', 0, 0, ?4, ?4)",
            params![chat_id, logical_turn_id, end_seq, now],
        )?;
        tx.commit()?;
        Ok(JobEnqueue::Queued)
    }

    /// Açık (`queued` + `running`) iş sayısı.
    pub fn memory_count_open_jobs(&self) -> Result<usize> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_jobs WHERE status IN ('queued', 'running')",
            [],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Açılışta bir kez: bu sürecin başlangıcından (`boot_started_at`) önce kiralanmış `running`
    /// işler yetimdir. Deneme hakkı varsa `queued` (kiralama temiz, `generation` artar, `attempt`
    /// korunur), yoksa `failed('timeout')` olur. Sonradan kiralanan canlı işlere dokunmaz.
    pub fn memory_requeue_orphaned(&self, boot_started_at: DateTime<Utc>) -> Result<usize> {
        let boot = ts_string(&boot_started_at);
        let now = ts_string(&Utc::now());
        let max = i64::from(MEMORY_JOB_MAX_ATTEMPTS);
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let failed = tx.execute(
            "UPDATE memory_jobs SET status = 'failed', error_code = 'timeout', lease_until = NULL,
             generation = generation + 1, updated_at = ?2
             WHERE status = 'running' AND updated_at < ?1 AND attempt >= ?3",
            params![boot, now, max],
        )?;
        let requeued = tx.execute(
            "UPDATE memory_jobs SET status = 'queued', lease_until = NULL,
             generation = generation + 1, updated_at = ?2
             WHERE status = 'running' AND updated_at < ?1 AND attempt < ?3",
            params![boot, now, max],
        )?;
        tx.commit()?;
        Ok(failed + requeued)
    }

    /// Geçerli kiralamayla başarısızlık bildirir ve işi `retry_at`'e kadar kiralanamaz tutar
    /// (backoff). Deneme hakkı yoksa `failed`. Eski jeton için `None`.
    pub fn memory_retry_job(
        &self,
        id: i64,
        generation: i64,
        error: MemoryJobError,
        retry_at: DateTime<Utc>,
    ) -> Result<Option<MemoryJobStatus>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT status, generation, attempt FROM memory_jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((status, row_generation, attempt)) = row else {
            return Ok(None);
        };
        if status != "running" || row_generation != generation {
            return Ok(None);
        }
        let exhausted = attempt >= i64::from(MEMORY_JOB_MAX_ATTEMPTS);
        let next = if exhausted { MemoryJobStatus::Failed } else { MemoryJobStatus::Queued };
        let lease = (!exhausted).then(|| ts_string(&retry_at));
        tx.execute(
            "UPDATE memory_jobs SET status = ?2, lease_until = ?3, error_code = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, to_wire(&next)?, lease, to_wire(&error)?, ts_string(&Utc::now())],
        )?;
        tx.commit()?;
        Ok(Some(next))
    }

    /// İptal/kapanışta işi deneme hakkı harcamadan geri verir (`queued`, `attempt - 1`).
    pub fn memory_release_job(&self, id: i64, generation: i64) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE memory_jobs SET status = 'queued', lease_until = NULL, error_code = 'cancelled',
             attempt = MAX(attempt - 1, 0), updated_at = ?3
             WHERE id = ?1 AND status = 'running' AND generation = ?2",
            params![id, generation, ts_string(&Utc::now())],
        )?;
        Ok(n == 1)
    }

    /// Özet+aday işini kuyruğa alır. `(chat, tur, end_seq)` zaten varsa `false` döner.
    pub fn memory_enqueue_job(&self, chat_id: &str, logical_turn_id: &str, end_seq: i64) -> Result<bool> {
        validate_chat_id(chat_id)?;
        check_line(logical_turn_id, MAX_ID_TOKEN_CHARS, false)?;
        if end_seq < 1 {
            return Err(anyhow::Error::from(ContractError::InvalidSeq));
        }
        let now = ts_string(&Utc::now());
        let conn = self.conn()?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO memory_jobs (chat_id, logical_turn_id, end_seq, status, attempt,
             generation, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', 0, 0, ?4, ?4)",
            params![chat_id, logical_turn_id, end_seq, now],
        )?;
        Ok(n == 1)
    }

    /// Bir işi kiralar: süresi dolmuş kiralamaları devralır, en fazla
    /// [`MEMORY_JOB_MAX_ATTEMPTS`] deneme yapar. Her kiralama `generation` değerini artırır.
    pub fn memory_claim_job(&self, now: DateTime<Utc>, lease: Duration) -> Result<Option<MemoryJobClaim>> {
        let now_s = ts_string(&now);
        let lease_s = ts_string(&(now + lease));
        let max = i64::from(MEMORY_JOB_MAX_ATTEMPTS);
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memory_jobs SET status = 'failed', error_code = 'timeout', lease_until = NULL,
             updated_at = ?1 WHERE status = 'running' AND lease_until < ?1 AND attempt >= ?2",
            params![now_s, max],
        )?;
        let cand: Option<(i64, String, String, i64, i64, i64)> = tx
            .query_row(
                "SELECT id, chat_id, logical_turn_id, end_seq, generation, attempt FROM memory_jobs
                 WHERE attempt < ?2 AND ((status = 'queued' AND (lease_until IS NULL OR lease_until <= ?1))
                       OR (status = 'running' AND lease_until < ?1))
                 ORDER BY id LIMIT 1",
                params![now_s, max],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let Some((id, chat_id, turn, end_seq, generation, attempt)) = cand else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE memory_jobs SET status = 'running', generation = generation + 1,
             attempt = attempt + 1, lease_until = ?2, updated_at = ?3
             WHERE id = ?1 AND generation = ?4",
            params![id, lease_s, now_s, generation],
        )?;
        if changed != 1 {
            return Err(anyhow!("iş kiralaması yarıştı"));
        }
        tx.commit()?;
        Ok(Some(MemoryJobClaim {
            id,
            chat_id,
            logical_turn_id: turn,
            end_seq,
            generation: generation + 1,
            attempt: u32::try_from(attempt + 1)?,
        }))
    }

    /// Yalnız geçerli kiralama jetonuyla (`generation`) tamamlar; eski sahip `false` alır.
    pub fn memory_complete_job(&self, id: i64, generation: i64) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE memory_jobs SET status = 'done', lease_until = NULL, updated_at = ?3
             WHERE id = ?1 AND status = 'running' AND generation = ?2",
            params![id, generation, ts_string(&Utc::now())],
        )?;
        Ok(n == 1)
    }

    /// Geçerli kiralamayla başarısızlık bildirir. Deneme hakkı varsa `queued`, yoksa `failed`.
    /// Eski jeton için `None` döner.
    pub fn memory_fail_job(
        &self,
        id: i64,
        generation: i64,
        error: MemoryJobError,
    ) -> Result<Option<MemoryJobStatus>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT status, generation, attempt FROM memory_jobs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((status, row_generation, attempt)) = row else {
            return Ok(None);
        };
        if status != "running" || row_generation != generation {
            return Ok(None);
        }
        let next = if attempt >= i64::from(MEMORY_JOB_MAX_ATTEMPTS) {
            MemoryJobStatus::Failed
        } else {
            MemoryJobStatus::Queued
        };
        tx.execute(
            "UPDATE memory_jobs SET status = ?2, lease_until = NULL, error_code = ?3, updated_at = ?4
             WHERE id = ?1",
            params![id, to_wire(&next)?, to_wire(&error)?, ts_string(&Utc::now())],
        )?;
        tx.commit()?;
        Ok(Some(next))
    }

    /// İş durumunu okur. `lease_until` çift anlamlıdır: `running` işte kiralama bitişi, `queued`
    /// işte backoff bitişi (`retry_at`; bu zamana dek claim edilemez). Diğer durumlarda `None`.
    pub fn memory_get_job(&self, chat_id: &str, logical_turn_id: &str, end_seq: i64) -> Result<Option<MemoryJob>> {
        let conn = self.conn()?;
        let row: Option<JobRow> = conn
            .query_row(
                "SELECT status, attempt, lease_until, generation, error_code FROM memory_jobs
                 WHERE chat_id = ?1 AND logical_turn_id = ?2 AND end_seq = ?3",
                params![chat_id, logical_turn_id, end_seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((status, attempt, lease, generation, error)) = row else {
            return Ok(None);
        };
        Ok(Some(MemoryJob {
            chat_id: chat_id.to_string(),
            logical_turn_id: logical_turn_id.to_string(),
            end_seq,
            status: from_wire(&status)?,
            attempt: u32::try_from(attempt)?,
            lease_until: lease.as_deref().map(parse_ts).transpose()?,
            generation,
            error: error.as_deref().map(from_wire::<MemoryJobError>).transpose()?,
        }))
    }

    /// `older_than`'dan önce güncellenmiş `done`/`failed` iş satırlarını siler (bütün tur başına
    /// birikimi sınırlamak için). Açık işlere dokunmaz. Henüz bağlı değildir (Task 8).
    pub fn memory_prune_jobs(&self, older_than: DateTime<Utc>) -> Result<usize> {
        let conn = self.conn()?;
        Ok(conn.execute(
            "DELETE FROM memory_jobs WHERE status IN ('done', 'failed') AND updated_at < ?1",
            params![ts_string(&older_than)],
        )?)
    }

    /// `since` veya sonrasında `failed` olan işler (en çok 100): `(chat_id, end_seq, hata)`.
    /// Nihai başarısızlığın çalışma durumuna güvenli kodla yansıtılması için kullanılır.
    pub fn memory_failed_jobs_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<(String, i64, Option<MemoryJobError>)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT chat_id, end_seq, error_code FROM memory_jobs
             WHERE status = 'failed' AND updated_at >= ?1 ORDER BY id LIMIT 100",
        )?;
        let rows = stmt.query_map(params![ts_string(&since)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (chat, end_seq, error) = row?;
            out.push((chat, end_seq, error.as_deref().map(from_wire::<MemoryJobError>).transpose()?));
        }
        Ok(out)
    }

    /// İşlemi başlatır. Aynı `request_id` varsa parmak izi kontrol edilir: aynıysa kayıtlı işlem
    /// döner, farklıysa `RequestConflict` hatası verilir.
    pub fn memory_begin_operation(&self, op: &MemoryOperation) -> Result<OperationBegin> {
        validate_request_id(&op.request_id)?;
        if op.state != MemoryOperationState::Pending {
            return Err(anyhow::Error::from(ContractError::InvalidField));
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_operation(&tx, &op.request_id)? {
            existing.check_replay(&op.request_hash)?;
            return Ok(OperationBegin::Replay(existing));
        }
        tx.execute(
            "INSERT INTO memory_operations (request_id, request_hash, source, kind, target_id,
             expected_revision, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7)",
            params![
                op.request_id,
                op.request_hash.prefixed(),
                to_wire(&op.source)?,
                to_wire(&op.kind)?,
                op.target_id.as_ref().map(|t| t.as_str().to_string()),
                op.expected_revision.map(|r| r.get() as i64),
                ts_string(&Utc::now()),
            ],
        )?;
        tx.commit()?;
        Ok(OperationBegin::Started)
    }

    /// İşlem durumunu ilerletir. Geçersiz geçişte veya bilinmeyen işlemde `false` döner;
    /// ayrıntı için [`Db::memory_advance_operation_outcome`].
    pub fn memory_advance_operation(&self, adv: &OperationAdvance<'_>) -> Result<bool> {
        Ok(self.memory_advance_operation_outcome(adv)? == AdvanceOutcome::Advanced)
    }

    /// Ayırt edilebilir sonuçla ilerletme: `Completed -> Completed` tekrarı `AlreadyAtState`.
    pub fn memory_advance_operation_outcome(&self, adv: &OperationAdvance<'_>) -> Result<AdvanceOutcome> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(current) = load_operation(&tx, adv.request_id)? else {
            return Ok(AdvanceOutcome::Rejected);
        };
        if current.state == MemoryOperationState::Completed && adv.to == MemoryOperationState::Completed {
            return Ok(AdvanceOutcome::AlreadyAtState);
        }
        if !transition_allowed(current.state, adv.to) {
            return Ok(AdvanceOutcome::Rejected);
        }
        tx.execute(
            "UPDATE memory_operations SET state = ?2,
             result_revision = COALESCE(?3, result_revision),
             content_hash = COALESCE(?4, content_hash),
             receipt_hash = COALESCE(?5, receipt_hash),
             error_code = COALESCE(?6, error_code),
             updated_at = ?7
             WHERE request_id = ?1",
            params![
                adv.request_id,
                to_wire(&adv.to)?,
                adv.result_revision.map(|r| r.get() as i64),
                adv.content_hash.map(Sha256Hash::prefixed),
                adv.receipt_hash.map(Sha256Hash::prefixed),
                adv.error_code,
                ts_string(&Utc::now()),
            ],
        )?;
        tx.commit()?;
        Ok(AdvanceOutcome::Advanced)
    }

    /// Uzlaştırma bekleyen (tamamlanmamış) işlemler.
    pub fn memory_pending_operations(&self) -> Result<Vec<MemoryOperation>> {
        let conn = self.conn()?;
        let ids = conn
            .prepare(
                "SELECT request_id FROM memory_operations
                 WHERE state IN ('pending', 'file_committed') ORDER BY id",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .filter_map(|id| load_operation(&conn, id).transpose())
            .collect()
    }
}

/// UYARI: FTS rowid'si `memory_items` örtük rowid'sine bağlıdır; `VACUUM` rowid'leri yeniden
/// numaralayabilir. İndeks yeniden kurulumu (`memory_rebuild_index`) yapılmadan `VACUUM` ÇALIŞTIRMAYIN.
/// `rebuild`: tablolar boşaltılmış yeniden kurulum yolu; FTS silmesi atlanır ve değişim kaydı
/// yazılmaz. FTS satırının rowid'si her zaman `memory_items.rowid` ile aynıdır, böylece normal
/// yolda silme/ekleme tam FTS taraması yerine rowid ile yapılır.
type StoredItem = (i64, String, String, String, bool, Option<String>, String, String, Option<String>, Option<String>);

fn upsert_item_tx(tx: &Transaction<'_>, item: &MemoryItemRecord, rebuild: bool) -> Result<UpsertOutcome> {
    let id = item.id.as_str();
    let existing: Option<StoredItem> = tx
        .query_row(
            "SELECT revision, content_hash, relative_path, visibility, remote_allowed, project,
             sensitivity, index_state, trashed_at, review_at FROM memory_items WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?;
    let is_new = existing.is_none();
    let mut kind = if is_new { MemoryChangeKind::Created } else { MemoryChangeKind::Updated };
    if let Some((rev, hash, path, vis, remote, project, sens, state, trashed, _review)) = existing {
        let current = revision_of(rev)?;
        if item.revision < current {
            return Ok(UpsertOutcome::StaleRevision { current });
        }
        if item.revision == current {
            if hash != item.content_hash.prefixed() {
                return Ok(UpsertOutcome::HashMismatch);
            }
            let new_state = to_wire(&item.index_state)?;
            let new_trashed = item.trashed_at.as_ref().map(ts_string);
            let same = path == item.relative_path
                && vis == to_wire(&item.visibility)?
                && remote == item.remote_allowed
                && project == item.project
                && sens == to_wire(&item.sensitivity)?
                && state == new_state
                && trashed == new_trashed;
            if same {
                // Aynı içerik: yalnız makbuz kanıt durumu yükseltilebilir.
                for receipt in &item.receipts {
                    insert_receipt_tx(tx, receipt)?;
                }
                return Ok(UpsertOutcome::Unchanged);
            }
            kind = if item.index_state == IndexState::Indexed
                && item.trashed_at.is_none()
                && (state != "indexed" || trashed.is_some())
            {
                MemoryChangeKind::Restored
            } else if item.trashed_at.is_some() && trashed.is_none() {
                MemoryChangeKind::Trashed
            } else {
                MemoryChangeKind::Updated
            };
        }
    }
    let path_owner: Option<String> = tx
        .query_row(
            "SELECT id FROM memory_items WHERE relative_path = ?1 COLLATE NOCASE AND id <> ?2",
            params![item.relative_path, id],
            |r| r.get(0),
        )
        .optional()?;
    if path_owner.is_some() {
        return Ok(UpsertOutcome::PathConflict);
    }
    let tags_json = serde_json::to_string(&item.tags)?;
    tx.execute(
        UPSERT_ITEM_SQL,
        params![
            id,
            item.relative_path,
            to_wire(&item.memory_type)?,
            item.project,
            to_wire(&item.visibility)?,
            item.remote_allowed,
            to_wire(&item.sensitivity)?,
            item.title,
            item.summary,
            item.body,
            tags_json,
            item.revision.get() as i64,
            item.content_hash.prefixed(),
            ts_string(&item.created_at),
            ts_string(&item.updated_at),
            item.review_at.as_ref().map(ts_string),
            to_wire(&item.index_state)?,
            item.trashed_at.as_ref().map(ts_string),
        ],
    )?;
    for receipt in &item.receipts {
        insert_receipt_tx(tx, receipt)?;
        tx.execute(
            "INSERT OR IGNORE INTO memory_item_receipts (item_id, revision, receipt_hash)
             VALUES (?1, ?2, ?3)",
            params![id, item.revision.get() as i64, receipt.hash.prefixed()],
        )?;
    }
    let rowid: i64 = tx.query_row("SELECT rowid FROM memory_items WHERE id = ?1", [id], |r| r.get(0))?;
    if !rebuild {
        tx.execute("DELETE FROM memory_fts WHERE rowid = ?1", [rowid])?;
    }
    tx.execute(
        "INSERT INTO memory_fts (rowid, item_id, title, summary, body, tags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            rowid,
            id,
            fold_text(&item.title),
            fold_text(&item.summary),
            fold_text(&item.body),
            fold_text(&item.tags.join(" ")),
        ],
    )?;
    if !rebuild {
        append_change_tx(tx, Some(id), None, Some(item.revision), kind)?;
    }
    Ok(if is_new { UpsertOutcome::Inserted } else { UpsertOutcome::Updated })
}

fn insert_receipt_tx(tx: &Transaction<'_>, r: &ReceiptRecord) -> Result<()> {
    let kind = match &r.source {
        SourceRef::Event { .. } => "event",
        SourceRef::File { .. } => "file",
        SourceRef::Import { .. } => "import",
    };
    // Olay makbuzlarında sohbet/aralık sütunları; diğerlerinde kaynağın tam JSON'u (kayıpsız).
    let (locator, chat, seq_from, seq_to) = match &r.source {
        SourceRef::Event { chat_id, seq_from, seq_to } => {
            (None, Some(chat_id.clone()), Some(*seq_from), Some(*seq_to))
        }
        other => (Some(serde_json::to_string(other)?), None, None, None),
    };
    tx.execute(
        "INSERT INTO memory_receipts (hash, relative_path, source_kind, source_locator,
         chat_id, seq_from, seq_to, claim_status, excerpt, evidence_state, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(hash) DO UPDATE SET evidence_state = 'present'
         WHERE memory_receipts.evidence_state = 'missing' AND excluded.evidence_state = 'present'",
        params![
            r.hash.prefixed(),
            r.relative_path,
            kind,
            locator,
            chat,
            seq_from,
            seq_to,
            to_wire(&r.claim_status)?,
            r.excerpt,
            if r.evidence_available { "present" } else { "missing" },
            ts_string(&r.captured_at),
        ],
    )?;
    Ok(())
}

fn append_change_tx(
    tx: &Transaction<'_>,
    item_id: Option<&str>,
    suggestion_id: Option<&str>,
    revision: Option<Revision>,
    kind: MemoryChangeKind,
) -> Result<()> {
    tx.execute(
        "INSERT INTO memory_changes (item_id, suggestion_id, revision, kind, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            item_id,
            suggestion_id,
            revision.map(|r| r.get() as i64),
            to_wire(&kind)?,
            ts_string(&Utc::now()),
        ],
    )?;
    Ok(())
}

fn receipts_for(conn: &Connection, item_id: &str, revision: Revision) -> Result<Vec<ReceiptView>> {
    let mut stmt = conn.prepare(
        "SELECT m.hash, m.source_kind, m.source_locator, m.chat_id, m.seq_from, m.seq_to,
         m.claim_status, m.excerpt, m.evidence_state, m.captured_at
         FROM memory_item_receipts r JOIN memory_receipts m ON m.hash = r.receipt_hash
         WHERE r.item_id = ?1 AND r.revision = ?2
         ORDER BY m.captured_at, m.hash LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![item_id, revision.get() as i64, MAX_DETAIL_RECEIPTS as i64],
            |r| {
                Ok::<ReceiptRow, rusqlite::Error>((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(hash, kind, locator, chat, from, to, claim, excerpt, evidence, captured)| {
            Ok(ReceiptView {
                hash: Sha256Hash::parse(&hash)?,
                source: source_from_row(&kind, locator, chat, from, to)?,
                claim_status: from_wire(&claim)?,
                captured_at: parse_ts(&captured)?,
                excerpt,
                evidence_available: evidence == "present",
            })
        })
        .collect()
}

fn source_from_row(
    kind: &str,
    locator: Option<String>,
    chat: Option<String>,
    seq_from: Option<i64>,
    seq_to: Option<i64>,
) -> Result<SourceRef> {
    if kind == "event" {
        return Ok(SourceRef::Event {
            chat_id: chat.ok_or_else(|| anyhow!("olay makbuzunda sohbet yok"))?,
            seq_from: seq_from.ok_or_else(|| anyhow!("olay makbuzunda seq yok"))?,
            seq_to: seq_to.ok_or_else(|| anyhow!("olay makbuzunda seq yok"))?,
        });
    }
    let json = locator.ok_or_else(|| anyhow!("makbuz kaynağı eksik"))?;
    Ok(serde_json::from_str(&json)?)
}

fn load_operation(conn: &Connection, request_id: &str) -> Result<Option<MemoryOperation>> {
    type OpRow = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let row: Option<OpRow> = conn
        .query_row(
            "SELECT request_id, request_hash, source, kind, target_id, expected_revision, state,
             result_revision, content_hash, receipt_hash FROM memory_operations WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?;
    let Some((req, hash, source, kind, target, expected, state, result_rev, content, receipt)) = row else {
        return Ok(None);
    };
    Ok(Some(MemoryOperation {
        request_id: req,
        request_hash: Sha256Hash::parse(&hash)?,
        source: from_wire(&source)?,
        kind: from_wire(&kind)?,
        target_id: target.as_deref().map(MemoryId::parse).transpose()?,
        expected_revision: expected.map(revision_of).transpose()?,
        state: from_wire(&state)?,
        result_revision: result_rev.map(revision_of).transpose()?,
        content_hash: content.as_deref().map(Sha256Hash::parse).transpose()?,
        receipt_hash: receipt.as_deref().map(Sha256Hash::parse).transpose()?,
    }))
}

fn transition_allowed(from: MemoryOperationState, to: MemoryOperationState) -> bool {
    use MemoryOperationState::{Completed, FileCommitted, Failed, Pending};
    matches!(
        (from, to),
        (Pending, FileCommitted | Completed | Failed) | (FileCommitted, Completed | Failed)
    )
}

fn row_to_view(r: ItemRow) -> Result<(MemoryItemView, String)> {
    let view = MemoryItemView {
        id: MemoryId::parse(&r.id)?,
        memory_type: from_wire(&r.memory_type)?,
        title: r.title,
        summary: r.summary,
        project: r.project,
        visibility: from_wire(&r.visibility)?,
        remote_allowed: r.remote_allowed,
        sensitivity: from_wire(&r.sensitivity)?,
        revision: revision_of(r.revision)?,
        content_hash: Sha256Hash::parse(&r.content_hash)?,
        created_at: parse_ts(&r.created_at)?,
        updated_at: parse_ts(&r.updated_at)?,
        review_at: r.review_at.as_deref().map(parse_ts).transpose()?,
        tags: serde_json::from_str(&r.tags_json)?,
        index_state: from_wire(&r.index_state)?,
        receipt_count: u32::try_from(r.receipt_count)?,
    };
    Ok((view, r.body))
}

fn snippet_of(summary: &str, body: &str) -> String {
    let base = if summary.trim().is_empty() { body } else { summary };
    base.chars().take(SNIPPET_CHARS).collect()
}

/// Sorgu (katlanmış), kitle ve proje için 16 hex karakterlik bağlayıcı.
fn cursor_binding(query: &str, audience: Audience, project: Option<&str>) -> String {
    let material = format!(
        "{}|{}|{}",
        fold_text(query).split_whitespace().collect::<Vec<_>>().join(" "),
        to_wire(&audience).unwrap_or_default(),
        project.unwrap_or("")
    );
    let prefixed = Sha256Hash::of(material.as_bytes()).prefixed();
    prefixed.trim_start_matches("sha256:").chars().take(16).collect()
}

/// `o{ofset}.{bağlayıcı}` biçimini doğrular; bozuk veya başka sorguya ait imleç aynı genel hatayı verir.
fn parse_search_cursor(cursor: &str, binding: &str) -> Result<usize> {
    let bad = || anyhow::Error::from(ContractError::InvalidField);
    let rest = cursor.strip_prefix('o').ok_or_else(bad)?;
    let (offset, bound) = rest.split_once('.').ok_or_else(bad)?;
    if bound != binding || offset.is_empty() || offset.len() > 7 || !offset.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    offset.parse::<usize>().map_err(|_| bad())
}

fn suggestion_audience_check(audience: Audience) -> Result<()> {
    match audience {
        Audience::Desktop | Audience::Telegram => Ok(()),
        Audience::Engine => Err(anyhow::Error::from(ContractError::InvalidField)),
    }
}

fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn revision_of(v: i64) -> Result<Revision> {
    Ok(Revision::new(u64::try_from(v)?)?)
}

fn ts_string(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn to_wire<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        Value::String(s) => Ok(s),
        _ => Err(anyhow!("sözleşme enum'u metin değil")),
    }
}

fn from_wire<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(Value::String(s.to_string()))?)
}
