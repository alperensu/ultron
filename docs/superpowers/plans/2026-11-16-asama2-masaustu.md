# Ultron Aşama 2 — Masaüstü Uygulaması: Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ultrond`'a bağlanan, iOS karanlık mod hissinde, akıcı animasyonlu bir Tauri 2 masaüstü uygulaması: projeler ve sohbetler, canlı sohbet akışı, motor devri, ajan penceresi, gerçek kota paneli, Claude Code geçmişinin içe aktarılması, tek kutudan proje yönlendirme ve Ultron talimat katmanı.

**Architecture:** Yeni workspace üyesi `apps/desktop/src-tauri` (crate `ultron-desktop`), `ultron-core::ipc::Client` ile `ultrond`'a bağlanan ince bir köprüdür; iş mantığı yoktur. Webview (Vite + React + TypeScript) yalnızca Tauri komutları ve `ultron://event` olayıyla konuşur. Yeni iş mantığı (proje kataloğu, yönlendirme, geçmiş içe aktarma, kota, talimatlar) `ultron-core`'a, yeni istekler `ultrond`'a eklenir.

**Tech Stack:** Rust (mevcut workspace, MSVC), Tauri 2, React 18, TypeScript (strict), Vite 6, Motion (motion/react), TanStack Virtual, react-markdown + rehype-sanitize, Zustand, Vitest + Testing Library, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-15-harness-design.md` (§6.3–§6.7, §6.9, §11, talimat katmanı notları). Görsel referans: `docs/design/mockups/ultron-v2-ios.html` (onaylanan yön: iOS karanlık mod, üst sekme yok, kenar çubuğu gezinme, kota halkası).

## Global Constraints

- Yalnızca koyu tema. Renkler `:root` token'larında (maketteki iOS karanlık palet: #000, #1C1C1E, #2C2C2E, ayırıcı rgba(84,84,88,0.65), vurgu #0A84FF, durum #30D158/#FF9F0A/#FF453A). Serif yazı yok; sistem yazı tipi yığını maketteki gibi.
- Animasyon yalnızca `transform`/`opacity`; yay eğrisi `cubic-bezier(0.32,0.72,0,1)`; `prefers-reduced-motion` ve Windows "animasyonları azalt" ayarına uyulur. Hedef: 60 FPS, uzun listeler sanallaştırılmış.
- **Sahte buton yok.** Henüz arka ucu olmayan özellikler (onaylar, öneriler, hafıza, Telegram, konsey) arayüzde **gösterilmez**; boş durum metni dürüstçe "Aşama N'de" der. Örnek veri yalnızca testlerde.
- Webview `ultrond`'a doğrudan bağlanmaz, token webview'e verilmez. Tauri yetenekleri (capabilities) en az düzeyde: fs/shell/http eklentisi yok. Sıkı CSP (`default-src 'self'`; harici script yok). Markdown çıktısı sanitize edilir; ham HTML render edilmez.
- Repoda gerçek veri yok: katalog, geçmiş ve kota testleri sentetik verilerle. Kişisel yollar ve ad yok.
- Claude kota okuması: `~/.claude/.credentials.json` içindeki token yalnızca bellekte, yalnızca kullanım uç noktasını okumak için; loglanmaz, diske yazılmaz; `config.toml` içindeki `claude_live_quota = false` ile kapanır; hata durumunda "tahmini" yola düşer ve bildirir (spec §9.9 kararı).
- Bağımlılıklar tam sürümle sabitlenir (ör. `@tauri-apps/cli@2.x.y`), npm kullanılır (`package-lock.json` commit edilir).
- Komutlar: Rust `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`; UI `npm --prefix apps/desktop run test`, `npm --prefix apps/desktop run lint`, `npm --prefix apps/desktop run typecheck`, `npm --prefix apps/desktop run e2e`.
- Canlı CLI testleri `#[ignore]` ve yalnızca onayla.

## Review Focus

