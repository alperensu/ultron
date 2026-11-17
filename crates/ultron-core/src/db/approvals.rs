//! Onay durum makinesi ve kalıcı kuyruk. Son karar SQLite CAS ile yalnız bir kez yazılır.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Db;
use super::StoredEvent;
use crate::config::EngineId;
use crate::events::EngineEvent;
use crate::import::mask_secrets;
use crate::policy::{Action, RiskCategory};

pub const MAX_ACTION_BYTES: usize = 16 * 1024;
pub const MAX_PENDING_APPROVALS: usize = 128;
pub const MAX_RESOLVED_APPROVALS: usize = 256;
pub const MAX_APPROVAL_PAGE: usize = 32;
pub const MAX_APPROVAL_PAGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Serialize)]
pub struct ApprovalPage {
    pub approvals: Vec<Approval>,
    pub next_cursor: Option<String>,
}

pub fn validate_action_size(action: &Action) -> Result<()> {
    if serde_json::to_vec(action)?.len() > MAX_ACTION_BYTES {
        bail!("onay eylemi çok büyük (en çok 16 KiB)");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Desktop,
    Telegram,
    Approver,
    Timeout,
    Restart,
}

/// Kaynak yalnız açık daemon constructor'ından gelir; category kaynak kanıtı değildir.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    PolicyTool,
    TurnElevation,
    #[default]
    Unknown,
}

