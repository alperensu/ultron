# Ultron Aşama 4 — Telegram Kapısı: Uygulama Planı

> **For agentic workers:** Bu plan dış koordinatör tarafından görev görev yürütülür. Her Codex şeridi yalnız kendi OWNS alanını yazar; alt ajan başlatmaz. Bağımsız doğruluk ve güvenlik incelemelerini koordinatör düzenler. Adımlar checkbox (`- [ ]`) ile izlenir.

**Goal:** Ultron'a yalnız izinli özel Telegram konuşmalarından emir, araç onayı ve bildirim kapısı eklemek; kota bekleyen işi kalıcı olarak takip edip güvenli biçimde devam ettirmek ve çalışan tur/zamanlayıcı süresince sistemi uyanık tutmak.

**Architecture:** Köprü `ultrond` içinde iptal edilebilir bir Tokio görevidir; Bot API `getUpdates` uzun yoklaması kullanır. Webhook, yeni servis veya dışarı açık port yoktur. Telegram ile masaüstü aynı daemon uygulama katmanından tur başlatır ve aynı onay çözücüsüne ulaşır. SQLite bağlama, gelen güncelleme kimliği, tur kaynağı ve kota bekleme kaydının kalıcı kaynağıdır; token yalnız Windows Credential Manager ve daemon belleğindedir.

**Tech Stack:** Mevcut Rust/Tokio/rusqlite, Tauri/React, workspace `reqwest = 0.12.28` (rustls) ve `windows-sys = 0.59`. Yeni Telegram SDK'sı gerekmez. `reqwest` workspace tanımında `json` özelliği yoktur: `serde_json` ile gövde kodla/çöz; sırf kolaylık için bağımlılık yükseltme. Credential Manager için mevcut `windows-sys` özelliklerine `Win32_Security_Credentials` eklenir.

**Spec:** `docs/superpowers/specs/2026-11-15-harness-design.md` §6.7–6.9 (kota, otomatik devam, güvenlik, Telegram), §7 Aşama 4 ve §11 madde 4. Aşama 3 kabulü: `docs/reports/2026-11-17-asama3-kabul.md`. İncelenen başlangıç: **83750ad**; önceden bulunan `docs/design/` kapsam dışıdır.

## Global Constraints

- Gerçek token, izinli ID, kullanıcı yolu, gerçek mesaj veya özel fixture repoya/rapora girmez. Bu plan hazırlanırken `.env`, kullanıcı config'i ve Credential Manager değerleri okunmaz. Koordinatörün bildirdiği hedef adı `ultron/telegram-bot-token` bir sır değeri değildir.
- Üretim taban adresi sabit `https://api.telegram.org` olur. Ortam değişkeni, config, IPC veya webview bunu değiştiremez. Yerel HTTP sunucusu yalnız `cfg(test)` bağımlılık enjeksiyonuyla açılır; mevcut `test-hooks` özelliği bile üretim URL geçersiz kılma kapısı oluşturmaz.
- Yetkilendirme **üç koşulu birlikte** ister: `from.id ∈ allowed_user_ids`, `chat.type == private`, `chat.id == from.id`. Callback için `callback_query.from` ile bağlı mesajın sohbeti doğrulanır; inline-message/mesajsız callback reddedilir. Kimliği eksik, grup, kanal, bot gönderen, düzenlenen mesaj ve desteklenmeyen ekler yürütülmez.
- İzinli liste boş/yoksa köprü **kapalıdır**; token okumak ve Telegram ağına çıkmak gerekmez. ID'ler pozitif `i64`, tekrarsız ve sınırlı sayıda olmalı. Reddedilen güncellemede yalnız toplam sayaç tutulur; içerik, isim ve ID loglanmaz/saklanmaz. Güncelleme offset'i içerik değildir.
- `SecretToken`/token taşıyan IPC için `Debug`, `Display`, anyhow hata zinciri, tracing, Bot API hata gövdesi ve URL'ler sır sızdırmaz. Ham `reqwest::Error` hiçbir üst katmana çıkmaz; `without_url()` ve sabit hata sınıfı kullanılır. HTTP yönlendirmeleri kapalıdır; token başka hosta taşınmaz.
- Telegram komut katmanı genel bir `Request` vekili değildir. Trash restore/purge, politika/izinli liste değişikliği, import ve Full tur yükseltme Telegram'dan yapılamaz; bu idari işlemler araç onayı olarak da açılamaz. Doğrudan araç komutu metni çalıştırılmaz; serbest metin yalnız normal kullanıcı iletisi olur.
- Otomatik devam, özgün kullanıcı hedefini sürdürür; yeni görev/yetki üretmez. Yeni riskli araç adımı mevcut `Approvals::check_owned` yolunda bekler; yanıt yoksa mevcut 300 saniyelik güvenli ret korunur. Eski onaylar ve tur belirteçleri yeniden kullanılmaz.
- Testler geçici `ULTRON_HOME`, `ULTRON_QUOTA_HOME`, benzersiz pipe ve sentetik motorlarla çalışır; `SummarizerMode::Off`, `claude_live_quota = false`. Gerçek Claude/Codex/agy, kullanıcı kota kaynakları, gerçek Credential Manager yazımı ve scheduled task kurulumu otomatik testlere dahil değildir.
- Her kod şeridinin ilk dönüşü: değişen dosyalar, sözleşme sapmaları, çalıştırılan kontroller ve engeller. Ortak dosya ihtiyacı koordinatöre döner; başka şeridin alanı izinsiz düzenlenmez. Yeni modül iskeletleri ürün davranışı taklit etmez; kabulde boş uygulama/TODO kalmaz.

## Mevcut Koddan Başlangıç Noktaları

| Mevcut dosya / sembol | Aşama 4'te kullanım ve eksik |
| --- | --- |
| `ultron-core/src/config.rs`: `Config::load`, `Config`, `EngineId` | `[telegram]` alanı yok; serde bilinmeyen tabloyu sessizce yok sayıyor. Var olan ayarlar korunarak eklenecek. |
| `ultrond/src/server.rs`: `start`, `dispatch`, `send_audited`, `wait_elevation` | Yaşam döngüsü ve tur başlatma burada; Telegram ayrı bir Send uygulaması kopyalamamalı. `State` ve `ConnectionAuth` özel türler. |
| `ultron-core/src/session.rs`: `reserve_turn`, `send_reserved`, `switch`, `cancel` | Tek sohbet/tur ve çalışma alanı kilitleri var; otomatik devam için özgün kullanıcı olayını yinelemeyen giriş yok. |
| `ultron-core/src/events.rs`: `QuotaExhausted { resets_at }`, `EngineSwitched`, `SwitchReason::Quota` | Motor geçişi olayı zaten sözleşme. Yeni kota yaşam döngüsü olayları bununla birlikte kullanılacak. |
| `ultron-core/src/db/mod.rs`: `SessionStatus::WaitingQuota`, `open_sessions` | Kota durumu mevcut; beklenen işin reset saati, kaynak turu ve devam sahipliği için tablo yok. |
| `ultrond/src/approvals.rs`: `check_owned`, `resolve`, `recover_pending`, `subscribe_global` | İlk karar DB'de atomik; restart bekleyen onayları kapatıyor; en çok 300 sn. Sohbetsiz onaylar için küresel akış gerekli. |
| `server.rs`: `require_approver`, `ResolveApproval` kolu | `by=desktop|telegram` yalnız eski etiket; kalıcı karar `ResolvedBy::Approver`. Telegram bu korumayı gevşetmeyecek. |
| `session.rs`: `request_elevation`, `server.rs`: `wait_elevation` | `ultron.turn.elevate` de araç görünümlü bir onaydır; Telegram araç onayı filtresinden açıkça çıkarılmalı. |
| `ultron-core/src/quota/mod.rs`: `QuotaService::report`, `QuotaReport`, `QuotaWindow` | Masaüstüyle aynı `/limit` kaynağı; Agy ölçümü Unknown. Reset hakkı otomatik kullanılmaz. |
| `engine/mod.rs`: `is_quota_message`; `engine/agy.rs`: `result_events` | `ed60475`, `error` alanını ve `quota reached` tanımasını ekliyor; Agy `resets_at` hâlâ `None`. |
| `engine/claude.rs`: `reset_from`, rate-limit olayı; `engine/codex.rs`: `failure` | Claude unix/reset alanını kısmen ayrıştırıyor; Codex hata reset'i yok. Yapılandırılmış kota pencereleri yeniden kullanılabilir. |
| `ultron-core/src/eventlog.rs`: `append`, `subscribe`, `write_persisted_transcript` | DB/döküm/yayın ve canlı tur belirteci maskelemesi mevcut. Telegram token'ı bu maske tarafından korunuyor varsayılmayacak. |
| `ultron-core/src/routing.rs`: `route`; `server.rs`: `Request::Route` | Proje eşleşmesi → son aktif proje → projesiz. Masaüstü `isAmbiguous`: confidence < 0.6 ve alternatif varsa soru. |
| `ultrond/src/keepawake.rs`: `KeepAwake::acquire`, `holders`, guard Drop | Ayrılmış OS iş parçacığında `ES_CONTINUOUS | ES_SYSTEM_REQUIRED`; ekran bayrağı yok. Yalnız aktif Send bugün tutucu alıyor. |
| `ultron/src/commands.rs`: `Cmd`, `connect_approver`, `one_reply` | CLI ekleme noktası; onaylayıcı bağlantısının SID/token doğrulaması korunacak. |
| `apps/desktop/src-tauri/src/bridge.rs`: `Bridge::request`; `src/screens/Settings.tsx` | Yönetim çağrıları onaylayıcı bağlantısına ayrılıyor; Settings'te Telegram yok. Masaüstü varlığı/heartbeat kaydı da yok. |

