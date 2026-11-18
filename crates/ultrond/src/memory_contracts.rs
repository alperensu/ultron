//! Daemon tarafı hafıza sözleşmeleri (Aşama 5a, Task 1): dar uygulama arayüzleri ve güvenli
//! hata kümesi. Davranış sonraki görevlerin sahipliğindedir (Task 6 servis, Task 8 entegrasyon);
//! burada yalnız imzalar vardır ve hiçbir sahte başarı üretilmez.

use std::future::Future;
use std::pin::Pin;

use ultron_core::memory::{
    AuthzDenied, ChatRef, ContractError, MemoryCaptureRequest, MemoryContext, MemoryId, MemoryItemDetail, MemoryItemView,
    MemoryPage, MemoryRefreshReport, MemorySearchHit, MemorySuggestion, MemoryType, MemoryUpdateRequest,
    Revision, Sha256Hash, ShadowReport, SuggestionStatus, check_text, validate_request_id, MAX_CAPTURE_TEXT_BYTES,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Kullanıcıya/IPC'ye dönen kapalı hata kümesi; ham dosya/model/DB metni taşımaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    /// Hafıza uygulama servisi henüz bağlanmadı (Task 8 entegrasyonu).
    NotImplemented,
    Unauthorized(AuthzDenied),
    InvalidRequest,
    NotFound,
    /// `expected_revision` / aday hash uyuşmazlığı; son yazan kazanmaz.
    Conflict,
    /// Dış değişiklik, bozuk YAML, sır veya kimlik çakışması nedeniyle güvenli conflict durumu.
    Quarantined,
    /// Başarı yalnız not kalıcı olduktan sonra döner; indeks gecikmesi ayrı bir durumdur.
    IndexUnavailable,
    Unavailable,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::NotImplemented => f.write_str("hafıza servisi henüz bağlanmadı"),
            MemoryError::Unauthorized(denied) => write!(f, "{denied}"),
            MemoryError::InvalidRequest => f.write_str("geçersiz hafıza isteği"),
            MemoryError::NotFound => f.write_str("hafıza kaydı bulunamadı"),
            MemoryError::Conflict => f.write_str("hafıza kaydı çakışması (revision/hash uyuşmuyor)"),
            MemoryError::Quarantined => f.write_str("hafıza kaydı güvenli çakışma durumunda"),
            MemoryError::IndexUnavailable => f.write_str("hafıza indeksi hazır değil"),
            MemoryError::Unavailable => f.write_str("hafıza servisi kullanılamıyor"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<AuthzDenied> for MemoryError {
    fn from(denied: AuthzDenied) -> Self {
        MemoryError::Unauthorized(denied)
    }
}

/// Masaüstü/CLI yüzeyi (`require_approver` sonrası). Tek kasa yazıcısı daemon'dır.
pub trait MemoryApplication: Send + Sync {
    fn list(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        project: Option<String>,
        memory_type: Option<MemoryType>,
    ) -> BoxFuture<'_, Result<MemoryPage<MemoryItemView>, MemoryError>>;
    fn get(&self, id: MemoryId) -> BoxFuture<'_, Result<MemoryItemDetail, MemoryError>>;
    fn search(
        &self,
        query: String,
        project: Option<String>,
        limit: Option<u32>,
    ) -> BoxFuture<'_, Result<MemoryPage<MemorySearchHit>, MemoryError>>;
    fn capture(&self, request: MemoryCaptureRequest) -> BoxFuture<'_, Result<MemoryItemDetail, MemoryError>>;
    fn update(&self, request: MemoryUpdateRequest) -> BoxFuture<'_, Result<MemoryItemDetail, MemoryError>>;
    fn delete(
        &self,
        request_id: String,
        id: MemoryId,
        expected_revision: Revision,
    ) -> BoxFuture<'_, Result<(), MemoryError>>;
    fn suggestions(
        &self,
        status: Option<SuggestionStatus>,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> BoxFuture<'_, Result<MemoryPage<MemorySuggestion>, MemoryError>>;
    fn resolve_suggestion(
        &self,
        request_id: String,
        id: MemoryId,
        accept: bool,
        expected_candidate_hash: Sha256Hash,
    ) -> BoxFuture<'_, Result<MemorySuggestion, MemoryError>>;
    fn refresh(&self, request_id: String) -> BoxFuture<'_, Result<MemoryRefreshReport, MemoryError>>;
    fn shadow_report(&self, since_days: Option<u32>) -> BoxFuture<'_, Result<ShadowReport, MemoryError>>;
}

/// Telegram'dan doğrulanmış güncelleme referansı; yalnız ağ geçidi, allowlist ve güncelleme
/// doğrulamasından sonra üretir. Ham Telegram kimlikleri kaynak makbuzu olarak serbest kullanılamaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedTelegramUpdate {
    pub update_id: i64,
}

/// Telegram `/not` yakalaması: yalnız metin ve doğrulanmış güncelleme. `chat_ref`, `project`,
/// `tags` ve `title` yoktur; böylece Telegram başka bir Ultron sohbetini kaynak gösteremez ve
/// proje kapsamı seçemez.
#[derive(Clone, PartialEq, Eq)]
pub struct TelegramCaptureRequest {
    pub request_id: String,
    pub text: String,
    pub update: VerifiedTelegramUpdate,
}

impl TelegramCaptureRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_request_id(&self.request_id)?;
        check_text(&self.text, MAX_CAPTURE_TEXT_BYTES, false)
    }
}

