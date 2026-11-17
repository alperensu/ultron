//! Sabit Telegram HTTPS sınırı; sağlayıcı metni veya URL hata sınırını geçmez.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;
use ultron_core::ipc::TelegramErrorCode;

use super::contracts::{
    AnswerCallbackQuery, BoxFuture, EditMessageText, SendMessage, SentMessage, TelegramTransport,
    TelegramUpdate, TelegramUser, TransportError,
};
use super::credentials::SecretToken;

const PRODUCTION_BASE_URL: &str = "https://api.telegram.org";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_WAIT_SECONDS: u32 = 60;
const UPDATES_LIMIT: u8 = 20;

/// `ResponseTooLarge` from `get_updates_checked` means even limit=1 exceeded the cap.
/// The bridge must stop/report or explicitly decide how to handle that offset.
#[derive(Debug)]
pub enum BotApiError {
    Transport(TransportError),
    ResponseTooLarge,
}

impl From<TransportError> for BotApiError {
    fn from(value: TransportError) -> Self {
        Self::Transport(value)
    }
}

impl BotApiError {
    fn into_transport(self) -> TransportError {
        match self {
            Self::Transport(error) => error,
            Self::ResponseTooLarge => error(TelegramErrorCode::InvalidResponse),
        }
    }
}

impl fmt::Display for BotApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(f),
            Self::ResponseTooLarge => f.write_str("Telegram response exceeds size limit"),
        }
    }
}
impl std::error::Error for BotApiError {}

/// Debug uygulanmaz: istemci URL'si ve token hiçbir tanılama çıktısına açılmaz.
pub struct BotApiClient {
    http: reqwest::Client,
    token: SecretToken,
    base_url: String,
    cancel: CancellationToken,
}

impl BotApiClient {
    pub fn new(token: SecretToken, cancel: CancellationToken) -> Result<Self, TransportError> {
        Self::build(
            token,
            cancel,
            PRODUCTION_BASE_URL.to_owned(),
            REQUEST_TIMEOUT,
        )
    }