Yukarıdaki `ultron-core/...` ve `ultrond/...` yolları `crates/` altındadır. Sonraki görevlerde tam depo yolları kullanılır. Burada sayılan yeni tür/adlar aşağıda **önerilen sözleşmedir**, mevcut işlev diye sunulmaz.

## Davranış ve Sözleşme Kararları

**Config:** `[telegram] allowed_user_ids = []`, `notify = "always"`, `auto_continue_enabled = true`, `auto_continue_wait_minutes = 45`, `fallback_order = []`. Otomatik devam daemon işlevi olduğundan Telegram kapalıyken de kota beklemesi kaydedilir; bildirim gönderilemez. Boş fallback sırası başka motoru kendiliğinden seçmez. Süre 1–1440 dakika, motor sırası tekrarsız ve bilinen `EngineId` değerleri; devre dışı motorlar çalışma anında atlanır. Eski dosyada tablo yoksa mevcut motor/özetleyici davranışı değişmez; ilk kota hatasında bu yeni devam politikası uygulanır. `enabled=false` mevcut zamanlayıcıyı durdurur, kaydı beklemede bırakır; yetki veya işi silmez.

**Away:** Masaüstü Rust kabuğu 30 saniyede bir onaylayıcı `DesktopHeartbeat { instance_id }` gönderir. Daemon son **90 saniye** içinde en az bir geçerli heartbeat aldıysa masaüstü var sayar; aksi halde `away=true`. Soketin açık olması, generic CLI/engine IPC trafiği veya pencere odağı kullanılmaz. Süre monotonic saatle ölçülür; daemon açılışı away sayılır. Bu, kişinin fiziksel olarak masada olduğunu iddia etmez. `notify=away`, masaüstü varken kendiliğinden gönderilen arka plan bildirimlerini bastırır; Telegram komut yanıtları, Telegram kaynaklı tur yanıtları ve o turun araç onayları her zaman döner. Uzakta yeni araç onayı daima bildirilir. Sessiz motor geçiş bildirimi, `disable_notification=true` demektir; olayın/bildirimin yok edilmesi değildir.

**Yerel yönetim IPC'si:** Önerilen istekler `TelegramStatus`, `TelegramSetToken { token }`, `TelegramTest`, `GetTelegramSettings`, `SetTelegramSettings { notify, auto_continue_enabled, auto_continue_wait_minutes, fallback_order }`, `DesktopHeartbeat { instance_id }`. Token yazımı, test gönderimi, ayar mutasyonu ve heartbeat onaylayıcı gerektirir; status sır/ID değeri döndürmez. İzinli ID listesi bu arayüzlerden değişmez; kullanıcının config'inde korunur. Token yazımı Credential Manager'a daemon tarafından yapılır; başarıdan sonra köprü kontrollü yeniden başlar, iki poller oluşmaz. Webview token setter'a erişmez.

**TurnService:** Önerilen daemon içi girişler `submit_user_message`, `resume_quota_step`, `resolve_tool_approval`, `status_snapshot`; bunlar IPC `dispatch` ve Telegram tarafından ortak kullanılır. `AcceptedTurn` benzersiz `logical_turn_id`, `chat_id`, özgün kullanıcı olay seq'si ve completion alıcısı döndürür. `TurnOrigin::Telegram { user_id, private_chat_id, update_id }` istemci tarafından seçilebilir IPC alanı değildir; sadece doğrulanmış güncellemeden üretilir. Ayrı bir engine/LLM veya shell yolu açılmaz.

**Bağlama ve yanıt:** Her izinli Telegram kullanıcısının bağımsız bir Ultron sohbet bağlaması vardır. `/yeni` bağlamayı temizler; sonraki metin yeni sohbeti aynı `route` algoritmasıyla oluşturur. Belirsizlik eşiği masaüstüyle aynı; seçim için kısa ömürlü, kullanıcıya bağlı inline nonce ve “Projesiz” seçeneği gönderilir, motor seçimden önce başlamaz. `/sohbetler` son 10 arşivlenmemiş sohbeti gösterir; seçim nonce'u ham chat ID yerine geçer. Bağlı sohbette mevcut proje/cwd/motor korunur; yeni metin sohbeti başka projeye yönlendirmez. Dolu/çalışan sohbete gönderimde açık “tur sürüyor” yanıtı verilir, sessiz kuyruk oluşturulmaz.

**Çıktı:** Sadece ilgili mantıksal Telegram turunun final asistan metni ve güvenli sonuç özeti gönderilir; araç çıktısı, ham stderr, `Unknown.raw` ve debug dökümleri gönderilmez. `parse_mode` kullanılmaz. Gövde, Türkçe kısaltma notu dahil en çok 4096 sınırını aşmaz; Unicode sınırını bozmayacak ve emoji için UTF-16 uzunluğunu da aşmayacak şekilde kırpılır. Not: “Yanıt kısaltıldı; tamamı masaüstünde.” Ek LLM özetleme çağrısı yoktur. Bağlama daha sonra değişse bile sonuç özgün Telegram alıcısına gider; diğer masaüstü turlarının finali bu kapsama alınmaz.

## Review Focus

1. Üçlü kimlik filtresi hem message hem callback'te; boş liste gerçek ağ/credential erişimini kapatıyor mu?
2. Hata URL'sindeki token, percent-encoded token, Bot API açıklaması, `Debug` ve hata zinciri hiçbir log/olay/IPC yanıtında kalıyor mu?
3. Callback nonce'u kullanıcı + özel sohbet + message_id + approval_id + karar + son kullanım süresine bağlı mı; çift tıklama/masaüstü yarışı ilk kararı koruyor mu?
4. Full yükseltme ile araç onayı ayrılıyor mu; Telegram yasak idari işlemleri dolaylı araç onayıyla açabiliyor mu?
5. Kota devamı özgün kullanıcı iletisini ve tamamlanmış araç yan etkilerini körlemesine tekrarlıyor mu; restart/Cancel/yeni mesaj eski beklemeyi geçersiz kılıyor mu?
6. Bekleme sahipliği ve olay seq sınırı kalıcı mı; broadcast taşmasında DB'den telafi var mı; saat atlaması/timer tekrarında çift tur oluşuyor mu?
7. Keep-awake tutucu sayısı artık çalışan tur sayısı sanılıyor mu; süresiz bekleme sistemi gereksiz yere uyanık tutuyor mu?
8. UI bağlantı durumunu sahte başarıyla göstermeden, aynı ayarları ve izinli ID sayısını sır taşımadan gösterebiliyor mu?

## Sıralama ve Paralellik

| Dalga | Görevler | Başlama kapısı / ortak dosya kuralı |
| --- | --- | --- |
| A | Task 1 | Sözleşme, modül kaydı ve tek seferlik manifest değişiklikleri önce. |
| B | Task 2 ∥ Task 3 | Task 1 kabulü; HTTP/credential ve SQLite dosyaları ayrıdır. |
| C | Task 4 | Task 2 ve 3 kabulü; ortak daemon uygulama sınırı sabitlenir. |
| D | Task 5 ∥ Task 6 ∥ Task 7 ∥ Task 9 | Task 4 sonrası bağımsız sahiplikler; kapasiteye göre şeritler sıraya alınır. |
| E | Task 8 | Task 3, 4, 7 kabulü; Task 5/6/9 ile paralel olabilir. |
| F | Task 10 | Tüm arka uç şeritleri bitince `server.rs` tek yazıcıyla bağlanır. |
| G | Task 11 | Arka uç/IPC kabulünden sonra desktop; UI en son uygulama işi. |
| H | Task 12 | Birleşik doğrulama, koordinatörün iki bağımsız incelemesi ve canlı kabul. |

`∥` aynı anda çalışabilir demektir, aynı dosyayı paylaşabilir demek değildir. Task 1'in oluşturduğu modül iskeletlerinin yazma sahipliği aşağıdaki görev başladığında o göreve devredilir. Manifest, modül kaydı, migration veya `server.rs` için sonradan gereken ortak düzeltmeler Task 10 sahibine iletilir; paralel şeritler bu dosyalara yazmaz.

---

### Task 1: Config, IPC ve şerit sözleşmelerini sabitle

**Scope / Owner:** `P4-contracts` — tek temel şeridi; diğer işlerin derlenebilir sınırlarını ve varsayılanlarını oluşturur. Paralel başlamaz.

