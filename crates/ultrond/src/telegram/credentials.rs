//! Kullanıcıya bağlı Windows generic credential sınırı. Testler Win32'ye erişmez.

use std::fmt;
#[cfg(windows)]
use zeroize::Zeroize;
use zeroize::Zeroizing;

use super::contracts::{BoxFuture, CredentialError, CredentialStore};

pub const CREDENTIAL_TARGET: &str = "ultron/telegram-bot-token";
const MAX_TOKEN_BYTES: usize = 2560;

/// Bilinçli taşıma/depolama erişimi dışında içerik açılmaz; Clone/Serialize yoktur.
pub struct SecretToken {
    value: Zeroizing<String>,
}

impl SecretToken {
    /// Yalnız CLI'nin tek son satır sonunu kaldırır; başka normalizasyon yapmaz.
    pub fn new(value: String) -> Result<Self, CredentialError> {
        // Wrap before validation/normalization so error paths also erase the input.
        let mut value = Zeroizing::new(value);
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        Self::from_exact(value)
    }

    fn from_exact(value: Zeroizing<String>) -> Result<Self, CredentialError> {
        validate_token(&value)?;
        Ok(Self { value })
    }

    pub(super) fn expose(&self) -> &str {
        &self.value
    }
}

fn validate_token(value: &str) -> Result<(), CredentialError> {
    let valid = value.len() <= MAX_TOKEN_BYTES
        && value.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && !secret.is_empty()
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if !valid {
        return Err(CredentialError::InvalidToken);
    }
    Ok(())
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<gizli>)")
    }
}

impl fmt::Display for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<gizli>")
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { os_code } => write!(f, "Credential unavailable ({os_code:?})"),
            Self::InvalidToken => f.write_str("Invalid Telegram token"),
        }
    }
}

impl std::error::Error for CredentialError {}

#[derive(Default)]
pub struct WindowsCredentialStore;

impl CredentialStore for WindowsCredentialStore {
    fn read_token(&self) -> BoxFuture<'_, Result<Option<SecretToken>, CredentialError>> {
        Box::pin(async {
            tokio::task::spawn_blocking(read_windows)
                .await
                .map_err(|_| CredentialError::Unavailable { os_code: None })?
        })
    }

    fn write_token<'a>(
        &'a self,
        token: &'a SecretToken,
    ) -> BoxFuture<'a, Result<(), CredentialError>> {
        Box::pin(async move {
            // Win32 çağrısı boyunca sahip olunan dar ömürlü kopya gerekir.
            // Writes always use UTF-8 without a BOM or terminating NUL.
            let blob = Zeroizing::new(token.expose().as_bytes().to_vec());
            tokio::task::spawn_blocking(move || write_windows(blob))
                .await
                .map_err(|_| CredentialError::Unavailable { os_code: None })?
        })
    }
}

fn decode_blob(blob: &[u8]) -> Result<SecretToken, CredentialError> {
    if blob.is_empty() || blob.len() > MAX_TOKEN_BYTES * 2 {
        return Err(CredentialError::InvalidToken);
    }
    // Telegram tokens are ASCII: UTF-16LE has a NUL in every high byte.
    let (pairs, remainder) = blob.as_chunks::<2>();
    if remainder.is_empty() && pairs.iter().all(|pair| pair[1] == 0) {
        let mut value = Zeroizing::new(String::with_capacity(blob.len() / 2));
        for pair in pairs {
            if !pair[0].is_ascii() {
                return Err(CredentialError::InvalidToken);
            }
            value.push(char::from(pair[0]));
        }
        return SecretToken::from_exact(value);
    }
    let value = std::str::from_utf8(blob).map_err(|_| CredentialError::InvalidToken)?;
    validate_token(value)?;
    SecretToken::from_exact(Zeroizing::new(value.to_owned()))
}

#[cfg(windows)]
fn read_windows() -> Result<Option<SecretToken>, CredentialError> {
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };

    struct OwnedCredential(*mut CREDENTIALW);
    impl Drop for OwnedCredential {
        fn drop(&mut self) {
            // SAFETY: CredReadW tarafından ayrılmış tek sahipli blok.
            unsafe {
                let credential = &mut *self.0;
                if !credential.CredentialBlob.is_null() && credential.CredentialBlobSize != 0 {
                    std::slice::from_raw_parts_mut(
                        credential.CredentialBlob,
                        credential.CredentialBlobSize as usize,
                    )
                    .zeroize();
                }
                CredFree(self.0.cast());
            }
        }
    }
    let target: Vec<u16> = CREDENTIAL_TARGET.encode_utf16().chain(Some(0)).collect();
    let mut ptr = std::ptr::null_mut();
    // SAFETY: target NUL sonlu; ptr geçerli bir çıktı adresidir.
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut ptr) } == 0 {
        let code = unsafe { GetLastError() };
        return if code == ERROR_NOT_FOUND {
            Ok(None)
        } else {
            Err(CredentialError::Unavailable {
                os_code: Some(code),
            })
        };
    }
    if ptr.is_null() {
        return Err(CredentialError::Unavailable { os_code: None });
    }
    let owned = OwnedCredential(ptr);
    // SAFETY: başarılı CredReadW bloğu owned yaşamı boyunca geçerlidir.
    let credential = unsafe { &*owned.0 };
    let len = credential.CredentialBlobSize as usize;
    if credential.Type != CRED_TYPE_GENERIC
        || len == 0
        || len > MAX_TOKEN_BYTES * 2
        || credential.CredentialBlob.is_null()
    {
        return Err(CredentialError::InvalidToken);
    }
    // SAFETY: Win32 blob adresini ve uzunluğunu birlikte sağlar; üst sınır kontrol edildi.
    let blob = unsafe { std::slice::from_raw_parts(credential.CredentialBlob, len) };
    decode_blob(blob).map(Some)
}

