//! `ultrond` köprüsü: istek/yanıt için tek paylaşılan bağlantı, akışlar için ayrı bağlantılar.
//! Token yalnız bu modülde okunur; webview'e hiçbir yoldan verilmez.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use ultron_core::Paths;
use ultron_core::client::connect_or_start;
use ultron_core::ipc::{self, Client, Request, Response};

/// Tek bir yanıt için beklenen en uzun süre (kısa istekler).
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Uzun süren yönetim işleri (proje içe aktarma, sohbet silme) için yanıt süresi.
const ADMIN_REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Webview'e giden tipli hata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeError {
    /// Servise bağlanılamadı ya da başlatılamadı.
    DaemonUnavailable { reason: String },
    /// Servis isteği hata ile reddetti (ör. henüz uygulanmamış istek); mesaj aynen iletilir.
    Daemon { message: String },
    /// İletişim/protokol hatası.
    Protocol { message: String },
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::DaemonUnavailable { reason } => write!(f, "servis yok: {reason}"),
            BridgeError::Daemon { message } | BridgeError::Protocol { message } => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for BridgeError {}

/// Köprü durumu: bağlantı sağlayıcı ve paylaşılan istemci.
pub struct Bridge {
    paths: Paths,
    /// `None` ise servis başlatılmaz, yalnız bağlanılır.
    ultrond_exe: Option<PathBuf>,
    shared: Mutex<Option<Client>>,
}

impl Bridge {
    pub fn new(paths: Paths, ultrond_exe: Option<PathBuf>) -> Bridge {
        Bridge {
            paths,
            ultrond_exe,
            shared: Mutex::new(None),
        }
    }

    /// Üretim yapılandırması: `ULTRON_HOME`/varsayılan yollar, `ultrond` uygulamanın yanında.
    pub fn from_env() -> Result<Bridge> {
        let paths = Paths::resolve()?;
        let exe = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|e| ultron_core::client::locate_ultrond(&e))
            .ok();
        Ok(Bridge::new(paths, exe))
    }

    /// Yeni, kimliği doğrulanmış bağlantı; servis yoksa (izin varsa) başlatır.
    pub async fn connect(&self) -> Result<Client, BridgeError> {
        let result = match &self.ultrond_exe {
            Some(exe) => connect_or_start(&self.paths, exe).await,
            None => connect_only(&self.paths).await,
        };
        result.map_err(|e| BridgeError::DaemonUnavailable {
            reason: format!("{e:#}"),
        })
    }

    /// Tek istek, tek yanıt (paylaşılan bağlantı; kopmuşsa bir kez yeniden bağlanır).
    pub async fn request(&self, req: Request) -> Result<Value, BridgeError> {
        // Yönetim istekleri onaylayıcı rolü ister, uzun sürebilir ve asla yeniden denenmez
        // (çift çalıştırma olmasın).
        let admin = matches!(
            req,
            Request::ImportProjects { .. }
                | Request::RemoveImportedChats { .. }
                | Request::ImportClaudeHistory { .. }
        );
        if admin
            || matches!(
                req,
                Request::ListApprovals { .. } | Request::ResolveApproval { .. }
            )
        {
            // Sır yalnız Rust köprüsünde kalır; normal bağlantıya veya motor ortamına taşınmaz.
            let _ready = self.connect().await?;
            let secret = std::fs::read_to_string(ipc::approver_token_path(&self.paths))
                .map_err(|_| protocol("onaylayıcı sırrı okunamadı"))?;
            let mut client = Client::connect(
                &ipc::pipe_name().map_err(|_| protocol("pipe çözülemedi"))?,
                secret.trim(),
            )
            .await
            .map_err(|_| protocol("onaylayıcı bağlantısı kurulamadı"))?;
            let wait = if admin { ADMIN_REPLY_TIMEOUT } else { REPLY_TIMEOUT };
            return match round_trip_timeout(&mut client, &req, wait).await {
                Ok(Some(resp)) => into_data(resp),
                _ => Err(protocol("onaylayıcı yanıtı alınamadı")),
            };
        }
        let mut guard = self.shared.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect().await?);
            }
            let client = guard.as_mut().expect("bağlantı yukarıda kuruldu");
            match round_trip(client, &req).await {
                Ok(Some(resp)) => return into_data(resp),
                // Bağlantı koptu: at ve yeniden dene.
                Ok(None) | Err(_) if attempt == 0 => *guard = None,
                Ok(None) => {
                    *guard = None;
                    return Err(protocol("servis bağlantıyı kapattı"));
                }
                Err(e) => {
                    *guard = None;
                    return Err(protocol(&format!("{e:#}")));
                }
            }
        }
        Err(protocol("istek tamamlanamadı"))
    }

    /// Akış başlatan isteklerin (Send) ilk yanıtı için yeni bağlantı; ardından bağlantı bırakılır
    /// (servis turu arka planda sürdürür).
    pub async fn request_detached(&self, req: Request) -> Result<Value, BridgeError> {
        let mut client = self.connect().await?;
        match round_trip(&mut client, &req).await {
            Ok(Some(resp)) => into_data(resp),
            Ok(None) => Err(protocol("servis bağlantıyı kapattı")),
            Err(e) => Err(protocol(&format!("{e:#}"))),
        }
    }
}

async fn connect_only(paths: &Paths) -> Result<Client> {
    let pipe = ipc::pipe_name()?;
    let token = ipc::read_token(paths)?;
    Client::connect(&pipe, &token).await
}

fn protocol(message: &str) -> BridgeError {
    BridgeError::Protocol {
        message: message.to_string(),
    }
}

async fn round_trip(client: &mut Client, req: &Request) -> Result<Option<Response>> {
    round_trip_timeout(client, req, REPLY_TIMEOUT).await
}

async fn round_trip_timeout(
    client: &mut Client,
    req: &Request,
    wait: Duration,
) -> Result<Option<Response>> {
    client.send(req).await?;
    match tokio::time::timeout(wait, client.next()).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("servis yanıtı zaman aşımına uğradı")),
    }
}

/// Yanıtı veriye çevirir; servis hatası tipli `Daemon` hatası olarak aynen geçer, veri uydurulmaz.
pub fn into_data(resp: Response) -> Result<Value, BridgeError> {
    match resp {
        Response::Ok { data } => Ok(data),
        Response::Error { message } => Err(BridgeError::Daemon { message }),
        Response::Event(_) => Err(protocol("beklenmeyen olay yanıtı")),
    }
}