impl ApprovalStatus {
    /// Saf durum geçişi: son durumlar değişmez, servis kaynakları onay veremez.
    pub fn resolve(self, approve: bool, by: ResolvedBy) -> Result<Self> {
        if self != Self::Pending {
            bail!("onay zaten çözüldü");
        }
        match (approve, by) {
            (true, ResolvedBy::Timeout | ResolvedBy::Restart) => bail!("geçersiz onay kaynağı"),
            (false, ResolvedBy::Restart) => Ok(Self::Expired),
            (false, _) => Ok(Self::Denied),
            (true, _) => Ok(Self::Approved),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub chat_id: Option<String>,
    pub engine: EngineId,
    pub category: RiskCategory,
    pub reason: String,
    /// Geçerli JSON metni; gizli değerler serileştirmeden önce maskelenir.
    pub action_json: String,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolved_by: Option<ResolvedBy>,
    #[serde(default)]
    pub kind: ApprovalKind,
}

impl Approval {
    /// Ek idari araç filtresi uygulama katmanında yapılır; elevation burada da kapalıdır.
    pub fn is_policy_tool(&self) -> bool {
        self.kind == ApprovalKind::PolicyTool
            && serde_json::from_str::<Action>(&self.action_json)
                .is_ok_and(|action| !is_elevation(&action))
    }

    /// Telegram'dan onaylanabilecek araç kümesi (koordinatör kararı, Faz 4 Görev 6):
    /// yalnız Delete ve GitHistory kategorileri ile düz dosya yazma/silme onayları
    /// (`rm`/`del`, `git push|reset|rebase|commit`, Write/Edit/Delete araçları).
    /// Install, Deploy, Publish, Secrets, SystemConfig, Finance, tur yükseltmesi,
    /// Unknown ve her `ultron.*` idari eylemi yalnız masaüstünde onaylanır
    /// ("masaüstünde onay gerekli", düğmesiz). İdari işaretler araç adına ve
    /// korunan çalışma zamanı yol bölümlerine karşı eşleştirilir; serbest metin
    /// (dal, dosya adı, commit mesajı) idari sayılmaz. Bu ek yetenek filtresi
    /// politika değerlendirmesini veya yerel onayı değiştirmez.
    pub fn is_telegram_tool(&self) -> bool {
        if !self.is_policy_tool()
            || !matches!(self.category, RiskCategory::Delete | RiskCategory::GitHistory)
            || self.action_json.contains("[MASKED]")
        {
            return false;
        }
        serde_json::from_str::<Action>(&self.action_json).is_ok_and(|action| remote_action(&action))
    }

    /// Telegram bildirimi için kısa özet (en çok 48 karakter): program/araç başı ve
    /// hedefin yalnız dosya adı. Kontrol karakterleri atılır, metin gizli bilgi
    /// maskesinden geçer; ham eylem JSON'u veya tam yol asla dönmez.
    pub fn telegram_summary(&self) -> String {
        let Ok(action) = serde_json::from_str::<Action>(&self.action_json) else { return String::new(); };
        let raw = match &action {
            Action::FileWrite { path } => format!("yaz {}", basename(&path.to_string_lossy())),
            Action::FileDelete { path } => format!("sil {}", basename(&path.to_string_lossy())),
            Action::Shell { command, .. } => shell_summary(command),
            Action::ToolCall { tool, input } => match input.as_object() {
                Some(fields) if matches!(tool.to_ascii_lowercase().as_str(), "bash" | "shell" | "powershell" | "run_command") => {
                    fields.get("command").and_then(Value::as_str).map(shell_summary).unwrap_or_default()
                }
                Some(fields) => {
                    let target = fields.get("path").or_else(|| fields.get("file_path")).and_then(Value::as_str)
                        .map(basename).unwrap_or_default();
                    format!("{} {}", tool, target)
                }
                None => String::new(),
            },
            Action::Network { .. } => String::new(),
        };
        let clean: String = raw.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
        let masked = mask_secrets(&crate::turn_grant::mask_live(&clean)).0;
        let squeezed = masked.split_whitespace().collect::<Vec<_>>().join(" ");
        if squeezed.chars().count() > 48 {
            format!("{}…", squeezed.chars().take(47).collect::<String>())
        } else {
            squeezed
        }
    }
}

fn basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default().to_owned()
}

fn shell_summary(command: &str) -> String {
    let Some(tokens) = shell_tokens(command) else { return String::new(); };
    let Some(program) = tokens.first() else { return String::new(); };
    let program = basename(program).to_lowercase();
    if program == "git" {
        let sub = tokens.get(1).map(String::as_str).unwrap_or_default();
        let target = if sub == "commit" { None } else {
            tokens.iter().skip(2).rfind(|token| !token.starts_with('-')).map(|token| basename(token))
        };
        return format!("git {sub} {}", target.unwrap_or_default());
    }
    let target = tokens.iter().skip(1).rfind(|token| !token.starts_with('-')).map(|token| basename(token));
    format!("{program} {}", target.unwrap_or_default())
}

/// Korunan çalışma zamanı yol bölümleri ve dosya adları (idari yazma hedefleri).
fn protected_path(text: &str) -> bool {
    text.split(['/', '\\']).any(|segment| {
        let seg = segment.to_ascii_lowercase();
        matches!(seg.as_str(), ".ultron" | ".git" | ".ssh" | ".claude" | ".codex" | ".gnupg"
            | "config.toml" | "policy.toml" | "approver" | "approver.token" | "token")
    })
}

fn remote_path(path: &std::path::Path) -> bool {
    let text = path.to_string_lossy();
    !text.is_empty()
        && !text.chars().any(|ch| ch.is_control() || "$%`*?[]".contains(ch))
        && !protected_path(&text)
}

/// Tek komut için jeton ayırıcı: yalnız güvenli karakterler; çift tırnak yalnız
/// `git commit` iletisi içindir. `; | & $ ` ( ) < >`, satır sonu ve kaçışlar reddedilir.
fn shell_tokens(command: &str) -> Option<Vec<String>> {
    const OUTSIDE: &str = " -_./\\:=@+,~*";
    const INSIDE: &str = " -_./:,@#+=?'";
    let mut tokens = Vec::new();
    let (mut current, mut started, mut quoted) = (String::new(), false, false);
    let mut used_quote = false;
    for ch in command.chars() {
        if quoted {
            match ch {
                '"' => quoted = false,
                ch if ch.is_alphanumeric() || INSIDE.contains(ch) => current.push(ch),
                _ => return None,
            }
        } else if ch == '"' {
            quoted = true;
            started = true;
            used_quote = true;
        } else if ch == ' ' {
            if started { tokens.push(std::mem::take(&mut current)); started = false; }
        } else if ch.is_alphanumeric() || OUTSIDE.contains(ch) {
            current.push(ch);
            started = true;
        } else {
            return None;
        }
    }
    if quoted { return None; }
    if started { tokens.push(current); }
    let git_commit = tokens.first().is_some_and(|t| t.eq_ignore_ascii_case("git"))
        && tokens.get(1).is_some_and(|t| t.eq_ignore_ascii_case("commit"));
    if used_quote && !git_commit { return None; }
    (!tokens.is_empty()).then_some(tokens)
}

/// `rm`/`del` hedefi: tek basit glob (`build/*`) olabilir; kök, ev, üst dizin ve çıplak `*` olamaz.
fn remote_delete_target(token: &str) -> bool {
    let trimmed = token.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || matches!(trimmed, "." | ".." | "~") || trimmed.ends_with(':')
        || token.starts_with('~') || token.contains('=')
    {
        return false;
    }
    if let Some(star) = token.find('*') {
        let prefix = &token[..star];
        let name_start = prefix.rfind(['/', '\\']).map_or(0, |i| i + 1);
        // Yıldız yalnız son bölümde; öncesinde gerçek bir dizin adı bulunmalı.
        let dir = prefix[..name_start].trim_end_matches(['/', '\\']);
        let rest = &token[star..];
        if rest.contains(['/', '\\']) || dir.is_empty() || matches!(dir, "." | "..") || dir.ends_with(':') {
            return false;
        }
    }
    !protected_path(token)
}

fn remote_shell(command: &str) -> bool {
    // Script, alt kabuk, zincir ve dinamik genişletme sınıflandırılmaz.
    let Some(tokens) = shell_tokens(command) else { return false; };
    let leaf = basename(&tokens[0]).to_lowercase();
    let program = leaf.strip_suffix(".exe").unwrap_or(&leaf);
    let args = &tokens[1..];
    match program {
        "rm" | "del" | "erase" | "remove-item" => {
            let mut targets = 0;
            for arg in args {
                let flag = arg.starts_with('-')
                    || (arg.len() == 2 && arg.starts_with('/') && program != "rm");
                if flag { continue; }
                if !remote_delete_target(arg) { return false; }
                targets += 1;
            }
            targets > 0
        }
        "git" => {
            let Some(sub) = args.first().map(|s| s.to_ascii_lowercase()) else { return false; };
            matches!(sub.as_str(), "push" | "reset" | "rebase" | "commit")
                && !args.iter().skip(1).any(|arg| {
                    let lower = arg.to_ascii_lowercase();
                    lower.contains('*') || lower == "-c" || lower == "-x" || lower.starts_with("--exec")
                        || lower.starts_with("--config") || lower.starts_with("--upload-pack")
                        || lower.starts_with("--receive-pack")
                })
        }
        _ => false,
    }
}

fn remote_action(action: &Action) -> bool {
    match action {
        Action::FileWrite { path } | Action::FileDelete { path } => remote_path(path),
        Action::Shell { command, cwd } => remote_path(cwd) && remote_shell(command),
        Action::Network { .. } => false,
        Action::ToolCall { tool, input } => {
            let Some(fields) = input.as_object() else { return false; };
            match tool.to_ascii_lowercase().as_str() {
                "bash" | "shell" | "powershell" | "run_command" => {
                    fields.keys().all(|key| matches!(key.as_str(), "command" | "cwd"))
                        && fields.get("command").and_then(Value::as_str).is_some_and(remote_shell)
                        && fields.get("cwd").is_none_or(|value| value.as_str().is_some_and(|path| remote_path(path.as_ref())))
                }
                "read" | "write" | "edit" | "delete" | "file_read" | "file_write" | "file_delete" => {
                    fields.keys().all(|key| matches!(key.as_str(), "path" | "file_path" | "content" | "old_string" | "new_string" | "replace_all"))
                        && fields.get("path").or_else(|| fields.get("file_path"))
                            .and_then(Value::as_str).is_some_and(|path| remote_path(path.as_ref()))
                        && fields.get("file_path").is_none_or(|value| value.as_str().is_some_and(|path| remote_path(path.as_ref())))
                }
                _ => false,
            }
        }
    }
}

fn is_elevation(action: &Action) -> bool {
    matches!(action, Action::ToolCall { tool, .. } if tool == "ultron.turn.elevate")
}

fn masked_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(mask_secrets(&text).0),
        Value::Array(items) => Value::Array(items.into_iter().map(masked_value).collect()),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| {
                    // Anahtar temelli JSON sırları da maskele; regex çıktısı JSON olmayabilir.
                    let probe = format!("{key}=ultron_mask_probe");
                    let value = if mask_secrets(&probe).0 != probe {
                        Value::String("[MASKED]".into())
                    } else {
                        masked_value(value)
                    };
                    (mask_secrets(&key).0, value)
                })
                .collect(),
        ),
        other => other,
    }
}

