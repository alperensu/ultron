//! Tauri komutları: her biri `ultrond`'a tek istek. Servis hataları tipli olarak aynen geçer.

use std::sync::Arc;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tauri::State;
use ultron_core::EngineId;
use ultron_core::ipc::{PolicyArg, Request, Response};

use crate::bridge::{Bridge, BridgeError};

pub type Shared<'a> = State<'a, Arc<Bridge>>;
type Out = Result<Value, BridgeError>;

/// `events_page` için sayfa üst sınırı.
const MAX_PAGE: usize = 1000;

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
pub struct ListChatsArgs {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Deserialize)]
pub struct ChatIdArgs {
    pub chat_id: String,
}

#[derive(Deserialize)]
pub struct UpdateChatArgs {
    pub chat_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub archived: Option<bool>,
    /// Alan yok: dokunma; `null`: projeden ayır; metin: projeye bağla.
    #[serde(default, deserialize_with = "double_option")]
    pub project_id: Option<Option<String>>,
}

#[derive(Deserialize)]
pub struct SendArgs {
    #[serde(default)]
    pub chat_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub engine: Option<EngineId>,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Deserialize)]
pub struct SwitchArgs {
    pub chat_id: String,
    pub to: EngineId,
}

#[derive(Deserialize)]
pub struct EventsPageArgs {
    pub chat_id: String,
    #[serde(default)]
    pub after_seq: Option<i64>,
    pub limit: usize,
}

#[derive(Deserialize)]
pub struct SearchArgs {
    #[serde(default)]
    pub chat_id: Option<String>,
    pub query: String,
    pub limit: usize,
}

#[derive(Deserialize)]
pub struct ResetRightsArgs {
    pub pool: String,
    pub count: u32,
}

#[derive(Deserialize)]
pub struct TextArgs {
    pub text: String,
}

#[derive(Deserialize)]
pub struct PathArgs {
    pub path: String,
}

#[derive(Deserialize)]
pub struct HistoryArgs {
    #[serde(default)]
    pub root: Option<String>,
}

