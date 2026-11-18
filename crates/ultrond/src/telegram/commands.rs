//! Dar komut sözlüğü; hiçbir komut shell/IPC vekili değildir.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ultron_core::Db;
use ultron_core::ipc::PolicyArg;
use super::contracts::*;
use super::notifications::{Outbound, reply, format_limit, format_status};

pub const HELP: &str = "Komutlar: /yeni, /sohbetler, /durum, /limit, /onay. Metin göndererek normal bir tur başlatabilirsiniz.";
pub const BUSY: &str = "Tur sürüyor; tamamlandıktan sonra yeniden gönderin.";
pub const SELECTION_PREFIX: &str = "sel:";
const SELECTION_TTL: Duration = Duration::from_secs(300);
const MAX_SELECTIONS: usize = 128;
const LIMIT_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(PartialEq, Eq)]
pub enum Command<'a> { New, Chats, Status, Limit, Approvals, Help, Text(&'a str) }

pub fn parse_command(text: &str) -> Command<'_> {
    let text = text.trim();
    if !text.starts_with('/') { return Command::Text(text); }
    // Argüman ve başka bot adı kabul edilmez; bilinmeyen slash modele gitmez.
    match text {
        "/yeni" => Command::New, "/sohbetler" => Command::Chats,
        "/durum" => Command::Status, "/limit" => Command::Limit,
        "/onay" => Command::Approvals, _ => Command::Help,
    }
}

pub fn supported_text(message: &TelegramMessage) -> bool {
    // Telegram'ın zararsız taşıma alanları dışında her ek alan fail-closed'dur.
    message.text.is_some() && message.extra.keys().all(|key| matches!(key.as_str(), "date" | "entities"))
}

enum Choice { Chat(String), Project { id: Option<String>, text: String } }
struct Selection { user: i64, chat: i64, expires: Instant, choice: Choice }

#[derive(Default)]
pub struct CommandResult {
    pub outbound: Vec<Outbound>,
    pub accepted: Option<AcceptedTurn>,
    pub show_approvals: bool,
}

pub struct CommandProcessor {
    db: Arc<Db>,
    app: Arc<dyn GatewayApplication>,
    clock: Arc<dyn Clock>,
    selections: HashMap<String, Selection>,
    limit_until: HashMap<i64, Instant>,
}

impl CommandProcessor {
    pub fn new(db: Arc<Db>, app: Arc<dyn GatewayApplication>, clock: Arc<dyn Clock>) -> Self {
        Self { db, app, clock, selections: HashMap::new(), limit_until: HashMap::new() }
    }

    fn clear_choices(&mut self, user: i64) { self.selections.retain(|_, choice| choice.user != user); }

    fn accept_command(&self, bot: i64, update: i64, user: i64) -> Result<bool, GatewayError> {
        let first = self.db.accept_telegram_update(bot, update, user).map_err(|_| GatewayError::Unavailable)?;
        if first { self.db.start_telegram_inbox(bot, update).map_err(|_| GatewayError::Unavailable)?; }
        Ok(first)
    }

    fn finish_command(&self, bot: i64, update: i64) -> Result<(), GatewayError> {
        self.db.complete_telegram_update(bot, update).map_err(|_| GatewayError::Unavailable)?;
        Ok(())
    }

