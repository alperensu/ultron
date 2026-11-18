//! Model çıktısından hafıza önerisi üretimi (Aşama 5a Task 4).
//!
//! Model JSON'u güvenilmezdir: aday sayısı/boyutu sınırlanır, kaynak `seq` değeri tur aralığında
//! ve gerçek bir kullanıcı/asistan olayına ait olmalıdır, sırlar saklanmadan önce maskelenir,
//! modelin `verified`/`claim_status` benzeri alanları yok sayılır. Sonuç yalnız `pending` öneridir;
//! hiçbir not yazılmaz.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use chrono::Utc;

use crate::db::memory::SuggestionInsert;
use crate::db::{Db, RECENT_EVENTS_WINDOW, StoredEvent};
use crate::import::mask_secrets;
use crate::memory::{
    ClaimStatus, MAX_CANDIDATE_BODY_BYTES, MAX_RECEIPT_EXCERPT_CHARS, MAX_SUMMARY_CHARS,
    MAX_TAG_CHARS, MAX_TAGS, MAX_TITLE_CHARS, MemoryId, MemorySuggestion, MemoryType, Receipt,
    Sha256Hash, SourceRef, SuggestionCandidate, SuggestionStatus,
    SuggestionTarget,
};
use crate::summarizer::{RawCandidate, digest_messages};

/// Bir iş için saklanacak en fazla aday.
pub const MAX_CANDIDATES_PER_JOB: usize = 5;

/// Aday doğrulama ve saklama sayaçları (içerik taşımaz).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidateReport {
    pub stored: usize,
    /// Aynı turda zaten kayıtlı önerilerle eşleşti (yeni satır açılmadı).
    pub existing: usize,
    pub rejected_unsourced: usize,
    pub rejected_out_of_range: usize,
    pub rejected_duplicate: usize,
    pub rejected_conflict: usize,
    pub rejected_invalid: usize,
    pub rejected_over_limit: usize,
}

/// Turun olay aralığı: son `user_text` olayından `end_seq`'e kadar.
pub fn turn_range(events: &[StoredEvent], end_seq: i64) -> Option<(i64, i64)> {
    let start = events
        .iter()
        .filter(|e| e.seq <= end_seq && e.kind == "user_text")
        .map(|e| e.seq)
        .max()?;
    Some((start, end_seq))
}

/// Tur makbuzu: aralıktaki kullanıcı/asistan metninden belirlenimci, maskeli alıntı. `captured_at`
/// aralığın son olayının zamanıdır; böylece aynı olaylardan aynı hash yeniden üretilir ve kabul
/// anında (Task 6) olaylardan yeniden kurulabilir.
pub fn turn_receipt(chat_id: &str, events: &[StoredEvent], end_seq: i64) -> Result<Receipt> {
    let (start, end) = turn_range(events, end_seq)
        .ok_or_else(|| anyhow::anyhow!("tur aralığı bulunamadı"))?;
    let in_range: Vec<&StoredEvent> = events.iter().filter(|e| e.seq >= start && e.seq <= end).collect();
    let owned: Vec<StoredEvent> = in_range.iter().map(|e| (*e).clone()).collect();
    let mut excerpt = String::new();
    for (_, role, text) in digest_messages(&owned) {
        if !excerpt.is_empty() {
            excerpt.push('\n');
        }
        excerpt.push_str(role);
        excerpt.push_str(": ");
        excerpt.push_str(&text);
        if excerpt.chars().count() > MAX_RECEIPT_EXCERPT_CHARS {
            break;
        }
    }
    let captured_at = in_range.last().map_or_else(Utc::now, |e| e.ts);
    let source = SourceRef::Event {
        chat_id: chat_id.to_string(),
        seq_from: start,
        seq_to: end,
    };
    Ok(Receipt::new(source, &excerpt, captured_at, ClaimStatus::Claimed)?)
}

/// Veritabanından olayları okuyup [`turn_receipt`] üretir (kabul anı yeniden kurulumu için).
pub fn turn_receipt_from_db(db: &Db, chat_id: &str, end_seq: i64) -> Result<Receipt> {
    let events = db.events_tail(chat_id, RECENT_EVENTS_WINDOW)?;
    let events: Vec<StoredEvent> = events.into_iter().filter(|e| e.seq <= end_seq).collect();
    turn_receipt(chat_id, &events, end_seq)
}

fn clip_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn clip_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Maskelemeden önce makul üst sınıra kırpar (regex maliyeti), Cc+Cf karakterlerini atar.
fn prep(text: &str, scan_chars: usize, keep_newlines: bool) -> String {
    let cropped: String = text.chars().take(scan_chars).collect();
    crate::workstate::strip_unsafe_chars(&cropped, keep_newlines)
}

fn norm(text: &str) -> String {
    one_line(text).to_lowercase()
}

enum Verdict {
    Keep(Box<SuggestionCandidate>),
    Unsourced,
    OutOfRange,
    Invalid,
}