/// Projesiz yeni sohbetin çalışma dizini: kullanıcı ev dizini, yoksa geçerli dizin.
fn default_cwd() -> String {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `Events` (follow yok) yanıtından en fazla `limit` olay okur.
pub async fn read_events_page(
    bridge: &Bridge,
    chat_id: &str,
    after_seq: Option<i64>,
    limit: usize,
) -> Out {
    let limit = limit.clamp(1, MAX_PAGE);
    let mut client = bridge.connect().await?;
    client
        .send(&Request::Events {
            chat_id: Some(chat_id.to_string()),
            follow: false,
            after_seq,
            // Bir fazla iste: `has_more`'u sayfa sonrası olaydan anlarız, akış temiz kapanır.
            limit: Some(u32::try_from(limit + 1).unwrap_or(u32::MAX)),
        })
        .await
        .map_err(proto)?;
    let mut events = Vec::new();
    let mut has_more = false;
    loop {
        match client.next().await.map_err(proto)? {
            Some(Response::Event(ev)) => {
                if events.len() == limit {
                    has_more = true;
                    break;
                }
                events.push(ev);
            }
            Some(Response::Ok { .. }) | None => break,
            Some(Response::Error { message }) => return Err(BridgeError::Daemon { message }),
        }
    }
    let next_after_seq = events.last().map(|e| e.seq).or(after_seq);
    Ok(json!({"events": events, "next_after_seq": next_after_seq, "has_more": has_more}))
}

fn proto(e: anyhow::Error) -> BridgeError {
    BridgeError::Protocol {
        message: format!("{e:#}"),
    }
}

#[tauri::command]
pub async fn list_projects(b: Shared<'_>) -> Out {
    b.request(Request::ListProjects).await
}

#[tauri::command]
pub async fn list_chats(b: Shared<'_>, args: ListChatsArgs) -> Out {
    b.request(Request::ListChats {
        project_id: args.project_id,
        include_archived: args.include_archived,
    })
    .await
}

/// Servis zarfındaki (`{"<anahtar>": ...}`) iç değeri çıkarır; zarf yoksa tipli protokol hatası.
fn unwrap_field(mut data: Value, key: &str) -> Out {
    data.get_mut(key)
        .map(Value::take)
        .ok_or_else(|| BridgeError::Protocol {
            message: format!("yanıtta `{key}` alanı yok"),
        })
}

/// `GetChat`: servis `{chat: ChatDetail}` döner; webview doğrudan `ChatDetail` alır.
pub async fn read_chat_detail(bridge: &Bridge, chat_id: &str) -> Out {
    let data = bridge
        .request(Request::GetChat {
            chat_id: chat_id.to_string(),
        })
        .await?;
    unwrap_field(data, "chat")
}

/// `ImportClaudeHistory`: servis `{report: ImportReport}` döner; webview doğrudan raporu alır.
pub async fn read_history_import(bridge: &Bridge, root: Option<String>) -> Out {
    let data = bridge.request(Request::ImportClaudeHistory { root }).await?;
    unwrap_field(data, "report")
}

#[tauri::command]
pub async fn get_chat(b: Shared<'_>, args: ChatIdArgs) -> Out {
    read_chat_detail(&b, &args.chat_id).await
}

#[tauri::command]
pub async fn update_chat(b: Shared<'_>, args: UpdateChatArgs) -> Out {
    b.request(Request::UpdateChat {
        chat_id: args.chat_id,
        title: args.title,
        pinned: args.pinned,
        archived: args.archived,
        project_id: args.project_id,
    })
    .await
}

#[tauri::command]
pub async fn send_message(b: Shared<'_>, args: SendArgs) -> Out {
    b.request_detached(Request::Send {
        chat_id: args.chat_id,
        cwd: default_cwd(),
        engine: args.engine,
        text: args.text,
        policy: PolicyArg::Workspace,
        project_id: args.project_id,
    })
    .await
}

#[tauri::command]
pub async fn switch_engine(b: Shared<'_>, args: SwitchArgs) -> Out {
    b.request(Request::Switch {
        chat_id: args.chat_id,
        to: args.to,
    })
    .await
}

#[tauri::command]
pub async fn cancel_turn(b: Shared<'_>, args: ChatIdArgs) -> Out {
    b.request(Request::Cancel {
        chat_id: args.chat_id,
    })
    .await
}

#[tauri::command]
pub async fn events_page(b: Shared<'_>, args: EventsPageArgs) -> Out {
    read_events_page(&b, &args.chat_id, args.after_seq, args.limit).await
}

#[tauri::command]
pub async fn history_search(b: Shared<'_>, args: SearchArgs) -> Out {
    b.request(Request::HistorySearch {
        chat_id: args.chat_id,
        query: args.query,
        limit: args.limit,
    })
    .await
}

#[tauri::command]
pub async fn get_quota(b: Shared<'_>) -> Out {
    b.request(Request::GetQuota).await
}

#[tauri::command]
pub async fn set_reset_rights(b: Shared<'_>, args: ResetRightsArgs) -> Out {
    b.request(Request::SetResetRights {
        pool: args.pool,
        count: args.count,
    })
    .await
}

#[tauri::command]
pub async fn route_message(b: Shared<'_>, args: TextArgs) -> Out {
    b.request(Request::Route { text: args.text }).await
}

#[tauri::command]
pub async fn import_catalog(b: Shared<'_>, args: PathArgs) -> Out {
    b.request(Request::ImportCatalog { path: args.path }).await
}

#[tauri::command]
pub async fn import_claude_history(b: Shared<'_>, args: HistoryArgs) -> Out {
    read_history_import(&b, args.root).await
}

#[tauri::command]
pub async fn status(b: Shared<'_>) -> Out {
    b.request(Request::Status).await
}