impl Db {
    pub fn create_approval(
        &self,
        chat_id: Option<&str>,
        engine: EngineId,
        category: RiskCategory,
        reason: &str,
        action: &Action,
    ) -> Result<Approval> {
        self.create_owned_approval(chat_id, engine, category, reason, action, None)
    }

    pub fn create_owned_approval(
        &self,
        chat_id: Option<&str>,
        engine: EngineId,
        category: RiskCategory,
        reason: &str,
        action: &Action,
        requester_id: Option<&str>,
    ) -> Result<Approval> {
        self.create_approval_with_kind(
            chat_id,
            engine,
            category,
            reason,
            action,
            requester_id,
            ApprovalKind::Unknown,
        )
    }

    pub fn create_policy_tool_approval(
        &self,
        chat_id: Option<&str>,
        engine: EngineId,
        category: RiskCategory,
        reason: &str,
        action: &Action,
        requester_id: Option<&str>,
    ) -> Result<Approval> {
        if is_elevation(action) {
            bail!("tur yükseltmesi araç onayı olamaz");
        }
        self.create_approval_with_kind(
            chat_id,
            engine,
            category,
            reason,
            action,
            requester_id,
            ApprovalKind::PolicyTool,
        )
    }

    pub fn create_turn_elevation_approval(
        &self,
        chat_id: &str,
        engine: EngineId,
        reason: &str,
        action: &Action,
    ) -> Result<Approval> {
        if !is_elevation(action) {
            bail!("geçersiz tur yükseltme eylemi");
        }
        self.create_approval_with_kind(
            Some(chat_id),
            engine,
            RiskCategory::SystemConfig,
            reason,
            action,
            None,
            ApprovalKind::TurnElevation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_approval_with_kind(
        &self,
        chat_id: Option<&str>,
        engine: EngineId,
        category: RiskCategory,
        reason: &str,
        action: &Action,
        requester_id: Option<&str>,
        kind: ApprovalKind,
    ) -> Result<Approval> {
        validate_action_size(action)?;
        if reason.len() > 4096
            || chat_id.is_some_and(|id| id.len() > 128)
            || requester_id.is_some_and(|id| id.len() > 128)
        {
            bail!("onay meta verisi çok büyük");
        }
        let row = Approval {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.map(str::to_owned),
            engine,
            category,
            reason: mask_secrets(reason).0,
            action_json: serde_json::to_string(&masked_value(serde_json::to_value(action)?))?,
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            resolved_at: None,
            resolved_by: None,
            kind,
        };
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: usize = tx.query_row(
            "SELECT COUNT(*) FROM approvals WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let backlog: usize =
            tx.query_row("SELECT COUNT(*) FROM approval_outbox", [], |r| r.get(0))?;
        if pending >= MAX_PENDING_APPROVALS || backlog >= 512 {
            bail!("onay kuyruğu dolu; yeni işlem reddedildi");
        }
        tx.execute(
            "INSERT INTO approvals(id, chat_id, engine, category, reason, action_json, status, created_at, requester_id, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9)",
            params![row.id, row.chat_id, super::engine_str(engine), serde_json::to_string(&category)?,
                row.reason, row.action_json, row.created_at.to_rfc3339(), requester_id, wire_name(kind)?],
        )?;
        approval_event_tx(
            &tx,
            &row,
            EngineEvent::ApprovalRequested {
                approval_id: row.id.clone(),
                category,
                reason: row.reason.clone(),
                summary: crate::eventlog::excerpt(&row.action_json, 4000),
            },
        )?;
        prune_resolved(&tx)?;
        tx.commit()?;
        Ok(row)
    }

    pub fn list_approvals(&self, status: Option<ApprovalStatus>) -> Result<Vec<Approval>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, engine, category, reason, action_json, status, created_at,
                    resolved_at, resolved_by, kind FROM approvals
             WHERE (?1 IS NULL OR status = ?1) ORDER BY created_at, id LIMIT 384",
        )?;
        let status = status.map(wire_name).transpose()?;
        let rows = stmt.query_map([status], read_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// İmleç son dönen kimliktir. Hem satır sayısı hem serileştirilmiş sayfa boyutu sınırlıdır.
    pub fn list_approvals_page(
        &self,
        status: Option<ApprovalStatus>,
        requester_id: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<ApprovalPage> {
        let limit = (limit.unwrap_or(16) as usize).clamp(1, MAX_APPROVAL_PAGE);
        if cursor.is_some_and(|id| id.len() > 128) {
            bail!("geçersiz onay imleci");
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, engine, category, reason, action_json, status, created_at,
                    resolved_at, resolved_by, kind FROM approvals
             WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR requester_id = ?2)
               AND id > ?3 ORDER BY id LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                status.map(wire_name).transpose()?,
                requester_id,
                cursor.unwrap_or(""),
                limit + 1
            ],
            read_row,
        )?;
        let mut page = ApprovalPage {
            approvals: Vec::new(),
            next_cursor: None,
        };
        let mut bytes = 128;
        for row in rows {
            let mut row = row?;
            let mut size = serde_json::to_vec(&row)?.len() + 1;
            if size + 128 > MAX_APPROVAL_PAGE_BYTES {
                // Yeni sınırdan önce yazılmış büyük eylemler listelemeyi kilitlemez.
                // Kalıcı kayıt korunur; yalnız liste gösterimi açıkça kısaltılır.
                row.action_json = serde_json::json!({"omitted": "Eski eylem liste boyutu sınırı nedeniyle gösterilmedi"}).to_string();
                row.reason = crate::eventlog::excerpt(&row.reason, 4096);
                size = serde_json::to_vec(&row)?.len() + 1;
            }
            if page.approvals.len() == limit || bytes + size > MAX_APPROVAL_PAGE_BYTES {
                if page.approvals.is_empty() {
                    bail!("eski onay kaydı sayfa sınırını aşıyor");
                }
                page.next_cursor = page.approvals.last().map(|row| row.id.clone());
                break;
            }
            bytes += size;
            page.approvals.push(row);
        }
        Ok(page)
    }

    pub fn approval_outbox(&self) -> Result<Vec<(i64, StoredEvent)>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT seq, event_json FROM approval_outbox ORDER BY seq LIMIT 768")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|r| {
            let (seq, json) = r?;
            Ok((seq, serde_json::from_str(&json)?))
        })
        .collect()
    }

    pub fn acknowledge_approval_event(&self, seq: i64) -> Result<()> {
        self.conn()?
            .execute("DELETE FROM approval_outbox WHERE seq = ?1", [seq])?;
        Ok(())
    }

    pub fn prune_approvals(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        prune_resolved(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_approval(&self, id: &str) -> Result<Option<Approval>> {
        self.conn()?
            .query_row(
                "SELECT id, chat_id, engine, category, reason, action_json, status, created_at,
                    resolved_at, resolved_by, kind FROM approvals WHERE id = ?1",
                [id],
                read_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// İlk çözen kazanır. UPDATE koşulu, farklı bağlantılar arasındaki yarışı da korur.
    pub fn resolve_approval(&self, id: &str, approve: bool, by: ResolvedBy) -> Result<Approval> {
        let status = ApprovalStatus::Pending.resolve(approve, by)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE approvals SET status = ?2, resolved_at = ?3, resolved_by = ?4
             WHERE id = ?1 AND status = 'pending'",
            params![
                id,
                wire_name(status)?,
                Utc::now().to_rfc3339(),
                wire_name(by)?
            ],
        )?;
        if changed == 0 {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM approvals WHERE id = ?1)",
                [id],
                |r| r.get(0),
            )?;
            bail!(if exists {
                "onay zaten çözüldü"
            } else {
                "onay bulunamadı"
            });
        }
        let row = tx.query_row(
            "SELECT id, chat_id, engine, category, reason, action_json, status, created_at,
                    resolved_at, resolved_by, kind FROM approvals WHERE id = ?1",
            [id],
            read_row,
        )?;
        approval_event_tx(
            &tx,
            &row,
            EngineEvent::ApprovalResolved {
                approval_id: row.id.clone(),
                status: row.status,
                by,
            },
        )?;
        prune_resolved(&tx)?;
        tx.commit()?;
        Ok(row)
    }
}