**OWNS / Files:** Modify `Cargo.toml`, `Cargo.lock`, `crates/ultrond/Cargo.toml`, `crates/ultrond/src/lib.rs`, `crates/ultron-core/src/config.rs`, `crates/ultron-core/src/ipc.rs`, `crates/ultron-core/src/events.rs`; Create `crates/ultrond/src/telegram/{mod.rs,contracts.rs}`. Yalnız modül kaydı için `crates/ultrond/src/telegram/{client.rs,credentials.rs,control.rs,bridge.rs,commands.rs,notifications.rs,approval_callbacks.rs}`, `crates/ultrond/src/{turn_service.rs,auto_continue.rs}` iskeletlerini oluştur; davranış uygulaması ilgili sahibindir. Test `crates/ultron-core/tests/paths_config.rs` ve config/IPC birim testleri.

**DO NOT TOUCH:** `server.rs`, mevcut kullanıcı config'i/credential deposu, DB migration'ları, CLI, desktop, `docs/design/`, toolchain dosyaları.

**Interfaces — Produces:**
- `TelegramConfig`, `NotifyMode::{Always,Away}`, süre ve fallback doğrulaması; `Config::load` eski dosyaları kabul eder. `allowed_user_ids` hata/debug metninde basılmaz. Config'in mevcut türetilmiş `Debug` davranışı da gözden geçirilerek bu liste gizlenir.
- Yukarıdaki IPC sözleşmeleri, sır taşımayan `TelegramStatusSnapshot`, ayar DTO'su; `Request` özel `Debug` eşlemesi yeni token setter için yalnız `<gizli>` döndürür. `TelegramSetToken` argv/webview kaynağı değildir.
- `contracts.rs` içinde `TelegramTransport`, `CredentialStore`, `GatewayApplication`, enjekte edilebilir saat ve `TurnOrigin`/completion sözleşmeleri. Async trait için mevcut dil/boxed future imkânları; yeni async-trait bağımlılığı gerekmez.
- Task 8'in üreteceği `QuotaWaitStarted`, `QuotaResumed`, `QuotaHandoff`, `QuotaWaitStopped` olaylarının alanlarını (`logical_turn_id`, motor/reset ve güvenli durum gerekçesi) burada sabitle ve `events.rs` içinde tanımla. Böylece Task 5 ve 8 birbirinin henüz yazılmamış enum varyantını beklemez; sonraki tüketici uyarlamaları kendi sahibine aittir.
- UI durum enum'u `Connected | MissingToken | MissingAllowedUsers | Error`; ilk başarılı Bot API doğrulaması/yoklaması öncesi “bağlı” denmez. Öncelik: boş liste → izinli kullanıcı yok, sonra token yok, sonra bağlantı hatası. Güvenli kısa hata kodu dışında ham sağlayıcı metni yok.
- `reqwest` ve gerekirse `serde` ultrond'a workspace üzerinden eklenir; Credential API özelliği tek manifest değişikliğinde eklenir. Test saati için Tokio `test-util` gerekiyorsa yalnız dev-dependency özelliği olarak açılır.

**Acceptance / Checks:**
- [ ] Eski config, boş/yok Telegram tablosu, geçerli liste, bozuk ID, tekrarlı fallback, bilinmeyen motor/notify ve süre sınırları için test; mevcut engines/reset_rights/summarizer korunur.
- [ ] Yeni IPC serde roundtrip ve `format!("{request:?}")` token/ID sızdırmama testleri; status JSON token/listenin kendisini içermez.
- [ ] `cargo test -p ultron-core --test paths_config`; `cargo test -p ultron-core --lib`; `cargo check -p ultrond`.
- [ ] İlk dönüş: kesin DTO/trait imzaları ve dosya sahipliği devri. Commit önerisi `feat(core): telegram yapılandırması ve kapı sözleşmeleri`.

### Task 2: Credential Manager ve güvenli Bot API istemcisi

**Scope / Owner:** `P4-telegram-client` — Task 1 sonrası Task 3 ile paralel; yalnız credential/HTTP sınırı.

**OWNS / Files:** `crates/ultrond/src/telegram/{credentials.rs,client.rs}` ve bu dosyalardaki birim testleri.

**DO NOT TOUCH:** Ortak manifest/kayıt dosyaları, `server.rs`, config dosyası, gerçek Credential Manager değerleri, callback/komut/DB/UI kodu.

**Interfaces — Produces:**
- `WindowsCredentialStore`: `CredReadW` / `CredWriteW` / `CredFree`, hedef `ultron/telegram-bot-token`, kullanıcıya bağlı generic credential. Başlangıçta okuma; bloklayan Win32 çağrısını Tokio worker'ında tutma. Credential blob uzunluğu/kodlaması ve API dönüş kodu doğrulanır; hata yalnız güvenli sınıf/Win32 kodudur.
- Token wrapper hiçbir biçimleyicide içeriği göstermez; bilinçli, dar ömürlü erişim dışında klonlama/serialize yok. Yazım öncesi uzunluk/ASCII/denetim karakteri doğrulaması; hata girdiyi alıntılamaz. CLI satır sonu dışında token sessizce değiştirilmez.
- Tipli `getMe`, `getUpdates(timeout=25)`, `sendMessage`, `editMessageText`, `answerCallbackQuery`. HTTP toplam timeout yoklama süresinden uzun; boyut sınırlı yanıt, iptal edilebilir istek, HTTP 429 `retry_after`, ağ/5xx için sınırlı üstel geri çekilme. 401 token hatası, 409 başka poller hatası status'a yansır; hızlı retry döngüsü olmaz.
- Üretim constructor sabit HTTPS host ve redirect kapalı istemci; yalnız `cfg(test)` constructor yerel `127.0.0.1` mock sunucuyu alır. Sentetik credential store testlerde gerçek Win32 okuma/yazma yapmaz.
- Tüm transport hataları boundary'de temizlenir: URL taşıyan reqwest hatası, API `description`, bozuk JSON ve beklenmeyen gövde ham olarak üst katmana/loga gitmez.

**Acceptance / Checks:**
- [ ] Yerel `tokio::net::TcpListener` mock'uyla başarılı çağrı, timeout, bağlantı reddi, 401/409/429/5xx, redirect, aşırı gövde ve malformed JSON testleri.
- [ ] `token_scrubbing_covers_url_debug_and_error_chain`: sentetik token'ın düz ve URL kodlu hali `Display`, `Debug`, hata zinciri, yakalanan tracing çıktısı ve dışarı taşınan JSON'da yok. API açıklamasında token yankılanması da test edilir.
- [ ] `production_url_ignores_environment_override`; `credential_missing_is_not_connected`; sahte CredentialStore roundtrip. Bu testler gerçek secret okumaz.
- [ ] `cargo test -p ultrond --lib telegram::client`; `cargo test -p ultrond --lib telegram::credentials`.
- [ ] İlk dönüş: güvenli hata türleri ve mock kurulum örneği. Commit `feat(ultrond): güvenli telegram istemcisi ve kimlik deposu`.

### Task 3: Telegram bağlaması ve kalıcı kota işi deposu

**Scope / Owner:** `P4-storage` — Task 1 sonrası Task 2 ile paralel. SQLite/migration için tek yazıcı; yeni şema eski veriye zarar vermez.

**OWNS / Files:** Modify `crates/ultron-core/src/db/{mod.rs,migrations.rs}`; Create `crates/ultron-core/src/db/{telegram.rs,quota_wait.rs}`; Test `crates/ultron-core/tests/{telegram_store.rs,quota_wait_store.rs}`. `db/approvals.rs` yalnız onayın kaynağını güvenli ayıran alan/constructor/sorgu için bu görevde seri olarak sahiplenilir, sonra Task 6'ya devredilir.

**DO NOT TOUCH:** `session.rs`, `server.rs`, credential/HTTP, politika kuralları, CLI/UI, mevcut kullanıcı SQLite dosyası.

**Interfaces — Produces:**
- Yeni ileri migration; sürüm numarası çalışma anındaki son migration'a göre seçilir, mevcut migration metni yeniden yazılmaz. `telegram_bindings(user_id PRIMARY KEY, chat_id NULL, updated_at)`, `telegram_poll_state(bot_id PRIMARY KEY, next_update_id)` ve izinli kabul kimlikleri için `telegram_inbox(bot_id, update_id, user_id, state, logical_turn_id)`; `(bot_id,update_id)` unique.
- `telegram_turn_origins(logical_turn_id PRIMARY KEY, chat_id, user_id, private_chat_id, user_event_seq, start_seq, end_seq, delivery_state)`; kaynak mantıksal tura bağlıdır, yalnız sohbetin “son Telegram kullanıcısı” alanı değildir. Gelen ham Telegram JSON'u ve reddedilen kullanıcı verisi saklanmaz; kabul edilmiş metin yalnız normal kullanıcı olayına yazılır.
- `quota_waits(id PRIMARY KEY, logical_turn_id UNIQUE, chat_id, source_session_id, engine, user_event_seq, checkpoint_seq, cwd, policy, reset_at NULL, state, generation, next_engine_index, attempt_count, next_attempt_at NULL, updated_at)`; bir sohbette tek etkin bekleme. FK/unique/check ve transaction ile `waiting → claimed → running → completed|waiting|cancelled|needs_attention` geçişleri. CAS/nesil koşulu çift sahiplenmeyi engeller.
- Kabul edilen işin özgün hedefini ve izin düzeyini olay referansından yeniden kur; Telegram token'ı, approver token'ı, `ULTRON_TURN_TOKEN`, nonce ve eski Full yetkisi saklama. Silinmiş/bozuk referans yeni işe dönüşmez; `needs_attention` olur.
- Approval kaynağı için `ApprovalKind::{PolicyTool,TurnElevation,Unknown}` alanı ve açık constructor'lar. Eski/belirsiz kayıt `Unknown` → yalnız masaüstü; `ultron.turn.elevate` kesinlikle tool-onayı sayılmaz. Category tek başına kaynak ispatı değildir.
- Inbox/turn kabulü için atomik depolama API'si: update'in yürütme kaydı ile kullanıcı olayı/iş kabulü aynı transaction'da bağlanır; checkpoint ilerlerken yeniden yürütme olmaz. Offset reset'i bot kimliğiyle ayrılır; token rotasyonu bot değiştirirse eski offset kullanılmaz.