#[cfg(windows)]
fn write_windows(mut blob: Zeroizing<Vec<u8>>) -> Result<(), CredentialError> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };
    // Boyut/kodlama Win32'ye geçmeden yeniden doğrulanır.
    validate_token(std::str::from_utf8(&blob).map_err(|_| CredentialError::InvalidToken)?)?;
    let mut target: Vec<u16> = CREDENTIAL_TARGET.encode_utf16().chain(Some(0)).collect();
    // SAFETY: CREDENTIALW sıfır alanları opsiyonel alanlar için geçerlidir.
    let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
    credential.Type = CRED_TYPE_GENERIC;
    credential.TargetName = target.as_mut_ptr();
    credential.CredentialBlobSize = blob.len() as u32;
    credential.CredentialBlob = blob.as_mut_ptr();
    // LOCAL_MACHINE kalıcılığı aynı Windows kullanıcısına bağlıdır; makine çapı erişim vermez.
    credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
    // SAFETY: bütün işaretçiler senkron çağrı boyunca geçerli kalır.
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(CredentialError::Unavailable {
            os_code: Some(unsafe { GetLastError() }),
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_windows() -> Result<Option<SecretToken>, CredentialError> {
    Err(CredentialError::Unavailable { os_code: None })
}

#[cfg(not(windows))]
fn write_windows(_blob: Zeroizing<Vec<u8>>) -> Result<(), CredentialError> {
    Err(CredentialError::Unavailable { os_code: None })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use ultron_core::ipc::{TelegramConnectionState, TelegramSettings, TelegramStatusSnapshot};

    #[derive(Default)]
    pub(crate) struct MemoryCredentialStore(Mutex<Option<Vec<u8>>>);

    impl CredentialStore for MemoryCredentialStore {
        fn read_token(&self) -> BoxFuture<'_, Result<Option<SecretToken>, CredentialError>> {
            Box::pin(async {
                self.0
                    .lock()
                    .unwrap()
                    .as_deref()
                    .map(decode_blob)
                    .transpose()
            })
        }
        fn write_token<'a>(
            &'a self,
            token: &'a SecretToken,
        ) -> BoxFuture<'a, Result<(), CredentialError>> {
            Box::pin(async move {
                *self.0.lock().unwrap() = Some(token.expose().as_bytes().to_vec());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn credential_missing_is_not_connected() {
        let store = MemoryCredentialStore::default();
        let token = store.read_token().await.unwrap();
        let health = TelegramStatusSnapshot::from_health(
            1,
            token.is_some(),
            false,
            None,
            TelegramSettings::from(&ultron_core::config::TelegramConfig::default()),
        );
        assert_eq!(health.state, TelegramConnectionState::MissingToken);
    }

    #[tokio::test]
    async fn memory_credential_store_roundtrip() {
        let store = MemoryCredentialStore::default();
        let token = SecretToken::new("12345:synthetic_TOKEN-1".into()).unwrap();
        store.write_token(&token).await.unwrap();
        assert_eq!(
            store.read_token().await.unwrap().unwrap().expose(),
            token.expose()
        );
        assert!(!format!("{token:?} {token}").contains(token.expose()));
    }

    #[test]
    fn token_and_blob_validation() {
        for value in [
            "",
            " ",
            "123:abc ",
            "123:abc\n\n",
            "123:abc\r",
            "a:abc",
            "123:ü",
            "123:ab/c",
            "123:abc\0",
        ] {
            assert!(SecretToken::new(value.into()).is_err());
        }
        assert!(SecretToken::new(format!("1:{}", "a".repeat(MAX_TOKEN_BYTES))).is_err());
        assert_eq!(
            SecretToken::new("123:abc\r\n".into()).unwrap().expose(),
            "123:abc"
        );
        assert!(decode_blob(b"123:abc\n").is_err());
        assert!(decode_blob(&[0xff]).is_err());
        assert!(decode_blob(&[]).is_err());
        assert!(SecretToken::new(format!("1:{}", "a".repeat(MAX_TOKEN_BYTES - 2))).is_ok());
    }

    #[test]
    fn synthetic_utf8_and_utf16le_credentials() {
        let token = "12345:synthetic_TOKEN-1";
        let wide: Vec<u8> = token.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_blob(token.as_bytes()).unwrap().expose(), token);
        assert_eq!(decode_blob(&wide).unwrap().expose(), token);
        assert!(decode_blob(&wide[..wide.len() - 1]).is_err());
        for invalid in ["123:abc\0", "123:ü", "abc:secret", "123:abc "] {
            let wide: Vec<u8> = invalid.encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert!(decode_blob(&wide).is_err());
        }
    }
}