1. **Servis kapalıyken uygulama açılır:** Uygulama `ultrond`'u başlatır; başlatamazsa net bir hata ekranı gösterir, beyaz ekran yok. Test: Task 8 `bridge_starts_daemon_or_reports_error`.
2. **Uzun ve hızlı akış:** 5.000 olaylı sohbet ve saniyede 50 olay akışı arayüzü kasmaz; kaydırma konumu korunur. Test: Task 11 e2e `chat_streams_5000_events_smoothly` (kare süresi p95 < 20 ms).
3. **Kötü niyetli içerik:** Motor çıktısındaki HTML/script, markdown linkleri `javascript:` vb. çalışmaz. Test: Task 11 `markdown_is_sanitized`.
4. **İçe aktarmada gizli bilgi:** Claude geçmişindeki token/anahtar desenleri maskelenir; bozuk JSONL satırları atlanır. Test: Task 4 `import_masks_secrets`, `import_skips_malformed_lines`.
5. **Kota kaynağı bozuk/yok:** Uç nokta hata verir ya da dosya yoksa panel "bilinmiyor/tahmini" gösterir, çökmez; token hiçbir log satırında geçmez. Test: Task 5 `quota_falls_back_when_endpoint_fails`, `token_never_logged`.

---

### Task 1: Ortak istemci ve servis başlatma `ultron-core`'a taşınır

**Files:** Create `crates/ultron-core/src/client.rs`; Modify `crates/ultron/src/commands.rs` (kendi `connect_or_start`'ını core'dakiyle değiştir), `crates/ultron-core/src/lib.rs`. Test `crates/ultron-core/tests/client.rs`.

**Interfaces — Produces:** `pub async fn connect_or_start(paths: &Paths, ultrond_exe: &Path) -> Result<Client>`; `pub fn locate_ultrond(current_exe: &Path) -> Result<PathBuf>` (aynı klasörde `ultrond.exe`, yoksa `ULTROND_PATH`).

- [ ] Test `locate_ultrond_prefers_sibling` ve `connect_or_start_reports_missing_daemon` (yanlış yol → anlamlı hata) yaz → FAIL → uygula (mevcut davranışı taşı, SID doğrulaması ve handle kalıtımı temizliği korunur) → PASS → `ultron` testleri hâlâ geçer → commit `refactor(core): istemci ve servis başlatma çekirdeğe taşındı`.

### Task 2: Servis API v2 ve şema v3

**Files:** Modify `crates/ultron-core/src/ipc.rs`, `crates/ultron-core/src/db/{mod.rs,migrations.rs}`, `crates/ultrond/src/server.rs`. Test `crates/ultrond/tests/api_v2.rs`, `crates/ultron-core/tests/db.rs`.

**Interfaces — Produces:**
- Şema v3 (ekleyici): `projects(id, name, path, aliases_json, description, last_active_at)`, `chats` + `pinned INTEGER DEFAULT 0`, `archived INTEGER DEFAULT 0`, `title_source TEXT DEFAULT 'auto'`, `updated_at`.
- `Request` yeni varyantlar: `ListProjects`, `ListChats { project_id: Option<String>, include_archived: bool }`, `GetChat { chat_id }` (→ chat meta + work_state + son motor), `UpdateChat { chat_id, title: Option<String>, pinned: Option<bool>, archived: Option<bool>, project_id: Option<Option<String>> }`, `GetQuota`, `ImportCatalog { path }`, `ImportClaudeHistory { root: Option<String> }`, `Route { text }`.
- Yanıt verileri JSON şemaları `ipc.rs` içinde `#[derive(Serialize)]` struct olarak: `ProjectInfo`, `ChatInfo { id, title, project_id, pinned, archived, updated_at, engine }`, `ChatDetail`, `QuotaReport` (Task 5), `RouteResult` (Task 3), `ImportReport { files, chats, messages, skipped, masked }`.
- `Send` içinde `project_id` verilmişse yeni sohbet o projeye bağlanır ve `cwd` projenin yolu olur.
- Sohbet başlığı: ilk kullanıcı mesajından en fazla 60 karakter (sözcük sınırında), `title_source='auto'`; kullanıcı değiştirince `'user'`.