impl std::fmt::Debug for TelegramCaptureRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramCaptureRequest")
            .field("request_id", &self.request_id)
            .field("text_bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

/// Telegram'a açılan dar giriş noktaları (`GatewayApplication` içine eklenecek). Genel IPC
/// yönlendirmesi değildir; uygulama katmanı üçlü kimlik filtresini yeniden denetler ve
/// düzenleme/silme/restore/purge sunmaz. `/not` içeriği otomatik `remote_allowed=true` olmaz.
pub trait MemoryTelegramPort: Send + Sync {
    fn capture_memory(&self, user_id: i64, request: TelegramCaptureRequest) -> BoxFuture<'_, Result<MemoryItemView, MemoryError>>;
    fn search_memory(&self, user_id: i64, query: String, limit: Option<u32>) -> BoxFuture<'_, Result<Vec<MemorySearchHit>, MemoryError>>;
    /// Yalnız yeni not ekleyen aday; mevcut notu değiştirenler `Unauthorized(UseDesktop)` döner.
    fn resolve_memory_suggestion(
        &self,
        user_id: i64,
        request_id: String,
        id: MemoryId,
        accept: bool,
        expected_candidate_hash: Sha256Hash,
    ) -> BoxFuture<'_, Result<MemorySuggestion, MemoryError>>;
}

/// Motor requester'ının dar `memory_search`/`memory_get` aracı. Sohbet, proje, motor/audience ve
/// yaşam süresi daemon tarafından canlı tur belirtecinden çözülür; belirteç yanıta/prompt'a basılmaz.
pub trait MemoryToolPort: Send + Sync {
    fn memory_search<'a>(
        &'a self,
        turn_token: &'a str,
        query: String,
        limit: Option<u32>,
    ) -> BoxFuture<'a, Result<MemoryContext, MemoryError>>;
    fn memory_get<'a>(&'a self, turn_token: &'a str, id: MemoryId) -> BoxFuture<'a, Result<MemoryContext, MemoryError>>;
}

/// Tur tamamlandığında sohbete bağlı yakalama için sohbet referansı doğrulayıcısı (Task 6).
pub trait ChatRefVerifier: Send + Sync {
    fn verify(&self, chat_ref: &ChatRef) -> BoxFuture<'_, Result<(), MemoryError>>;
}

/// Sunucu `dispatch` içindeki hafıza kolları, Task 8 servisi bağlanana kadar bunu döndürür:
/// açık "henüz bağlanmadı" hatası; başarı simüle edilmez.
pub fn not_wired_error() -> anyhow::Error {
    anyhow::anyhow!(MemoryError::NotImplemented.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_capture_is_text_only_bounded_and_redacted() {
        let ok = TelegramCaptureRequest {
            request_id: "tg-1".into(),
            text: "sentetik not".into(),
            update: VerifiedTelegramUpdate { update_id: 7 },
        };
        assert!(ok.validate().is_ok());
        assert!(!format!("{ok:?}").contains("sentetik not"));
        let long = TelegramCaptureRequest { text: "x".repeat(MAX_CAPTURE_TEXT_BYTES + 1), ..ok.clone() };
        assert!(long.validate().is_err());
        let bad_id = TelegramCaptureRequest { request_id: "../x".into(), ..ok };
        assert!(bad_id.validate().is_err());
    }
}
