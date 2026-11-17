//! IPC protokolü: satır başına bir JSON, Windows named pipe üzerinden.
//!
//! Güvenlik: pipe yalnız mevcut kullanıcı SID'ine açık bir DACL ile ve uzak istemciler
//! reddedilerek oluşturulur; her bağlantı `run\token` dosyasındaki rastgele token'ı
//! `Hello` ile göstermek zorundadır; tek satır en fazla [`MAX_LINE_BYTES`] bayttır.
//! İstemci, bağlandığı pipe sunucusunun aynı kullanıcıya ait olduğunu doğrulamadan token göndermez.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::config::EngineId;
use crate::db::StoredEvent;
use crate::engine::ToolPolicy;
use crate::paths::Paths;

/// Tek bir JSON satırının üst sınırı (1 MiB).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// İstemci tarafında kabul edilen yanıt satırı üst sınırı.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Araç politikası (tel biçimi).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyArg {
    ReadOnly,
    Workspace,
    Full,
}

impl From<PolicyArg> for ToolPolicy {
    fn from(p: PolicyArg) -> ToolPolicy {
        match p {
            PolicyArg::ReadOnly => ToolPolicy::ReadOnly,
            PolicyArg::Workspace => ToolPolicy::Workspace,
            PolicyArg::Full => ToolPolicy::Full,
        }
    }
}

/// İstemciden servise istek.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        token: String,
    },
    Send {
        chat_id: Option<String>,
        cwd: String,
        engine: Option<EngineId>,
        text: String,
        policy: PolicyArg,
        /// Bu tur için Full politikası ister; yetki ancak onaylayıcı kararıyla verilir.
        #[serde(default)]
        elevated: bool,
        /// Yeni sohbeti bu projeye bağlar; `cwd` projenin yolu olur.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    Switch {
        chat_id: String,
        to: EngineId,
    },
    Cancel {
        chat_id: String,
    },
    Events {
        chat_id: Option<String>,
        follow: bool,
        after_seq: Option<i64>,
        /// Yalnız `follow: false` ve sohbetli istekte: en fazla bu kadar olay gönderip `done` der.
        #[serde(default)]
        limit: Option<u32>,
    },
    HistorySearch {
        chat_id: Option<String>,
        query: String,
        limit: usize,
    },
    Status,
    ListProjects,
    ListChats {
        project_id: Option<String>,
        include_archived: bool,
    },
    GetChat {
        chat_id: String,
    },
    UpdateChat {
        chat_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pinned: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        archived: Option<bool>,
        /// `None`: dokunma; `Some(None)`: projeden ayır; `Some(Some(id))`: projeye bağla.
        #[serde(
            default,
            deserialize_with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        project_id: Option<Option<String>>,
    },
    GetQuota,
    ImportCatalog {
        path: String,
    },
    ImportClaudeHistory {
        root: Option<String>,
    },
    /// Projeleri bulur/oluşturur ve her projeye geçmiş notu yazar (sohbet içe aktarmaz).
    /// `home`: `.claude`/`.codex` köklerinin bulunduğu dizin (varsayılan: kullanıcı profili).
    ImportProjects {
        #[serde(default)]
        dry_run: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home: Option<String>,
    },
    /// Claude geçmişi içe aktarıcısının oluşturduğu sohbetleri yedek alarak siler.
    RemoveImportedChats {
        #[serde(default)]
        dry_run: bool,
    },
    Route {
        text: String,
    },
    SetResetRights {
        pool: String,
        count: u32,
    },
    PolicyCheck {
        action: crate::policy::Action,
        chat_id: Option<String>,
        engine: EngineId,
        #[serde(default = "default_approval_wait_secs")]
        wait_secs: u32,
    },
    ListApprovals {
        status: Option<crate::db::approvals::ApprovalStatus>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    ResolveApproval {
        id: String,
        approve: bool,
        by: String,
    },
}

