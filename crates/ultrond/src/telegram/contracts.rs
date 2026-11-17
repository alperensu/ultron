//! Şeritler arası sözleşmeler; burada ağ, depolama veya ürün davranışı uygulanmaz.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use ultron_core::config::EngineId;
use ultron_core::db::approvals::{Approval, ApprovalStatus};
use ultron_core::events::QuotaWaitReason;
use ultron_core::ipc::{ChatInfo, PolicyArg, RouteResult, TelegramErrorCode};
use ultron_core::quota::QuotaReport;

use super::credentials::SecretToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Ham sağlayıcı yanıtı, URL veya serbest hata metni taşımaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportError {
    pub code: TelegramErrorCode,
    pub retry_after_seconds: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    Unavailable { os_code: Option<u32> },
    InvalidToken,
}

pub trait CredentialStore: Send + Sync {
    fn read_token(&self) -> BoxFuture<'_, Result<Option<SecretToken>, CredentialError>>;
    fn write_token<'a>(&'a self, token: &'a SecretToken)
        -> BoxFuture<'a, Result<(), CredentialError>>;
}

// Gelen kimlikler ve mesajlar Debug ile loglanmaz. Ek alanlar kaybolmaz:
// command/auth katmanı desteklenmeyen ekleri ve düzenlenen mesajları reddeder.
#[derive(Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    pub is_bot: bool,
}

#[derive(Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    pub from: TelegramUser,
    pub message: Option<TelegramMessage>,
    pub inline_message_id: Option<String>,
    pub data: Option<String>,
}

#[derive(Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    pub callback_query: Option<TelegramCallbackQuery>,
    pub edited_message: Option<TelegramMessage>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    pub callback_data: String,
}

#[derive(Serialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// parse_mode alanı bilerek yoktur; metin düz gönderilir.
#[derive(Serialize)]
pub struct SendMessage {
    pub chat_id: i64,
    pub text: String,
    pub disable_notification: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Serialize)]
pub struct EditMessageText {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
    /// Boş klavye, karar sonrası düğmeleri kaldırır.
    pub reply_markup: InlineKeyboardMarkup,
}

#[derive(Serialize)]
pub struct AnswerCallbackQuery {
    pub callback_query_id: String,
    pub text: String,
}

#[derive(Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

pub trait TelegramTransport: Send + Sync {
    fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>>;
    /// Uygulama timeout=25 ve yalnız message/callback_query ister.
    fn get_updates(&self, offset: Option<i64>)
        -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>>;
    fn send_message(&self, message: SendMessage)
        -> BoxFuture<'_, Result<SentMessage, TransportError>>;
    fn edit_message_text(&self, message: EditMessageText)
        -> BoxFuture<'_, Result<(), TransportError>>;
    fn answer_callback_query(&self, answer: AnswerCallbackQuery)
        -> BoxFuture<'_, Result<(), TransportError>>;
}

/// UTC kalıcı reset için, monotonic saat heartbeat/nonce için kullanılır.
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> DateTime<Utc>;
    fn now_monotonic(&self) -> Instant;
    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()>;
}

/// Yalnız üçlü kimlik filtresinden geçen daemon içi kaynak; IPC DTO'su değildir.
/// Debug/Serialize/Deserialize uygulanmaz.
pub enum TurnOrigin {
    Local,
    Telegram { user_id: i64, private_chat_id: i64, update_id: i64 },
}

pub struct SubmitUserMessage {
    pub chat_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: PathBuf,
    pub engine: Option<EngineId>,
    pub text: String,
    pub policy: PolicyArg,
    pub elevated: bool,
    pub origin: TurnOrigin,
}

/// Depo kaydı yüklenip generation/checkpoint/policy doğrulanır; eski grant taşınmaz.
pub struct ResumeQuotaStep {
    pub wait_id: String,
    pub generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayError {
    Busy,
    Unauthorized,
    InvalidInput,
    NotFound,
    StaleGeneration,
    NeedsAttention,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    Completed,
    Cancelled,
    QuotaExhausted,
    Failed,
}

pub struct TurnCompletion {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub start_seq: i64,
    pub end_seq: i64,
    pub outcome: CompletionOutcome,
    /// Yalnız bu turun final asistan metni; tool/stderr/Unknown içermez.
    pub assistant_text: Option<String>,
}

pub struct AcceptedTurn {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub user_event_seq: i64,
    pub completion: oneshot::Receiver<Result<TurnCompletion, GatewayError>>,
}

pub struct ActiveTurnSnapshot {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub engine: EngineId,
}

pub struct QuotaWaitSnapshot {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub engine: EngineId,
    pub resets_at: Option<DateTime<Utc>>,
    pub reason: QuotaWaitReason,
}

pub struct GatewayStatusSnapshot {
    pub active_turns: Vec<ActiveTurnSnapshot>,
    pub pending_approvals: Vec<Approval>,
    pub quota_waits: Vec<QuotaWaitSnapshot>,
    pub away: bool,
}

/// Telegram'a idari IPC, token/ayar yazımı veya Full yükseltme yetkisi vermez.
/// Uygulama bu sınırda tekrar yetki/allowlist/tool-kind denetimi yapar.
pub trait GatewayApplication: Send + Sync {
    fn submit_user_message(&self, request: SubmitUserMessage)
        -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>>;
    fn resume_quota_step(&self, request: ResumeQuotaStep)
        -> BoxFuture<'_, Result<AcceptedTurn, GatewayError>>;
    fn resolve_tool_approval<'a>(&'a self, origin: TurnOrigin, approval_id: &'a str, approve: bool)
        -> BoxFuture<'a, Result<ApprovalStatus, GatewayError>>;
    fn status_snapshot(&self) -> BoxFuture<'_, Result<GatewayStatusSnapshot, GatewayError>>;
    fn list_chats(&self) -> BoxFuture<'_, Result<Vec<ChatInfo>, GatewayError>>;
    fn route<'a>(&'a self, text: &'a str, last_active_project: Option<&'a str>)
        -> BoxFuture<'a, Result<RouteResult, GatewayError>>;
    fn quota_report(&self) -> BoxFuture<'_, Result<QuotaReport, GatewayError>>;
    fn get_binding(&self, user_id: i64) -> BoxFuture<'_, Result<Option<String>, GatewayError>>;
    fn set_binding<'a>(&'a self, user_id: i64, chat_id: Option<&'a str>)
        -> BoxFuture<'a, Result<(), GatewayError>>;
}