    pub async fn message(&mut self, bot: i64, update: i64, message: &TelegramMessage, busy: bool) -> Result<CommandResult, GatewayError> {
        let user = message.from.as_ref().ok_or(GatewayError::Unauthorized)?.id;
        let chat = message.chat.id;
        if self.db.telegram_inbox_entry(bot, update).map_err(|_| GatewayError::Unavailable)?.is_some()
            || self.db.telegram_next_update_id(bot).map_err(|_| GatewayError::Unavailable)?.is_some_and(|next| update < next) {
            return Ok(CommandResult::default());
        }
        let command = if supported_text(message) { parse_command(message.text.as_deref().unwrap_or_default()) } else { Command::Help };
        if let Command::Text(text) = command && !text.is_empty() && !busy {
            self.clear_choices(user);
            let binding = self.app.get_binding(user).await?;
            if let Some(id) = binding {
                return self.submit(bot, update, user, Some(id), None, text.to_string()).await;
            }
            let last = self.db.last_active_project().map_err(|_| GatewayError::Unavailable)?;
            let route = self.app.route(text, last.as_deref()).await?;
            if route.confidence >= 0.6 || route.alternatives.is_empty() {
                return self.submit(bot, update, user, None, route.project_id, text.to_string()).await;
            }
            if !self.accept_command(bot, update, user)? { return Ok(CommandResult::default()); }
            let mut options = Vec::new();
            let mut projects = route.alternatives;
            if let Some(id) = route.project_id { projects.insert(0, id); }
            let mut seen = std::collections::HashSet::new();
            for id in projects.into_iter().filter(|id| seen.insert(id.clone())).take(10) {
                if let Some(project) = self.db.get_project(&id).map_err(|_| GatewayError::Unavailable)? {
                    options.push((project.name, Choice::Project { id: Some(id), text: text.to_string() }));
                }
            }
            options.push(("Projesiz".into(), Choice::Project { id: None, text: text.to_string() }));
            let result = self.choices(user, chat, "Hangi projede devam edilsin?", options);
            self.finish_command(bot, update)?;
            return Ok(result);
        }
        if !self.accept_command(bot, update, user)? { return Ok(CommandResult::default()); }
        let result = async {
            if !supported_text(message) {
                return Ok(single(chat, "Bu içerik desteklenmiyor. Yalnız düz metin gönderin."));
            }
            match command {
                Command::New => {
                    self.clear_choices(user);
                    self.app.set_binding(user, None).await?;
                    Ok(single(chat, "Bağlama temizlendi. Sonraki metin yeni sohbet açacak."))
                }
                Command::Chats => {
                    self.clear_choices(user);
                    let mut chats = self.app.list_chats().await?;
                    chats.retain(|c| !c.archived);
                    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                    let options = chats.into_iter().take(10).map(|c| (c.title, Choice::Chat(c.id))).collect::<Vec<_>>();
                    if options.is_empty() { Ok(single(chat, "Arşivlenmemiş sohbet yok.")) }
                    else { Ok(self.choices(user, chat, "Bir sohbet seçin:", options)) }
                }
                Command::Status => Ok(single(chat, format_status(&self.app.status_snapshot().await?))),
                Command::Limit => {
                    let now = self.clock.now_monotonic();
                    self.limit_until.retain(|_, until| *until > now);
                    if let std::collections::hash_map::Entry::Vacant(entry) = self.limit_until.entry(user) {
                        entry.insert(now + LIMIT_COOLDOWN);
                        Ok(single(chat, format_limit(&self.app.quota_report().await?)))
                    } else {
                        Ok(single(chat, "Kota sorgusunu yinelemek için lütfen bekleyin."))
                    }
                }
                Command::Approvals => Ok(CommandResult { show_approvals: true, ..Default::default() }),
                Command::Text(_) => Ok(single(chat, if busy { BUSY } else { "Boş metin gönderilemez." })),
                Command::Help => Ok(single(chat, HELP)),
            }
        }.await;
        self.finish_command(bot, update)?;
        Ok(result.unwrap_or_else(|error| single(chat, error_text(error))))
    }

    fn choices(&mut self, user: i64, chat: i64, title: &str, options: Vec<(String, Choice)>) -> CommandResult {
        let now = self.clock.now_monotonic();
        self.selections.retain(|_, choice| choice.expires > now);
        if self.selections.len() + options.len() > MAX_SELECTIONS { return single(chat, "Seçim kapasitesi dolu; biraz sonra yeniden deneyin."); }
        let mut rows = Vec::new();
        for (label, choice) in options {
            let nonce = random_nonce();
            self.selections.insert(nonce.clone(), Selection { user, chat, expires: now + SELECTION_TTL, choice });
            rows.push(vec![InlineKeyboardButton { text: super::notifications::bounded_text(&label).chars().take(80).collect(), callback_data: format!("{SELECTION_PREFIX}{nonce}") }]);
        }
        let mut message = reply(chat, title);
        message.message.reply_markup = Some(InlineKeyboardMarkup { inline_keyboard: rows });
        CommandResult { outbound: vec![message], ..Default::default() }
    }