**Acceptance / Checks:**
- [ ] Eski DB'den migration'da chat/events/approvals ve maskelenmiş içerik korunur; migration ikinci kez zararsızdır.
- [ ] Binding restart roundtrip, yinelenen update, aynı sohbete iki wait, CAS yarışı, generation iptali, bozuk event referansı ve inbox kabul transaction rollback testleri.
- [ ] `approval_kind_migration_defaults_to_desktop_only`; geçmiş bekleyen onaylara uzaktan yetki verilmez. Çözümün mevcut ilk-karar transaction'ı korunur.
- [ ] `cargo test -p ultron-core --test telegram_store`; `cargo test -p ultron-core --test quota_wait_store`; `cargo test -p ultron-core --test db`; `cargo test -p ultron-core --lib db::`.
- [ ] İlk dönüş: migration, transaction API'si, inbox crash sınırı. Commit `feat(core): telegram ve kota bekleme kalıcı kayıtları`.

### Task 4: Ortak daemon uygulama katmanı ve yerel Telegram yönetimi

**Scope / Owner:** `P4-application` — Task 2/3 kabulünden sonra. Köprü şeritlerinin kullanacağı dar uygulama yüzeyi; `server.rs` bağlaması Task 10'undur.

**OWNS / Files:** `crates/ultrond/src/turn_service.rs`, `crates/ultrond/src/telegram/control.rs`; Modify `crates/ultron/src/commands.rs`; Test `crates/ultron/tests/cli.rs` ve yeni `crates/ultron/tests/telegram.rs`. Birim testler ilgili daemon dosyalarında.

**DO NOT TOUCH:** `server.rs`, `approvals.rs`, `session.rs`, DB şeması/manifest/kayıt, HTTP/credential implementasyonu, UI.

**Interfaces — Produces:**
- `GatewayApplication` uygulaması için tur kabul/sonuç, list chats, route, quota, approvals ve binding servisleri. `dispatch` içindeki mevcut sıra referanstır: proje/cwd çöz → motor varlığını ve `reserve_turn` kilidini doğrula → kullanıcı olayını kabul transaction'ıyla bağla → onay/denetim/grant/çalıştırma → sonuç. Taşıma adaptöründen bağımsız; IPC streaming korunur.
- `send_audited`/`persist_audit` davranışını ortak runner'a taşıma taslağı; Task 10 bunların tek aktif uygulamasını bağlar. Ayrı Telegram çağrısı `SessionManager::send` üzerinden güvenlik adımlarını atlayamaz.
- Yerel config yazımı `toml::Table` üzerinden yalnız ilgili anahtarları günceller; bilinmeyen tablo, `allowed_user_ids`, motorlar/reset_rights korunur. `set_reset_rights` ile aynı **tek config mutex** + benzersiz temp + flush/sync/rename kullanımı Task 10'a açık entegrasyon şartıdır. Çalışma anındaki ayar snapshot'ı ancak başarılı yazımdan sonra değişir.
- `ultron telegram set-token`: `connect_approver` ile doğrulanmış daemon'a sınırlı stdin okumasından gelen token'ı gönderir. Token argümanı veya env fallback yok; en çok 4 KiB girdi, EOF/satır sonu, boş/bozuk/aşırı girdi güvenli hata; token stdout/stderr'de görünmez. Token değeri komut geçmişine yazılmaz.
- `ultron telegram status`: kapalı/token yok/bağlı/hata, izinli sayı ve güvenli ayar özeti. `ultron telegram test`: onaylayıcı, izinli private ID'lere sabit test metni; her alıcı sonucu toplam sayıyla gösterilir, sahte başarı yok. Boş liste/token yoksa ağ çağrısı yapılmaz.
- Tüm kontrol eylemlerinde rol kontrolü uygulama sınırında açıkça zorunlu; Telegram uygulama nesnesi token setter/settings/import yönetim yetkisi taşımaz.

**Acceptance / Checks:**
- [ ] CLI parse testinde argv token seçeneği reddedilir; stdin sentetik verisi çıktı/debug'da yok; requester setter/test/settings yapamaz.
- [ ] `config_update_preserves_telegram_ids_and_unknown_tables`, hatalı yazımda önceki dosya/snapshot korunur; reset rights ile yarış regresyonu Task 10'a aktarılır.
- [ ] Fake uygulamayla binding yeni/eski chat, çalışan tur reddi, project cwd ve sonucu doğru logical_turn'a bağlama.
- [ ] `cargo test -p ultron --test cli`; `cargo test -p ultron --test telegram`; `cargo test -p ultrond --lib turn_service`; `cargo test -p ultrond --lib telegram::control`. Bu şeritte fake uygulama/transport kullanılır; henüz bağlanmamış gerçek daemon üzerinden başarı iddia edilmez.
- [ ] İlk dönüş: Task 5/6/8 için derlenebilir uygulama imzaları ve ortak dosya entegrasyon listesi. Commit `feat: telegram yerel yönetimi ve ortak tur uygulaması`.

### Task 5: Uzun yoklama, emirler ve bildirimler

**Scope / Owner:** `P4-gateway` — Task 4 sonrası Task 6/7/9 ile paralel; Telegram çalışma döngüsünün tek sahibi.

**OWNS / Files:** `crates/ultrond/src/telegram/{bridge.rs,commands.rs,notifications.rs}` ve kendi birim testleri. HTTP/client, callback resolver ve server bağlantı kodu ayrı şeritlerdedir.

**DO NOT TOUCH:** `server.rs`, `telegram/mod.rs`, `contracts.rs`, DB/IPC/config, `approvals.rs`, `approval_callbacks.rs`, `session.rs`, CLI/UI.

**Interfaces — Produces:**
- Tek `run_gateway(cancel)`; `getUpdates` yalnız message/callback_query tiplerini ister. Her update yetkilendirilir, iş kabulü kalıcılaşır veya güvenle yok sayılır, sonra next offset ilerler. Retry/yeniden başlatmada aynı update başka kullanıcı turu üretmez. Uzun LLM turunu poll döngüsünde bekleme; bounded iş/çıkış kuyruğu, kullanıcı başına sıralama, açık busy yanıtı.
- `/yeni`, `/sohbetler`, `/durum`, `/limit`, `/onay` ve serbest metin. Komut argümanı shell'e aktarılmaz; bilinmeyen slash komutu yardım döner, modele emir olarak iletilmez. Fotoğraf/ses/belge metni veya caption çalıştırılmaz; yetkili kullanıcıya desteklenmediği söylenir.
- Yeni sohbet yönlendirmesi `routing::route(db,text,last_active_project)` ile; masaüstü belirsizlik davranışı korunur. Sohbet/proje seçimi için ayrı namespace'te tek kullanımlık, kullanıcıya bağlı 128 bit rastgele nonce; sonradan gelen seçimde chat/project geçerliliği tekrar denetlenir. `/yeni` önceki bekleyen seçimleri geçersiz kılar.
- `/durum`: gerçek aktif turlar, bekleyen onaylar ve kota işleri ayrı listelenir; `/limit`: aynı `QuotaService::report`, pencere/kalan reset hakkı/source etiketi; `None` ve Agy için “bilinmiyor”. Reset hakkını kullanma işlemi yoktur. Saatler kullanıcı metninde Europe/Istanbul (UTC+03:00), depoda UTC.
- `EventLog::subscribe` + `Approvals::subscribe_global`; onay istekleri, ilgili Telegram turunun final/hatası, `EngineSwitched`, kota başlangıç/devam/devir bildirimi. Yinelenen kanallardan gelen aynı olay `event.id` ile tekleştirilir; broadcast lag'de kalıcı seq'den replay yapılır. Sohbetsiz onaylar kaybolmaz.
- Telegram-origin final çıktısı kayıtlı tur seq aralığından alınır; başka tur/session parçaları birleşmez. 4096 sınırı ve notu, Türkçe/emoji/boş final/hata için saf formatter; ek model çağrısı yok. İş sonucunun finali ile “bitti” bildirimi tek mesajda birleştirilir.
- `notify=always|away` davranışı yukarıdaki heartbeat tanımıyla; switch bildiriminde `disable_notification=true`. Gönderim başarısızlığı motorun tamamlanmış işini yeniden çalıştırmaz. Ağ sonucu belirsiz `sendMessage` tekrarında Telegram exactly-once garantisi verilmez; olası tekrar ve teslim edilemeyen durum kayda/status'a yansır.