fn prune_resolved(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM approvals WHERE id IN (
        SELECT id FROM approvals WHERE status != 'pending'
        ORDER BY resolved_at DESC, id DESC LIMIT -1 OFFSET ?1)",
        [MAX_RESOLVED_APPROVALS],
    )?;
    Ok(())
}

/// Durum ve kalıcı olay aynı transaction'da yazılır. Döküm/yayın hatası sonucu silemez.
fn approval_event_tx(
    tx: &rusqlite::Transaction<'_>,
    row: &Approval,
    event: EngineEvent,
) -> Result<()> {
    tx.execute(
        "INSERT INTO approval_outbox(approval_id, event_json) VALUES (?1, '{}')",
        [&row.id],
    )?;
    let seq = tx.last_insert_rowid();
    let payload = serde_json::to_value(event)?;
    let mut stored = StoredEvent {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id: row.chat_id.clone().unwrap_or_default(),
        session_id: None,
        seq,
        ts: Utc::now(),
        kind: payload["type"].as_str().unwrap_or_default().to_owned(),
        payload,
    };
    if row.chat_id.is_some() {
        stored =
            Db::append_event_tx(tx, &stored)?.ok_or_else(|| anyhow!("onay olayı zaten var"))?;
    }
    tx.execute(
        "UPDATE approval_outbox SET event_json = ?2 WHERE seq = ?1",
        params![seq, serde_json::to_string(&stored)?],
    )?;
    Ok(())
}

