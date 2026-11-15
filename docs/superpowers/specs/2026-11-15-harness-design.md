# Harness — Tasarım Belgesi

- Tarih: 2026-11-15
- Durum: Taslak, kullanıcı onayı bekliyor
- Ad: "harness" (geçici)
- Kapsam: Tüm sistemin mimarisi + Aşama 1 (Çekirdek) ve Aşama 2 (Masaüstü v1) ayrıntılı tasarımı. Sonraki aşamalar kendi tasarım turlarıyla ayrıntılandırılır.

Bu belgede **[K]** kullanıcının konuşmada verdiği kararları, **[Ö]** Claude'un önerip kullanıcının itiraz etmediği ya da kendi verdiği kararları gösterir. [Ö] maddeleri kullanıcı okurken değiştirebilir.

---

## 1. Amaç ve neden

Tek bir geliştiricinin (Windows, öğrenci, Claude Pro + ChatGPT Plus + Google AI Plus) kendi çalışma biçimine göre şekillenen, kişisel bir AI harness'i. Jarvis benzeri bir kişisel asistan: kullanıcıyı, projelerini, evindeki cihazları ve kullandığı araçları bilir, onun yerine düşünür ve iş yapar. [K]

Bugünkü sorunlar:
- Proje bir motorla (Claude) başlıyor, kota bitince başka bir motora (Codex) geçmek bağlam kaybı yüzünden zor. Proje durumu tek bir motorun oturumuna gömülü.
- Kurallar, hafıza ve orkestrasyon (`CLAUDE.md`, `codex-fleet` skill'i, `lane.py`, Claude hafıza klasörü) Claude Code'un içine dağılmış durumda. Kullanıcıya ait, motordan bağımsız bir yerde değiller.
- ChatGPT masaüstü uygulamasındaki proje ve sohbet geçmişi rahatlığı CLI'da yok.
- Kullanıcı okuldayken evdeki bilgisayarda iş yaptıramıyor.

Temel ilke: **Model gider, hafıza ve durum kalır.** Motorlar değiştirilebilir işçilerdir. Hafıza, proje durumu, kurallar ve geçmiş harness'e aittir. [K]

Esin kaynakları: Mert Çobanov / Onur Tirpan sohbeti ("herkes kendi harness'ını yapmalı"), avenoxai/avenoxbeyin V3 (ortak Markdown hafıza), OpenAI ChatGPT/Codex masaüstü uygulaması (arayüz ve proje düzeni).

## 2. Hedefler, hedef olmayanlar, anti-hedefler

### Hedefler
1. Claude Code, Codex ve Antigravity CLI'larını **üyelikle** (API anahtarı olmadan) çalıştırmak. [K]
2. İsteğe bağlı olarak yerel modeller ve API modelleri (ör. DeepSeek) eklenebilmesi; eklenen her model **her rolde** kullanılabilir: beyin, işçi, konsey üyesi, onay modeli. [K]
3. Tüm modellerin ortak kullandığı hafıza (ikinci beyin). [K]
4. Model değiştiğinde bağlam kaybının pratikte sıfıra inmesi. [K]
5. ChatGPT masaüstü tarzında projeler ve sohbet geçmişi. [K]
6. Ana ekranda tek bir sohbet kutusu: yazılan mesajın hangi projeye ait olduğunu harness kendisi anlar ve o projenin ajanı gibi davranır. [K]
7. Uzaktan erişim için Telegram. Telefona özel bir uygulama yok. [K]
8. Bilgisayar üzerinde tam kontrol: dosya, terminal, tarayıcı, masaüstü uygulamaları, SSH. [K]
9. Kullanıcının istemediği bir şeyin asla silinmemesi. [K]
10. Kota ve sıfırlanma görünürlüğü (sıfırlama hakları dahil); kota yenilenince otomatik devam. [K]
11. Token tasarrufu temel ilke. [K]
12. Üç orkestrasyon modu (Tam, Dengeli, Eko), görsel yönlendirme ekranı, canlı alt ajan izleme. [K]
13. Kullanıcı görevlendirdiğinde, onun adına üçüncü kişilerle (anne, müşteri) konuşup isteği anlayıp yerine getirmek. [K]

### Hedef olmayanlar
- Başkaları için genelleştirilmiş bir ürün, eklenti pazarı ya da kurulum sihirbazı. Kod açık kaynak olur ama kişisel ihtiyaca göre yazılır. [K]
- Mobil uygulama (ileride Tauri Android ile olabilir, bu belgenin kapsamı dışında). [K]
- Kendi LLM'ini eğitmek. Yerel modeller yalnızca dar işler için kullanılır.

### Anti-hedefler (teknik olarak çalışsa bile başarısızlık sayılır)
- İstenmeyen silme, istenmeyen deploy ya da istenmeyen finansal işlem.
- Sessiz motor değişimi: kullanıcıya bildirilmeden yapılan bir geçiş. (Video dersi.)
- Üçüncü kişi mesajlarının (anne, müşteri, web sayfası) komut olarak çalışması.
- Kullanıcının kişisel bilgilerinin bir müşteriye ya da izinsiz bir uzak modele sızması.
- Claude'un üyelik giriş bilgisini çıkarıp resmi CLI dışında kullanmak (Anthropic kurallarına aykırı).
- Pro/Plus kotasını gereksiz yere yakan bir tasarım.

## 3. Kısıtlar

- **İşletim sistemi:** Windows 11. Donanım: Ryzen 5 7600, 32 GB RAM, RTX 4060 8 GB.
- **Üyelikler:** Claude Pro, ChatGPT Plus (Codex), Google AI Plus (Antigravity). Varsayılan yolda API anahtarı yok. [K]
- **Antigravity:** Kullanıcının mevcut kuralı yalnızca Gemini ve Flash-low. Bu kural harness'e motor ayarı olarak taşınır, kullanıcı değiştirebilir. Sağlayıcı riski kullanıcı tarafından kabul edilmiş (6 Kasım 2026). [K]
- **Claude kullanımı:** Model çağrıları yalnızca resmi `claude` CLI'ı üzerinden, kullanıcının bilgisayarında kişisel kullanım için. OAuth token'ı başka bir araçta model çağırmak için kullanılmaz. Kota okuma için token kullanımı ayrı bir karardır (§9, madde 9).
- **Codex:** `codex exec` içinden bilgisayar kontrolü (CUA) doğrulanmış değil. Bilgisayar kontrolü harness'in kendi araçlarıyla yapılır.
- **Açık kaynak:** Gizli bilgiler, kişisel notlar ve ev ağı bilgisi baştan koddan ayrı tutulur, repoya girmez.
- **Kullanıcının genel kuralları** (global `CLAUDE.md`) harness'in varsayılan politikasıdır: inceleme kapıları, gizli bilgi kuralları, finans botlarında dikkat, yayın onayı.

## 4. Yaklaşımlar

### Değerlendirilen yollar

| Yol | Artı | Eksi | Karar |
|---|---|---|---|
| **Tauri 2: Rust çekirdek + React/TypeScript arayüz** | Hafif ve hızlı, Rust süreç yönetiminde güvenli, Tauri 2 Android'i de destekliyor, React'i işçi modeller çok iyi biliyor | Rust daha yavaş yazılıyor, Tauri Android olgun değil | **Seçildi** [K] |
| Electron + TypeScript | En zengin ajan/MCP ekosistemi, en hızlı geliştirme | Ağır (yüzlerce MB RAM), "hızlı ve yenilikçi" isteğine uymuyor, Android ayrı yazılmalı | Elendi |
| Flutter | Masaüstü + Android tek arayüz | Ajan/CLI ekosistemi zayıf, çekirdek için yine başka dil gerekir | Elendi |
| Rust + GPUI (Discord'un 2026 yolu) | En hızlı, en yenilikçi | Çok genç, hazır bileşen yok, Android yok, işçi modeller GPUI'yi az biliyor | Ertelendi: çekirdek ayrık olduğu için arayüz ileride GPUI'ye taşınabilir [K] |

### Hafıza için değerlendirilen yollar
- **avenoxbeyin'i olduğu gibi kullanmak:** Python, hook tabanlı, yazma ajanın inisiyatifine bağlı, arama yalnız kelime eşleştirme. Harness CLI'ları kendisi sürdüğü için hook'a gerek yok. Elendi; fikirleri alındı (bkz. §6.4). [Ö]
- **Yalnızca vektör veritabanı:** Okunamaz, Git'le izlenemez, kaynak gösteremez. Elendi. [Ö]
- **Markdown kaynak + yerel SQLite indeks (BM25 + yerel embedding) + kaynak makbuzları:** Seçildi. [Ö]

## 5. Mimari genel bakış

```
                ┌──────────── Kapılar ─────────────┐
                │ Masaüstü (Tauri+React)  Telegram │  (ileride Android)
                └───────────────┬──────────────────┘
                                │ yerel IPC (yalnız localhost / named pipe)
┌───────────────────────────────▼──────────────────────────────────┐
│ harnessd — Rust çekirdek servis (Windows'ta arka planda sürekli) │
│                                                                  │
│  Oturum/Görev yöneticisi ── Orkestratör (modlar, yönlendirme)   │
│  Bağlam & devir katmanı ── Hafıza (ikinci beyin)                 │
│  Güvenlik kapısı (sabit kurallar + yerel onay modeli + çöp)      │
│  Kota izleyici & zamanlayıcı (otomatik devam)                    │
│  Araç sağlayıcı (MCP: masaüstü, tarayıcı, SSH, hafıza, geçmiş)   │
│  Temsilci (üçüncü kişi konuşmaları)                              │
└───────┬───────────────┬────────────────┬──────────────┬──────────┘
        │               │                │              │
   Üyelik CLI'ları   Yerel modeller   API modelleri   Dış kanallar
   claude, codex,    (Ollama vb.)     (OpenAI-uyumlu  (Telegram Bot,
   agy                                 uç noktalar)    TDLib, WhatsApp*)
```

- **harnessd** her şeyin sahibi: durum, hafıza, kurallar, kayıtlar. Arayüz pencereleri kapansa da çalışır (Telegram, zamanlanmış işler, otomatik devam). [Ö]
- **Kapılar** yalnızca harnessd ile konuşur ve iş mantığı içermez. Arayüz değiştirilebilir. [Ö]
- **Motorlar** alt süreçtir. Harness onları başlatır, çıktı akışlarını okur, durdurur.
- **Sabit kurallar ve gizli bilgiler** harnessd'nin içinde durur. Motorların erişebildiği bir ayar dosyasında değil, ajanlar bunları değiştiremez. (Video dersi: ajan kendi kısıtını kaldırmıştı.) [Ö]
- IPC yalnızca yerel makinede, kullanıcı oturumuna bağlı bir kimlik doğrulama token'ı ile. Dışarı açık port yok. [Ö]

## 6. Bileşenler

### 6.1 Çekirdek servis (harnessd)

- **Dil:** Rust (tokio). Windows'ta kullanıcı oturumu açılınca başlar. Windows Service olarak değil, kullanıcı bağlamında çalışır, çünkü masaüstü otomasyonu ve kullanıcı oturumundaki CLI girişleri gerekiyor. [Ö]
- **Depolama:**
  - `%LOCALAPPDATA%\harness\state\harness.db` — SQLite: projeler, sohbetler, mesajlar, görevler, olaylar, kota ölçümleri, indeks. [Ö]
  - `~/harness-brain/` (yapılandırılabilir) — Markdown hafıza kasası (vault), Git ile sürümlenebilir. [Ö]
  - `%LOCALAPPDATA%\harness\transcripts\` — ham motor akışları (JSONL). [Ö]
  - Gizli bilgiler: Windows Credential Manager. Repoda ve düz dosyada tutulmaz. [Ö]
- **Bilgisayarı uyanık tutma:** Aktif ya da bekleyen bir iş varken sistemin uykuya geçmesini engeller (`SetThreadExecutionState`; `codex-fleet`'teki `Invoke-KeepAwake.ps1` mantığı). [Ö]
- **Olay günlüğü:** Her motor çağrısı, araç çağrısı, onay kararı, dosya değişikliği ve geçiş tek bir olay akışına yazılır. Arayüz ve Telegram bu akıştan beslenir.

### 6.2 Motor katmanı

Her motor aynı arayüzü uygular:

```
trait Engine {
  fn id() / kind()            // subscription_cli | local | api
  fn capabilities()           // streaming, resume, tools, images, max_context, roles
  async fn start(task, context_pack, tool_set, policy) -> Session
  async fn send(session, message)
  fn events(session) -> Stream<EngineEvent>  // mesaj, araç çağrısı, komut, diff, kota sinyali, hata
  async fn stop(session)
  async fn quota() -> QuotaSnapshot          // okunabildiği kadarıyla
}
```

**Üyelik CLI'ları** [K]:
- **Claude Code:** `claude -p` + JSON akış çıktısı ve girdisi (`--output-format stream-json`, `--input-format stream-json`), oturum devamı (`--resume`), araç ve MCP kısıtlama, izin modu. Kesin bayraklar CLI sürümüne göre builder tarafından doğrulanır.
- **Codex:** `codex exec --json`, `codex exec resume`, sandbox ve onay politikası. Model ve efor kullanıcının mevcut politikasındaki adlarla: `gpt-6.1-sol`, `gpt-6.1-terra`, `gpt-6-luna`. Astra adlı modeller kullanılmaz (kullanıcı politikası).
- **Antigravity (agy):** Mevcut `lane.py` entegrasyonu referans alınır. Varsayılan model kuralı: Gemini, Flash-low. JSON/akış desteği belirsiz: Aşama 1'de test edilir, yoksa satır tabanlı akış ayrıştırılır ve arayüzde "sınırlı canlı görünüm" etiketi gösterilir.

**Yerel modeller:** Ollama (ya da eşdeğeri) üzerinden. Varsayılan roller: proje tahmini, onay sınıflandırması, hafıza önerisi, başlık üretimi, log özeti, embedding. Model seçimi Aşama 3'te kullanıcının gerçek geçmişi üzerinde ölçülerek yapılır. [Ö]

**API modelleri:** OpenAI uyumlu uç nokta (adres + model adı + Credential Manager'daki anahtar). Günlük ve aylık harcama sınırı var; sınıra gelince durur ve bildirir. Varsayılan durumda hiçbir API motoru tanımlı değildir. [K]

**Roller:** `brain` (orkestra şefi), `worker`, `reviewer`, `council_member`, `approver`, `router`, `summarizer`, `embedder`. Her rol herhangi bir motora atanabilir. Atama orkestrasyon modundan gelir (§6.6). [K]

**Sessiz geçiş yasağı:** Bir motor kota, hata ya da kullanıcı kararıyla değiştiğinde olay günlüğüne, sohbete ve (kullanıcı uzaktaysa) Telegram'a bildirim gider. [Ö]

### 6.3 Bağlam ve devir katmanı

Hafıza motorda değil, harness'te tutulur. Üç katman var [Ö]:

1. **Tam döküm:** Motorun JSON akışından gelen her mesaj, araç çağrısı, komut çıktısı ve diff. Kaynak budur. Motorun kendi oturum dosyasına bağımlı kalınmaz.
2. **Çalışma durumu** (sohbet ya da görev başına, yapılandırılmış ve her adımda güncellenen):
   ```
   goal, plan[], decisions[{what, why, when, source_event}], files_touched[],
   done[], next_step, open_questions[], engine_history[], approvals_pending[]
   ```
   Güncellemeyi ucuz bir model (yerel ya da Flash) her adımın sonunda yapar. Kritik alanlar (kararlar, onaylar) olaylardan kural tabanlı olarak çıkarılır.
3. **Uzun süreli hafıza:** §6.4.

**Devir paketi** (motor değişince ya da yeni bir oturum başlarken): sistem talimatı + proje talimatları + çalışma durumu + son N mesaj olduğu gibi + daha eskilerin özeti + ilgili hafıza notları. Paket bağlam bütçesine göre kesilir; kesildiği bilgisi pakette açıkça yazar. Motora ayrıca `history_search` ve `memory_search` araçları verilir; gerektiğinde ayrıntıyı tam dökümden kendisi çeker.

**Dışarıdan değişiklik bildirimi:** Harness projenin dosya sistemini izler. Ajan dışında yapılan değişiklikleri (kullanıcı düzenlemesi, başka bir motorun işi, git işlemleri) bir sonraki çalıştırmada pakete "son oturumdan beri değişenler" olarak ekler. [Ö]

**Dürüst sınır:** Kayıp sıfır değildir. Önceki modelin yazıya dökmediği düşünceler aktarılamaz. Hedef, "nerede kalmıştık" kaybının olmaması.

### 6.4 Hafıza: ikinci beyin

**Biçim** [Ö] (avenoxbeyin'den alınan ilkeler):
- Gerçeğin kaynağı Markdown kasası. İndeks (SQLite FTS5 + yerel embedding) kasanın dışında, yeniden üretilebilir.
- Notlarda frontmatter: `type` (user/project/home/tool/person/rule/decision), `project`, `visibility` (normal/private), `remote_allowed`, `sensitivity`, `review_at`, `aliases`, `facts`, `revision`.
- **Kaynak makbuzları:** Her kalıcı bilgi, hangi olaydan veya dosyadan geldiğini gösteren değiştirilemez ve hash adlı bir makbuzla bağlıdır. Model bir bilgi söylediğinde kaynağını gösterebilir, kaynakta olmayanı "bilmiyorum" der.
- Ajan iddiası ile doğrulanmış bilgi ayrıdır.

**Kasa yapısı** [Ö]:
```
harness-brain/
  me/            # kullanıcı: tercihler, kurallar, okul, çalışma biçimi
  projects/<ad>/ # proje notu, kararlar, talimatlar (AGENTS.md/CLAUDE.md bağlantısı)
  home/          # cihaz envanteri: cihaz, IP/MAC, port, servis, nasıl erişilir
  tools/         # kullanılan araçlar, hesaplar (gizli bilgi değil, yalnız adı ve amacı)
  people/        # anne, müşteriler: tercih edilen kanal, yetki seviyesi, notlar
  daily/         # günlük iş defteri (Daily Done)
  receipts/      # değiştirilemez kaynak makbuzları
  inbox/         # hızlı yakalama kartları
```

**Yazma:** Ajanın inisiyatifine bırakılmaz. Her iş adımından sonra harness ucuz bir modelle **hafıza önerileri** çıkarır. Öneriler tekrarlar birleştirilerek, çelişkiler işaretlenerek işlenir. Emin olunmayanlar ve `me/`, `people/`, `rule` türündeki değişiklikler kullanıcı onayına düşer. [Ö]

**Günlük iş defteri:** Biten her iş için tek satır: zaman, proje, klasör, motor, tek cümle özet, kota tüketimi. Bu defter ana ekran özetini, proje tahminini ve zaman/proje ısı haritasını besler. [Ö]

**Okuma:** Bağlam bütçesiyle `retrieve(query, project, audience, limit, budget_chars)` → kaynak atfı + kesilme bilgisi. `audience` alanı sızıntıyı önler: `remote_allowed=false` ya da `private` notlar ilgili motorlara (ör. API modeli) ve üçüncü kişilere gitmez. Finans botu notları varsayılan olarak `remote_allowed=false`'dur. [Ö]

**Hızlı yakalama:** Global kısayolla açılan küçük pencere; link, metin, dosya ya da ekran görüntüsünü `inbox/`'a kart olarak atar. [Ö]

**Başlangıç verisi:** `~/.claude/projects-catalog.md`, Claude hafıza dosyaları ve proje notları kasaya aktarılır. Claude Code oturum geçmişi (`~/.claude/projects/**/*.jsonl`) sohbet olarak içe aktarılır. ChatGPT dışa aktarımı sonradan eklenebilir. [Ö — varsayılan, kullanıcı itiraz etmedi]

### 6.5 Projeler ve sohbetler

ChatGPT/Codex masaüstü uygulamasının düzeni referans alınır, ama marka ve görsel kimlik kopyalanmaz. [K]
- **Kenar çubuğu:** Yeni sohbet, arama, bildirim zili, Sabitlenenler, Projeler (altında sohbetler), Projesiz sohbetler. Sohbette sabitle, yeniden adlandır, arşivle ve `...` menüsü.
- **Projeler = klasörler.** Katalogdaki projeler hazır gelir; yeni bir klasörde çalışılınca proje otomatik eklenir. Her projenin talimatları, hafıza notu ve dosyaları vardır.
- **Sohbet/Çalışma modu** düğmesi: hızlı soru ya da projede iş.
- **Arama:** Anlama göre (embedding + BM25) tüm sohbetlerde ve hafızada.
- **Otomatik başlık;** her sohbet herhangi bir motorla devam ettirilebilir (§6.3).

**Tek kutudan otomatik proje yönlendirme** [K]:
- Ana ekrandaki kutuya yazılan mesaj için yönlendirici (yerel model + günlük defter + embedding) projeyi tahmin eder. Sıra: mesajdaki açık işaretler (proje, dosya ya da özellik adı) → son aktif proje → emin değilse tek satırlık soru ("Masora mı, VibeAudit mi?").
- Seçilen projenin talimatları, hafızası ve çalışma durumu yüklenir. Asistan o projenin ajanı gibi davranır. Sohbet arka planda o projenin altına kaydedilir.
- Yanlış tahmin, "hayır, X projesi" ile düzeltilir ve yönlendirici bundan öğrenir.

### 6.6 Orkestrasyon

**Modlar** [K]: Tam, Dengeli, Eko. Başlangıç dağılımı [Ö]:

| | Beyin | Zor | Normal | Kolay | İnceleme |
|---|---|---|---|---|---|
| Tam | Opus / Sol high | Sol high | Sol medium | Luna | Sol medium/high |
| Dengeli | Opus / Sol | Sol medium | Terra | Luna / Flash | Sol medium |
| Eko | Sonnet / Terra | Sol medium (gerekirse) | Luna / Flash | Yerel | Sol medium |

- İnceleme hücreleri kullanıcının bugünkü kuralı gereği Sol'dur. Kullanıcı değiştirebilir.
- Pro pakette Tam mod Claude kotasını hızlı bitirir. Harness bu modda kotayı daha sık gösterir ve eşikte Dengeli'ye geçmeyi önerir.
- Model ve efor sınırları kullanıcı politikasından gelir: Sol/Terra en fazla high, Luna yalnız max, Astra yok.

**Görsel yönlendirme ekranı** [K]: Satırlarda iş türleri (mimari, kod, test, araştırma, inceleme, görsel, doküman, hafıza işleri), sütunlarda modlar, hücrelerde açılır menüden model ve efor. Her hücrenin yanında haftalık kota tüketimi ve başarısızlık sayısı. Bu ekran zamanla gelişecek; v1'de tablo yeterli.

**`codex-fleet` politikasının taşınması:** İş sınıfı → rota, kota kontrolü, inceleme kapıları (bağımsız doğruluk + güvenlik incelemesi), düzeltme/tırmanma sınırları ve brief şablonu harness'in orkestratör politikasına dönüşür. Skill bir başlangıç politikası olarak okunur. Kod içine gömülmez, kullanıcı düzenleyebilir. [Ö]

**Alt ajan yönetimi ve canlı izleme** [K]:
- Sohbetin sağ panelinde aktif ajanlar: motor, görev adı, süre, şu anki adım ve durum (çalışıyor, bekliyor, bitti, hata).
- Bir ajana tıklanınca ayrı pencere açılır. Şef→ajan brief'i, ajanın mesajları, çalıştırdığı komutlar, okuduğu dosyalar ve ajan→şef raporları anlık akar.
- Kullanıcı ajana doğrudan yazabilir, ajanı durdurabilir, başka bir motora devredebilir ya da işi devralabilir.
- Biten ajanın raporu, diff'i ve kota tüketimi sohbette kart olarak kalır.
- Sağ panelde ayrıca ChatGPT'deki gibi Değişiklikler (diff), Çıktılar (yerel önizleme) ve Kaynaklar bölümleri bulunur.

**Konsey** (Aşama 6): Aynı soruyu birden çok motora sorup cevapları karşılaştırmak ya da tartıştırmak. Ayrıntılı tasarım o aşamada yapılır.

### 6.7 Kota, token tasarrufu ve otomatik devam

**Kota paneli** [K]: Motor başına 5 saatlik ve haftalık pencereler, sıfırlanma saatleri, kalan **sıfırlama hakları** (haftalık limiti bir kereye mahsus sıfırlama), API harcaması. Ana ekranda kısa hali, Telegram'da `/limit`.
- Kaynak: Mevcut `limit` skill'inin veri yolları başlangıç noktası. Sıfırlama haklarının nereden okunacağı bilinmiyor; Aşama 2'de araştırılır. Otomatik okunamazsa kullanıcı elle girer. Harness uydurma bir sayı göstermez.
- Sıfırlama hakkını harness asla kendi başına kullanmaz. Yalnızca öneri verir ("haftalık %95, sıfırlanmaya 4 gün var").

**Otomatik devam** [K]: Kota dolunca görev "kota bekliyor" durumuna geçer. [Ö] Sıfırlanmaya kısa süre (ayarlanabilir, varsayılan 45 dk) kaldıysa bekler, o saatte uyanır ve kaldığı adımdan devam eder. Daha uzunsa kullanıcının belirlediği sıradaki motora devreder (bildirimle). Gece kendiliğinden devam ederken yalnızca daha önce onaylanmış işe devam edilir; yeni riskli bir adım gelirse durur ve onay Telegram'a düşer.

**Token tasarrufu ilkeleri** [K + Ö]:
1. Sık yapılan küçük işler yerel modelde (yönlendirme, onay, başlık, hafıza önerisi, log özeti, sınıflandırma).
2. Ucuz motor önce; zor iş pahalı motora.
3. Bağlam bütçesi: tüm geçmiş değil, devir paketi gönderilir.
4. Uzun komut çıktıları ve loglar modele gitmeden önce yerelde kırpılır ve özetlenir.
5. Her çalıştırmada yalnızca gereken araç ve MCP sunucuları yüklenir. Tasarruf Aşama 1'de ölçülür.
6. CLI oturum devamı kullanılır; önbelleğe alınmış bağlam daha ucuz.
7. Acil olmayan ağır işler kotası önce sıfırlanacak havuza ya da geceye kaydırılır.
8. Görev ve sohbet başına kota tüketimi görünür.
9. Ekonomik mod düğmesi (Eko orkestrasyon + küçük bağlam bütçesi + daha az otomatik kontrol).

### 6.8 Güvenlik ve onay

Dört katman [K]:
1. **Sabit kurallar** (harnessd'nin içinde, kodla tanımlı ve yalnız kullanıcının arayüzden değiştirebildiği). Her zaman kullanıcıya sorulanlar: silme (çöp dışı), canlı sunucuya deploy, finans botları ve canlı veritabanları, git geçmişini yeniden yazma ve force push, yayın ve mesaj gönderme (temsilci görevi dışında), kimlik bilgileri, sıfırlama hakkı kullanma, sistem ayarı ve yazılım kurulumu. Kullanıcı uzaktaysa soru Telegram'a gelir.
2. **Yerel onay modeli** (Jev tarzı, açık kaynak, API'siz): Gri alandaki araç çağrıları için "bu, kullanıcının istediği işin parçası mı?" kararı. Bağlam: görev hedefi + son adımlar + komut. Kararsızsa sorar. Model Aşama 3'te kullanıcının gerçek komut geçmişiyle ölçülerek seçilir. [K]
3. **Motorların kendi korumaları:** Claude Code izin modu, Codex sandbox ve onay politikası ek bir katman olarak açık kalır.
4. **Geri alınabilirlik:** Harness'in araçlarıyla yapılan silmeler `harness-trash/`'e taşınır (saklama süresi ayarlanabilir). Riskli toplu değişikliklerden önce anlık görüntü alınır (git stash/commit ya da dosya kopyası).

**Kapsam notu:** Motorlar kendi yerleşik kabuk araçlarıyla da silme yapabilir. Bu yüzden harness motorları, yerleşik yıkıcı araçları kısıtlanmış şekilde başlatır ve dosya işlemlerini kendi MCP araçlarına yönlendirir. Hangi motorda bunun ne kadar mümkün olduğu Aşama 1'de doğrulanır. Mümkün olmayan yerde sınır belgelenir ve o motor riskli görevlerde kullanılmaz. [Ö]

**Diğer kurallar:**
- Üçüncü kişi içeriği (web sayfası, e-posta, anne/müşteri mesajı, araç çıktısı) güvenilmez girdi olarak işaretlenir ve asla yetki vermez.
- Gizli bilgiler Credential Manager'da durur, loglara ve hafızaya yazılmaz. Push öncesi gizli anahtar taraması yapılır.
- Telegram botu yalnızca kullanıcının Telegram ID'sine yanıt verir. Bot token'ı Credential Manager'dadır.
- Doğrulama ilkesi: ajanın "bitti" demesi kanıt değildir. Kabul için test çıktısı, diff ve kontrol gerekir.

### 6.9 Kapılar

**Masaüstü uygulaması (ana arayüz)** [K] — Tauri 2 + React + TypeScript. Ekranlar:
- **Ana ekran:** İkinci beyin o anki ihtiyaca göre içeriği seçer (gece olanlar, bekleyen onaylar, öneriler, kota özeti). Yazı kutusu ve bekleyen onaylar hep aynı yerde. [K + Ö]
- **Sohbet:** Mesajlar, katlanır "X dk boyunca çalıştı" özetleri, model/mod seçici, devir düğmesi, erişim modu çipi, sağ panel (ajanlar, değişiklikler, çıktılar, kaynaklar).
- **Ajan penceresi:** §6.6.
- **Zamanlananlar:** Yeni görev, duraklatılanlar, geçmiş çalıştırmalar.
- **Özelleştir:** Motorlar, roller ve modlar (yönlendirme tablosu), MCP araçları, skill'ler, sabit kurallar, kişiler ve yetkiler.
- **Kota paneli, Hafıza tarayıcısı** (notlar, öneriler, makbuzlar, envanter), **Ayarlar.**
- ChatGPT özelliklerinden ✓ olanlar: sohbet/çalışma modu, kenar çubuğu, arama, bildirim, sağ panel, katlanır iş özetleri, erişim modu, model seçici, zamanlananlar, eklentiler/beceriler, mesaj eylemleri. ~ (sonra): kitaplık, görsel galerisi, sesli mod. ✗: Sites, Haritalar, GPT'ler.

**Telegram (uzaktan erişim)** [K]: Serbest metin emirleri (ana kutuyla aynı yönlendirme), `/limit`, `/durum`, `/onay` akışı (satır içi butonlar), ilerleme ve bitiş bildirimleri, hata olunca ekran görüntüsü ve log özeti. Uzun yanıtlar özetlenir, tamamı masaüstünde kalır.

**İleride:** Android (Tauri), GPUI arayüzü, istenirse Tailscale ile doğrudan erişim. Dışa açık port açılmaz.

### 6.10 Temsilci: üçüncü kişi konuşmaları

[K] Yalnızca kullanıcı görevlendirdiğinde çalışır ("müşteri yazdı, uygulama çerçevesinde ne istiyorsa hallet", "annem yazdı, bak ne istiyorsa hallet").
- **Görev:** Kişi + kanal + kullanıcının çizdiği kapsam. Kapsam dışı istekler (fiyat, söz, ödeme, teslim tarihi, başka iş) kullanıcıya sorulur.
- **Kimlik:** "Alperen'in asistanı" olarak tanıtır, kendini kullanıcı gibi göstermez. [Ö]
- **Yetki:** Kişinin mesajı komut çalıştırtamaz. Asistan yalnızca görevin gerektirdiği araçları kullanır (ör. anne için video indirip gönderme). Kullanıcının özel hafızası `audience=third_party` filtresiyle gizlenir.
- **Kapanış:** İş bitince görev kapanır ve kullanıcıya tek satır özet gider. Kişi sonradan yine yazarsa asistan cevap vermez, kullanıcıya haber verir.
- **Canlı izleme:** Konuşma alt ajan penceresi gibi izlenebilir; kullanıcı istediği an devralabilir.
- **Kanallar:**
  - Telegram: Müşteri bota yazıyorsa Bot API. Kullanıcının kişisel hesabı gerekiyorsa TDLib ile kullanıcı hesabı (resmi altyapı, ama hesap kullanıcı adına konuşur).
  - WhatsApp: **Açık karar (§9).** Kişisel hesapta resmi yol yok. Gayriresmi kütüphaneler WhatsApp kurallarına aykırı ve numara engellenme riski taşıyor. Resmi Business API ayrı numara ve ücret istiyor.

### 6.11 Bilgisayar kontrolü araçları

Harness motorlara MCP araçları sağlar (motordan bağımsız) [Ö]:
- Dosya işlemleri (silme çöpe gider), kabuk (güvenlik kapısından geçer).
- Masaüstü otomasyonu (agent-desktop), tarayıcı (Playwright ve kullanıcının Chrome'u), Roblox Studio MCP, SSH (sunucu listesi hafızadaki envanterden).
- Ekran görüntüsü, pano.
- `memory_search`, `history_search`, `daily_log`, `ask_user` (onay ya da soru; kullanıcı uzaktaysa Telegram'a gider).

## 7. Aşamalar

Her aşama kendi uygulama planıyla ilerler. Kod değiştiren her aşamada bağımsız doğruluk ve güvenlik incelemesi yapılır.

### Aşama 1 — Çekirdek (ayrıntılı)
Kapsam: harnessd iskeleti, SQLite şeması, olay günlüğü, Engine arayüzü, Claude/Codex/agy sürücüleri, tam döküm kaydı, çalışma durumu, devir paketi, `history_search`, yerel IPC, geçici bir CLI istemcisi (`harness chat`).
Kabul ölçütleri:
- Aynı sohbet Claude'la başlatılır, Codex'e devredilir, Codex son kararı ve sıradaki adımı doğru söyler (kaydedilmiş test senaryosu).
- Üç motorun akışları olay günlüğüne düşer. agy için canlı akış desteği raporlanır.
- Motor değişimi bildirim olayı üretir.
- Motor başına "yalnız gereken araçlarla başlatma" ve yerleşik yıkıcı araçları kısıtlama imkânı ölçülüp raporlanır.
- Token ölçümü: aynı görev için tam bağlam ile devir paketi karşılaştırılır.
- harnessd çökse bile tamamlanmış adımlar kaybolmaz (SQLite WAL, olay idempotent).

### Aşama 2 — Masaüstü v1 (ayrıntılı)
Kapsam: Tauri uygulaması; kenar çubuğu, projeler ve sohbetler, sohbet ekranı, model/mod seçici ve devir, sağ panelde ajan listesi ve canlı ajan penceresi (v1: izleme + durdurma + mesaj yazma), kota paneli (sıfırlama hakkı araştırması dahil), katalogdan proje içe aktarma, tek kutudan proje yönlendirmenin ilk sürümü (kural + son aktif proje + soru), Claude Code geçmişinin içe aktarılması.
Kabul ölçütleri:
- Masaüstü ve küçük pencere boyutlarında görsel kontrol.
- Uygulama kapatılıp açılınca sohbetler ve çalışan görevler yerinde.
- Canlı ajan penceresi gecikmesi 1 saniyenin altında (yerel).
- Kota paneli `limit` skill'iyle aynı değerleri gösterir; bilinmeyen değerler "bilinmiyor" diye görünür.

### Sonraki aşamalar (özet; her biri ayrı tasarım turu)
3. **Güvenlik ve onay:** Sabit kurallar arayüzü, çöp ve anlık görüntü, yerel onay modeli seçimi ve ölçümü. *Telegram'dan önce gelmesi şart.*
4. **Telegram kapısı:** Emir, onay, bildirim; otomatik devam ve keep-awake.
5. **İkinci beyin:** Kasa, makbuzlar, hafıza önerileri, embedding araması, günlük defter, ev envanteri, uyarlanır ana ekran, hızlı yakalama, ChatGPT dışa aktarım içe alma.
6. **Orkestrasyon v2 ve konsey:** Görsel yönlendirme tablosu, Tam/Dengeli/Eko, yerel ve API motorları, harcama sınırları, konsey.
7. **Temsilci:** Telegram (Bot API / TDLib), WhatsApp kararı.
8. **Sonra:** Android, GPUI, çok makine.

Not: Kullanıcı mevcut Claude Code + `codex-fleet` düzenini harness Aşama 4'e gelene kadar kullanmaya devam eder.

## 8. Kararlar özeti

**Kullanıcının kararları [K]:** Kişisel, açık kaynak; Tauri + Rust çekirdek + React; ana arayüz masaüstü, Telegram uzaktan erişim, mobil uygulama yok; üyelik CLI'ları (Claude, Codex, Antigravity), varsayılan olarak API yok, isteğe bağlı model eklenebilir ve her rolde kullanılabilir; ortak hafıza; bağlam kaybı olmadan devir; ChatGPT tarzı projeler/sohbetler ve arayüz; tek kutudan otomatik proje; tam bilgisayar kontrolü; istenmeyen silme yok; yerel Jev tarzı onay modeli; kota, sıfırlanma ve sıfırlama hakları görünürlüğü; otomatik devam; token tasarrufu; üç orkestrasyon modu, görsel yönlendirme, canlı alt ajan izleme; görevlendirmeyle üçüncü kişi temsilciliği.

**Claude'un kararları [Ö] (değiştirilebilir):** harnessd'nin kullanıcı bağlamında çalışması; depolama yerleri; Markdown kasa + SQLite FTS5 + yerel embedding; kaynak makbuzları; harness tarafından çıkarılan hafıza önerileri; günlük iş defteri; ev envanteri notu; sessiz geçiş yasağı; dışarıdan değişiklik bildirimi; sabit kuralların daemon içinde durması; dört katmanlı güvenlik; mod tablosundaki ilk dağılım; otomatik devam eşiği (45 dk); "Alperen'in asistanı" kimliği; aşama sırası; ad "harness"; Claude Code geçmişinin içe aktarılması.

## 9. Açık sorular ve riskler

1. **WhatsApp temsilciliği:** Gayriresmi kütüphane (engellenme riski) / resmi Business API (ayrı numara ve ücret) / WhatsApp'sız. Aşama 7'de kullanıcı karar verir.
2. **Sıfırlama haklarının veri kaynağı** bilinmiyor (Aşama 2 araştırması; olmazsa elle giriş).
3. **Antigravity akış desteği** belirsiz (Aşama 1 testi).
4. **Yerleşik yıkıcı araçların kısıtlanabilirliği** motorlara göre değişir (Aşama 1 ölçümü). Kısıtlanamayan motor riskli görevlerden çıkarılır.
5. **Sağlayıcı kuralları:** Claude için yalnızca resmi CLI ve kişisel kullanım. Antigravity için kullanıcı riski kabul etmiş. Kurallar değişirse motor ayarı kapatılabilir olmalı.
6. **Pro/Plus kotası:** Tam mod ve uzun döngüler kotayı hızla tüketir. Varsayılan mod Dengeli. [Ö]
7. **Yerel model kalitesi** 8 GB VRAM'de sınırlı. Yalnızca dar işler; onay modelinde kararsızlık durumunda kullanıcıya sorulur.
8. **Codex bilgisayar kontrolü** `codex exec` içinden doğrulanmamış; harness kendi araçlarını kullanır.
9. **Claude kota verisi:** Mevcut `limit` skill'i Claude kotasını `~/.claude/.credentials.json` içindeki OAuth token'ıyla `api.anthropic.com/api/oauth/usage` uç noktasından okuyor. Bu belgelenmemiş bir uç nokta. Kullanıcı kararı bekliyor (bkz. §11).

## 11. İnceleme sonrası netleştirmeler

Bağımsız builder incelemesinden (2026-11-15) çıkan sorular ve cevapları. Hepsi [Ö], aksi belirtilmedikçe.

1. **Aşama 1'de çalışma durumu nasıl üretilir?** Kararlar, dosya değişiklikleri, onaylar ve motor geçişleri olaylardan kural tabanlı olarak çıkarılır. Serbest metin alanları (hedef, sıradaki adım, açık sorular) agy Flash-low ile özetlenir; üyelikten çalışır, ek maliyet yok. agy kullanılamazsa o adımda alan boş kalır ve devir paketine son mesajlardan fazlası eklenir. Yerel model Aşama 3'te özetleyici olarak eklenebilir. Token ölçümü bu kurulumla yapılır.
2. **Çalışan ajana mesaj yazmak motorlara göre farklı.** Claude Code: kalıcı süreç ve stream-json girdi; mesaj o anki turdan sonra iletilir. Codex `exec`: tur ortasında enjeksiyon yok; mesaj kuyruğa alınır ve tur bitince `exec resume` ile gönderilir. Kullanıcı "kes ve gönder" de diyebilir. agy: Aşama 1 testine göre. Arayüz her motor için hangi davranışın geçerli olduğunu gösterir; desteklenmeyen eylem gizlenmez, "tur bitince iletilecek" etiketi gösterilir.
3. **Yıkıcı araç kısıtlaması Aşama 1'i bloklamaz.** Sonuç motor yetenek tablosuna yazılır. Aşama 2'de kısıtlanamayan motorun yanında uyarı rozeti çıkar. "Riskli görevlerden çıkarma" kuralı Aşama 3'te uygulanır.
4. **IPC:** harnessd ile Tauri'nin Rust tarafı arasında named pipe (yalnız mevcut kullanıcı ACL'i). Webview yalnızca Tauri komutları ve olaylarıyla konuşur; harnessd'ye doğrudan bağlanmaz, token webview'e verilmez. Telegram köprüsü harnessd'nin içinde çalışır.
5. **Yaşam döngüsü:** harnessd Aşama 1'den itibaren oturum açılışında Görev Zamanlayıcı ile başlar. Masaüstü uygulaması harnessd çalışmıyorsa onu başlatır. Uygulama kapatılınca harnessd ve motor alt süreçleri çalışmaya devam eder; "çalışan görevler yerinde" bu anlama gelir. Aşama 2 kabul testinde uygulama kapatılıp açılır, görev akışı kaldığı yerden görünür.
6. **Proje yönlendirmesinin verisi:** `~/.claude/projects-catalog.md` biçimi `- **Ad** — \`yol\` — açıklama`. İçe aktarılır; ad, klasör adı, takma adlar ve açıklama anahtar kelimeleri projeler tablosuna yazılır. Kural eşleştirmesi bu alanlara ve projedeki dosya/klasör adlarına göre yapılır. Ardından son aktif proje, en son da soru gelir.
7. **Claude Code geçmişinin içe aktarılması:** Tüm oturumlar alınır ama içe aktarmadan önce gizli bilgi taraması yapılır ve bulunanlar maskelenir. İçe aktarılan sohbetler `engine=claude (imported)` olarak işaretlenir; Claude'un kendi `--resume` özelliğiyle değil, devir paketiyle herhangi bir motorda devam ettirilir. Büyük hacim için içe aktarma artımlıdır ve indeksleme arka planda yapılır.
8. **Kota verisi:** Codex tarafı yerel oturum ölçümlerinden ve mevcut kaynaklardan okunur. Claude tarafı §9 madde 9'daki karara bağlı. Karar "hayır" olursa Claude kotası yalnızca CLI'ın verdiği sinyallerden (kota hatası, CLI'ın kendi kullanım çıktısı varsa o) ve harness'in kendi tüketim ölçümünden tahmin edilir; arayüzde "tahmini" etiketi gösterilir.
9. **agy referansı ve test verisi:** agy sürücüsü için referans `C:/Users/Alperen/.claude/lanes/lane.py`. Bu kullanıcının yerel betiği; kopyalanmaz, davranışı (komut satırı, model kuralı, hata sınıflandırması) temel alınır. Test fixture'ları: repoda yalnızca sentetik ya da temizlenmiş akışlar bulunur. Gerçek kayıtlar `%LOCALAPPDATA%\harness\fixtures-private\` altında durur, repoya girmez; push öncesi gizli anahtar taraması yapılır.

## 10. Builder'a bırakılanlar

- Crate ve paket seçimi (tokio, rusqlite/sqlx, tauri plugin'leri, MCP için `rmcp`), IPC biçimi (named pipe ya da localhost + token), şema ayrıntıları.
- CLI bayraklarının sürüme göre doğrulanması ve sürücü ayrıntıları.
- Embedding ve yönlendirme için yerel modelin seçimi (ölçüme dayalı).
- Arayüz bileşen kütüphanesi ve görsel tasarım (ChatGPT düzeni referans, özgün görünüm).
- Test altyapısı: motor sürücüleri için kaydedilmiş akış fixture'ları, devir senaryoları için uçtan uca testler.
