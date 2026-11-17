//! Tauri komutları: her biri `ultrond`'a tek istek. Servis hataları tipli olarak aynen geçer.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tauri::State;
use ultron_core::EngineId;
use ultron_core::db::{SearchHit, StoredEvent};
use ultron_core::ipc::{
    CatalogImportReport, ChatDetail, ChatInfo, PolicyArg, ProjectInfo, Request,
    Response, RouteResult,
};
use ultron_core::quota::QuotaReport;

use crate::bridge::{Bridge, BridgeError};

pub type Shared<'a> = State<'a, Arc<Bridge>>;
type Out<T> = Result<T, BridgeError>;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectsResult {
    pub projects: Vec<ProjectInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatsResult {
    pub chats: Vec<ChatInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatResult {
    pub chat: ChatInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatIdResult {
    pub chat_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<StoredEvent>,
    pub next_after_seq: Option<i64>,
    pub has_more: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResetRightsResult {
    pub pool: String,
    pub count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResult {
    pub version: String,
    pub pid: u32,
    pub running_turns: usize,
    pub default_engine: EngineId,
}

#[derive(Deserialize)]
struct ChatDetailEnvelope {
    chat: ChatDetail,
}

#[derive(Deserialize)]
struct QuotaEnvelope {
    quota: QuotaReport,
}

#[derive(Deserialize)]
struct ProjectImportEnvelope {
    report: ultron_core::ipc::ProjectImportReport,
}

/// Tür uyuşmazlıkları webview'e bozuk veri yerine `protocol` hatası olarak gider.
fn decode<T: DeserializeOwned>(data: Value) -> Out<T> {
    serde_json::from_value(data).map_err(|e| BridgeError::Protocol {
        message: format!("servis yanıtı beklenen biçimde değil: {e}"),
    })
}

async fn request<T: DeserializeOwned>(bridge: &Bridge, req: Request) -> Out<T> {
    decode(bridge.request(req).await?)
}

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
    #[serde(default)]
    pub elevated: bool,
}

#[cfg(test)]
mod send_tests {
    use super::*;

    #[test]
    fn desktop_send_defaults_to_workspace_and_accepts_explicit_elevation() {
        let mut value = serde_json::json!({"text": "sentetik"});
        let legacy: SendArgs = serde_json::from_value(value.clone()).unwrap();
        assert!(!legacy.elevated);
        value["elevated"] = true.into();
        assert!(serde_json::from_value::<SendArgs>(value).unwrap().elevated);
    }
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
) -> Out<EventsPage> {
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
    Ok(EventsPage {
        events,
        next_after_seq,
        has_more,
    })
}

fn proto(e: anyhow::Error) -> BridgeError {
    BridgeError::Protocol {
        message: format!("{e:#}"),
    }
}

#[tauri::command]
pub async fn list_projects(b: Shared<'_>) -> Out<ProjectsResult> {
    list_projects_impl(&b).await
}

pub async fn list_projects_impl(bridge: &Bridge) -> Out<ProjectsResult> {
    request(bridge, Request::ListProjects).await
}

#[tauri::command]
pub async fn list_chats(b: Shared<'_>, args: ListChatsArgs) -> Out<ChatsResult> {
    list_chats_impl(&b, args).await
}

pub async fn list_chats_impl(bridge: &Bridge, args: ListChatsArgs) -> Out<ChatsResult> {
    request(
        bridge,
        Request::ListChats {
            project_id: args.project_id,
            include_archived: args.include_archived,
        },
    )
    .await
}

/// `GetChat`: servis `{chat: ChatDetail}` döner; webview doğrudan `ChatDetail` alır.
pub async fn read_chat_detail(bridge: &Bridge, chat_id: &str) -> Out<ChatDetail> {
    let data: ChatDetailEnvelope = request(
        bridge,
        Request::GetChat {
            chat_id: chat_id.to_string(),
        },
    )
    .await?;
    Ok(data.chat)
}

#[tauri::command]
pub async fn get_chat(b: Shared<'_>, args: ChatIdArgs) -> Out<ChatDetail> {
    get_chat_impl(&b, args).await
}

pub async fn get_chat_impl(bridge: &Bridge, args: ChatIdArgs) -> Out<ChatDetail> {
    read_chat_detail(bridge, &args.chat_id).await
}

#[tauri::command]
pub async fn update_chat(b: Shared<'_>, args: UpdateChatArgs) -> Out<ChatResult> {
    update_chat_impl(&b, args).await
}

pub async fn update_chat_impl(bridge: &Bridge, args: UpdateChatArgs) -> Out<ChatResult> {
    request(
        bridge,
        Request::UpdateChat {
            chat_id: args.chat_id,
            title: args.title,
            pinned: args.pinned,
            archived: args.archived,
            project_id: args.project_id,
        },
    )
    .await
}

#[tauri::command]
pub async fn send_message(b: Shared<'_>, args: SendArgs) -> Out<ChatIdResult> {
    send_message_impl(&b, args).await
}

pub async fn send_message_impl(bridge: &Bridge, args: SendArgs) -> Out<ChatIdResult> {
    decode(
        bridge
            .request_detached(Request::Send {
                chat_id: args.chat_id,
                cwd: default_cwd(),
                engine: args.engine,
                text: args.text,
                policy: PolicyArg::Workspace,
                elevated: args.elevated,
                project_id: args.project_id,
            })
            .await?,
    )
}

#[tauri::command]
pub async fn switch_engine(b: Shared<'_>, args: SwitchArgs) -> Out<ChatIdResult> {
    switch_engine_impl(&b, args).await
}

pub async fn switch_engine_impl(bridge: &Bridge, args: SwitchArgs) -> Out<ChatIdResult> {
    request(
        bridge,
        Request::Switch {
            chat_id: args.chat_id,
            to: args.to,
        },
    )
    .await
}

#[tauri::command]
pub async fn cancel_turn(b: Shared<'_>, args: ChatIdArgs) -> Out<ChatIdResult> {
    cancel_turn_impl(&b, args).await
}

pub async fn cancel_turn_impl(bridge: &Bridge, args: ChatIdArgs) -> Out<ChatIdResult> {
    request(
        bridge,
        Request::Cancel {
            chat_id: args.chat_id,
        },
    )
    .await
}

#[tauri::command]
pub async fn events_page(b: Shared<'_>, args: EventsPageArgs) -> Out<EventsPage> {
    events_page_impl(&b, args).await
}

pub async fn events_page_impl(bridge: &Bridge, args: EventsPageArgs) -> Out<EventsPage> {
    read_events_page(bridge, &args.chat_id, args.after_seq, args.limit).await
}

#[tauri::command]
pub async fn history_search(b: Shared<'_>, args: SearchArgs) -> Out<SearchResult> {
    history_search_impl(&b, args).await
}

pub async fn history_search_impl(bridge: &Bridge, args: SearchArgs) -> Out<SearchResult> {
    request(
        bridge,
        Request::HistorySearch {
            chat_id: args.chat_id,
            query: args.query,
            limit: args.limit,
        },
    )
    .await
}

#[tauri::command]
pub async fn get_quota(b: Shared<'_>) -> Out<QuotaReport> {
    get_quota_impl(&b).await
}

pub async fn get_quota_impl(bridge: &Bridge) -> Out<QuotaReport> {
    let data: QuotaEnvelope = request(bridge, Request::GetQuota).await?;
    Ok(data.quota)
}

#[tauri::command]
pub async fn set_reset_rights(b: Shared<'_>, args: ResetRightsArgs) -> Out<ResetRightsResult> {
    set_reset_rights_impl(&b, args).await
}

pub async fn set_reset_rights_impl(
    bridge: &Bridge,
    args: ResetRightsArgs,
) -> Out<ResetRightsResult> {
    request(
        bridge,
        Request::SetResetRights {
            pool: args.pool,
            count: args.count,
        },
    )
    .await
}

#[tauri::command]
pub async fn route_message(b: Shared<'_>, args: TextArgs) -> Out<RouteResult> {
    route_message_impl(&b, args).await
}

pub async fn route_message_impl(bridge: &Bridge, args: TextArgs) -> Out<RouteResult> {
    request(bridge, Request::Route { text: args.text }).await
}

#[tauri::command]
pub async fn import_catalog(b: Shared<'_>, args: PathArgs) -> Out<CatalogImportReport> {
    import_catalog_impl(&b, args).await
}

pub async fn import_catalog_impl(bridge: &Bridge, args: PathArgs) -> Out<CatalogImportReport> {
    request(bridge, Request::ImportCatalog { path: args.path }).await
}

#[tauri::command]
pub async fn import_projects(b: Shared<'_>) -> Out<ultron_core::ipc::ProjectImportReport> {
    import_projects_impl(&b).await
}

/// `ImportProjects`: projeleri bulur ve geçmiş notlarını yazar; sohbet içe aktarmaz.
pub async fn import_projects_impl(bridge: &Bridge) -> Out<ultron_core::ipc::ProjectImportReport> {
    let data: ProjectImportEnvelope = request(
        bridge,
        Request::ImportProjects {
            dry_run: false,
            home: None,
        },
    )
    .await?;
    Ok(data.report)
}

#[tauri::command]
pub async fn status(b: Shared<'_>) -> Out<StatusResult> {
    status_impl(&b).await
}

pub async fn status_impl(bridge: &Bridge) -> Out<StatusResult> {
    request(bridge, Request::Status).await
}