**Acceptance / Checks:**
- [ ] `auth_filter_requires_user_private_chat_and_matching_chat_id`: yanlış/eksik kullanıcı, grup/supergroup/channel, izinli kullanıcı ama başka chat, callback kimliği ve yetkisiz ekler; reddedilen içerik DB/tracing'e düşmez.
- [ ] Tekrarlı update + daemon yeniden başlatma, poll iptali/backoff, busy, stale seçim, `/yeni` ve chat binding restart testleri; test sunucusuna gerçek dış host isteği yok.
- [ ] `telegram_final_truncates_unicode_with_note`, `limit_unknown_matches_desktop`, `origin_is_per_turn_not_per_chat`, `away_does_not_suppress_command_replies`, lag/replay tekleştirme testleri.
- [ ] `cargo test -p ultrond --lib telegram::bridge`; `cargo test -p ultrond --lib telegram::commands`; `cargo test -p ultrond --lib telegram::notifications`.
- [ ] İlk dönüş: update kabul/offset sınırı ve bildirim kapsamı. Commit `feat(ultrond): telegram emirleri ve bildirim köprüsü`.

### Task 6: Araç onayları için nonce ve ortak çözüm yolu

**Scope / Owner:** `P4-telegram-approvals` — Task 4 sonrası Task 5/7/9 ile paralel. Callback ve araç onayı sınırlaması; mevcut onay güvenliğini genişletmez.

**OWNS / Files:** `crates/ultrond/src/telegram/approval_callbacks.rs`, `crates/ultrond/src/approvals.rs`; `crates/ultron-core/src/db/approvals.rs` Task 3'ten devralınır (yeni migration yok). Test `crates/ultrond/tests/approvals.rs` ve callback modülündeki testler.

**DO NOT TOUCH:** `server.rs`, `session.rs`, politika değerlendirme kuralları, Telegram command/notification/client dosyaları, şema, CLI/UI.

**Interfaces — Produces:**
- `check_owned` yeni kaydı `PolicyTool` olarak damgalar; `TurnElevation` ve legacy `Unknown` Telegram'da yalnız “masaüstünde onay gerekli” bilgisi alır, düğme almaz. Restore/purge, politika ayarı/import gibi idari eylemler uzaktan çözülebilir araç kümesine dahil edilmez; belirsiz sınıflandırma masaüstüne bırakılır.
- Her notification/alıcı/karar için tahmin edilemez en az 128 bit nonce; `callback_data` kısa op etiketi + nonce (en çok 64 byte), approval ID veya bearer token içermez. Sunucu map'i nonce'u approval/user/chat/message/approve/deadline ile eşler; notification gönderimi başarısızsa yetki oluşmaz. Harita boyutu sınırlı, süre bitişinde temizlenir.
- Callback tekrar üçlü auth filtresinden geçer; map'teki kullanıcı/chat/message eşleşmesi ve güncel allowlist doğrulanır. Onay hâlâ pending ve gerçekten tool kökenliyse ortak `resolve_tool_approval` yolu `Approvals::resolve(..., ResolvedBy::Approver)` çağırır. Telegram için sahte pipe bağlantısı/token dosyası okuması gerekmez; güven, doğrulanmış iç capability'dir.
- Masaüstü IPC kolu aynı ortak çözüm işlevine bağlanmak üzere Task 10'a teslim edilir. `ResolvedBy::Telegram` mevcut enum'da bulunsa da rol kanıtı yerine kullanılmaz; kanal bilgisi gerekiyorsa ayrı sır taşımayan audit metadata'sıdır.
- Nonce tek kullanımda atomik tüketilir; aynı approval'a ait kardeş nonce'lar da geçersiz olur. İlk DB kararı kazanır. Geçerli bildirim masaüstünde çözülmüşse `answerCallbackQuery` ve message edit ile “zaten karara bağlandı”; karar sonrası düğmeler kaldırılır. Ret/onay metni gerçekteki DB kararına göre yazılır. Edit başarısızlığı kararı geri almaz.
- Restart'ta RAM nonce'ları geçersizdir ve `recover_pending` eski onayları expired yapar. Yeni Telegram teslimi onayın 300 saniyelik süresini uzatmaz. Onaylı silmenin `ultron trash` yönlendirmesi mevcut haliyle korunur; restore/purge yetkisi verilmez.

**Acceptance / Checks:**
- [ ] `callback_nonce_is_single_use`, kullanıcı/chat/message değiştirme, karar bitini değiştirme, expiry, restart, expired approval, allowlist'ten çıkarılmış kullanıcı ve tahmin edilen nonce testleri.
- [ ] `desktop_telegram_race_first_decision_wins`; duplicate callback “zaten karara bağlandı”; callback yanıtı ve edit hatası DB'yi ikinci kez değiştirmez.
- [ ] `telegram_cannot_resolve_elevation_unknown_or_admin_actions`; yalnız maskeli sınırlı özet gider, ham action JSON gitmez; requester `ResolveApproval` regresyon testleri korunur.
- [ ] `cargo test -p ultrond --lib telegram::approval_callbacks`; `cargo test -p ultrond --test approvals`; `cargo test -p ultron-core --lib db::approvals`.
- [ ] İlk dönüş: ortak çözüm API'si ve filtre karar tablosu. Commit `feat(ultrond): telegram araç onaylarını güvenli çöz`.

### Task 7: Kota reset ayrıştırması ve aynı adımı sürdürme girişi

**Scope / Owner:** `P4-session-resume` — Task 4 sonrası Task 5/6/9 ile paralel; SessionManager ve motor hata/reset sözleşmesinin tek sahibi.

**OWNS / Files:** `crates/ultron-core/src/session.rs`, `crates/ultron-core/src/engine/{mod.rs,agy.rs,claude.rs,codex.rs}`; Test `crates/ultron-core/tests/{session.rs,agy_driver.rs,claude_driver.rs,codex_driver.rs}` ve ilgili modül testleri.

**DO NOT TOUCH:** `engine/runner.rs` sonucu gereksiz yere değiştirme, `server.rs`, DB migration/API dosyaları, daemon scheduler, onay kuyruğu implementasyonu, HTTP/CLI/UI.

**Interfaces — Produces:**
- `is_quota_message` tekrar kullanılır; sınıflandırma yalnız error/result/rate-limit yollarında. Agy `result_events` içindeki `error` fallback'i korunur; “Resets in 119h”, dakika/saniye ve birleşik süre gibi desteklenen dar biçimler enjekte edilmiş `now` ile reset'e çevrilir. Başarılı asistan metninde “quota reached” yazması tetiklemez. Bozuk/belirsiz/taşan süre `None`; tahmini saat uydurulmaz.
- Claude `reset_from` ve yapılandırılmış `resetsAt` önceliklidir. Codex için gerçekten mevcut yapılandırılmış reset varsa kullan; yoksa ilgili kota penceresiyle Task 8'de eşleştir. Agy kota ölçüm paneli hâlâ Unknown olabilir; hata reset'ini canlı yüzde ölçümü diye sunma.
- Önerilen `resume_quota_step` çekirdek girişi özgün `user_event_seq/checkpoint_seq` ve kalıcı iş kimliğiyle çalışır; aynı `UserText` yeniden append edilmez. Aynı motor/native ID uygunsa resume, diğer motorda `build_handoff_before`/mevcut devir paketi. Devir `SessionManager::switch(..., SwitchReason::Quota)` ile ve kilit serbestken yapılır; gizli `SwitchReason::User` geçişine düşmez.
- İlk Telegram kabulünde de Task 3'ün atomik inbox+kullanıcı olayı yazımı ile mevcut `send_reserved` içindeki `UserText` append'i iki kez çalışmamalı. Dar bir `send_reserved_with_accepted_user` girişi veya eşdeğer doğrulanmış olay referansı ekle; olayın chat/iş/metin bağını doğrula, var olan `send_reserved` çağıranlarının sözleşmesini koru. SQLite kabulünden sonra EventLog'un kalıcı döküm/yayın telafisi Task 10'da bağlanır.
- “Aynı adım” bu fazda olay checkpoint'inden **kalan işe devam** demektir; eski tool çağrısını veya özgün kullanıcı emrini körlemesine replay değildir. Tamamlanmış etkiler bağlama taşınır; belirsiz, yinelenmesi geri alınamaz yan etki varsa `needs_attention`, otomatik yürütme yok. Harici araçlarda exactly-once garantisi verilmez.
- Devam, normal tur/workspace kilidi, denetim ve güncel politika ile çalışır; yeni `TurnGrant` alır. Full yükseltme rezervasyona bağlıdır ve persist edilmez: Full işin otomatik yeniden başlaması yeni yerel onay olmadan mümkün değildir; Telegram bunu yükseltemez. Araç onayı eski karardan devralınmaz.
- `request_elevation` açık `TurnElevation` constructor'ını kullanır. Quota outcome'u tek başına reset taşımadığı için dar bir attempt sonucu/DB olayı referansı üretilir; genel `TurnOutcome` tüketicilerinin mevcut wire biçimi korunur.