fn default_approval_wait_secs() -> u32 {
    300
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn send_without_elevated_keeps_legacy_default() {
        let legacy = serde_json::json!({
            "type": "send", "chat_id": null, "cwd": ".", "engine": "codex",
            "text": "sentetik", "policy": "workspace"
        });
        let request: Request = serde_json::from_value(legacy.clone()).unwrap();
        assert!(matches!(request, Request::Send { elevated: false, .. }));
        let mut elevated = legacy;
        elevated["elevated"] = true.into();
        let request: Request = serde_json::from_value(elevated).unwrap();
        assert!(matches!(request, Request::Send { elevated: true, .. }));
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn policy_check_defaults_timeout_and_debug_hides_action() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "type": "policy_check", "action": {"Shell": {"command": "rm x; token=synthetic-value", "cwd": "."}},
            "chat_id": null, "engine": "claude"
        })).unwrap();
        assert!(matches!(
            &request,
            Request::PolicyCheck { wait_secs: 300, .. }
        ));
        assert!(!format!("{request:?}").contains("synthetic-value"));
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), request);
        for status in ["pending", "approved", "denied", "expired"] {
            let request: Request = serde_json::from_value(
                serde_json::json!({"type": "list_approvals", "status": status}),
            )
            .unwrap();
            assert!(matches!(
                request,
                Request::ListApprovals {
                    status: Some(_),
                    limit: None,
                    cursor: None
                }
            ));
        }
    }
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Proje özeti (yanıt verisi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub path: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub last_active_at: Option<String>,
}

/// Yerel proje yönlendirmesinin sonucu; alternatifler proje kimlikleridir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteResult {
    pub project_id: Option<String>,
    pub confidence: f32,
    pub reason: String,
    pub alternatives: Vec<String>,
}

/// Katalogdan eklenen ve güncellenen proje sayıları.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogImportReport {
    pub imported: usize,
    pub updated: usize,
}

/// Sohbet özeti (yanıt verisi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatInfo {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub updated_at: String,
    pub engine: Option<EngineId>,
}

/// Tek sohbetin ayrıntısı: meta, çalışma durumu ve son oturumun motoru.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatDetail {
    pub chat: ChatInfo,
    pub work_state: Option<Value>,
    pub last_engine: Option<EngineId>,
}

/// İçe aktarma raporu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub files: usize,
    pub chats: usize,
    pub messages: usize,
    pub skipped: usize,
    pub masked: usize,
}

/// `ImportProjects` satırı: bir proje klasörü.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectImportRow {
    pub path: String,
    pub name: String,
    pub claude_sessions: usize,
    pub codex_sessions: usize,
    /// İşçi / alt ajan oturumları (Claude + Codex toplamı); not için kısaltılarak kullanılır.
    #[serde(default)]
    pub worker_sessions: usize,
    /// Proje bu çalıştırmada oluşturuldu (dry-run'da: oluşturulacak).
    pub project_created: bool,
    /// Not bu çalıştırmada yazıldı (dry-run'da: yazılacak).
    pub note_updated: bool,
    /// "güncellendi", "güncel", "güncellenecek", "kaynak yok" veya hata metni.
    pub note_status: String,
}

/// `ImportProjects` raporu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectImportReport {
    pub dry_run: bool,
    pub rows: Vec<ProjectImportRow>,
    pub created: usize,
    pub notes_updated: usize,
    pub summarizer: String,
    /// Kapsayıcı klasör/ev dizini/çözülemeyen worktree nedeniyle hiçbir projeye atfedilemeyen oturumlar.
    #[serde(default)]
    pub unmatched_sessions: usize,
}

/// `RemoveImportedChats` raporu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveChatsReport {
    pub dry_run: bool,
    pub chats: usize,
    pub events: usize,
    /// SQLite yedeğinin yolu (dry-run'da veya silinecek sohbet yoksa `None`).
    pub backup: Option<String>,
}

