//! Kalıcı bekleme sahipliği. Her geçiş nesli artırır; eski timer/claim yeniden
//! sahiplenemez. Çökme anındaki claimed/running otomatik replay edilmez.

use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{Db, StoredEvent, engine_parse, engine_str, parse_ts, raw_event_row, raw_to_event};
use crate::config::EngineId;
use crate::events::EngineEvent;
use crate::ipc::PolicyArg;

/// Full tek turluk onaydır. Yalnız yeni yerel onay gerektiği saklanır;
/// önceki yükseltmenin yetkisi veya grant'i taşınmaz, otomatik devam kapalıdır.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePolicy {
    ReadOnly,
    Workspace,
    RequiresLocalApproval,
}

impl From<PolicyArg> for ResumePolicy {
    fn from(policy: PolicyArg) -> Self {
        match policy {
            PolicyArg::ReadOnly => Self::ReadOnly,
            PolicyArg::Workspace => Self::Workspace,
            PolicyArg::Full => Self::RequiresLocalApproval,
        }
    }
}

impl ResumePolicy {
    /// İstenen politika; bu değer hiçbir şekilde yükseltme onayı/grant değildir.
    pub fn as_policy_arg(self) -> PolicyArg {
        match self {
            Self::ReadOnly => PolicyArg::ReadOnly,
            Self::Workspace => PolicyArg::Workspace,
            Self::RequiresLocalApproval => PolicyArg::Full,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Workspace => "workspace",
            Self::RequiresLocalApproval => "requires_local_approval",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaWaitState {
    Waiting,
    Claimed,
    Running,
    Completed,
    Cancelled,
    NeedsAttention,
}

impl QuotaWaitState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::NeedsAttention => "needs_attention",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "waiting" => Ok(Self::Waiting),
            "claimed" => Ok(Self::Claimed),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "needs_attention" => Ok(Self::NeedsAttention),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }

    fn can_advance_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Claimed, Self::Running)
                | (Self::Running, Self::Waiting | Self::Completed)
                | (
                    Self::Waiting | Self::Claimed | Self::Running,
                    Self::Cancelled | Self::NeedsAttention
                )
                | (Self::NeedsAttention, Self::Cancelled)
        )
    }
}

