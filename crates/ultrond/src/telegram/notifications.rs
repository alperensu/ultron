//! Düz metin biçimleme ve kalıcı olaylardan bildirim telafisi.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use chrono::{DateTime, FixedOffset, Utc};
use ultron_core::config::NotifyMode;
use ultron_core::db::telegram::{DeliveryState, TelegramTurnOrigin};
use ultron_core::db::{Db, StoredEvent};
use ultron_core::events::EngineEvent;
use ultron_core::quota::{Pool, QuotaReport, QuotaSource};
use super::contracts::{BoxFuture, CompletionOutcome, GatewayError, GatewayStatusSnapshot, SendMessage};

pub const MESSAGE_LIMIT: usize = 4096;
pub const TRUNCATION_NOTE: &str = "\nYanıt kısaltıldı; tamamı masaüstünde.";

/// DB sahibi adaptörde kalıcı origin satırlarını okur; yalnız aktif turlar
/// restart sonrası final telafisi için yeterli değildir.
pub trait OriginReader: Send + Sync {
    fn origins_for_chat<'a>(&'a self, chat_id: &'a str)
        -> BoxFuture<'a, Result<Vec<TelegramTurnOrigin>, GatewayError>>;
}

/// Gönderim kuyruğundaki final mantıksal tur kimlikleri; send_loop işi bitirince bırakır.
pub type InFlight = Arc<Mutex<HashSet<String>>>;

pub fn bounded_text(text: &str) -> String {
    if text.encode_utf16().count() <= MESSAGE_LIMIT { return text.into(); }
    let budget = MESSAGE_LIMIT - TRUNCATION_NOTE.encode_utf16().count();
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        if used + c.len_utf16() > budget { break; }
        out.push(c);
        used += c.len_utf16();
    }
    out.push_str(TRUNCATION_NOTE);
    out
}

pub fn istanbul_time(at: Option<DateTime<Utc>>) -> String {
    at.map(|at| at.with_timezone(&FixedOffset::east_opt(3 * 3600).expect("sabit UTC ofseti"))
        .format("%d.%m.%Y %H:%M UTC+03:00").to_string()).unwrap_or_else(|| "bilinmiyor".into())
}