// Token'ı günlüklere sızdırmamak için elle yazılmış Debug.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Hello { .. } => f.write_str("Hello { token: <gizli> }"),
            Request::Send {
                chat_id, engine, ..
            } => f
                .debug_struct("Send")
                .field("chat_id", chat_id)
                .field("engine", engine)
                .finish_non_exhaustive(),
            Request::Switch { chat_id, to } => f
                .debug_struct("Switch")
                .field("chat_id", chat_id)
                .field("to", to)
                .finish(),
            Request::Cancel { chat_id } => {
                f.debug_struct("Cancel").field("chat_id", chat_id).finish()
            }
            Request::Events {
                chat_id, follow, ..
            } => f
                .debug_struct("Events")
                .field("chat_id", chat_id)
                .field("follow", follow)
                .finish_non_exhaustive(),
            Request::HistorySearch { chat_id, .. } => f
                .debug_struct("HistorySearch")
                .field("chat_id", chat_id)
                .finish_non_exhaustive(),
            Request::Status => f.write_str("Status"),
            Request::PolicyCheck { .. } => f.write_str("PolicyCheck { action: <gizli> }"),
            Request::ListApprovals { status, .. } => f
                .debug_struct("ListApprovals")
                .field("status", status)
                .finish_non_exhaustive(),
            Request::ResolveApproval { id, approve, .. } => f
                .debug_struct("ResolveApproval")
                .field("id", id)
                .field("approve", approve)
                .finish_non_exhaustive(),
            Request::ListProjects => f.write_str("ListProjects"),
            Request::ListChats { project_id, .. } => f
                .debug_struct("ListChats")
                .field("project_id", project_id)
                .finish_non_exhaustive(),
            Request::GetChat { chat_id } => {
                f.debug_struct("GetChat").field("chat_id", chat_id).finish()
            }
            Request::UpdateChat { chat_id, .. } => f
                .debug_struct("UpdateChat")
                .field("chat_id", chat_id)
                .finish_non_exhaustive(),
            Request::GetQuota => f.write_str("GetQuota"),
            Request::ImportCatalog { .. } => f.write_str("ImportCatalog"),
            Request::ImportClaudeHistory { .. } => f.write_str("ImportClaudeHistory"),
            Request::ImportProjects { .. } => f.write_str("ImportProjects"),
            Request::RemoveImportedChats { .. } => f.write_str("RemoveImportedChats"),
            Request::Route { .. } => f.write_str("Route"),
            Request::SetResetRights { pool, count } => f
                .debug_struct("SetResetRights")
                .field("pool", pool)
                .field("count", count)
                .finish(),
        }
    }
}

/// Servisten istemciye yanıt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok { data: Value },
    Event(StoredEvent),
    Error { message: String },
}

/// Satır okuma hatası.
#[derive(Debug)]
pub enum FrameError {
    /// Satır sınırı aşıldı.
    TooLong,
    /// Bağlantı satır ortasında kesildi.
    Truncated,
    InvalidUtf8,
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLong => write!(f, "satır en fazla {MAX_LINE_BYTES} bayt olabilir"),
            FrameError::Truncated => f.write_str("bağlantı satır ortasında kesildi"),
            FrameError::InvalidUtf8 => f.write_str("satır geçerli UTF-8 değil"),
            FrameError::Io(e) => write!(f, "g/ç hatası: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Bir satırı en fazla `max` bayt tamponlayarak okur. Bağlantı temiz kapandıysa `Ok(None)`.
/// Sınır aşılırsa daha fazla veri tamponlanmadan `TooLong` döner.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<String>, FrameError> {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if acc.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            if acc.len() + i > max {
                return Err(FrameError::TooLong);
            }
            acc.extend_from_slice(&buf[..i]);
            reader.consume(i + 1);
            break;
        }
        if acc.len() + buf.len() > max {
            return Err(FrameError::TooLong);
        }
        let n = buf.len();
        acc.extend_from_slice(buf);
        reader.consume(n);
    }
    if acc.last() == Some(&b'\r') {
        acc.pop();
    }
    String::from_utf8(acc)
        .map(Some)
        .map_err(|_| FrameError::InvalidUtf8)
}

/// Mesajı tek satır JSON olarak yazar ve boşaltır.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Sabit zamanlı bayt dizisi karşılaştırması (uzunluk farkı da sabit işle ele alınır).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// Pipe adı: `ULTRON_PIPE_NAME` (testler/geçersiz kılma) ya da `\\.\pipe\ultron-<SID>`.
pub fn pipe_name() -> Result<String> {
    if let Some(name) = std::env::var_os("ULTRON_PIPE_NAME") {
        return Ok(name.to_string_lossy().into_owned());
    }
    Ok(format!(r"\\.\pipe\ultron-{}", sys::current_user_sid()?))
}

/// `run\token` dosyasının yolu.
pub fn token_path(paths: &Paths) -> std::path::PathBuf {
    paths.run.join("token")
}