fn wire_name(value: impl Serialize) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("geçersiz onay değeri"))
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    fn parse<T: serde::de::DeserializeOwned>(
        r: &rusqlite::Row<'_>,
        i: usize,
        quoted: bool,
    ) -> rusqlite::Result<T> {
        let text: String = r.get(i)?;
        let json = if quoted {
            serde_json::to_string(&text).expect("metin JSON olur")
        } else {
            text
        };
        serde_json::from_str(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    }
    Ok(Approval {
        id: r.get(0)?,
        chat_id: r.get(1)?,
        engine: parse(r, 2, true)?,
        category: parse(r, 3, false)?,
        reason: r.get(4)?,
        action_json: r.get(5)?,
        status: parse(r, 6, true)?,
        created_at: parse(r, 7, true)?,
        resolved_at: r
            .get::<_, Option<String>>(8)?
            .map(|_| parse(r, 8, true))
            .transpose()?,
        resolved_by: r
            .get::<_, Option<String>>(9)?
            .map(|_| parse(r, 9, true))
            .transpose()?,
        kind: parse(r, 10, true)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_filter_keeps_unknown_elevation_admin_and_ambiguous_actions_local() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(&temp.path().join("db.sqlite")).unwrap();
        let actions = [
            (Action::FileDelete { path: "synthetic.txt".into() }, true),
            (Action::Shell { command: "rm synthetic.txt".into(), cwd: "synthetic/workspace".into() }, true),
            (Action::Shell { command: "git push origin main".into(), cwd: "synthetic/workspace".into() }, true),
            (Action::Shell { command: "ultron trash restore synthetic".into(), cwd: "synthetic/workspace".into() }, false),
            (Action::Shell { command: "rm synthetic.txt; ultron telegram set-token".into(), cwd: "synthetic/workspace".into() }, false),
            (Action::Shell { command: "powershell -EncodedCommand synthetic".into(), cwd: "synthetic/workspace".into() }, false),
            (Action::FileWrite { path: "synthetic/config.toml".into() }, false),
            (Action::FileWrite { path: "synthetic/policy.toml".into() }, false),
            (Action::ToolCall { tool: "ultron.trash.purge".into(), input: serde_json::json!({}) }, false),
            (Action::ToolCall { tool: "ultron.settings".into(), input: serde_json::json!({ "allowed_user_ids": [101] }) }, false),
            (Action::ToolCall { tool: "import_catalog".into(), input: serde_json::json!({}) }, false),
            (Action::ToolCall { tool: "future_unknown_tool".into(), input: serde_json::json!({}) }, false),
            (Action::ToolCall { tool: "Write".into(), input: serde_json::json!({ "path": "synthetic.txt", "command": "admin" }) }, false),
            (Action::ToolCall { tool: "Write".into(), input: serde_json::json!({ "path": "synthetic.txt", "content": "synthetic" }) }, true),
            (Action::ToolCall { tool: "Bash".into(), input: serde_json::json!({ "command": "rm synthetic.txt" }) }, true),
            (Action::FileDelete { path: "$dynamic".into() }, false),
        ];
        for (action, expected) in actions {
            let row = db.create_policy_tool_approval(None, EngineId::Claude, RiskCategory::Delete, "synthetic", &action, None).unwrap();
            assert_eq!(row.is_telegram_tool(), expected, "{action:?}");
            let mut unknown = row.clone(); unknown.kind = ApprovalKind::Unknown;
            assert!(!unknown.is_telegram_tool());
            let mut elevation = row.clone(); elevation.kind = ApprovalKind::TurnElevation;
            assert!(!elevation.is_telegram_tool());
            for category in [RiskCategory::Secrets, RiskCategory::SystemConfig] {
                let mut sensitive = row.clone(); sensitive.category = category;
                assert!(!sensitive.is_telegram_tool());
            }
            let mut malformed = row; malformed.action_json = "not-json".into();
            assert!(!malformed.is_telegram_tool());
        }
    }

    #[test]
    fn telegram_decision_table() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(&temp.path().join("db.sqlite")).unwrap();
        let cwd = "synthetic/workspace";
        let shell = |command: &str| Action::Shell { command: command.into(), cwd: cwd.into() };
        let rows = [
            // (kategori, eylem, Telegram'dan onaylanabilir mi)
            (RiskCategory::GitHistory, shell("git push origin feature/import-csv"), true),
            (RiskCategory::GitHistory, shell("git push origin fix/restore-policy-docs"), true),
            (RiskCategory::GitHistory, shell("git commit -m \"fix import of policy docs\""), true),
            (RiskCategory::GitHistory, shell("git commit -am \"don't panic\""), true),
            (RiskCategory::GitHistory, shell("git reset --hard HEAD~1"), true),
            (RiskCategory::GitHistory, shell("git rebase main"), true),
            (RiskCategory::Delete, shell("rm -rf imports"), true),
            (RiskCategory::Delete, shell("rm -rf build/*"), true),
            (RiskCategory::Delete, shell("rm restore_notes.txt"), true),
            (RiskCategory::Delete, Action::FileDelete { path: "synthetic/import_policy.txt".into() }, true),
            (RiskCategory::Delete, Action::FileWrite { path: "synthetic/src/main.rs".into() }, true),
            (RiskCategory::Delete, Action::ToolCall { tool: "Write".into(), input: serde_json::json!({ "file_path": "synthetic/a.txt", "content": "x" }) }, true),
            // Zincir, yönlendirme, genişletme, alt kabuk ve satır sonu reddedilir.
            (RiskCategory::GitHistory, shell("git push origin main; rm -rf x"), false),
            (RiskCategory::GitHistory, shell("git push origin main && git reset --hard"), false),
            (RiskCategory::GitHistory, shell("git commit -m \"$(whoami)\""), false),
            (RiskCategory::GitHistory, shell("git commit -m \"a `b`\""), false),
            (RiskCategory::GitHistory, shell("git commit -m \"a | b\""), false),
            (RiskCategory::GitHistory, shell("git commit -m \"a\nb\""), false),
            (RiskCategory::GitHistory, shell("git push > out.txt"), false),
            (RiskCategory::GitHistory, shell("git rebase --exec \"make\" main"), false),
            (RiskCategory::GitHistory, shell("git -c core.hooksPath=x push"), false),
            (RiskCategory::GitHistory, shell("git status"), false),
            (RiskCategory::Delete, shell("echo \"hi\""), false),
            (RiskCategory::Delete, shell("rm -rf *"), false),
            (RiskCategory::Delete, shell("rm -rf /"), false),
            (RiskCategory::Delete, shell("rm -rf ~"), false),
            (RiskCategory::Delete, shell("rm -rf ."), false),
            (RiskCategory::Delete, shell("rm -rf (x)"), false),
            (RiskCategory::Delete, shell("rm -rf $HOME"), false),
            (RiskCategory::Delete, shell("rm -rf"), false),
            (RiskCategory::Delete, shell("rm a\nrm b"), false),
            // İdari araç adı, korunan yol ve idari komutlar masaüstünde kalır.
            (RiskCategory::Delete, shell("ultron trash restore synthetic"), false),
            (RiskCategory::Delete, shell("rm ~/.ultron/config.toml"), false),
            (RiskCategory::Delete, shell("rm -rf synthetic/.git"), false),
            (RiskCategory::Delete, Action::FileWrite { path: "synthetic/config.toml".into() }, false),
            (RiskCategory::Delete, Action::FileWrite { path: "synthetic/.ultron/x".into() }, false),
            (RiskCategory::Delete, Action::ToolCall { tool: "ultron.trash.purge".into(), input: serde_json::json!({}) }, false),
            (RiskCategory::Delete, Action::ToolCall { tool: "ultron.settings".into(), input: serde_json::json!({}) }, false),
            // Kategori kararı: Install/Deploy/Publish/Secrets/SystemConfig/Finance yalnız masaüstü.
            (RiskCategory::Install, shell("rm synthetic.txt"), false),
            (RiskCategory::Deploy, shell("git push origin main"), false),
            (RiskCategory::Publish, shell("git push origin main"), false),
            (RiskCategory::Secrets, Action::FileWrite { path: "synthetic/a.txt".into() }, false),
            (RiskCategory::SystemConfig, Action::FileWrite { path: "synthetic/a.txt".into() }, false),
            (RiskCategory::Finance, Action::FileWrite { path: "synthetic/a.txt".into() }, false),
        ];
        for (category, action, expected) in rows {
            let row = db.create_policy_tool_approval(None, EngineId::Claude, category, "synthetic", &action, None).unwrap();
            assert_eq!(row.is_telegram_tool(), expected, "{category:?} {action:?}");
            drain(&db);
            db.resolve_approval(&row.id, false, ResolvedBy::Timeout).unwrap();
            drain(&db);
        }
    }

    #[test]
    fn telegram_summary_is_short_masked_and_path_free() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(&temp.path().join("db.sqlite")).unwrap();
        let make = |action: Action| db.create_policy_tool_approval(None, EngineId::Claude, RiskCategory::Delete, "r", &action, None).unwrap();
        let file = make(Action::FileDelete { path: "synthetic/dir/note.txt".into() });
        assert_eq!(file.telegram_summary(), "sil note.txt");
        let git = make(Action::Shell { command: "git push origin feature/import-csv".into(), cwd: "w".into() });
        assert_eq!(git.telegram_summary(), "git push import-csv");
        let rm = make(Action::Shell { command: "rm -rf synthetic/dir/build".into(), cwd: "w".into() });
        assert_eq!(rm.telegram_summary(), "rm build");
        let long = make(Action::FileWrite { path: format!("d/{}", "x".repeat(200)).into() });
        assert!(long.telegram_summary().chars().count() <= 48);
        let secret = make(Action::FileWrite { path: "d/password=synthetic-secret.txt".into() });
        assert!(!secret.telegram_summary().contains("synthetic-secret"));
        let control = make(Action::FileWrite { path: "d/a\nb\u{1b}c.txt".into() });
        assert!(!control.telegram_summary().chars().any(char::is_control));
    }

    fn action() -> Action {
        Action::FileDelete {
            path: "synthetic.txt".into(),
        }
    }

    fn drain(db: &Db) {
        for (seq, _) in db.approval_outbox().unwrap() {
            db.acknowledge_approval_event(seq).unwrap();
        }
    }

    #[test]
    fn approval_limits_pagination_ownership_and_retention_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("db.sqlite")).unwrap();
        let oversized = Action::FileDelete {
            path: "x".repeat(MAX_ACTION_BYTES).into(),
        };
        assert!(
            db.create_approval(
                None,
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &oversized
            )
            .is_err()
        );
        let large = Action::FileDelete {
            path: "x".repeat(MAX_ACTION_BYTES - 100).into(),
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_PENDING_APPROVALS {
            ids.push(
                db.create_owned_approval(
                    None,
                    EngineId::Claude,
                    RiskCategory::Delete,
                    "test",
                    &large,
                    Some("owner-a"),
                )
                .unwrap()
                .id,
            );
        }
        assert!(
            db.create_approval(
                None,
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &action()
            )
            .unwrap_err()
            .to_string()
            .contains("kuyruğu dolu")
        );
        assert!(
            db.list_approvals_page(None, Some("owner-b"), None, None)
                .unwrap()
                .approvals
                .is_empty()
        );
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = db
                .list_approvals_page(None, Some("owner-a"), Some(u32::MAX), cursor.as_deref())
                .unwrap();
            let response = crate::ipc::Response::Ok {
                data: serde_json::to_value(&page).unwrap(),
            };
            assert!(serde_json::to_vec(&response).unwrap().len() < crate::ipc::MAX_LINE_BYTES);
            assert!(page.approvals.len() <= MAX_APPROVAL_PAGE);
            seen.extend(page.approvals.iter().map(|row| row.id.clone()));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        ids.sort();
        assert_eq!(seen, ids);
        drain(&db);
        for id in &ids {
            db.resolve_approval(id, false, ResolvedBy::Timeout).unwrap();
            drain(&db);
        }
        for _ in 0..MAX_RESOLVED_APPROVALS {
            let row = db
                .create_approval(
                    None,
                    EngineId::Claude,
                    RiskCategory::Delete,
                    "test",
                    &action(),
                )
                .unwrap();
            db.resolve_approval(&row.id, false, ResolvedBy::Timeout)
                .unwrap();
            drain(&db);
        }
        assert_eq!(
            db.list_approvals(None).unwrap().len(),
            MAX_RESOLVED_APPROVALS
        );
    }

    #[test]
    fn terminal_transition_and_event_roll_back_together() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("db.sqlite")).unwrap();
        db.insert_chat("chat", None, "test", Utc::now()).unwrap();
        let row = db
            .create_approval(
                Some("chat"),
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &action(),
            )
            .unwrap();
        db.conn().unwrap().execute_batch("CREATE TRIGGER reject_outbox BEFORE INSERT ON approval_outbox BEGIN SELECT RAISE(ABORT, 'sentetik hata'); END;").unwrap();
        assert!(
            db.resolve_approval(&row.id, true, ResolvedBy::Desktop)
                .is_err()
        );
        assert_eq!(
            db.get_approval(&row.id).unwrap().unwrap().status,
            ApprovalStatus::Pending
        );
        assert_eq!(db.events_for_chat("chat", None).unwrap().len(), 1);
        db.conn()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_outbox;")
            .unwrap();
        db.resolve_approval(&row.id, false, ResolvedBy::Timeout)
            .unwrap();
        let events = db.events_for_chat("chat", None).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "approval_resolved");
    }

    #[test]
    fn legacy_large_actions_list_below_ipc_frame_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("db.sqlite")).unwrap();
        let json =
            serde_json::to_string(&serde_json::json!({"path": "x".repeat(512 * 1024)})).unwrap();
        for n in 0..33 {
            db.conn().unwrap().execute(
                "INSERT INTO approvals(id, engine, category, reason, action_json, created_at) VALUES (?1, 'claude', ?2, 'test', ?3, ?4)",
                params![format!("legacy-{n:02}"), serde_json::to_string(&RiskCategory::Delete).unwrap(), json, Utc::now().to_rfc3339()]).unwrap();
        }
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = db
                .list_approvals_page(None, None, Some(32), cursor.as_deref())
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < crate::ipc::MAX_LINE_BYTES);
            for row in &page.approvals {
                assert!(row.action_json.contains("omitted"));
            }
            count += page.approvals.len();
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(count, 33);
        assert_eq!(
            db.get_approval("legacy-00").unwrap().unwrap().action_json,
            json
        );
    }

    #[test]
    fn global_event_cursor_survives_reopen_and_outbox_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.sqlite");
        let last = {
            let db = Db::open(&path).unwrap();
            let row = db
                .create_approval(
                    None,
                    EngineId::Claude,
                    RiskCategory::Delete,
                    "test",
                    &action(),
                )
                .unwrap();
            db.resolve_approval(&row.id, false, ResolvedBy::Timeout)
                .unwrap();
            let events = db.approval_outbox().unwrap();
            let last = events.last().unwrap().1.seq;
            drain(&db);
            last
        };
        let db = Db::open(&path).unwrap();
        db.create_approval(
            None,
            EngineId::Claude,
            RiskCategory::Delete,
            "test",
            &action(),
        )
        .unwrap();
        assert!(db.approval_outbox().unwrap()[0].1.seq > last);
    }

    #[test]
    fn undelivered_event_backlog_rejects_new_requests_but_allows_terminal_events() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("db.sqlite")).unwrap();
        for _ in 0..256 {
            let row = db
                .create_approval(
                    None,
                    EngineId::Claude,
                    RiskCategory::Delete,
                    "test",
                    &action(),
                )
                .unwrap();
            db.resolve_approval(&row.id, false, ResolvedBy::Timeout)
                .unwrap();
        }
        assert!(
            db.create_approval(
                None,
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &action()
            )
            .unwrap_err()
            .to_string()
            .contains("kuyruğu dolu")
        );
        assert_eq!(db.approval_outbox().unwrap().len(), 512);
    }

    #[test]
    fn pending_transitions_and_terminal_states_are_final() {
        for by in [
            ResolvedBy::Desktop,
            ResolvedBy::Telegram,
            ResolvedBy::Approver,
        ] {
            assert_eq!(
                ApprovalStatus::Pending.resolve(true, by).unwrap(),
                ApprovalStatus::Approved
            );
            assert_eq!(
                ApprovalStatus::Pending.resolve(false, by).unwrap(),
                ApprovalStatus::Denied
            );
        }
        assert_eq!(
            ApprovalStatus::Pending
                .resolve(false, ResolvedBy::Timeout)
                .unwrap(),
            ApprovalStatus::Denied
        );
        assert_eq!(
            ApprovalStatus::Pending
                .resolve(false, ResolvedBy::Restart)
                .unwrap(),
            ApprovalStatus::Expired
        );
        for status in [
            ApprovalStatus::Approved,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
        ] {
            for by in [
                ResolvedBy::Desktop,
                ResolvedBy::Telegram,
                ResolvedBy::Approver,
                ResolvedBy::Timeout,
                ResolvedBy::Restart,
            ] {
                assert!(status.resolve(true, by).is_err());
                assert!(status.resolve(false, by).is_err());
            }
        }
        assert!(
            ApprovalStatus::Pending
                .resolve(true, ResolvedBy::Timeout)
                .is_err()
        );
        assert!(
            ApprovalStatus::Pending
                .resolve(true, ResolvedBy::Restart)
                .is_err()
        );
    }

    #[test]
    fn database_first_wins_and_masks_valid_json() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(&temp.path().join("db.sqlite")).unwrap();
        let row = db.create_approval(None, EngineId::Claude, RiskCategory::Delete, "test", &Action::ToolCall {
            tool: "delete".into(), input: serde_json::json!({"password": "synthetic-value", "command": "rm x; token=synthetic-value"}),
        }).unwrap();
        assert!(!row.action_json.contains("synthetic-value"));
        assert!(serde_json::from_str::<Value>(&row.action_json).is_ok());
        let other = Db::open(&temp.path().join("db.sqlite")).unwrap();
        db.resolve_approval(&row.id, true, ResolvedBy::Desktop)
            .unwrap();
        assert!(
            other
                .resolve_approval(&row.id, false, ResolvedBy::Telegram)
                .is_err()
        );
        assert_eq!(
            other.get_approval(&row.id).unwrap().unwrap().status,
            ApprovalStatus::Approved
        );
    }

    #[test]
    fn concurrent_database_resolvers_have_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db.sqlite");
        let db = Db::open(&path).unwrap();
        let row = db
            .create_approval(
                None,
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &Action::FileDelete {
                    path: "synthetic.txt".into(),
                },
            )
            .unwrap();
        let a = Db::open(&path).unwrap();
        let b = Db::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let outcomes = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                a.resolve_approval(&row.id, true, ResolvedBy::Desktop)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                b.resolve_approval(&row.id, false, ResolvedBy::Telegram)
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(
            outcomes
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e.to_string() == "onay zaten çözüldü")
        );
        let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
        assert_eq!(db.get_approval(&row.id).unwrap().unwrap(), winner);
    }

    #[test]
    fn invalid_persisted_record_rolls_back_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(&temp.path().join("db.sqlite")).unwrap();
        let row = db
            .create_approval(
                None,
                EngineId::Claude,
                RiskCategory::Delete,
                "test",
                &Action::FileDelete {
                    path: "synthetic.txt".into(),
                },
            )
            .unwrap();
        // Bozuk bir eski kayıt çözümlenirken durum değişikliği de geri alınmalı.
        db.conn()
            .unwrap()
            .execute(
                "UPDATE approvals SET category = 'bozuk' WHERE id = ?1",
                [&row.id],
            )
            .unwrap();
        assert!(
            db.resolve_approval(&row.id, true, ResolvedBy::Desktop)
                .is_err()
        );
        let status: String = db
            .conn()
            .unwrap()
            .query_row(
                "SELECT status FROM approvals WHERE id = ?1",
                [&row.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }
}