/// Metin/araç girdisi/yetki belirteci kopyalanmaz; hedef olay referansından yüklenir.
pub struct NewQuotaWait {
    pub id: String,
    pub logical_turn_id: String,
    pub chat_id: String,
    pub source_session_id: String,
    pub engine: EngineId,
    pub user_event_seq: i64,
    pub checkpoint_seq: i64,
    pub cwd: PathBuf,
    pub policy: ResumePolicy,
    pub reset_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

pub struct QuotaWait {
    pub id: String,
    pub logical_turn_id: String,
    pub chat_id: String,
    pub source_session_id: String,
    pub engine: EngineId,
    pub user_event_seq: i64,
    pub checkpoint_seq: i64,
    pub cwd: PathBuf,
    pub policy: ResumePolicy,
    pub reset_at: Option<DateTime<Utc>>,
    pub state: QuotaWaitState,
    pub generation: i64,
    pub next_engine_index: i64,
    pub attempt_count: i64,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

pub struct QuotaWaitProgress {
    pub checkpoint_seq: i64,
    pub next_engine_index: i64,
    pub reset_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// Yetki içermez. Sıra değişiklikleri ve restart boyunca aynı beklemenin planını korur.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaWaitSchedule {
    pub fallback_order: Option<Vec<EngineId>>,
    pub planned_reset_at: Option<DateTime<Utc>>,
    pub reset_deadline: Option<DateTime<Utc>>,
}

pub struct QuotaResumeContext {
    pub wait: QuotaWait,
    pub user_event: StoredEvent,
    pub checkpoint: StoredEvent,
}

const COLUMNS: &str = "id, logical_turn_id, chat_id, source_session_id, engine,
    user_event_seq, checkpoint_seq, cwd, policy, reset_at, state, generation,
    next_engine_index, attempt_count, next_attempt_at, updated_at";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<QuotaWait> {
    fn convert<T>(value: Result<T>) -> rusqlite::Result<T> {
        value.map_err(|_| rusqlite::Error::InvalidQuery)
    }
    let policy: String = r.get(8)?;
    Ok(QuotaWait {
        id: r.get(0)?,
        logical_turn_id: r.get(1)?,
        chat_id: r.get(2)?,
        source_session_id: r.get(3)?,
        engine: convert(engine_parse(&r.get::<_, String>(4)?))?,
        user_event_seq: r.get(5)?,
        checkpoint_seq: r.get(6)?,
        cwd: PathBuf::from(r.get::<_, String>(7)?),
        policy: match policy.as_str() {
            "read_only" => ResumePolicy::ReadOnly,
            "workspace" => ResumePolicy::Workspace,
            "requires_local_approval" => ResumePolicy::RequiresLocalApproval,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        reset_at: r
            .get::<_, Option<String>>(9)?
            .map(|s| convert(parse_ts(&s)))
            .transpose()?,
        state: QuotaWaitState::parse(&r.get::<_, String>(10)?)?,
        generation: r.get(11)?,
        next_engine_index: r.get(12)?,
        attempt_count: r.get(13)?,
        next_attempt_at: r
            .get::<_, Option<String>>(14)?
            .map(|s| convert(parse_ts(&s)))
            .transpose()?,
        updated_at: convert(parse_ts(&r.get::<_, String>(15)?))?,
    })
}

fn load(conn: &rusqlite::Connection, id: &str) -> Result<Option<QuotaWait>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM quota_waits WHERE id = ?1"),
            [id],
            read_row,
        )
        .optional()?)
}

fn event_at(conn: &rusqlite::Connection, chat_id: &str, seq: i64) -> Result<Option<StoredEvent>> {
    let raw = conn.query_row(
        "SELECT id, chat_id, session_id, seq, ts, kind, payload_json FROM events WHERE chat_id = ?1 AND seq = ?2",
        params![chat_id, seq], raw_event_row,
    ).optional()?;
    // Bozuk JSON/tarih yeni göreve dönüşmez; referans doğrulaması bunu dikkat bekler yapar.
    Ok(raw.and_then(|raw| raw_to_event(raw).ok()))
}

fn references(
    conn: &rusqlite::Connection,
    wait: &QuotaWait,
) -> Result<Option<(StoredEvent, StoredEvent)>> {
    if wait.policy == ResumePolicy::RequiresLocalApproval {
        return Ok(None);
    }
    let origin_mismatch: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM telegram_turn_origins WHERE logical_turn_id = ?1
            AND (chat_id != ?2 OR user_event_seq != ?3))",
        params![wait.logical_turn_id, wait.chat_id, wait.user_event_seq],
        |r| r.get(0),
    )?;
    let session_matches: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM engine_sessions WHERE id = ?1 AND chat_id = ?2 AND engine = ?3)",
        params![wait.source_session_id, wait.chat_id, engine_str(wait.engine)], |r| r.get(0),
    )?;
    let newer_user: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE chat_id = ?1 AND seq > ?2 AND kind = 'user_text')",
        params![wait.chat_id, wait.user_event_seq], |r| r.get(0),
    )?;
    if origin_mismatch
        || !session_matches
        || newer_user
        || wait.checkpoint_seq < wait.user_event_seq
    {
        return Ok(None);
    }
    let Some(user) = event_at(conn, &wait.chat_id, wait.user_event_seq)? else {
        return Ok(None);
    };
    let Some(checkpoint) = event_at(conn, &wait.chat_id, wait.checkpoint_seq)? else {
        return Ok(None);
    };
    let user_valid = user.kind == "user_text"
        && matches!(
            serde_json::from_value::<EngineEvent>(user.payload.clone()),
            Ok(EngineEvent::UserText { text }) if !text.trim().is_empty()
        );
    let checkpoint_valid = serde_json::from_value::<EngineEvent>(checkpoint.payload.clone())
        .is_ok_and(|event| {
            serde_json::to_value(event)
                .ok()
                .and_then(|payload| payload["type"].as_str().map(str::to_owned))
                .is_some_and(|kind| kind == checkpoint.kind)
        });
    Ok((user_valid && checkpoint_valid).then_some((user, checkpoint)))
}