    fn build(
        token: SecretToken,
        cancel: CancellationToken,
        base_url: String,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Ortam proxy'si token taşıyan URL'yi başka bir sunucuya yönlendiremez.
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|e| sanitize_reqwest(e, false))?;
        Ok(Self {
            http,
            token,
            base_url,
            cancel,
        })
    }

    /// Yalnız unit-test derlemesinde loopback HTTP enjeksiyonu; test-hooks açmaz.
    #[cfg(test)]
    pub(crate) fn for_test(
        token: SecretToken,
        cancel: CancellationToken,
        address: std::net::SocketAddr,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        if !address.ip().is_loopback() || !address.is_ipv4() {
            return Err(error(TelegramErrorCode::InvalidResponse));
        }
        Self::build(token, cancel, format!("http://{address}"), timeout)
    }

    async fn call<T: DeserializeOwned, B: Serialize>(
        &self,
        method: &str,
        body: B,
        read_only: bool,
    ) -> Result<T, TransportError> {
        self.call_checked(method, body, read_only)
            .await
            .map_err(BotApiError::into_transport)
    }

    /// Keeps the offset unchanged while reducing oversized batches down to one update.
    pub async fn get_updates_checked(
        &self,
        offset: Option<i64>,
    ) -> Result<Vec<TelegramUpdate>, BotApiError> {
        let mut limit = UPDATES_LIMIT;
        loop {
            let result = self
                .call_checked(
                    "getUpdates",
                    serde_json::json!({
                        "offset": offset, "limit": limit, "timeout": 25,
                        "allowed_updates": ["message", "callback_query"]
                    }),
                    true,
                )
                .await;
            match result {
                Err(BotApiError::ResponseTooLarge) if limit > 1 => limit = (limit / 2).max(1),
                _ => return result,
            }
        }
    }

    async fn call_checked<T: DeserializeOwned, B: Serialize>(
        &self,
        method: &str,
        body: B,
        read_only: bool,
    ) -> Result<T, BotApiError> {
        let body =
            serde_json::to_vec(&body).map_err(|_| error(TelegramErrorCode::InvalidResponse))?;
        for attempt in 0..MAX_ATTEMPTS {
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(error(TelegramErrorCode::Network).into()),
                result = self.call_once(method, &body, !read_only) => result,
            };
            match result {
                Ok(result) => return Ok(result),
                Err(BotApiError::ResponseTooLarge) => return Err(BotApiError::ResponseTooLarge),
                Err(BotApiError::Transport(e)) => {
                    // Mutasyonlar belirsiz teslimde yinelenmez. 429 açık bir ret olduğundan güvenlidir.
                    let delay = match e.code {
                        TelegramErrorCode::RateLimited => e
                            .retry_after_seconds
                            .filter(|s| *s <= MAX_RETRY_WAIT_SECONDS)
                            .map(|s| s.max(1)),
                        TelegramErrorCode::Network
                        | TelegramErrorCode::Timeout
                        | TelegramErrorCode::ProviderUnavailable
                            if read_only =>
                        {
                            Some(1 << attempt)
                        }
                        _ => None,
                    };
                    if attempt + 1 == MAX_ATTEMPTS || delay.is_none() {
                        return Err(e.into());
                    }
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Err(error(TelegramErrorCode::Network).into()),
                        _ = tokio::time::sleep(Duration::from_secs(u64::from(delay.unwrap_or(1)))) => {}
                    }
                }
            }
        }
        unreachable!("bounded attempts always return")
    }

    async fn call_once<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &[u8],
        mutation: bool,
    ) -> Result<T, BotApiError> {
        let url = format!("{}/bot{}/{method}", self.base_url, self.token.expose());
        let mut response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| sanitize_reqwest(e, mutation))?;
        let status = response.status();
        let status_error = match status.as_u16() {
            401 => Some(TelegramErrorCode::Unauthorized),
            409 => Some(TelegramErrorCode::PollConflict),
            429 => Some(TelegramErrorCode::RateLimited),
            500..=599 => Some(TelegramErrorCode::ProviderUnavailable),
            _ if !status.is_success() => Some(TelegramErrorCode::InvalidResponse),
            _ => None,
        };
        // HTTP hata sınıfı bozuk/aşırı gövdede de korunur; gövde hiçbir zaman loglanmaz.
        let fallback = status_error.unwrap_or(TelegramErrorCode::InvalidResponse);
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(
                status_error.map_or(BotApiError::ResponseTooLarge, |code| error(code).into())
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| status_error.map_or_else(|| sanitize_reqwest(e, mutation), error))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(
                    status_error.map_or(BotApiError::ResponseTooLarge, |code| error(code).into())
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Envelope<T> = serde_json::from_slice(&bytes).map_err(|_| error(fallback))?;
        if let Some(code) = status_error {
            return Err(TransportError {
                code,
                retry_after_seconds: if code == TelegramErrorCode::RateLimited {
                    envelope.parameters.and_then(|p| p.retry_after)
                } else {
                    None
                },
            }
            .into());
        }
        if !envelope.ok {
            let code = match envelope.error_code {
                Some(401) => TelegramErrorCode::Unauthorized,
                Some(409) => TelegramErrorCode::PollConflict,
                Some(429) => TelegramErrorCode::RateLimited,
                Some(500..=599) => TelegramErrorCode::ProviderUnavailable,
                _ => TelegramErrorCode::InvalidResponse,
            };
            return Err(TransportError {
                code,
                retry_after_seconds: if code == TelegramErrorCode::RateLimited {
                    envelope.parameters.and_then(|p| p.retry_after)
                } else {
                    None
                },
            }
            .into());
        }
        envelope
            .result
            .ok_or_else(|| error(TelegramErrorCode::InvalidResponse).into())
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<u16>,
    parameters: Option<Parameters>,
    // description bilerek alınmaz: sır yankısı üst katmana taşınamaz.
}

#[derive(Deserialize)]
struct Parameters {
    retry_after: Option<u32>,
}

fn error(code: TelegramErrorCode) -> TransportError {
    TransportError {
        code,
        retry_after_seconds: None,
    }
}