    /// None, callback'in araç onayı namespace'ine ait olabileceğini belirtir.
    pub async fn selection(&mut self, bot: i64, update: i64, callback: &TelegramCallbackQuery, busy: bool)
        -> Result<Option<CommandResult>, GatewayError> {
        let Some(nonce) = callback.data.as_deref().and_then(|data| data.strip_prefix(SELECTION_PREFIX)) else { return Ok(None); };
        let user = callback.from.id;
        let chat = callback.message.as_ref().ok_or(GatewayError::Unauthorized)?.chat.id;
        let valid = self.selections.get(nonce).is_some_and(|s| s.user == user && s.chat == chat && s.expires > self.clock.now_monotonic());
        if !valid {
            if self.accept_command(bot, update, user)? { self.finish_command(bot, update)?; }
            return Ok(Some(single(chat, "Seçim eskidi. Komutu veya metni yeniden gönderin.")));
        }
        let choice = self.selections.remove(nonce).ok_or(GatewayError::InvalidInput)?.choice;
        self.clear_choices(user);
        if let Choice::Project { id, text } = choice {
            if busy {
                if self.accept_command(bot, update, user)? { self.finish_command(bot, update)?; }
                return Ok(Some(single(chat, BUSY)));
            }
            if let Some(id) = &id && self.db.get_project(id).map_err(|_| GatewayError::Unavailable)?.is_none() {
                if self.accept_command(bot, update, user)? { self.finish_command(bot, update)?; }
                return Ok(Some(single(chat, "Proje artık yok; metni yeniden gönderin.")));
            }
            return self.submit(bot, update, user, None, id, text).await.map(Some);
        }
        if !self.accept_command(bot, update, user)? { return Ok(Some(CommandResult::default())); }
        let Choice::Chat(id) = choice else { unreachable!() };
        let result = self.app.set_binding(user, Some(&id)).await;
        self.finish_command(bot, update)?;
        Ok(Some(single(chat, if result.is_ok() { "Sohbet bağlandı." } else { "Sohbet artık kullanılamıyor; /sohbetler ile yeniden seçin." })))
    }

    async fn submit(&self, bot: i64, update: i64, user: i64, chat_id: Option<String>, project_id: Option<String>, text: String) -> Result<CommandResult, GatewayError> {
        let request = SubmitUserMessage { chat_id, project_id, cwd: PathBuf::new(), engine: None, text,
            policy: PolicyArg::Workspace, elevated: false, origin: TurnOrigin::Telegram { user_id: user, private_chat_id: user, update_id: update } };
        match self.app.submit_user_message(request).await {
            Ok(accepted) => Ok(CommandResult { accepted: Some(accepted), ..Default::default() }),
            Err(GatewayError::StaleGeneration) => Ok(CommandResult::default()),
            Err(error) => {
                if !self.accept_command(bot, update, user)? { return Ok(CommandResult::default()); }
                self.finish_command(bot, update)?;
                Ok(single(user, error_text(error)))
            }
        }
    }
}

fn single(chat: i64, text: impl Into<String>) -> CommandResult { CommandResult { outbound: vec![reply(chat, text)], ..Default::default() } }
pub fn error_text(error: GatewayError) -> &'static str {
    match error { GatewayError::Busy => BUSY, GatewayError::NotFound => "Sohbet veya proje bulunamadı.",
        GatewayError::NeedsAttention => "İşlem dikkat bekliyor; masaüstünden kontrol edin.",
        _ => "İşlem şu anda tamamlanamıyor; yeniden deneyin." }
}

/// İki bağımsız UUID'nin sabit version/variant bitleri dışındaki 64'er rastgele
/// biti seçilir: 128 bit CSPRNG, ham sohbet/proje kimliği taşımaz.
fn random_nonce() -> String {
    let mut output = String::with_capacity(32);
    for uuid in [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()] {
        for byte in uuid.as_bytes()[..6].iter().chain(&uuid.as_bytes()[14..]) {
            use std::fmt::Write;
            let _ = write!(output, "{byte:02x}");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slash_arguments_and_unknown_commands_never_reach_model() {
        for text in ["/sil x", "/yeni && echo x", "/durum@other", "/onay abc", "/full"] { assert!(parse_command(text) == Command::Help); }
        assert!(parse_command("normal metin") == Command::Text("normal metin"));
        let a = random_nonce(); let b = random_nonce();
        assert_eq!(a.len(), 32); assert_ne!(a, b);
    }
    #[test]
    fn captions_attachments_and_forwarded_text_are_not_commands() {
        for extra in [serde_json::json!({"photo": [], "caption": "/yeni"}), serde_json::json!({"voice": {}}), serde_json::json!({"document": {}}), serde_json::json!({"forward_origin": {}})] {
            let mut value = serde_json::json!({"message_id": 1, "from": {"id": 101, "is_bot": false}, "chat": {"id":101,"type":"private"}, "text":"/yeni"});
            value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(!supported_text(&serde_json::from_value(value).unwrap()));
        }
    }
}