fn attention_tx(tx: &Transaction<'_>, id: &str, generation: i64, now: DateTime<Utc>) -> Result<()> {
    tx.execute(
        "UPDATE quota_waits SET state = 'needs_attention', generation = generation + 1,
         next_attempt_at = NULL, updated_at = ?3 WHERE id = ?1 AND generation = ?2",
        params![id, generation, now.to_rfc3339()],
    )?;
    Ok(())
}

pub(super) fn invalidate_chat_tx(
    tx: &Transaction<'_>,
    chat_id: &str,
    now: DateTime<Utc>,
) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE quota_waits SET state = 'cancelled', generation = generation + 1,
         next_attempt_at = NULL, updated_at = ?2 WHERE chat_id = ?1
         AND state IN ('waiting', 'claimed', 'running', 'needs_attention')",
        params![chat_id, now.to_rfc3339()],
    )?)
}

pub(super) fn recover_waits_tx(tx: &Transaction<'_>, now: DateTime<Utc>) -> Result<usize> {
    let waits = {
        let mut stmt = tx.prepare(&format!(
            "SELECT {COLUMNS} FROM quota_waits WHERE state IN ('waiting', 'claimed', 'running')"
        ))?;
        stmt.query_map([], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut count = 0;
    for wait in waits {
        if wait.state != QuotaWaitState::Waiting || references(tx, &wait)?.is_none() {
            attention_tx(tx, &wait.id, wait.generation, now)?;
            count += 1;
        }
    }
    Ok(count)
}

impl Db {
    /// Waiting neslinde ilk fallback zincirini ve her yeni reset'in deadline'ını sabitler.
    /// Aynı nesilde yarışan scheduler'lar aynı planı alır; nesil/yetki değiştirmez.
    pub fn prepare_quota_wait_schedule(
        &self,
        id: &str,
        generation: i64,
        fallback_order: &[EngineId],
        reset_deadline: Option<DateTime<Utc>>,
    ) -> Result<Option<QuotaWaitSchedule>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(wait) = load(&tx, id)? else { return Ok(None) };
        if wait.generation != generation || wait.state != QuotaWaitState::Waiting {
            return Ok(None);
        }
        let json: String = tx.query_row(
            "SELECT scheduler_json FROM quota_waits WHERE id = ?1", [id], |r| r.get(0),
        )?;
        let mut schedule: QuotaWaitSchedule = serde_json::from_str(&json)?;
        if schedule.fallback_order.is_none() && !fallback_order.is_empty() {
            ensure!(fallback_order.len() <= 3, "motor zinciri sınırı aşıldı");
            schedule.fallback_order = Some(fallback_order.to_vec());
        }
        if schedule.planned_reset_at != wait.reset_at {
            schedule.planned_reset_at = wait.reset_at;
            schedule.reset_deadline = reset_deadline;
        }
        tx.execute(
            "UPDATE quota_waits SET scheduler_json = ?3 WHERE id = ?1 AND generation = ?2 AND state = 'waiting'",
            params![id, generation, serde_json::to_string(&schedule)?],
        )?;
        tx.commit()?;
        Ok(Some(schedule))
    }

    /// Yürütücü kabul etmeden ayar nedeniyle bırakılan claim. Running/terminal geri açılmaz.
    /// Gerçekte başlamayan claim deneme bütçesini tüketmez; eski nesil geçersizleşir.
    pub fn release_quota_wait_claim(
        &self,
        id: &str,
        generation: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<QuotaWait>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(wait) = load(&tx, id)? else { return Ok(None) };
        if wait.generation != generation || wait.state != QuotaWaitState::Claimed {
            return Ok(None);
        }
        if references(&tx, &wait)?.is_none() {
            attention_tx(&tx, id, generation, now)?;
        } else {
            tx.execute(
                "UPDATE quota_waits SET state = 'waiting', generation = generation + 1,
                 attempt_count = MAX(0, attempt_count - 1), updated_at = ?3
                 WHERE id = ?1 AND generation = ?2 AND state = 'claimed'",
                params![id, generation, now.to_rfc3339()],
            )?;
        }
        let wait = load(&tx, id)?;
        tx.commit()?;
        Ok(wait)
    }

    /// Yeni bekleme daima generation=0 ile başlar. Bozuk olay referansı dikkat bekler
    /// olarak saklanır; olmayan sohbet/oturum FK hatasıdır ve hiçbir kayıt yazılmaz.
    pub fn create_quota_wait(&self, new: &NewQuotaWait, now: DateTime<Utc>) -> Result<QuotaWait> {
        let cwd = new
            .cwd
            .to_str()
            .ok_or_else(|| anyhow!("çalışma yolu Unicode değil"))?;
        ensure!(
            !cwd.is_empty() && cwd.len() <= 32768 && !cwd.contains('\0'),
            "geçersiz çalışma yolu"
        );
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO quota_waits(id, logical_turn_id, chat_id, source_session_id, engine,
             user_event_seq, checkpoint_seq, cwd, policy, reset_at, state, next_attempt_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'waiting', ?11, ?12)",
            params![new.id, new.logical_turn_id, new.chat_id, new.source_session_id, engine_str(new.engine),
                new.user_event_seq, new.checkpoint_seq, cwd, new.policy.as_str(), new.reset_at.map(|t| t.to_rfc3339()),
                new.next_attempt_at.map(|t| t.to_rfc3339()), now.to_rfc3339()],
        )?;
        let wait = load(&tx, &new.id)?.ok_or_else(|| anyhow!("bekleme kaydı yok"))?;
        if references(&tx, &wait)?.is_none() {
            attention_tx(&tx, &wait.id, wait.generation, now)?;
        }
        let wait = load(&tx, &new.id)?.ok_or_else(|| anyhow!("bekleme kaydı yok"))?;
        tx.commit()?;
        Ok(wait)
    }

    pub fn quota_wait(&self, id: &str) -> Result<Option<QuotaWait>> {
        let conn = self.conn()?;
        load(&conn, id)
    }

    /// Restart/scheduler görünümü; terminal kayıtlar otomatik iş listesine girmez.
    pub fn pending_quota_waits(&self) -> Result<Vec<QuotaWait>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM quota_waits
            WHERE state IN ('waiting', 'claimed', 'running', 'needs_attention') ORDER BY updated_at, id"))?;
        Ok(stmt
            .query_map([], read_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Tek yazıcı transaction'ında nesil + waiting + zaman koşulu. Yeni nesil geri
    /// döner; aynı nesil ikinci kez kazanamaz. reset_at politikasını scheduler seçer.
    pub fn claim_quota_wait(
        &self,
        id: &str,
        generation: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<QuotaWait>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(wait) = load(&tx, id)? else {
            return Ok(None);
        };
        if wait.generation != generation
            || wait.state != QuotaWaitState::Waiting
            || wait.next_attempt_at.is_some_and(|at| at > now)
        {
            return Ok(None);
        }
        if references(&tx, &wait)?.is_none() {
            attention_tx(&tx, id, generation, now)?;
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE quota_waits SET state = 'claimed', generation = generation + 1,
             attempt_count = attempt_count + 1, updated_at = ?3 WHERE id = ?1 AND generation = ?2 AND state = 'waiting'",
            params![id, generation, now.to_rfc3339()],
        )?;
        let wait = load(&tx, id)?;
        tx.commit()?;
        Ok(wait)
    }

    /// Claim dışındaki yasal durum geçişleri. None eski nesil/yok kayıt demektir.
    /// Progress yalnız running sonrası checkpoint/fallback ilerlemesidir.
    pub fn advance_quota_wait(
        &self,
        id: &str,
        generation: i64,
        next: QuotaWaitState,
        progress: Option<&QuotaWaitProgress>,
        now: DateTime<Utc>,
    ) -> Result<Option<QuotaWait>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut wait) = load(&tx, id)? else {
            return Ok(None);
        };
        if wait.generation != generation {
            return Ok(None);
        }
        ensure!(
            wait.state.can_advance_to(next),
            "geçersiz kota bekleme geçişi"
        );
        if let Some(progress) = progress {
            ensure!(
                wait.state == QuotaWaitState::Running
                    && matches!(next, QuotaWaitState::Waiting | QuotaWaitState::Completed),
                "bu geçişte checkpoint değiştirilemez"
            );
            ensure!(
                progress.checkpoint_seq >= wait.checkpoint_seq
                    && progress.next_engine_index >= wait.next_engine_index,
                "checkpoint veya motor sırası gerileyemez"
            );
            wait.checkpoint_seq = progress.checkpoint_seq;
            wait.next_engine_index = progress.next_engine_index;
            wait.reset_at = progress.reset_at;
            wait.next_attempt_at = progress.next_attempt_at;
        }
        if matches!(
            next,
            QuotaWaitState::Running | QuotaWaitState::Waiting | QuotaWaitState::Completed
        ) && references(&tx, &wait)?.is_none()
        {
            attention_tx(&tx, id, generation, now)?;
        } else {
            let next_attempt = if next == QuotaWaitState::Waiting {
                wait.next_attempt_at
            } else {
                None
            };
            tx.execute(
                "UPDATE quota_waits SET state = ?3, generation = generation + 1, checkpoint_seq = ?4,
                 next_engine_index = ?5, reset_at = ?6, next_attempt_at = ?7, updated_at = ?8
                 WHERE id = ?1 AND generation = ?2",
                params![id, generation, next.as_str(), wait.checkpoint_seq, wait.next_engine_index,
                    wait.reset_at.map(|t| t.to_rfc3339()), next_attempt.map(|t| t.to_rfc3339()), now.to_rfc3339()],
            )?;
        }
        let wait = load(&tx, id)?;
        tx.commit()?;
        Ok(wait)
    }

    /// Yetki belirteci üretmez. Geçerli claimed neslin özgün hedefi/checkpoint'i
    /// döner; motor rezervasyonu ve yeni grant TurnService sorumluluğundadır.
    pub fn quota_resume_context(
        &self,
        id: &str,
        generation: i64,
        now: DateTime<Utc>,
    ) -> Result<Option<QuotaResumeContext>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(wait) = load(&tx, id)? else {
            return Ok(None);
        };
        if wait.generation != generation || wait.state != QuotaWaitState::Claimed {
            return Ok(None);
        }
        let Some((user_event, checkpoint)) = references(&tx, &wait)? else {
            attention_tx(&tx, id, generation, now)?;
            tx.commit()?;
            return Ok(None);
        };
        tx.commit()?;
        Ok(Some(QuotaResumeContext {
            wait,
            user_event,
            checkpoint,
        }))
    }

    pub fn cancel_quota_waits(&self, chat_id: &str, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::telegram::interrupt_telegram_chat_tx(&tx, chat_id, true)?;
        let count = invalidate_chat_tx(&tx, chat_id, now)?;
        tx.commit()?;
        Ok(count)
    }

    /// Running/claimed harici yan etkisi belirsizdir. Waiting doğrulanır ve korunur;
    /// completed/cancelled hiçbir koşulda yeniden açılmaz. İkinci çağrı zararsızdır.
    pub fn recover_quota_waits(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = recover_waits_tx(&tx, now)?;
        tx.commit()?;
        Ok(count)
    }
}