/// Yalnız insan arayüzlerinin kullandığı onaylayıcı sırrı; motor ortamına aktarılmaz.
/// Hello.token bu sır ise approver, normal daemon token'ı ise requester rolü alınır.
pub fn approver_token_path(paths: &Paths) -> std::path::PathBuf {
    paths.run.join("approver-token")
}

/// Servisin yazdığı requester token'ını iki rolün dosyaları hazırsa okur.
pub fn read_token(paths: &Paths) -> Result<String> {
    // Onaylayıcı sırrını okumadan hazır olduğunu doğrula; eksikse başlatma döngüsü bekler.
    let approver_path = approver_token_path(paths);
    let metadata = std::fs::metadata(&approver_path).with_context(|| {
        format!(
            "onaylayıcı token dosyası hazır değil: {}",
            approver_path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!("onaylayıcı token yolu dosya değil");
    }
    let path = token_path(paths);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("token okunamadı: {}", path.display()))?;
    Ok(text.trim().to_string())
}

type PipeReader = BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>;
type PipeWriter = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>;

/// Servise bağlı istemci.
pub struct Client {
    reader: PipeReader,
    writer: PipeWriter,
}

impl Client {
    /// Pipe'a bağlanır (sunucunun aynı kullanıcıya ait olduğunu doğrular) ve `Hello` gönderir.
    pub async fn connect(pipe: &str, token: &str) -> Result<Client> {
        let mut client = Client::open(pipe).await?;
        client
            .send(&Request::Hello {
                token: token.to_string(),
            })
            .await?;
        match client.next().await? {
            Some(Response::Ok { .. }) => Ok(client),
            Some(Response::Error { message }) => bail!("servis reddetti: {message}"),
            _ => bail!("servis beklenmeyen yanıt verdi"),
        }
    }

    /// Kimlik doğrulamadan bağlanır (testler ve `connect` için).
    pub async fn open(pipe: &str) -> Result<Client> {
        let pipe_client = sys::connect_pipe(pipe).await?;
        let (r, w) = tokio::io::split(pipe_client);
        Ok(Client {
            reader: BufReader::new(r),
            writer: w,
        })
    }

    pub async fn send(&mut self, req: &Request) -> Result<()> {
        write_msg(&mut self.writer, req).await
    }

    /// Bir sonraki yanıt; bağlantı kapandıysa `None`.
    pub async fn next(&mut self) -> Result<Option<Response>> {
        match read_frame(&mut self.reader, MAX_RESPONSE_BYTES).await {
            Ok(Some(line)) => Ok(Some(
                serde_json::from_str(&line).context("yanıt çözümlenemedi")?,
            )),
            Ok(None) => Ok(None),
            Err(FrameError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    /// Ham baytları yazar (protokol ihlali testleri için).
    pub async fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

/// Windows'a özgü güvenlik yardımcıları.
pub mod sys {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Result, anyhow, bail};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const ERROR_PIPE_BUSY: i32 = 231;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: geçerli, bize ait bir tutamaç.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn sid_of_token(token: HANDLE) -> Result<String> {
        let mut needed = 0u32;
        // SAFETY: boyut sorgusu; null tampon ve sıfır uzunluk.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            bail!(
                "TOKEN_USER boyutu alınamadı: {}",
                io::Error::last_os_error()
            );
        }
        // u64 dizisi, TOKEN_USER için yeterli hizalamayı sağlar.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: tampon `needed` bayttan büyük.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            bail!("TOKEN_USER okunamadı: {}", io::Error::last_os_error());
        }
        // SAFETY: tampon başarıyla TOKEN_USER ile dolduruldu.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` geçerli; `out` sistem tarafından ayrılır ve LocalFree ile bırakılır.
        if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
            bail!("SID metne çevrilemedi: {}", io::Error::last_os_error());
        }
        // SAFETY: `out` NUL ile biten UTF-16 dizisi.
        let text = unsafe {
            let mut len = 0usize;
            while *out.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(out, len))
        };
        // SAFETY: `out` ConvertSidToStringSidW tarafından ayrıldı.
        unsafe { LocalFree(out.cast()) };
        Ok(text)
    }

    /// Mevcut işlemin kullanıcı SID'i (`S-1-5-21-...`).
    pub fn current_user_sid() -> Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess sahte tutamaç döner.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            bail!("işlem token'ı açılamadı: {}", io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        sid_of_token(token.0)
    }

    fn process_user_sid(pid: u32) -> Result<String> {
        // SAFETY: yalnız sorgu hakkı istenir.
        let proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if proc.is_null() {
            bail!("sunucu işlemi açılamadı: {}", io::Error::last_os_error());
        }
        let proc = OwnedHandle(proc);
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `proc` geçerli.
        if unsafe { OpenProcessToken(proc.0, TOKEN_QUERY, &mut token) } == 0 {
            bail!("sunucu token'ı açılamadı: {}", io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        sid_of_token(token.0)
    }

    /// Yalnız tek bir SID'e erişim veren, devralmayan (protected) DACL.
    pub struct SecurityDescriptor {
        ptr: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: güvenlik tanımlayıcısı oluşturulduktan sonra salt okunurdur.
    unsafe impl Send for SecurityDescriptor {}
    // SAFETY: yukarıdaki gerekçe.
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        /// `D:P(A;;<rights>;;;<sid>)` tanımlayıcısı; `rights` ör. `GA` ya da `FA`.
        pub fn owner_only(sid: &str, rights: &str) -> Result<SecurityDescriptor> {
            let sddl = wide(std::ffi::OsStr::new(&format!("D:P(A;;{rights};;;{sid})")));
            let mut ptr: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `sddl` NUL ile biter; `ptr` LocalFree ile bırakılır (Drop).
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut ptr,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                bail!(
                    "güvenlik tanımlayıcısı kurulamadı: {}",
                    io::Error::last_os_error()
                );
            }
            Ok(SecurityDescriptor { ptr })
        }

        pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.ptr,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: ptr ConvertStringSecurityDescriptorToSecurityDescriptorW ile ayrıldı.
            unsafe { LocalFree(self.ptr) };
        }
    }

    /// Pipe sunucu örneği oluşturur: yalnız `sd`'deki kullanıcı, uzak istemci yok.
    /// `first` doğruysa ad zaten başkasında olduğunda oluşturma başarısız olur.
    pub fn create_server(
        name: &str,
        first: bool,
        sd: &SecurityDescriptor,
    ) -> io::Result<NamedPipeServer> {
        let mut sa = sd.attributes();
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .pipe_mode(PipeMode::Byte);
        // SAFETY: `sa` bu çağrı boyunca geçerli; tanımlayıcı `sd` ile yaşar.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }

    /// Pipe'a bağlanır (meşgulse yeniden dener) ve sunucunun aynı kullanıcıya ait olduğunu doğrular.
    pub async fn connect_pipe(name: &str) -> Result<NamedPipeClient> {
        let mut attempts = 0;
        let client = loop {
            match ClientOptions::new().open(name) {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 50 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(anyhow!(e).context(format!("pipe açılamadı: {name}"))),
            }
        };
        let mut pid = 0u32;
        // SAFETY: tutamaç `client` ile yaşar.
        if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut pid) } == 0 {
            bail!(
                "pipe sunucusu doğrulanamadı: {}",
                io::Error::last_os_error()
            );
        }
        if process_user_sid(pid)? != current_user_sid()? {
            bail!("pipe sunucusu başka bir kullanıcıya ait; token gönderilmedi");
        }
        Ok(client)
    }

    /// Sistem rastgele sayı üretecinden 32 bayt, hex.
    pub fn random_token() -> Result<String> {
        let mut bytes = [0u8; 32];
        // SAFETY: tampon 32 bayt; algoritma null + sistem RNG bayrağı.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            bail!("rastgele sayı üretilemedi (NTSTATUS {status:#x})");
        }
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Dosyayı yalnız mevcut kullanıcıya açık (devralmayan DACL) olarak, oluşturulurken
    /// ACL ile birlikte yazar. Önceki dosya silinir.
    pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
        use std::io::Write;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!(e).context("eski token dosyası silinemedi")),
        }
        let sd = SecurityDescriptor::owner_only(&current_user_sid()?, "FA")?;
        let sa = sd.attributes();
        let name = wide(path.as_os_str());
        // SAFETY: `name` NUL ile biter; `sa` çağrı boyunca geçerli.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("özel dosya oluşturulamadı: {}", io::Error::last_os_error());
        }
        // SAFETY: tutamaç geçerli ve tekil sahipliğe devrediliyor.
        let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
}