fn build_candidate(raw: &RawCandidate, citable: &BTreeSet<i64>, range: (i64, i64)) -> Verdict {
    let seqs = raw.source_seq.as_deref().unwrap_or(&[]);
    if seqs.is_empty() {
        return Verdict::Unsourced;
    }
    if seqs.iter().any(|s| *s < range.0 || *s > range.1) {
        return Verdict::OutOfRange;
    }
    if !seqs.iter().all(|s| citable.contains(s)) {
        return Verdict::Unsourced;
    }
    let Some(memory_type) = raw
        .memory_type
        .as_deref()
        .and_then(|t| serde_json::from_value::<MemoryType>(serde_json::Value::String(t.trim().to_lowercase())).ok())
    else {
        return Verdict::Invalid;
    };
    let title = clip_chars(&one_line(&mask_secrets(&prep(raw.title.as_deref().unwrap_or(""), MAX_TITLE_CHARS * 8, false)).0), MAX_TITLE_CHARS);
    let body = clip_bytes(
        mask_secrets(&prep(raw.body.as_deref().unwrap_or(""), MAX_CANDIDATE_BODY_BYTES * 2, true)).0.trim(),
        MAX_CANDIDATE_BODY_BYTES,
    );
    if title.is_empty() || body.is_empty() {
        return Verdict::Invalid;
    }
    let summary_src = raw.summary.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or(&body);
    let summary = clip_chars(
        &one_line(&mask_secrets(&prep(summary_src, MAX_SUMMARY_CHARS * 8, false)).0),
        MAX_SUMMARY_CHARS,
    );
    let mut tags: Vec<String> = Vec::new();
    for tag in raw.tags.iter().flatten().take(MAX_TAGS) {
        let tag = clip_chars(&one_line(&mask_secrets(&prep(tag, MAX_TAG_CHARS * 8, false)).0), MAX_TAG_CHARS).to_lowercase();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    let mut candidate = SuggestionCandidate {
        target: SuggestionTarget::New,
        memory_type,
        title,
        summary,
        body,
        tags,
        project: None,
    };
    if candidate.validate().is_err() {
        candidate.tags.clear();
        if candidate.validate().is_err() {
            return Verdict::Invalid;
        }
    }
    Verdict::Keep(Box::new(candidate))
}

/// Ham adayları doğrular, tekilleştirir ve `pending` öneri olarak saklar. Hiçbir not yazılmaz.
pub fn store_candidates(
    db: &Db,
    chat_id: &str,
    logical_turn_id: &str,
    end_seq: i64,
    events: &[StoredEvent],
    raws: &[RawCandidate],
) -> Result<CandidateReport> {
    let mut report = CandidateReport::default();
    if raws.is_empty() {
        return Ok(report);
    }
    let events: Vec<StoredEvent> = events.iter().filter(|e| e.seq <= end_seq).cloned().collect();
    let Some(range) = turn_range(&events, end_seq) else {
        report.rejected_unsourced = raws.len();
        return Ok(report);
    };
    let citable: BTreeSet<i64> = digest_messages(&events)
        .into_iter()
        .map(|(seq, _, _)| seq)
        .filter(|s| *s >= range.0 && *s <= range.1)
        .collect();
    let receipt = turn_receipt(chat_id, &events, end_seq)?;
    let receipt_hash = receipt.hash()?;
    let mut seen_bodies: BTreeSet<String> = BTreeSet::new();
    let mut seen_titles: HashMap<String, String> = HashMap::new();
    let mut kept: Vec<SuggestionCandidate> = Vec::new();
    for raw in raws {
        match build_candidate(raw, &citable, range) {
            Verdict::Unsourced => report.rejected_unsourced += 1,
            Verdict::OutOfRange => report.rejected_out_of_range += 1,
            Verdict::Invalid => report.rejected_invalid += 1,
            Verdict::Keep(candidate) => {
                let (title, body) = (norm(&candidate.title), norm(&candidate.body));
                if seen_bodies.contains(&body) {
                    report.rejected_duplicate += 1;
                } else if seen_titles.get(&title).is_some_and(|b| *b != body) {
                    report.rejected_conflict += 1;
                } else if kept.len() >= MAX_CANDIDATES_PER_JOB {
                    report.rejected_over_limit += 1;
                } else {
                    seen_bodies.insert(body.clone());
                    seen_titles.insert(title, body);
                    kept.push(*candidate);
                }
            }
        }
    }
    let receipts = vec![receipt_hash];
    for candidate in kept {
        let candidate_hash: Sha256Hash = candidate.candidate_hash(&receipts)?;
        let suggestion = MemorySuggestion {
            id: MemoryId::new(),
            chat_id: chat_id.to_string(),
            logical_turn_id: logical_turn_id.to_string(),
            end_seq,
            candidate,
            candidate_hash,
            receipts: receipts.clone(),
            status: SuggestionStatus::Pending,
            created_at: Utc::now(),
            decided_at: None,
            decided_by: None,
        };
        match db.memory_insert_suggestion(&suggestion)? {
            SuggestionInsert::Inserted(_) => report.stored += 1,
            SuggestionInsert::Existing(_) => report.existing += 1,
        }
    }
    Ok(report)
}