- [ ] Testler: `list_chats_orders_pinned_then_recent`, `update_chat_rename_pin_archive`, `send_with_project_binds_chat_and_cwd`, `auto_title_from_first_message`, `migration_v3_is_additive` → FAIL → uygula → PASS → commit `feat(ultrond): sohbet ve proje API'si`.

### Task 3: Proje kataloğu ve yönlendirme v1

**Files:** Create `crates/ultron-core/src/projects.rs`, `crates/ultron-core/src/routing.rs`. Test `crates/ultron-core/tests/routing.rs` (sentetik katalog fixture'ı).

**Interfaces — Produces:**
- `parse_catalog(md: &str) -> Vec<ProjectSeed>`: satır biçimi `- **Ad** — \`yol\` — açıklama` (tire/uzun tire toleranslı); bozuk satır atlanır.
- `RouteResult { project_id: Option<String>, confidence: f32, reason: String, alternatives: Vec<String> }`.
- `route(db: &Db, text: &str, active_project: Option<&str>) -> Result<RouteResult>`: sıra (spec §6.5): (1) metinde proje adı/takma adı/klasör adı ya da projedeki üst düzey dosya/klasör adı (sözcük sınırı, Türkçe büyük-küçük harf duyarsız) → 0.9; birden çok eşleşme → en yüksek skor, diğerleri `alternatives`; (2) yoksa son aktif proje → 0.5; (3) hiçbiri yoksa `None`.
- Arayüz kuralı: confidence < 0.6 ve alternatif varsa kullanıcıya tek satır soru.

- [ ] Testler: `catalog_parses_and_skips_bad_lines`, `route_explicit_name_wins`, `route_ambiguous_returns_alternatives`, `route_falls_back_to_last_active`, `route_turkish_case_insensitive` ("ÖDEME" ↔ "ödeme") → FAIL → uygula → PASS → commit `feat(core): proje kataloğu ve yönlendirme`.

### Task 4: Claude Code geçmişinin içe aktarılması

**Files:** Create `crates/ultron-core/src/import/{mod.rs,claude_history.rs,secrets.rs}`. Test `crates/ultron-core/tests/import.rs` + `tests/fixtures/claude_history/**` (sentetik).

**Interfaces — Produces:**
- `mask_secrets(s: &str) -> (String, usize)`: en az şu desenler `[MASKED]` olur: `sk-ant-…`, `sk-…` (≥20), `ghp_|gho_|github_pat_…`, `AKIA[0-9A-Z]{16}`, `xox[baprs]-…`, `AIza…`, JWT (`eyJ…\.eyJ…\.`), `-----BEGIN … PRIVATE KEY-----` blokları, URL içi `user:pass@`, `password=|token=|secret=` sonrası değer.
- `import_claude_history(db: &Db, root: &Path, state: &mut ImportState) -> Result<ImportReport>`: `root/**/*.jsonl` dosyalarında yalnızca kullanıcı ve asistan metin mesajları alınır (araç çağrıları özet satırına indirgenir); her oturum dosyası bir sohbet; oturumun `cwd` alanı bilinen bir proje yoluyla eşleşirse o projeye bağlanır; sohbet `engine = "claude-imported"`; artımlı: dosya başına son işlenen bayt ofseti `import_state` tablosunda; aynı dosya iki kez içe aktarılınca mesaj çoğalmaz.
- İçe aktarılan sohbet devir paketiyle (native resume değil) herhangi bir motorda sürdürülür.

- [ ] Testler: `import_masks_secrets`, `import_skips_malformed_lines`, `import_is_incremental_and_idempotent`, `import_binds_project_by_cwd` → FAIL → uygula → PASS → commit `feat(core): claude geçmişi içe aktarma`.

### Task 5: Kota modülü ve bütçe modu

**Files:** Create `crates/ultron-core/src/quota/{mod.rs,claude.rs,codex.rs,mode.rs}`. Test `crates/ultron-core/tests/quota.rs`.

**Interfaces — Produces:**
- `QuotaWindow { label: String, used_pct: Option<f32>, resets_at: Option<DateTime<Utc>>, window_secs: u64, source: QuotaSource }`, `enum QuotaSource { Live, Estimated, Unknown }`.
- `PoolReport { pool: Pool, windows: Vec<QuotaWindow>, reset_rights: Option<u32>, mode: BudgetMode }`, `enum Pool { Claude, Codex, Agy }`, `enum BudgetMode { Tam, Dengeli, Eko }`, `QuotaReport { pools: Vec<PoolReport>, fetched_at }`.
- `budget_mode(windows: &[QuotaWindow], now) -> BudgetMode`: her pencere için `pace = (1 - used/100) / (kalan_süre / pencere_süresi)`; en kötü pencere: pace ≥ 1.0 → Tam; 0.5 ≤ pace < 1.0 → Dengeli; pace < 0.5 ya da used ≥ 90 → Eko; ölçüm yoksa Dengeli.
- Claude canlı: `credentials.json` → `claudeAiOauth.accessToken` (süresi geçmişse canlı yok) → `GET https://api.anthropic.com/api/oauth/usage` (header `anthropic-beta: oauth-2025-04-20`), 5 sn zaman aşımı, 60 sn önbellek. HTTP için `reqwest` (rustls, workspace'e eklenir; gerekçe raporlanır). Yanıt alan adları kullanıcının mevcut kota betiğiyle aynıdır; worker referans olarak kullanıcının yerel kota betiğini (yolu brief'te verilir) **yalnızca okur**, kopyalamaz.
- Codex: `~/.codex/sessions/**` altındaki en yeni oturum JSONL'ındaki son `rate_limits` kaydı (primary/secondary `used_percent`, `resets_in_seconds`/`resets_at`); kaynak yoksa Unknown. Referans aynı betik.
- Agy: ölçüm yok → Unknown/Dengeli.
- Sıfırlama hakları: `config.toml` `[reset_rights] claude = N, codex = M` (elle); arayüzden düzenlenir (Task 2'ye `SetResetRights { pool, count }` isteği eklenir).
- Token hiçbir `tracing`, hata mesajı ya da IPC yanıtında yer almaz.

- [ ] Testler: `budget_mode_thresholds` (tablo testi), `quota_falls_back_when_endpoint_fails` (yerel sahte HTTP sunucusu ya da enjekte edilebilir HTTP istemcisi), `codex_rate_limits_parsed_from_session_fixture`, `token_never_logged` (tracing yakalayıcı + hata metinlerinde token yok), `live_quota_disabled_by_config` → FAIL → uygula → PASS → commit `feat(core): kota okuma ve bütçe modu`.

### Task 6: Ultron talimat katmanı

**Files:** Create `prompts/{core.md,role-brain.md,role-worker.md,role-delegate.md,mode-general.md,NOTICE}`, `crates/ultron-core/src/prompts.rs`; Modify `engine/{claude.rs,codex.rs,agy.rs}`, `session.rs`. Test `crates/ultron-core/tests/prompts.rs`.

**Interfaces — Produces:**
- İçerik: spec "Talimat katmanı" + konuşmadaki taslak (ortak çekirdek: değişmez kurallar + Karpathy esinli 4 ilke; şef; işçi; temsilci). Kişisel bilgi yok; çekirdek kart Aşama 5'te hafızadan gelir (şimdilik yok). `NOTICE`: "Karpathy-inspired guidelines, multica-ai/andrej-karpathy-skills, MIT" atfı.
- `enum Role { Brain, Worker, Reviewer, Delegate }`, `enum WorkMode { Code, General }`; `compose(role, mode, project_files: &[PathBuf]) -> String` (sabit kısımlar önce). Dosyalar `include_str!` ile gömülür; `ULTRON_HOME/prompts/*.md` varsa onu kullanır.
- Enjeksiyon: Claude → `--append-system-prompt-file <run\prompt-<chat>.md>` (yerleşik talimat korunur); Codex ve agy → istemin başına `[ULTRON TALİMATLARI]` bloğu (yerleşik talimatlar değiştirilmez). Sohbette varsayılan rol Brain; WorkMode: sohbet bir projeye bağlı ve cwd'de kod varsa Code, değilse General.
- **Genel mod sondajı:** Claude'un output-style mekanizması `--settings` ile uygulanabiliyor mu? `#[ignore] live_output_style_probe`: geçici bir `.claude/output-styles/ultron-genel.md` içeren çalışma dizininde `--settings '{"outputStyle":"ultron-genel"}'` ile kısa bir tur; sonuç (uygulandı mı, kod talimatları kalktı mı) rapora. Uygulanamıyorsa General modda `mode-general.md` yalnızca append edilir. Kullanıcının `~/.claude` klasörüne yazılmaz.

- [ ] Testler: `compose_orders_stable_first`, `worker_prompt_has_no_core_card_placeholder_leak`, `claude_command_appends_prompt_file`, `codex_prompt_prefixed_not_replaced` → FAIL → uygula → PASS → canlı sondaj bir kez → commit `feat(core): ultron talimat katmanı`.

### Task 7: Masaüstü iskeleti ve tasarım sistemi

**Files:** Create `apps/desktop/**` (Vite React TS), `apps/desktop/src-tauri/**` (crate `ultron-desktop`, root `Cargo.toml` workspace üyesi), `apps/desktop/AGENTS.md`; Modify root `AGENTS.md` haritası.

**Interfaces — Produces:**
- `src/design/tokens.css` (maket paleti, tipografi ölçeği: Large Title 34/700, Title 22/600, Headline 17/600, Body 15/17, Footnote 13), `src/design/motion.ts` (`spring`, `stagger`, `reducedMotion()`), bileşenler `src/ui/{Sidebar,NavBar,LargeTitle,GroupedList,Row,Sheet,CapsuleButton,Switch,QuotaRing,Chip,EmptyState}.tsx`.
- Tauri: pencere 1280×800 min 1024×700, başlık çubuğu özel (sürüklenebilir NavBar), `capabilities/default.json` yalnızca kendi komutları; CSP.
- Kurallar `apps/desktop/AGENTS.md`'de (koyu tema, sahte buton yok, sanitize, token yok).

- [ ] Testler (Vitest): `QuotaRing renders percent and color thresholds`, `Sheet traps focus and closes on Escape`, `reduced motion disables transforms`; `npm run lint`, `typecheck` temiz; `npx tauri build --debug` başarılı → commit `feat(desktop): iskelet ve tasarım sistemi`.

### Task 8: Tauri köprüsü

**Files:** Create `apps/desktop/src-tauri/src/{bridge.rs,commands.rs,events.rs}`, `apps/desktop/src/lib/bridge.ts` (tipli sarmalayıcı + test için sahte köprü).

**Interfaces — Produces:**
- Tauri komutları (her biri `ultrond`'a tek istek): `list_projects`, `list_chats`, `get_chat`, `update_chat`, `send_message { chat_id?, text, engine?, project_id? }`, `switch_engine`, `cancel_turn`, `events_page { chat_id, after_seq, limit }`, `history_search`, `get_quota`, `set_reset_rights`, `route_message`, `import_catalog`, `import_claude_history`, `status`.
- Olay köprüsü: uygulama açılınca `Events { follow: true }` aboneliği; her olay `ultron://event` olarak yayılır; bağlantı koparsa üstel geri çekilmeyle yeniden bağlanır ve son `seq`'ten devam eder.
- Başlangıç: `connect_or_start` (Task 1). Başarısızsa arayüze `DaemonUnavailable { reason }`.

- [ ] Testler (Rust): `bridge_starts_daemon_or_reports_error`, `event_bridge_resumes_after_reconnect` (sahte pipe sunucusu ya da `ULTRON_PIPE_NAME` ile gerçek `ultrond` + FakeEngine) → FAIL → uygula → PASS → commit `feat(desktop): servis köprüsü`.

### Task 9: Kenar çubuğu, Bugün ekranı ve tek kutu

**Files:** `apps/desktop/src/screens/{Today.tsx}`, `src/features/{sidebar,composer}/**`, `src/state/store.ts`.

**Davranış:**
- Kenar çubuğu (maket v2 sırası): Ultron, Yeni sohbet, Ara (history_search sonuçları), Bugün, Sabitlenenler, Projeler (açılır, altında sohbetler), Projesiz sohbetler, altta Ayarlar ve servis durumu. Bildirimler ve Hafıza **gösterilmez** (Aşama 4/5). Sağ tık / `…` menüsü: yeniden adlandır, sabitle, arşivle, projeye taşı.
- Bugün: büyük başlık + tek satır özet; "Sen yokken" = son 24 saatte biten turlar (`turn_completed` olaylarından, sohbet başlığı + motor + süre); kota şeridi; boş durumlar dürüst. "Seni bekleyen" ve "Önerim" bölümleri yok (Aşama 3/5).
- Tek kutu (yapışkan alt bar): yazarken 300 ms gecikmeyle `route_message`; çip "→ Proje (otomatik)"; tıklanınca proje seçici; confidence < 0.6 ve alternatif varsa gönderimden önce tek satır soru. Gönderince yeni sohbet açılır ve Sohbet ekranına geçilir (paylaşılan öğe animasyonu).

- [ ] Testler: Vitest `composer shows routed project chip`, `ambiguous route asks before sending`; Playwright (sahte köprü) `sidebar_navigation_and_context_menu`, `today_shows_recent_turns` → commit `feat(desktop): kenar çubuğu, bugün ve tek kutu`.

### Task 10: Sohbet ekranı

**Files:** `apps/desktop/src/screens/Chat.tsx`, `src/features/chat/**` (`MessageList`, `WorkSummary`, `Markdown`, `EngineMenu`).

**Davranış:**
- Olaylar `events_page` ile sayfalı yüklenir, canlı olaylar akıştan eklenir; TanStack Virtual; kullanıcı yukarıdayken otomatik kaydırma durur, "yeni mesajlar ↓" kapsülü çıkar.
- Ardışık `assistant_text` parçaları tek balonda birleşir (kare başına toplu render). İki asistan mesajı arasındaki araç/komut/dosya olayları katlanır **"N dk boyunca çalıştı ›"** satırına; açılınca tek satırlık özetler (komut + çıkış kodu, dosya + değişiklik).
- Markdown: react-markdown + rehype-sanitize; kod blokları kopyalanabilir; linkler yalnızca `https:`/`http:` ve harici tarayıcıda açılır.
- Üst bar: sohbet başlığı (büyük başlık kaydırınca küçülür), motor menüsü ("Motor: Claude ▾" → Claude/Codex/agy; değiştirmek `switch_engine` + onay sheet'i), çalışırken "Durdur".
- `EngineSwitched` ve `QuotaExhausted` olayları sohbette belirgin sistem satırı olarak görünür (sessiz geçiş yok).
- Sağ panel sekmeleri: **Ajanlar** ve **Değişiklikler** (FileChanged listesi). Çıktılar/Kaynaklar yok (sonraki aşama).

- [ ] Testler: Vitest `fragments_coalesce_into_one_bubble`, `work_summary_groups_tool_events`, `markdown_is_sanitized` (`<script>`, `<img onerror>`, `javascript:` linki etkisiz); Playwright `chat_streams_5000_events_smoothly` (sahte köprü saniyede 50 olay; `performance` ile kare süreleri ölçülür, p95 < 20 ms), `engine_switch_shows_system_line` → commit `feat(desktop): sohbet ekranı`.

### Task 11: Ajan paneli ve ajan penceresi

**Files:** `src/features/agents/**` (`AgentList`, `AgentSheet`).

**Davranış:**
- Ajan listesi: bu sohbetin motor oturumları (motor, durum, süre, o anki adım = son olayın tek satır özeti) + Claude oturumunda `Task`/alt ajan araç çağrıları (ToolCall `tool == "Task"`; açıklama ve durum ToolResult'tan).
- Ajan penceresi (alt sheet): seçilen oturumun olayları canlı akar (satırlar kayarak belirir); **Durdur** = `cancel_turn`; **Devral** = motor seçip `switch_engine`; **Mesaj yaz** = mesaj kuyruğa alınır ve tur bitince aynı sohbete gönderilir, satırda "tur bitince iletilecek" etiketi (tur ortası enjeksiyon sonraki aşamada; spec §11.2).

- [ ] Testler: Vitest `queued_message_sent_after_turn_completed`; Playwright `agent_sheet_streams_and_stops` → commit `feat(desktop): ajan paneli ve penceresi`.

### Task 12: Kota paneli, ayarlar ve içe aktarma akışı

**Files:** `src/features/quota/**`, `src/screens/Settings.tsx`.

**Davranış:**
- Üst bardaki kota halkası en sıkışık havuzun doluluğu ve rengi (≥90 kırmızı, ≥70 turuncu, aksi yeşil); tıklayınca sağdan sheet: havuz başına pencereler (çubuk + "sıfırlanma 2 sa 10 dk"), kaynak etiketi (Canlı/Tahmini/Bilinmiyor), bütçe modu rozeti, sıfırlama hakkı sayısı (düzenlenebilir). Her 60 sn ve sheet açılınca yenilenir.
- Ayarlar: servis durumu; "Claude kotasını canlı oku" anahtarı; varsayılan motor; "Proje kataloğunu içe aktar" (dosya seçici Tauri dialog **yerine** yol metin kutusu — dialog eklentisi eklenmez) ve "Claude Code geçmişini içe aktar" (ilerleme + `ImportReport` sonucu: dosya/sohbet/mesaj/atlanan/maskelenen).

- [ ] Testler: Vitest `quota_ring_color_thresholds`, `quota_sheet_shows_source_labels`, `import_report_rendered`; Playwright `settings_toggle_live_quota` → commit `feat(desktop): kota paneli, ayarlar ve içe aktarma`.

### Task 13: Kabul, performans ve rapor

**Files:** `apps/desktop/e2e/acceptance.spec.ts`, `docs/reports/2026-xx-xx-asama2-kabul.md`.

- [ ] Gerçek `ultrond` + FakeEngine ile uçtan uca: uygulama açılır → tek kutudan mesaj → sohbet akar → motor devri sistem satırı → uygulama kapatılıp açılınca sohbet ve olaylar yerinde.
- [ ] Ekran görüntüleri 1280×800 ve 1920×1080 (Bugün, Sohbet, Ajan sheet, Kota sheet); `prefers-reduced-motion` açıkken animasyon yok.
- [ ] Performans: Task 10 ölçümü + uygulama boşta RAM (Görev Yöneticisi değeri rapora).
- [ ] Bir kez canlı: Claude ile gerçek kısa sohbet + kota panelinde canlı Claude verisi (onaylı).
- [ ] `npx tauri build` (release) kurulum dosyası üretir; boyutu rapora.
- [ ] Rapor + commit `test: aşama 2 kabul raporu`.

---

## Kapsam dışı
Onay kuyruğu, çöp kutusu, yerel onay modeli (Aşama 3); Telegram ve otomatik devam (Aşama 4); hafıza, uyarlanır ana ekran, öneriler, ev envanteri (Aşama 5); orkestrasyon modlarının motor seçimine uygulanması, konsey, API/yerel motorlar (Aşama 6); tur ortası mesaj enjeksiyonu; ChatGPT geçmişi içe aktarma.
