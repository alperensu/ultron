//! Gölge onay puanı deposu. Yalnız gözlem verisi yazar ve okur; onay durumuna, zaman aşımına
//! veya yürütmeye dokunan hiçbir yol içermez. Ham eylem metni bu tabloda yoktur.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::Db;
use super::approvals::{ApprovalStatus, ResolvedBy};
use crate::memory::contracts::ApprovalShadowScore;

/// Rapor için bir `Ask` onayının kararı ve (varsa) gölge puanı.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowAskRow {
    pub approval_id: String,
    pub created_at: DateTime<Utc>,
    pub status: ApprovalStatus,
    pub resolved_by: Option<ResolvedBy>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub score: Option<ShadowScoreRow>,
}

/// Saklanan gölge puanının rapor için gerekli alanları.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowScoreRow {
    pub p_safe: f64,
    pub model: String,
    pub prompt_version: String,
    pub measured_at: DateTime<Utc>,
}

fn status_from_db(text: &str) -> Result<ApprovalStatus> {
    Ok(match text {
        "pending" => ApprovalStatus::Pending,
        "approved" => ApprovalStatus::Approved,
        "denied" => ApprovalStatus::Denied,
        "expired" => ApprovalStatus::Expired,
        other => return Err(anyhow!("bilinmeyen onay durumu: {other}")),
    })
}

fn resolved_by_from_db(text: &str) -> Result<ResolvedBy> {
    Ok(match text {
        "desktop" => ResolvedBy::Desktop,
        "telegram" => ResolvedBy::Telegram,
        "approver" => ResolvedBy::Approver,
        "timeout" => ResolvedBy::Timeout,
        "restart" => ResolvedBy::Restart,
        other => return Err(anyhow!("bilinmeyen çözen: {other}")),
    })
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc))
}

type RawRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl Db {
    /// Puanı idempotent yazar: aynı `approval_id` için ilk yazım kazanır. Dönüş `true` ise yeni
    /// satır yazıldı. Onay tablosuna veya durumuna yazılmaz; yabancı anahtar onayı yoksa hata verir.
    pub fn insert_shadow_score(&self, score: &ApprovalShadowScore) -> Result<bool> {
        score
            .validate()
            .map_err(|_| anyhow!("gölge puan sözleşmeye uymuyor"))?;
        let duration_ms = i64::try_from(score.duration_ms)
            .map_err(|_| anyhow!("gölge süre sınır dışı"))?;
        let conn = self.conn()?;
        let written = conn.execute(
            "INSERT INTO approval_shadow_scores(approval_id, model, prompt_version, p_safe,
                measured_at, duration_ms, action_fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(approval_id) DO NOTHING",
            params![
                score.approval_id,
                score.model,
                score.prompt_version,
                score.p_safe,
                score.measured_at.to_rfc3339(),
                duration_ms,
                score.action_fingerprint.prefixed(),
            ],
        )?;
        Ok(written == 1)
    }

    /// Mevcut tutulan `Ask` onaylarını kararları ve varsa gölge puanlarıyla döndürür (en eski
    /// önce). Yalnız yarı açık `[since, until)` penceresindeki onaylar okunur (SQL'de sınırlı;
    /// `julianday` biçim farklarından etkilenmez).
    pub fn shadow_ask_rows(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Vec<ShadowAskRow>> {
        let raw: Vec<RawRow> = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare(
                "SELECT a.id, a.created_at, a.status, a.resolved_by, a.resolved_at,
                    s.p_safe, s.model, s.prompt_version, s.measured_at
                 FROM approvals a
                 LEFT JOIN approval_shadow_scores s ON s.approval_id = a.id
                 WHERE julianday(a.created_at) >= julianday(?1) AND julianday(a.created_at) < julianday(?2)
                 ORDER BY a.created_at, a.id",
            )?;
            let rows = stmt.query_map([since.to_rfc3339(), until.to_rfc3339()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<RawRow>>>()?
        };
        raw.into_iter()
            .map(
                |(id, created, status, resolved_by, resolved_at, p_safe, model, prompt, measured)| {
                    let score = match (p_safe, model, prompt, measured) {
                        (Some(p_safe), Some(model), Some(prompt_version), Some(measured)) => {
                            Some(ShadowScoreRow {
                                p_safe,
                                model,
                                prompt_version,
                                measured_at: parse_time(&measured)?,
                            })
                        }
                        _ => None,
                    };
                    Ok(ShadowAskRow {
                        approval_id: id,
                        created_at: parse_time(&created)?,
                        status: status_from_db(&status)?,
                        resolved_by: resolved_by.as_deref().map(resolved_by_from_db).transpose()?,
                        resolved_at: resolved_at.as_deref().map(parse_time).transpose()?,
                        score,
                    })
                },
            )
            .collect()
    }

    /// Tek onayın gölge puanı (yoksa `None`). Yalnız test ve tanılama için.
    pub fn shadow_score_for(&self, approval_id: &str) -> Result<Option<ShadowScoreRow>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT p_safe, model, prompt_version, measured_at FROM approval_shadow_scores
                 WHERE approval_id = ?1",
                [approval_id],
                |row| {
                    Ok((
                        row.get::<_, f64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(p_safe, model, prompt_version, measured)| {
            Ok(ShadowScoreRow {
                p_safe,
                model,
                prompt_version,
                measured_at: parse_time(&measured)?,
            })
        })
        .transpose()
    }
}