**Acceptance / Checks:**
- [ ] Sentetik agy `119h`, `44m`, bozuk/aşırı süre, hata alanı ve başarı metni regresyonları; Claude mevcut reset fixture'ları ve bilinmeyen reset.
- [ ] `quota_resume_does_not_duplicate_user_text`, native resume ve handoff, completed tool etkilerinin replay edilmemesi, Full/no-grant, tur/workspace yarışları, yeni onayda bekleme testleri.
- [ ] `cargo test -p ultron-core --test agy_driver`; `cargo test -p ultron-core --test claude_driver`; `cargo test -p ultron-core --test codex_driver`; `cargo test -p ultron-core --test session`; `cargo test -p ultron-core --test turn_env`.
- [ ] İlk dönüş: reset kanıtı, devam API'si ve belirsiz yan etki politikası. Commit `feat(core): kota resetini ayrıştır ve kalan adımı sürdür`.

### Task 8: Kalıcı otomatik devam durum makinesi ve zamanlayıcı

**Scope / Owner:** `P4-auto-continue` — Task 3/4/7 sonrası; Task 5/6/9 ile paralel olabilir. Güvenlik/yetki/süreç yönetimi bu daemon şeridindedir.

**OWNS / Files:** `crates/ultrond/src/auto_continue.rs` ve dosyadaki saf durum makinesi/fake clock testleri. Task 1'de sabitlenen yaşam döngüsü event türlerini kullanır; CLI/desktop tüketici eşlemeleri Task 10/11'e bildirilir.

**DO NOT TOUCH:** `server.rs`, `session.rs`, `events.rs`, storage/migration, keep-awake implementasyonu, callback/bridge/CLI/UI dosyaları.

**Interfaces — Produces:**
- `QuotaExhausted` sonucunda kalıcı `waiting` kaydı; reset önceliği: ilgili attempt'in güvenilir reset'i, yoksa aynı motorun hatayla ilişkili ve dolmuş kota pencereleri. Birden fazla bloklayıcı pencere varsa hepsinin açılacağı en geç saat; yüzde/reset bilinmiyorsa kesin saat sayılmaz. Reset hakkı otomatik tüketilmez.
- `reset_at` biliniyor ve `now < reset_at <= now + wait_minutes` ise timer; tam 45 dakika dahil. Daha uzun veya bilinmiyorsa configured `fallback_order` içindeki sıradaki etkin, kayıtlı, bu devir zincirinde denenmemiş motora geç; sıra boş/tükenmişse `waiting` ve `/durum`da açık neden. Alternatifsiz süresiz beklemede aktif timer/keep-awake yoktur.
- Reset geçmişteyse bir kez claim/resume denemesi; yeniden kota hatasında eski reset ile hızlı döngü yok: reset yeniden değerlendirme ve sınırlı retry/backoff. Denenen motorlar/attempt sayısı kalıcıdır; fallback A→B→A döngüsü kurulmaz. Tek bir başarısız devam aynı adımı sürekli yeniden çalıştırmaz.
- Önerilen `QuotaWaitStarted`, `QuotaResumed`, `QuotaHandoff`, `QuotaWaitStopped` olayları iş kimliği ve güvenli nedenle yazılır; `EngineSwitched` ayrıca zorunludur. Yayın kaybında kalıcı durumdan bildirim telafisi; olay yazılamıyorsa sessizce motor başlatma yok.
- Fake-clock destekli scheduler; lock/transaction `await` boyunca tutulmaz. Timer uyanınca generation ve config tekrar doğrulanır, claim yapılır, aynı TurnService güvenlik yolu çağrılır. `Cancel` bekleme kaydını da iptal eder; kullanıcı yeni mesaj/switch ile işi değiştirirse eski generation çalışamaz. Bu davranış Task 10 IPC bağlamasına teslim edilir.
- Restart: önce eski onaylar expire, yarım running turlar mevcut kuralla interrupted; sonra güvenilir `waiting` kayıtlarını yükle ve timer'ları kur. Crash anında `claimed/running` kalan iş körlemesine tekrar başlamaz: tamamlanmış olay/checkpoint ile uzlaştır, belirsizse `needs_attention`. Tamamlanmış/iptal edilmiş eski iş dirilmez.
- Zamanlayıcının somut future reset beklemesi boyunca bir keep-awake guard taşınır; yürütmeye geçerken guard bırakma/alma arasında gereksiz uyku penceresi bırakılmaz. Süresiz waiting/disabled/needs_attention sistem uyanıklığı istemez.

**Acceptance / Checks:**
- [ ] `auto_continue_waits_at_threshold`, 45:00/45:01, bilinmeyen/past reset, birden çok quota penceresi, enabled toggle, fallback yok/devre dışı/tükenmiş, kota tekrarında backoff; saat ileri/geri atlaması.
- [ ] `restart_restores_waiting_once`, iki scheduler claim yarışı, checkpoint sonrası crash, Cancel/yeni mesaj/switch generation yarışı, eksik kaynak olay ve Full iş için yerel onay bekleme.
- [ ] `new_tool_approval_stops_unattended_resume`: yeni Ask Telegram bildirimine gider, cevap gelmeden araç çalışmaz; timeout deny. Yetki atlama/otomatik onay yok.
- [ ] `cargo test -p ultrond --lib auto_continue`; `cargo test -p ultron-core --test quota_wait_store`; `cargo test -p ultron-core --test quota`.
- [ ] İlk dönüş: geçiş tablosu, fake-clock kanıtı ve Task 10 lifecycle bağlantıları. Commit `feat(ultrond): kota bekleyen işi kalıcı ve güvenli sürdür`.

### Task 9: Keep-awake yaşam döngüsü

**Scope / Owner:** `P4-keepawake` — Task 4 sonrası diğer davranış şeritlerinden bağımsız; küçük ve sınırlı görev.

**OWNS / Files:** `crates/ultrond/src/keepawake.rs` ve aynı dosyadaki testler.

**DO NOT TOUCH:** `server.rs`, scheduler, SessionManager, Windows güç ayarları/registry, UI/manifest.

**Interfaces — Produces:**
- Mevcut ayrılmış OS thread + referans sayımlı guard modeli korunur. `ES_SYSTEM_REQUIRED` var, `ES_DISPLAY_REQUIRED` yok; son tutucu düşünce aynı thread `ES_CONTINUOUS` ile bırakır.
- Test edilebilir dar power-call adaptörüyle 0→1 edinme, 1→0 bırakma, Win32 başarısızlığı ve kapanış görünür olur. Gerçek Win32 çağrı hatası token/veri içermeyen şekilde raporlanır; status “uyku engellendi” başarısını uydurmaz.
- API aktif tur ve gerçek pending timer için aynı guard'ı destekler. `holders()` güç tutucularıdır; aktif tur sayacı değildir. Sayısal `running_turns` değişikliği Task 10 sahibine teslim edilir.

**Acceptance / Checks:**
- [ ] İki turn + bir timer, farklı sıralarda Drop/Cancel, timer→turn geçişi, scheduler shutdown ve başarısız power çağrısı testleri; son tutucu sonrası bırakma tam bir kez.
- [ ] Enjeksiyon testinde display bayrağı hiçbir çağrıda yok; süresiz wait guard üretmez (scheduler testi Task 8).
- [ ] `cargo test -p ultrond --lib keepawake`.
- [ ] İlk dönüş: guard API'si ve güç hatası davranışı. Commit `test(ultrond): tur ve kota zamanlayıcısı uyanıklık yaşam döngüsü`.

### Task 10: Daemon entegrasyonu, IPC yetkileri ve restart kabulü

**Scope / Owner:** `P4-daemon-integration` — Task 2–9 kabulünden sonra koordinatörün tek ortak dosya şeridi; paralel modül değişikliği yapılmaz.

**OWNS / Files:** `crates/ultrond/src/{server.rs,lib.rs}`, `crates/ultrond/src/telegram/{mod.rs,contracts.rs}` (sözleşme yalnız doğrulanmış uyarlama); Create `crates/ultrond/src/telegram/integration_tests.rs`; Modify `crates/ultrond/tests/{restart.rs,config_race.rs,send_ack.rs,quota_api.rs}` ve gerekirse `tests/common/mod.rs`. Önceki şeritlerin kayıt/manifest ve yeni event CLI render uyarlamaları için `Cargo.toml`, `Cargo.lock`, `crates/ultrond/Cargo.toml`, `crates/ultron/src/commands.rs` bu noktada seri olarak devralınır.

**DO NOT TOUCH:** Desktop, kullanıcının canlı daemon/config/credential deposu; davranış modüllerinde yeni özellik tasarlama. Bulunan davranış kusuru ilgili sahibine geri verilir, örtülü geniş refactor yapılmaz.