fn plain_inline(line: &str, out: &mut String) {
    let mut rest = line;
    let mut bold: Option<(&str, usize)> = None;
    while let Some(ch) = rest.chars().next() {
        if ["https://", "http://", "ftp://", "www.", "mailto:"].iter().any(|prefix| {
            rest.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        }) {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let url = &rest[..end];
            // Yalnız biçimlendirmenin tamamı URL ise çevresindeki işareti ayır.
            let url = bold.filter(|(marker, at)| at + marker.len() == out.len())
                .and_then(|(marker, _)| url.strip_suffix(marker)).unwrap_or(url);
            out.push_str(url);
            rest = &rest[url.len()..];
            continue;
        }
        if let Some(code) = rest.strip_prefix('`')
            && let Some(end) = code.find('`').filter(|end| *end > 0)
        {
            out.push_str(&code[..end]);
            rest = &code[end + 1..];
            continue;
        }
        if let Some(marker) = ["**", "__"].into_iter().find(|marker| rest.starts_with(marker)) {
            let before = line[..line.len() - rest.len()].chars().next_back();
            let after = rest[marker.len()..].chars().next();
            let word = |c: char| c.is_alphanumeric() || c == '_';
            if let Some((opening, at)) = bold
                && opening == marker
                && out.len() > at + marker.len()
                && before.is_some_and(|c| !c.is_whitespace())
                && !after.is_some_and(word)
            {
                out.replace_range(at..at + marker.len(), "");
                bold = None;
            } else {
                if !before.is_some_and(word) && after.is_some_and(|c| !c.is_whitespace() && c != ch) {
                    bold = Some((marker, out.len()));
                }
                out.push_str(marker);
            }
            rest = &rest[marker.len()..];
        } else {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
}

fn markdown_to_plain(text: &str) -> String {
    let mut out = String::new();
    let mut fence = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let trimmed = body.trim_start_matches(' ');
        let indent = body.len() - trimmed.len();
        let ticks = trimmed.bytes().take_while(|b| *b == b'`').count();
        if let Some(opening) = fence {
            if indent <= 3 && ticks >= opening && trimmed[ticks..].trim().is_empty() {
                fence = None;
            } else {
                out.push_str(line);
            }
            continue;
        }
        if indent <= 3 && ticks >= 3 && !trimmed[ticks..].contains('`') {
            fence = Some(ticks);
            continue;
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        let content = if indent <= 3 && (1..=6).contains(&hashes)
            && let Some(heading) = trimmed[hashes..].strip_prefix([' ', '\t'])
        {
            heading.trim_start_matches([' ', '\t'])
        } else if let Some(item) = trimmed.strip_prefix("* ").or_else(|| trimmed.strip_prefix("- ")) {
            out.push_str(&body[..indent]);
            out.push_str("• ");
            item
        } else {
            body
        };
        plain_inline(content, &mut out);
        out.push_str(&line[body.len()..]);
    }
    out
}

pub fn format_final(text: Option<&str>, outcome: CompletionOutcome) -> String {
    let status = match outcome {
        CompletionOutcome::Completed => "Tur tamamlandı.",
        CompletionOutcome::Cancelled => "Tur iptal edildi.",
        CompletionOutcome::QuotaExhausted => "Tur kota bekliyor.",
        CompletionOutcome::Failed => "Tur tamamlanamadı; ayrıntılar masaüstünde.",
    };
    bounded_text(&match text.filter(|s| !s.trim().is_empty()) {
        Some(text) if outcome == CompletionOutcome::Completed => {
            format!("{status}\n\n{}", markdown_to_plain(text))
        }
        _ => status.into(),
    })
}

pub fn format_limit(report: &QuotaReport) -> String {
    let mut out = format!("Kotalar · {}", istanbul_time(Some(report.fetched_at)));
    for pool in &report.pools {
        let name = match pool.pool { Pool::Claude => "Claude", Pool::Codex => "Codex", Pool::Agy => "Agy" };
        out.push_str(&format!("\n{name} · kalan reset hakkı: {}", pool.reset_rights.map(|n| n.to_string()).unwrap_or_else(|| "bilinmiyor".into())));
        if pool.windows.is_empty() { out.push_str("\n  kota: bilinmiyor · kaynak: bilinmiyor"); }
        for window in &pool.windows {
            let unknown = pool.pool == Pool::Agy || window.source == QuotaSource::Unknown;
            let remaining = window.used_pct.filter(|v| v.is_finite() && !unknown)
                .map(|v| format!("%{:.0}", (100.0 - v).clamp(0.0, 100.0))).unwrap_or_else(|| "bilinmiyor".into());
            let source = match window.source { QuotaSource::Live if !unknown => "canlı", QuotaSource::Estimated if !unknown => "tahmini", _ => "bilinmiyor" };
            out.push_str(&format!("\n  {} · kalan: {remaining} · reset: {} · kaynak: {source}", window.label,
                istanbul_time(if unknown { None } else { window.resets_at })));
        }
    }
    bounded_text(&out)
}

pub fn format_status(status: &GatewayStatusSnapshot) -> String {
    let mut out = format!("Aktif turlar: {}", status.active_turns.len());
    for turn in &status.active_turns { out.push_str(&format!("\n  {} · {:?}", turn.chat_id, turn.engine)); }
    out.push_str(&format!("\nBekleyen onaylar: {}", status.pending_approvals.len()));
    for approval in &status.pending_approvals {
        out.push_str(if approval.is_policy_tool() { "\n  Araç onayı · /onay" } else { "\n  Yerel onay · masaüstü" });
    }
    out.push_str(&format!("\nKota işleri: {}", status.quota_waits.len()));
    for wait in &status.quota_waits { out.push_str(&format!("\n  {} · {:?} · reset: {} · {:?}", wait.chat_id, wait.engine, istanbul_time(wait.resets_at), wait.reason)); }
    bounded_text(&out)
}

pub fn should_notify(mode: NotifyMode, away: bool, telegram_origin: bool, tool_approval: bool) -> bool {
    telegram_origin || (tool_approval && away) || mode == NotifyMode::Always || away
}

pub struct Outbound {
    pub message: SendMessage,
    pub logical_turn_id: Option<String>,
}

pub fn reply(chat_id: i64, text: impl Into<String>) -> Outbound {
    Outbound { message: SendMessage { chat_id, text: bounded_text(&text.into()), disable_notification: false, reply_markup: None }, logical_turn_id: None }
}

/// Canlı kanallar yalnız uyandırır. Her sohbet kalıcı seq sırasıyla okunur;
/// iki kanaldaki aynı event.id aynı kalıcı kayda dönüşür.
pub struct NotificationCursor {
    db: Arc<Db>,
    origins: Arc<dyn OriginReader>,
    positions: HashMap<String, ReplayPosition>,
    in_flight: InFlight,
}

#[derive(Default)]
struct ReplayPosition {
    after: i64,
    recipients: HashSet<i64>,
}

impl NotificationCursor {
    pub fn new(db: Arc<Db>, origins: Arc<dyn OriginReader>) -> Self {
        Self { db, origins, positions: HashMap::new(), in_flight: Arc::default() }
    }

    pub fn in_flight_handle(&self) -> InFlight { self.in_flight.clone() }

    /// Abonelikler bu çağrıdan önce kurulmalıdır. Ready finaller ayrı kalıcı
    /// durumdan okunur; eski arka plan olayları restart'ta tekrar bildirilmez.
    pub fn initialize(&mut self) -> Result<(), GatewayError> {
        for chat in self.db.list_chats(None, true).map_err(|_| GatewayError::Unavailable)? {
            let seq = self.db.last_event(&chat.id).map_err(|_| GatewayError::Unavailable)?.map_or(0, |e| e.seq);
            self.positions.insert(chat.id, ReplayPosition { after: seq, ..Default::default() });
        }
        Ok(())
    }

    /// `capacity`: kuyrukta kalan boş yer. Çıktı bunu aşmaz; imleç yalnız çıktıya
    /// alınan alıcıları kaydeder; tamamlanmayan fan-out sonraki tick'te devam eder.
    pub async fn replay(&mut self, allowed: &[i64], mode: NotifyMode, away: bool, capacity: usize) -> Result<Vec<Outbound>, GatewayError> {
        let mut output = Vec::new();
        for chat in self.db.list_chats(None, true).map_err(|_| GatewayError::Unavailable)? {
            if output.len() >= capacity { break; }
            let origins = self.origins.origins_for_chat(&chat.id).await?;
            let position = self.positions.entry(chat.id.clone()).or_default();
            // Bir sayfa/tick: büyük geçmiş poll/cancel işlemlerini aç bırakmaz.
            let page = self.db.events_for_chat_page(&chat.id, Some(position.after), 128).map_err(|_| GatewayError::Unavailable)?;
            'events: for event in page {
                if let Some((text, silent, logical_id)) = event_text(&event) {
                    for user in allowed {
                        if position.recipients.contains(user) { continue; }
                        let origin = origins.iter().find(|o| o.user_id == *user && o.private_chat_id == *user
                            && if let Some(id) = &logical_id { o.logical_turn_id == *id }
                            else { event.seq >= o.start_seq && o.end_seq.is_none_or(|end| event.seq <= end) });
                        if should_notify(mode, away, origin.is_some(), false) {
                            if output.len() == capacity { break 'events; }
                            let mut message = reply(*user, text.clone());
                            message.message.disable_notification = silent;
                            output.push(message);
                            position.recipients.insert(*user);
                        }
                    }
                }
                position.after = event.seq;
                position.recipients.clear();
            }
            for origin in origins {
                if output.len() >= capacity { break; }
                if origin.delivery_state != DeliveryState::Ready || !allowed.contains(&origin.user_id)
                    || origin.user_id != origin.private_chat_id { continue; }
                if self.in_flight.lock().map_err(|_| GatewayError::Unavailable)?.contains(&origin.logical_turn_id) { continue; }
                if let Some(message) = final_from_origin(&self.db, &origin)? {
                    self.in_flight.lock().map_err(|_| GatewayError::Unavailable)?.insert(origin.logical_turn_id);
                    output.push(message);
                }
            }
        }
        Ok(output)
    }
}

/// Yalnız kayıtlı tur aralığı ve aynı session içindeki son asistan bloğu okunur.
pub fn final_from_origin(db: &Db, origin: &TelegramTurnOrigin) -> Result<Option<Outbound>, GatewayError> {
    let Some(end) = origin.end_seq else { return Ok(None); };
    if origin.delivery_state != DeliveryState::Ready || end < origin.start_seq { return Ok(None); }
    let mut after = origin.start_seq;
    let mut text = String::new();
    let mut session = None;
    let mut outcome = CompletionOutcome::Failed;
    loop {
        let page = db.events_for_chat_page(&origin.chat_id, Some(after), 128).map_err(|_| GatewayError::Unavailable)?;
        if page.is_empty() { break; }
        let mut progressed = false;
        for event in page {
            if event.seq > end { break; }
            progressed = true;
            after = event.seq;
            if event.session_id != session { text.clear(); session = event.session_id.clone(); }
            match serde_json::from_value::<EngineEvent>(event.payload) {
                Ok(EngineEvent::AssistantText { text: chunk }) => text.push_str(&chunk),
                Ok(EngineEvent::ToolCall { .. } | EngineEvent::ToolResult { .. } | EngineEvent::Command { .. }) => text.clear(),
                Ok(EngineEvent::TurnCompleted) => outcome = CompletionOutcome::Completed,
                Ok(EngineEvent::TurnCancelled) if event.seq == end => outcome = CompletionOutcome::Cancelled,
                Ok(EngineEvent::Error { .. }) => outcome = CompletionOutcome::Failed,
                _ => {},
            }
        }
        if !progressed || after >= end { break; }
    }
    let mut message = reply(origin.private_chat_id, format_final(Some(&text), outcome));
    message.logical_turn_id = Some(origin.logical_turn_id.clone());
    Ok(Some(message))
}

fn event_text(event: &StoredEvent) -> Option<(String, bool, Option<String>)> {
    match serde_json::from_value::<EngineEvent>(event.payload.clone()).ok()? {
        EngineEvent::EngineSwitched { from, to, .. } => Some((format!("Motor değişti: {from:?} → {to:?}."), true, None)),
        EngineEvent::QuotaWaitStarted { logical_turn_id, engine, resets_at, .. } => Some((format!("{engine:?} kota bekliyor. Reset: {}", istanbul_time(resets_at)), false, Some(logical_turn_id))),
        EngineEvent::QuotaResumed { logical_turn_id, engine, .. } => Some((format!("Kota işi {engine:?} ile devam ediyor."), false, Some(logical_turn_id))),
        EngineEvent::QuotaHandoff { logical_turn_id, from, to, .. } => Some((format!("Kota işi devredildi: {from:?} → {to:?}."), true, Some(logical_turn_id))),
        EngineEvent::QuotaWaitStopped { logical_turn_id, .. } => Some(("Kota beklemesi durdu; durumu masaüstünden kontrol edin.".into(), false, Some(logical_turn_id))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ultron_core::quota::{PoolReport, QuotaWindow, BudgetMode};

    #[test]
    fn telegram_final_truncates_unicode_with_note() {
        let out = format_final(Some(&"Türkçe 🦀👩‍💻".repeat(1000)), CompletionOutcome::Completed);
        assert!(out.encode_utf16().count() <= MESSAGE_LIMIT);
        assert!(out.ends_with(TRUNCATION_NOTE));
        assert!(out.starts_with("Tur tamamlandı."));
        assert_eq!(format_final(Some(""), CompletionOutcome::Failed), "Tur tamamlanamadı; ayrıntılar masaüstünde.");
    }

    #[test]
    fn telegram_final_renders_markdown_as_plain_turkish_text() {
        let markdown = concat!(
            "# Özet\n###### Ayrıntılar\n",
            "Şu an **Claude Opus 5.5** ve __Türkçe yanıt__ hazır.\n",
            "* `snake_case` ve 2*3\n  - **`çıktı`**\n",
            "https://örnek.example/a**b**?q=__değer__ #etiket\n",
            "**Bağlantı https://örnek.example/** burada**\n",
            "```powershell\n# yorum\n- **aynen**  \n    `kod`\n```\n",
            "Son satır: snake_case ve yarım **işaret."
        );
        assert_eq!(format_final(Some(markdown), CompletionOutcome::Completed), concat!(
            "Tur tamamlandı.\n\nÖzet\nAyrıntılar\n",
            "Şu an Claude Opus 5.5 ve Türkçe yanıt hazır.\n",
            "• snake_case ve 2*3\n  • çıktı\n",
            "https://örnek.example/a**b**?q=__değer__ #etiket\n",
            "Bağlantı https://örnek.example/** burada\n",
            "# yorum\n- **aynen**  \n    `kod`\n",
            "Son satır: snake_case ve yarım **işaret."
        ));
    }

    #[test]
    fn away_does_not_suppress_command_replies() {
        assert!(!should_notify(NotifyMode::Away, false, false, false));
        assert!(should_notify(NotifyMode::Away, false, true, false));
        assert!(should_notify(NotifyMode::Away, true, false, true));
        assert!(!reply(101, "Komut yanıtı").message.disable_notification);
    }

    #[test]
    fn limit_unknown_matches_desktop() {
        let report = QuotaReport { fetched_at: Utc::now(), pools: vec![PoolReport { pool: Pool::Agy,
            windows: vec![QuotaWindow { label: "kota".into(), used_pct: Some(20.0), resets_at: Some(Utc::now()), window_secs: 0, source: QuotaSource::Unknown }],
            reset_rights: None, mode: BudgetMode::Dengeli, note: None }] };
        let text = format_limit(&report);
        assert!(text.contains("kalan: bilinmiyor"));
        assert!(text.contains("reset: bilinmiyor"));
        assert!(text.contains("kaynak: bilinmiyor"));
        assert!(text.contains("UTC+03:00"));
    }

    struct NoOrigins;
    impl OriginReader for NoOrigins {
        fn origins_for_chat<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<TelegramTurnOrigin>, GatewayError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }
    fn fixture() -> (tempfile::TempDir, Arc<Db>, ultron_core::EventLog) {
        let temp = tempfile::tempdir().unwrap();
        let paths = ultron_core::Paths::from_root(temp.path().join("synthetic"));
        paths.ensure().unwrap();
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("c1", None, "sentetik", Utc::now()).unwrap();
        let log = ultron_core::EventLog::new(db.clone(), paths);
        (temp, db, log)
    }

    fn user_event(chat: &str) -> StoredEvent {
        StoredEvent { id: uuid::Uuid::new_v4().to_string(), chat_id: chat.into(), session_id: None, seq: 0, ts: Utc::now(),
            kind: "user_text".into(), payload: serde_json::json!({"type": "user_text", "text": "sentetik hedef"}) }
    }

    #[test]
    fn origin_is_per_turn_not_per_chat() {
        let (_temp, db, log) = fixture();
        let mut ends = Vec::new();
        for (update, user, logical, text) in [(1, 101, "turn-a", "A-final"), (2, 202, "turn-b", "B-final")] {
            let event = user_event("c1");
            let ultron_core::db::telegram::TelegramAcceptance::Accepted(_) = db.accept_telegram_turn(&ultron_core::db::telegram::TelegramTurnAcceptance {
                bot_id: 100, update_id: update, user_id: user, private_chat_id: user, logical_turn_id: logical, event: &event }).unwrap()
            else { panic!("kabul bekleniyordu"); };
            log.append("c1", None, EngineEvent::AssistantText { text: text.into() }).unwrap();
            let done = log.append("c1", None, EngineEvent::TurnCompleted).unwrap();
            assert!(db.complete_telegram_turn(logical, done.seq).unwrap());
            ends.push(logical);
        }
        let a = final_from_origin(&db, &db.telegram_turn_origin("turn-a").unwrap().unwrap()).unwrap().unwrap();
        let b = final_from_origin(&db, &db.telegram_turn_origin("turn-b").unwrap().unwrap()).unwrap().unwrap();
        assert_eq!(a.message.chat_id, 101);
        assert_eq!(b.message.chat_id, 202);
        assert!(a.message.text.contains("A-final") && !a.message.text.contains("B-final"));
        assert!(b.message.text.contains("B-final") && !b.message.text.contains("A-final"));
    }

    #[tokio::test]
    async fn replay_deduplicates_and_catches_up_after_lag() {
        let (_temp, db, log) = fixture();
        let mut cursor = NotificationCursor::new(db.clone(), Arc::new(NoOrigins));
        cursor.initialize().unwrap();
        // Kanal taşması simülasyonu: canlı aboneye bakılmadan çok sayıda olay yazılır.
        for _ in 0..300 {
            log.append("c1", None, EngineEvent::EngineSwitched { from: ultron_core::EngineId::Claude, to: ultron_core::EngineId::Codex,
                reason: ultron_core::events::SwitchReason::Quota }).unwrap();
        }
        let mut total = 0;
        for _ in 0..4 {
            let batch = cursor.replay(&[101], NotifyMode::Always, false, 1000).await.unwrap();
            assert!(batch.iter().all(|m| m.message.disable_notification));
            total += batch.len();
        }
        assert_eq!(total, 300);
        // Aynı olay iki kanaldan tetiklense de kalıcı imleç ikinci gönderimi üretmez.
        assert!(cursor.replay(&[101], NotifyMode::Always, false, 1000).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn away_mode_suppresses_background_but_not_origin_notifications() {
        let (_temp, db, log) = fixture();
        let mut cursor = NotificationCursor::new(db.clone(), Arc::new(NoOrigins));
        cursor.initialize().unwrap();
        log.append("c1", None, EngineEvent::EngineSwitched { from: ultron_core::EngineId::Claude, to: ultron_core::EngineId::Codex,
            reason: ultron_core::events::SwitchReason::Quota }).unwrap();
        assert!(cursor.replay(&[101], NotifyMode::Away, false, 1000).await.unwrap().is_empty());
    }

    #[test]
    fn long_intermediate_text_gets_single_trailing_note_and_no_midstream_note() {
        let (_temp, db, log) = fixture();
        let event = user_event("c1");
        let ultron_core::db::telegram::TelegramAcceptance::Accepted(_) = db.accept_telegram_turn(&ultron_core::db::telegram::TelegramTurnAcceptance {
            bot_id: 100, update_id: 1, user_id: 101, private_chat_id: 101, logical_turn_id: "t", event: &event }).unwrap()
        else { panic!("kabul bekleniyordu"); };
        for _ in 0..4 { log.append("c1", None, EngineEvent::AssistantText { text: "a".repeat(5000) }).unwrap(); }
        let done = log.append("c1", None, EngineEvent::TurnCompleted).unwrap();
        assert!(db.complete_telegram_turn("t", done.seq).unwrap());
        let out = final_from_origin(&db, &db.telegram_turn_origin("t").unwrap().unwrap()).unwrap().unwrap();
        assert_eq!(out.message.text.matches("Yanıt kısaltıldı").count(), 1);
        assert!(out.message.text.ends_with(TRUNCATION_NOTE));
        assert!(out.message.text.encode_utf16().count() <= MESSAGE_LIMIT);
    }

    #[test]
    fn stored_final_is_converted_before_truncation() {
        let (_temp, db, log) = fixture();
        let event = user_event("c1");
        let ultron_core::db::telegram::TelegramAcceptance::Accepted(_) = db.accept_telegram_turn(&ultron_core::db::telegram::TelegramTurnAcceptance {
            bot_id: 100, update_id: 1, user_id: 101, private_chat_id: 101, logical_turn_id: "t", event: &event }).unwrap()
        else { panic!("kabul bekleniyordu"); };
        let markdown = "**ışık** ".repeat(600);
        assert!(markdown.encode_utf16().count() > MESSAGE_LIMIT);
        log.append("c1", None, EngineEvent::AssistantText { text: markdown }).unwrap();
        log.append("c1", None, EngineEvent::AssistantText { text: "__bitti__".into() }).unwrap();
        let done = log.append("c1", None, EngineEvent::TurnCompleted).unwrap();
        assert!(db.complete_telegram_turn("t", done.seq).unwrap());
        let out = final_from_origin(&db, &db.telegram_turn_origin("t").unwrap().unwrap()).unwrap().unwrap();
        assert_eq!(out.message.text, format!("Tur tamamlandı.\n\n{}bitti", "ışık ".repeat(600)));
    }

    #[test]
    fn final_without_events_in_range_terminates() {
        let (_temp, db, log) = fixture();
        log.append("c1", None, EngineEvent::TurnCompleted).unwrap();
        let origin = TelegramTurnOrigin { logical_turn_id: "t".into(), chat_id: "c1".into(), user_id: 101, private_chat_id: 101,
            user_event_seq: 0, start_seq: 0, end_seq: Some(0), delivery_state: DeliveryState::Ready };
        assert!(final_from_origin(&db, &origin).unwrap().is_some());
    }

    #[tokio::test]
    async fn replay_respects_capacity_and_keeps_cursor_for_unqueued_events() {
        let (_temp, db, log) = fixture();
        let mut cursor = NotificationCursor::new(db.clone(), Arc::new(NoOrigins));
        cursor.initialize().unwrap();
        for _ in 0..10 {
            log.append("c1", None, EngineEvent::EngineSwitched { from: ultron_core::EngineId::Claude, to: ultron_core::EngineId::Codex,
                reason: ultron_core::events::SwitchReason::Quota }).unwrap();
        }
        assert!(cursor.replay(&[101], NotifyMode::Always, false, 0).await.unwrap().is_empty());
        let mut total = 0;
        for _ in 0..5 {
            let batch = cursor.replay(&[101], NotifyMode::Always, false, 3).await.unwrap();
            assert!(batch.len() <= 3);
            total += batch.len();
        }
        assert_eq!(total, 10);
    }
}