fn sanitize_reqwest(error_value: reqwest::Error, mutation: bool) -> TransportError {
    let safe = error_value.without_url();
    let code = if mutation && !safe.is_connect() && !safe.is_builder() {
        TelegramErrorCode::DeliveryUncertain
    } else if safe.is_timeout() {
        TelegramErrorCode::Timeout
    } else {
        TelegramErrorCode::Network
    };
    error(code)
}

// Yalnız sabit enum ve sayısal retry süresi; source zinciri yoktur.
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Telegram transport: {:?}", self.code)
    }
}
impl std::error::Error for TransportError {}

impl TelegramTransport for BotApiClient {
    fn get_me(&self) -> BoxFuture<'_, Result<TelegramUser, TransportError>> {
        Box::pin(async { self.call("getMe", serde_json::json!({}), true).await })
    }
    fn get_updates(
        &self,
        offset: Option<i64>,
    ) -> BoxFuture<'_, Result<Vec<TelegramUpdate>, TransportError>> {
        Box::pin(async move {
            self.get_updates_checked(offset)
                .await
                .map_err(BotApiError::into_transport)
        })
    }
    fn send_message(
        &self,
        message: SendMessage,
    ) -> BoxFuture<'_, Result<SentMessage, TransportError>> {
        Box::pin(async move { self.call("sendMessage", message, false).await })
    }
    fn edit_message_text(
        &self,
        message: EditMessageText,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let _: SentMessage = self.call("editMessageText", message, false).await?;
            Ok(())
        })
    }
    fn answer_callback_query(
        &self,
        answer: AnswerCallbackQuery,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            let accepted: bool = self.call("answerCallbackQuery", answer, false).await?;
            if accepted {
                Ok(())
            } else {
                Err(error(TelegramErrorCode::InvalidResponse))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::contracts::InlineKeyboardMarkup;
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TOKEN: &str = "12345:synthetic_TOKEN-1";

    struct MockServer {
        address: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl MockServer {
        async fn start(replies: Vec<String>, delay: Duration) -> Self {
            Self::start_with_body_delay(replies, delay, Duration::ZERO).await
        }

        async fn start_with_body_delay(
            replies: Vec<String>,
            delay: Duration,
            body_delay: Duration,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            let task = tokio::spawn(async move {
                for reply in replies {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0; 4096];
                        let count = socket.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        assert!(request.len() < 32 * 1024);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|s| s.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    captured
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(request).unwrap());
                    tokio::time::sleep(delay).await;
                    let (headers, body) = reply.split_once("\r\n\r\n").unwrap();
                    let _ = socket
                        .write_all(format!("{headers}\r\n\r\n").as_bytes())
                        .await;
                    tokio::time::sleep(body_delay).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                }
            });
            Self {
                address,
                requests,
                task,
            }
        }

        fn client(&self, timeout: Duration) -> BotApiClient {
            BotApiClient::for_test(
                SecretToken::new(TOKEN.into()).unwrap(),
                CancellationToken::new(),
                self.address,
                timeout,
            )
            .unwrap()
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    fn response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn message() -> SendMessage {
        SendMessage {
            chat_id: 42,
            text: "Sentetik Türkçe yanıt 😀 <b>düz metin</b>".into(),
            disable_notification: true,
            reply_markup: None,
        }
    }

    #[tokio::test]
    async fn typed_methods_and_wire_contract() {
        let mock = MockServer::start(vec![
            response(200, r#"{"ok":true,"result":{"id":7,"is_bot":true}}"#),
            response(200, r#"{"ok":true,"result":[{"update_id":3,"message":{"message_id":8,"from":{"id":42,"is_bot":false},"chat":{"id":42,"type":"private"},"text":"merhaba","photo":[{}]}}]}"#),
            response(200, r#"{"ok":true,"result":{"message_id":9}}"#),
            response(200, r#"{"ok":true,"result":{"message_id":9}}"#),
            response(200, r#"{"ok":true,"result":true}"#),
        ], Duration::ZERO).await;
        let client = mock.client(REQUEST_TIMEOUT);
        assert_eq!(client.get_me().await.unwrap().id, 7);
        let updates = client.get_updates(Some(3)).await.unwrap();
        assert!(
            updates[0]
                .message
                .as_ref()
                .unwrap()
                .extra
                .contains_key("photo")
        );
        assert_eq!(client.send_message(message()).await.unwrap().message_id, 9);
        client
            .edit_message_text(EditMessageText {
                chat_id: 42,
                message_id: 9,
                text: "tamam".into(),
                reply_markup: InlineKeyboardMarkup {
                    inline_keyboard: vec![],
                },
            })
            .await
            .unwrap();
        client
            .answer_callback_query(AnswerCallbackQuery {
                callback_query_id: "nonce".into(),
                text: "tamam".into(),
            })
            .await
            .unwrap();
        let requests = mock.requests.lock().unwrap();
        for (request, method) in requests.iter().zip([
            "getMe",
            "getUpdates",
            "sendMessage",
            "editMessageText",
            "answerCallbackQuery",
        ]) {
            assert!(request.starts_with(&format!("POST /bot{TOKEN}/{method} ")));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(body.get("parse_mode").is_none());
            if method == "getUpdates" {
                assert_eq!(body["timeout"], 25);
                assert_eq!(body["limit"], 20);
                assert_eq!(body["offset"], 3);
                assert_eq!(
                    body["allowed_updates"],
                    serde_json::json!(["message", "callback_query"])
                );
            }
            if method == "sendMessage" {
                assert_eq!(body["text"], message().text);
            }
        }
    }

    #[tokio::test]
    async fn unauthorized_conflict_and_rate_limit_are_safe() {
        for (status, expected) in [
            (401, TelegramErrorCode::Unauthorized),
            (409, TelegramErrorCode::PollConflict),
            (429, TelegramErrorCode::RateLimited),
        ] {
            let body = format!(
                r#"{{"ok":false,"error_code":{status},"description":"{TOKEN}","parameters":{{"retry_after":120}}}}"#
            );
            let mock = MockServer::start(vec![response(status, &body)], Duration::ZERO).await;
            let failure = mock.client(REQUEST_TIMEOUT).get_me().await.err().unwrap();
            assert_eq!(failure.code, expected);
            assert_eq!(
                failure.retry_after_seconds,
                if status == 429 { Some(120) } else { None }
            );
            assert_eq!(mock.count(), 1);
        }
    }

    #[tokio::test]
    async fn status_survives_truncated_and_delayed_bodies_without_poll_retry() {
        for (status, expected) in [
            (401, TelegramErrorCode::Unauthorized),
            (409, TelegramErrorCode::PollConflict),
            (429, TelegramErrorCode::RateLimited),
        ] {
            for delayed in [false, true] {
                let reply = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{"
                );
                let mock = MockServer::start_with_body_delay(
                    vec![reply],
                    Duration::ZERO,
                    if delayed {
                        Duration::from_millis(200)
                    } else {
                        Duration::ZERO
                    },
                )
                .await;
                let failure = mock
                    .client(Duration::from_millis(100))
                    .get_updates(Some(9))
                    .await
                    .err()
                    .unwrap();
                assert_eq!(failure.code, expected);
                assert_eq!(failure.retry_after_seconds, None);
                assert_eq!(mock.count(), 1);
            }
        }
    }

    fn unicode_updates(count: usize, text_length: usize) -> String {
        let updates: Vec<_> = (0..count)
            .map(|i| {
                serde_json::json!({
                    "update_id": 9 + i,
                    "message": {"message_id": i, "chat": {"id": 42, "type": "private"},
                        "text": "界".repeat(text_length)}
                })
            })
            .collect();
        serde_json::json!({"ok": true, "result": updates}).to_string()
    }

    #[tokio::test]
    async fn unicode_batches_reduce_limit_without_advancing_offset() {
        let large = unicode_updates(100, 4096);
        assert!(large.len() > MAX_RESPONSE_BYTES);
        let small = unicode_updates(20, 4096);
        assert!(small.len() < MAX_RESPONSE_BYTES);
        let mock = MockServer::start(vec![response(200, &small)], Duration::ZERO).await;
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .get_updates(Some(9))
                .await
                .unwrap()
                .len(),
            20
        );
        assert_eq!(mock.count(), 1);

        // A larger individual update still requires adaptive reduction.
        let mock = MockServer::start(
            vec![
                response(200, &unicode_updates(20, 20_000)),
                response(200, &unicode_updates(10, 20_000)),
            ],
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .get_updates_checked(Some(9))
                .await
                .unwrap()
                .len(),
            10
        );
        let requests = mock.requests.lock().unwrap();
        for (request, limit) in requests.iter().zip([20, 10]) {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["offset"], 9);
            assert_eq!(body["limit"], limit);
        }
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn single_oversized_update_is_distinct_for_bridge() {
        let huge = unicode_updates(1, MAX_RESPONSE_BYTES / 3 + 1);
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{huge}\r\n0\r\n\r\n",
            huge.len()
        );
        for reply in [response(200, &huge), chunked] {
            let mock = MockServer::start(vec![reply; 5], Duration::ZERO).await;
            let failure = mock
                .client(REQUEST_TIMEOUT)
                .get_updates_checked(None)
                .await
                .err()
                .unwrap();
            assert!(matches!(failure, BotApiError::ResponseTooLarge));
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            for (request, limit) in requests.iter().zip([20, 10, 5, 2, 1]) {
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert!(body["offset"].is_null());
                assert_eq!(body["limit"], limit);
            }
        }
    }

    #[tokio::test]
    async fn bounded_exponential_retry_and_retry_after() {
        let mock = MockServer::start(vec![response(503, "bad gateway"); 3], Duration::ZERO).await;
        let start = tokio::time::Instant::now();
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .get_me()
                .await
                .err()
                .unwrap()
                .code,
            TelegramErrorCode::ProviderUnavailable
        );
        assert_eq!(mock.count(), 3);
        assert!(start.elapsed() >= Duration::from_secs(3));
        let mock = MockServer::start(
            vec![
                response(429, r#"{"ok":false,"parameters":{"retry_after":1}}"#),
                response(200, r#"{"ok":true,"result":{"message_id":9}}"#),
            ],
            Duration::ZERO,
        )
        .await;
        let start = tokio::time::Instant::now();
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .send_message(message())
                .await
                .unwrap()
                .message_id,
            9
        );
        assert_eq!(mock.count(), 2);
        assert!(start.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn timeout_connection_refused_and_uncertain_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = BotApiClient::for_test(
            SecretToken::new(TOKEN.into()).unwrap(),
            CancellationToken::new(),
            address,
            REQUEST_TIMEOUT,
        )
        .unwrap();
        assert_eq!(
            client.get_me().await.err().unwrap().code,
            TelegramErrorCode::Network
        );
        let mock =
            MockServer::start(vec![response(200, "{}"); 3], Duration::from_millis(150)).await;
        assert_eq!(
            mock.client(Duration::from_millis(50))
                .get_me()
                .await
                .err()
                .unwrap()
                .code,
            TelegramErrorCode::Timeout
        );
        assert_eq!(mock.count(), 3);
        let mock = MockServer::start(vec![response(200, "{}")], Duration::from_millis(150)).await;
        assert_eq!(
            mock.client(Duration::from_millis(50))
                .send_message(message())
                .await
                .err()
                .unwrap()
                .code,
            TelegramErrorCode::DeliveryUncertain
        );
        assert_eq!(mock.count(), 1);
        let mock = MockServer::start(vec![response(503, "bad gateway")], Duration::ZERO).await;
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .send_message(message())
                .await
                .err()
                .unwrap()
                .code,
            TelegramErrorCode::ProviderUnavailable
        );
        assert_eq!(mock.count(), 1);
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        let destination = MockServer::start(vec![response(200, "{}")], Duration::ZERO).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.address
        );
        let mock = MockServer::start(vec![redirect], Duration::ZERO).await;
        assert_eq!(
            mock.client(REQUEST_TIMEOUT)
                .get_me()
                .await
                .err()
                .unwrap()
                .code,
            TelegramErrorCode::InvalidResponse
        );
        assert_eq!(destination.count(), 0);
    }

    #[tokio::test]
    async fn oversized_and_malformed_responses_are_rejected() {
        let huge = "x".repeat(MAX_RESPONSE_BYTES + 1);
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{huge}\r\n0\r\n\r\n",
            huge.len()
        );
        for reply in [
            response(200, &huge),
            chunked,
            response(200, "{broken"),
            response(200, r#"{"ok":true,"result":{"unexpected":true}}"#),
            response(200, r#"{"ok":false}"#),
        ] {
            let mock = MockServer::start(vec![reply], Duration::ZERO).await;
            assert_eq!(
                mock.client(REQUEST_TIMEOUT)
                    .get_me()
                    .await
                    .err()
                    .unwrap()
                    .code,
                TelegramErrorCode::InvalidResponse
            );
            assert_eq!(mock.count(), 1);
        }
    }

    #[tokio::test]
    async fn request_and_retry_sleep_are_cancellable() {
        for (reply, delay) in [
            (response(200, "{}"), Duration::from_secs(30)),
            (response(503, "{}"), Duration::ZERO),
        ] {
            let mock = MockServer::start(vec![reply], delay).await;
            let client = mock.client(REQUEST_TIMEOUT);
            let cancel = client.cancel.clone();
            let pending = tokio::spawn(async move { client.get_me().await.err().unwrap() });
            tokio::time::timeout(Duration::from_secs(2), async {
                while mock.count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
            let failure = tokio::time::timeout(Duration::from_millis(200), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failure.code, TelegramErrorCode::Network);
            assert_eq!(mock.count(), 1);
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_scrubbing_covers_url_debug_and_error_chain() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let encoded = TOKEN.replace(':', "%3A");
        for reply in [
            response(
                401,
                &format!(r#"{{"ok":false,"description":"{TOKEN} {encoded}"}}"#),
            ),
            response(200, &format!("broken {TOKEN} {encoded}")),
        ] {
            let mock = MockServer::start(vec![reply], Duration::ZERO).await;
            let failure = mock.client(REQUEST_TIMEOUT).get_me().await.err().unwrap();
            let chain = anyhow::Error::new(failure).context("safe boundary");
            let json = serde_json::json!({"code": failure.code, "retry_after_seconds": failure.retry_after_seconds}).to_string();
            let output = format!("{failure} {failure:?} {chain:#} {chain:?} {json}");
            tracing::warn!(error = %failure, diagnostic = ?failure, "synthetic failure");
            assert!(!output.contains(TOKEN));
            assert!(!output.contains(&encoded));
        }
        // URL taşıyan reqwest hatası da aynı sınırda temizlenir.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let raw = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
            .get(format!("http://{address}/bot{TOKEN}/getMe"))
            .send()
            .await
            .unwrap_err();
        assert!(raw.url().unwrap().as_str().contains(TOKEN));
        let failure = sanitize_reqwest(raw, false);
        assert!(!format!("{failure:?} {failure}").contains(TOKEN));
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("synthetic failure"));
        assert!(!logs.contains(TOKEN), "tracing leaked synthetic token");
        assert!(!logs.contains(&encoded));
        assert!(
            !format!(
                "{:?} {}",
                SecretToken::new(TOKEN.into()).unwrap(),
                SecretToken::new(TOKEN.into()).unwrap()
            )
            .contains(TOKEN)
        );
    }

    #[test]
    fn production_url_ignores_environment_override() {
        if std::env::var_os("ULTRON_SYNTHETIC_URL_PROBE").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "telegram::client::tests::production_url_ignores_environment_override",
                ])
                .env("ULTRON_SYNTHETIC_URL_PROBE", "1")
                .env("TELEGRAM_API_URL", "http://127.0.0.1:1")
                .env("TELEGRAM_BOT_API_URL", "http://127.0.0.1:1")
                .env("ULTRON_TELEGRAM_API_URL", "http://127.0.0.1:1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let client = BotApiClient::new(
            SecretToken::new(TOKEN.into()).unwrap(),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(client.base_url, "https://api.telegram.org");
        assert!(REQUEST_TIMEOUT > Duration::from_secs(25));
    }
}