**Interfaces — Produces:**
- `start` state kurulumunda credential/bridge ve scheduler'ı tek kez başlat; boş allowlist'te poller yok. `ServerHandle` kapanışında poller, retry, timer ve notification görevlerini cancel edip join et; turn abort ve keep-awake bırakımı sızıntısız. Token yenilemede eski poller bitmeden yenisi başlamaz.
- `dispatch::Send` ve Telegram aynı TurnService'i çağırır; kabul öncesi motor/rezervasyon doğrulaması, scratch cwd, elevation, `turn_grant::issue`, `send_audited` ve hata günlüğü korunur. Yeni uygulama katmanı mevcut güvenlik yolunun tek sahibi olur; kopya uygulama kaldırılır.
- `ResolveApproval` için `require_approver` korunur, sonra ortak resolver çağrılır. Requester yeni setter/test/settings/heartbeat ile yetki kazanamaz. Telegram uygulama capability'si yalnız araç onayı çözer; idari IPC'leri yönlendirmez.
- `set_reset_rights` ve Telegram ayar yazımı aynı config lock'ına alınır; atomic merge testi eski `[telegram] allowed_user_ids` ve bilinmeyen alanları korur. `GetQuota` mevcut `Config::load`/`QuotaService::report` davranışını sürdürür.
- `Status.running_turns` gerçek aktif tur sayısından hesaplanır; timer'lar ayrı `quota_wait_tasks`/durum alanıdır. IPC'nin mevcut tüketicileri bozulmaz. Heartbeat yalnız onaylayıcıdan kabul edilir; disconnected generic socket sayısı masaüstü varlığı sayılmaz.
- `Cancel`, `Switch` ve yeni kullanıcı mesajı bekleme generation'ını transaction sınırında geçersiz kılar; geç kalan timer rezervasyon alamaz. `recover_interrupted` WaitingQuota'yı yanlışlıkla running saymaz; yeni restart uzlaştırması eskisinin yerine körlemesine geçmez.
- Mock daemon entegrasyonu `integration_tests.rs` içinde `#[cfg(test)]` ile gerçek `server::start` yaşam döngüsü + fake engines + yerel mock HTTP + sahte credential + fake clock kullanır. Teste özel `start_with_test_dependencies` üretim API/IPC/env üzerinden açılmaz. Bu, library test target'ında çalışan daemon entegrasyon testidir; gerçek servis değildir.

**Acceptance / Checks:**
- [ ] `daemon_telegram_command_approval_and_reply`: sentetik getUpdates → filtre → routing/binding → fake tur → policy Ask → nonce callback → aynı resolver → final send; masaüstüyle yarış ve reddetme varyantı.
- [ ] `daemon_restart_resumes_quota_wait_once`: DB kapat/aç, timer restore, tek fake tur, tek kullanıcı olayı; cancelled/completed/crash-ambiguous tekrar çalışmaz.
- [ ] Token eksik/liste boş, token setter sırasında tek poller, ağ kesintisi, shutdown, reconnect, 90 sn heartbeat expiry ve yetkisiz IPC matrisi. Sentetik token logs/events/transcript/status hata zincirinde aranır.
- [ ] `cargo build -p ultron-core --bin ultron-fake-cli`; `cargo test -p ultrond --lib telegram::integration_tests -- --test-threads=1`; `cargo test -p ultrond --test restart`; `cargo test -p ultrond --test config_race`; `cargo test -p ultrond --test send_ack`; `cargo test -p ultrond --test quota_api`; `cargo test -p ultrond --features test-hooks`.
- [ ] İlk dönüş: birleşik sözleşme, entegrasyon kanıtı ve bilinen teslim sınırlamaları. Commit `feat(ultrond): telegram ve otomatik devam yaşam döngüsünü bağla`.

### Task 11: Masaüstü Ayarlar > Telegram ve heartbeat

**Scope / Owner:** `P4-desktop` — Task 10 sonrası son uygulama şeridi; `apps/desktop/AGENTS.md` ve mevcut tasarım token/bileşenlerini kullanır.

**OWNS / Files:** Create `apps/desktop/src/features/telegram/{TelegramSettings.tsx,TelegramSettings.test.tsx,telegram.css}`; Modify `apps/desktop/src/screens/Settings.tsx`, `apps/desktop/src/lib/{bridge.ts,securityBridge.test.ts}`, `apps/desktop/src-tauri/src/{bridge.rs,commands.rs,lib.rs}`, `apps/desktop/src-tauri/build.rs`, `apps/desktop/src-tauri/capabilities/default.json`, komut izinleri için mevcut üretim düzenindeki `apps/desktop/src-tauri/permissions/**`; Test `apps/desktop/src-tauri/tests/{bridge.rs,security_contract.rs}`, `apps/desktop/e2e/telegram-settings.spec.ts`. Yeni kota yaşam döngüsü event'leri için mevcut TS event türü/tüketici ve gerekirse `apps/desktop/src/KitchenSink.tsx` aynı şerittedir.

**DO NOT TOUCH:** Rust core/daemon/CLI, token setter endpoint'ini webview'e ekleme, CSP gevşetme, shell/http/fs eklentisi, `docs/design/`.

**Interfaces — Produces:**
- Durum metinleri “Bağlı / Token yok / İzinli kullanıcı yok / Hata”; izinli kullanıcı **sayısı**, notify modu, auto-continue açık/kapalı, 45 dk varsayılan süre ve fallback sırası. İzinli gerçek ID listesi/token görünmez. Token kurulumuna yalnız `ultron telegram set-token` açıklaması; token input veya panodan okuma yok.
- “Test mesajı gönder” gerçek `TelegramTest` onaylayıcı köprüsüne gider; pending'de çift tıklama engeli, ayrı başarılı/başarısız ve kısmi teslim durumu. Kaydetme hatasında başarısız ayar uygulanmış gibi görünmez; eski async yanıt yeni formu ezmez.
- `Bridge::request` yönetim ayrımına yeni ayar/test/heartbeat eklenir; otomatik tekrar edilmeyen mutasyonlar korunur. Tauri komut kaydı, build manifest, dar capability ve TS çağrıları eşleşir; token setter kayıtlı olmaz.
- Rust shell lifetime görevi per-instance heartbeat üretir; bağlantı hazırken hemen, sonra 30 sn. Uygulama kapanışında görev durur; daemon yoksa bounded retry, pencere gizlenmesi/odak fiziksel away diye yorumlanmaz. JS timer veya sayfa gezinmesi heartbeat'in tek kaynağı olmaz.
- Mevcut `Row`, `GroupedList`, `CapsuleButton`, token'lar ve reduced-motion kullanılır. Türkçe etiketler, klavye odağı, `aria-busy`/status/alert, 390 px dar pencere ve masaüstü düzeni; örnek veri yalnız test/katalogdadır.

**Acceptance / Checks:**
- [ ] Vitest: dört status, sayı/süre roundtrip, kaydetme hatası, geç async yanıt, double-click, token alanı yok, unknown kota, enabled/fallback form doğrulaması.
- [ ] Tauri sözleşmesi: requester test/settings yapamaz; komut manifest/capability eşleşir; token setter TS/Tauri'de yok; Rust heartbeat görevi kapanır ve generic request heartbeat sayılmaz.
- [ ] `cargo test -p ultron-desktop --test bridge`; `cargo test -p ultron-desktop --test security_contract`; `npm --prefix apps/desktop run test -- src/features/telegram/TelegramSettings.test.tsx src/lib/securityBridge.test.ts`; `npm --prefix apps/desktop run typecheck`; `npm --prefix apps/desktop run lint`; `npm --prefix apps/desktop run build`; `npm --prefix apps/desktop run e2e -- telegram-settings.spec.ts`.
- [ ] Masaüstü + dar pencere gerçek render ve klavye/etkileşim kontrolü; sahte bridge ekran görüntüsü canlı Bot API kanıtı diye raporlanmaz. İlk dönüş: UI/contract kanıtı. Commit `feat(desktop): telegram ayarları ve masaüstü varlık sinyali`.

### Task 12: Birleşik kabul, canlı test ve rapor

**Scope / Owner:** `P4-acceptance` — koordinatör; bütün şeritler birleşince. Bu planı yazan worker canlı test veya kod/güvenlik incelemesi yapmaz.

**OWNS / Files:** Create `crates/ultrond/tests/live_telegram_acceptance.rs`, `docs/reports/<tarih>-asama4-kabul.md`. Kod bulgusu düzeltmeleri koordinatör tarafından dosyanın sahibine geri verilir; rapor yazarı unrelated dosya değiştirmez.

**DO NOT TOUCH:** Gerçek mesaj/credential değerlerini fixture/rapora koyma, gerçek motor kotası harcama, sağlayıcı hesabını değiştirme, izinli listeyi genişletme, webhook kurma, MSI/dağıtım, kullanıcının `docs/design/` çalışması.

**Acceptance / Checks:**
- [ ] Tam kapı: `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; Task 10 mock entegrasyonu ve Task 11 frontend/contract/e2e kapıları. `--ignored` genel workspace üzerinde çalıştırılmaz.
- [ ] Koordinatör final diff'i (`git diff --no-ext-diff --no-textconv <başlangıç> -- ...` + staged/unstaged ve yeni dosyalar) iki bağımsız salt okunur incelemeye verir: doğruluk ve güvenlik; hassas/persistent kapsam nedeniyle uygun Sol `high`. Reviewer dosya yazmaz, state değiştiren test ve alt delegasyon yapmaz. Geçerli bulgu düzeltilir, ilgili test ve etkilenen inceleme tekrarlanır. Bu doküman görevi bu incelemeleri çalıştırmış sayılmaz.
- [ ] Tek gerçek mesaj kabul testi `#[ignore = "canlı Telegram gönderimi; yalnız koordinatör"]`; ayrıca `ULTRON_LIVE_TELEGRAM_ACCEPTANCE=1` yoksa **hiç ağ/credential erişimi yapmadan hata ile durur**. Token env'den/argv'den alınmaz. Test, kullanıcının seçili daemon'ına mevcut approver IPC ile `TelegramTest` gönderir; daemon mevcut allowlist'e sabit ve kişisel veri içermeyen mesaj yollar. Alıcı parametresi/alternatif ID yoktur.
- [ ] Canlı test **yalnız koordinatör** tarafından açık ortam kapısıyla çalıştırılır; öncesinde token var ve izinli liste boş değil durumu sır içermeyen status'tan görülür. Bot hiç başlatılmamış/engellenmişse gerçek Bot API hatası kabul engelidir; entegrasyon başarılı taklit edilmez. Kullanıcının Telegram'da botla özel sohbeti başlatmış olması önkoşuldur.
- [ ] PowerShell canlı komutları, ayrı satırlar halinde: `$env:ULTRON_LIVE_TELEGRAM_ACCEPTANCE = '1'`; `cargo test -p ultrond --test live_telegram_acceptance live_telegram_sends_test_to_allowed_users -- --ignored --exact --nocapture --test-threads=1`; `Remove-Item Env:ULTRON_LIVE_TELEGRAM_ACCEPTANCE`. Gönderim gerçek allowed ID'ler dışına çıkmaz; CLI/LLM işi başlatılmaz. Birden çok izinli ID varsa bu test her birine bir sabit test mesajı gönderir.
- [ ] Rapor: revizyon ve kapsam; config varsayılanları/away tanımı; test komutları ve sonuçları; iki incelemenin bağımsızlığı/bulguları; mock ile canlı ayrımı; restart/nonce/scrubbing/keep-awake kanıtı; gerçek mesaj teslim sonucu (metin/ID/token olmadan). `Passed / Findings / Not run / Not applicable` doğru kullanılır.
- [ ] Canlı test veya görsel kontrol yapılmadıysa raporda **Not run**; Aşama 4 tam kabulü diye işaretlenmez. Kod değişikliklerinde kapanmamış in-scope P0/P1/P2 kabul engelidir. Commit `test: aşama 4 telegram kabulü`.

## Koordinatör Kararları ile Kod Arasındaki Uyarlamalar

1. **Yeni `[telegram]` şu an okunmuyor.** Task 1 typed config ekler; gerçek yerel dosyaya dokunmadan Task 4/10 yazımlarda bilinmeyen alanları korur.
2. **`ResolvedBy::Telegram` var ama IPC bunu kullanmıyor.** Mevcut `ResolveApproval` her kararı `Approver` olarak kaydediyor; `wait_elevation` özellikle bunu bekliyor. Telegram için etiketi yetkiye çevirmek yerine aynı iç resolver + ayrı tool-only capability kullanılır.
3. **Araç/onay tür ayrımı yok.** `ultron.turn.elevate` da `Action::ToolCall`; yalnız category/tool şekli yeterli değil. Task 3 kaynak türü ekler, Task 6 remote filtreyi uygular. Legacy kayıt masaüstünde kalır.
4. **Agy reset ayrıştırması henüz yok.** `ed60475` kota tanımasıdır; Task 7 dar süre ayrıştırması ekler. Claude'un mevcut reset yolu ve `QuotaService::report` korunur; Agy yüzde değerleri uydurulmaz.
5. **Kalıcı WaitingQuota, kalıcı görev zamanlayıcısı değil.** `SessionStatus::WaitingQuota` var; reset/özgün adım/claim/generation yok. Task 3/7/8 ekler. `recover_interrupted` yalnız Running'e dokunuyor; bu iyi davranış korunur.
6. **“Aynı adım” için araç işlemi günlüğü garantisi yok.** Mevcut model `UserText`/olay/native resume/devir paketidir. Plan olay checkpoint'iyle devam sağlar, harici yan etkilerde exactly-once iddiasında bulunmaz; belirsiz crash `needs_attention` olur.
7. **`running_turns = keep.holders()` artık doğru olmayacak.** Task 10 gerçek tur sayacı ile kota timer sayısını ayırır; mevcut UI çalışan tur sayısı şişmez.
8. **Away ölçümü yok.** Salt pipe bağlantısı CLI ve motorları da sayar. Plan yeni Rust heartbeat lease'ini (30/90 sn) seçer; fiziksel kullanıcı varlığını ölçtüğünü iddia etmez.
9. **Keep-awake zaten doğru sistem bayrağını kullanıyor.** Yeniden güç mekanizması kurmak yerine mevcut guard kota timer'ına genişletilir. Süresiz waiting uyanıklık gerektirmez.
10. **Spec uzun yanıt özeti/ekran görüntüsü söylüyor.** Bu fazın koordinatör kararı bunu tarihli olarak daraltır: yerel kırpma, ek LLM yok, ekran görüntüsü yok. Spec dosyası bu plan tarafından değiştirilmez; bu uyarlama kabul raporunda açık yazılır.
11. **Tam bitiş özetleyiciyi bekleyebiliyor.** Aşama 3 raporundaki en çok 120 sn gecikme mevcut; bu faz yeni LLM özetleyici eklemez. Telegram finali tur sonucu/seq sınırıyla ilişkilendirilir; testler `summarizer=none` ile deterministiktir. Ürün gecikmesi düzeltilmediyse raporda kalır.

## Kapsam Dışı

- Telegram ekran görüntüsü ve ham log dökümü: masaüstü/özel veri ifşası riski; yalnız güvenli kısa hata/sonuç özeti.
- Sesli mesaj, dosya/belge, fotoğraf ve ekler: transkripsiyon/indirme/yükleme yüzeyini bu faza açmamak için; desteklenmeyen içerik çalıştırılmaz.
- Üçüncü kişiyle temsilci konuşması, TDLib/kullanıcı hesabı ve delegasyon mesajlaşması: yetki/audience modeli Aşama 7'ye aittir.
- MSI ve dağıtım: yerel gateway/daemon kabulünden ayrı paketleme işi.
- Kota sıfırlama hakkını otomatik kullanmak, yeni motor satın almak/API çağrısı açmak, eski Full onayını taşımak: bu fazın yetkisi değildir.

## Kalan Riskler

- Bot token'ını ele geçiren kişi bot trafiğini okuyabilir ve bot adına mesaj gönderebilir; gerçek Telegram kullanıcı `from.id` değerini taklit edemez. Bu yüzden allowlist doğrulaması token gizliliğinin yerine geçmez; sızıntıda token rotasyonu gerekir.
- Aynı Windows kullanıcısı bağlamındaki süreçler Credential Manager/approver verilerine ulaşabilir. Bu faz ayrı OS hesabı/kısıtlı token izolasyonu sağlamaz; Aşama 3 kalan riski sürer.
- Telegram sunucuları gönderilen emir, onay özeti ve yanıtların içeriğini görür. Token şifreli yerde durması mesaj içeriğini Telegram'dan gizlemez.
- Telegram gönderimi HTTP timeout'undan sonra kabul edilmiş olabilir; kesin-once teslim garantisi yoktur. Yinelenen güncelleme motoru yeniden çalıştırmamalı, belirsiz outbound teslim sonucu raporda dürüstçe belirtilmelidir.
- Harici araç yan etkisinin crash anında tamamlanıp tamamlanmadığı olaylardan kesin anlaşılmayabilir. Böyle iş otomatik replay yerine dikkat bekler; bu sınırlama “kaldığı yerden devam” vaadinde açık kalır.

## Koordinatöre Açık Noktalar

- Uygulamayı engelleyen eksik karar yok: fallback varsayılanı boş, away 30/90 sn heartbeat, eski Full yetkisini yeniden kullanmama ve belirsiz yan etkide `needs_attention` bu planın somut tercihleridir. Bu tercihler değiştirilirse kod şeritleri başlamadan sözleşme güncellenmelidir.
- Canlı kabul için botun izinli kullanıcı tarafından önceden başlatılmış olması ve koordinatörün mevcut daemon'ı seçmesi gerekir. Bu çalışma bunları doğrulamadı; gerçek token/ID okumadı ve mesaj göndermedi.
- Aşama 3 masaüstü elle kontrolü raporda bekliyor. Yeni Telegram UI kontrolü o eski kabul eksikliğini otomatik kapatmış sayılmamalıdır.
