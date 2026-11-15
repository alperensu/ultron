# Ultron Aşama 1 — Çekirdek: Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Claude Code, Codex ve agy CLI'larını üyelikle süren; her olayı kaydeden, çalışma durumunu tutan ve bir sohbeti motorlar arasında bağlam kaybı olmadan devredebilen bir Rust çekirdek servisi (`ultrond`) ve geçici bir terminal istemcisi (`ultron`).

**Architecture:** Bir Cargo workspace'i, üç crate'ten oluşur: `ultron-core` (kütüphane: yollar, SQLite, olaylar, motor arayüzü, sürücüler, çalışma durumu, devir paketi, IPC protokolü), `ultrond` (servis) ve `ultron` (CLI istemci). Motorlar alt süreç olarak başlatılır, JSON akışları satır satır `EngineEvent`'e çevrilir, olay günlüğüne (SQLite + JSONL döküm) yazılır. İstemci servisle named pipe üzerinden satır başına bir JSON mesajla konuşur.

**Tech Stack:** Rust stable (edition 2024), tokio, serde/serde_json, rusqlite (bundled, FTS5), thiserror/anyhow, tracing, uuid, chrono, clap, tempfile (test), windows-sys (keep-awake, ACL).

**Spec:** `docs/superpowers/specs/2026-11-15-harness-design.md` (§5, §6.1–6.3, §6.8 kapsam notu, §7 Aşama 1, §11)

## Global Constraints

- Platform: Windows 11, kullanıcı oturumu bağlamı. Windows Service değil.
- Varsayılan yolda API anahtarı yok; motorlar yalnızca resmi CLI'larla çağrılır: `claude` (2.1.325 ile doğrulandı), `codex` (0.154.0), `agy` (1.5.8).
- Aşama 1'de `~/.claude/.credentials.json` ve herhangi bir OAuth token'ı **okunmaz** (kota okuma Aşama 2'de).
- Codex modelleri: `gpt-6.1-sol`, `gpt-6.1-terra`, `gpt-6-luna`. Astra adlı model yok. Sol/Terra efor en fazla `high`, Luna yalnız `max`.
- agy modeli varsayılan `gemini-3.8-flash-low`; yalnız Gemini modelleri kabul edilir.
- Durum kökü: `%LOCALAPPDATA%\ultron\` (testlerde `ULTRON_HOME` ortam değişkeni bunu ezer). Alt dizinler: `state\ultron.db`, `transcripts\`, `fixtures-private\`, `run\`.
- Repoda gerçek oturum kaydı, kişisel veri ya da gizli bilgi bulunmaz. Fixture'lar sentetiktir. Gerçek kayıtlar `fixtures-private\` altında kalır.
- Ad tek bir sabitte: `ultron_core::APP_NAME = "ultron"`.
- Canlı CLI testleri `#[ignore]` ile işaretlidir ve yalnızca `cargo test -- --ignored live_` ile, kullanıcı onayıyla çalışır (kota harcar).

## Review Focus

1. **CLI süreci akış ortasında çöker ya da öldürülür:** Yarım kalan son satır atılır, oturum `Failed` olur, o ana kadarki olaylar korunur. Test: Task 4 `runner_child_killed_midstream_keeps_events`.
2. **Yeni CLI sürümünden bilinmeyen JSON olay tipi:** Çökme yerine `EngineEvent::Unknown { raw }` üretilir ve kaydedilir. Test: Task 5/6/7 `*_unknown_event_type_is_preserved`.
3. **Motor kota hatası:** Genel hata yerine `EngineEvent::QuotaExhausted` üretilir (sonraki aşamalarda otomatik devamı besler). Test: Task 5/6/7 `*_quota_error_maps_to_quota_exhausted`.
4. **Türkçe metin ve boşluklu Windows yolları** (ör. `C:\Users\Alperen\Documents\ChatGPT\roblox 2`): Argümanlar ve çalışma dizini bozulmadan iletilir. Test: Task 4 `runner_passes_unicode_and_spaced_cwd`.
5. **Servis oturum ortasında yeniden başlar:** Olaylar tekrar yazılmaz, oturum durumu veritabanından geri yüklenir. Test: Task 2 `event_insert_is_idempotent`, Task 12 `daemon_restart_restores_sessions`.

---

### Task 0: Araç zinciri ve workspace iskeleti

**Ön koşul (kullanıcı onayı gerekir; yazılım kurulumu):** `rustup` (stable, `x86_64-pc-windows-msvc`) ve MSVC C++ Build Tools. Onay yoksa plan burada durur.

**Files:**
- Create: `Cargo.toml` (workspace: `crates/ultron-core`, `crates/ultrond`, `crates/ultron`), `rust-toolchain.toml` (`channel = "stable"`), `.gitignore` (`target/`, `*.db`, `fixtures-private/`), `crates/*/Cargo.toml`, `crates/ultron-core/src/lib.rs`, `crates/ultrond/src/main.rs`, `crates/ultron/src/main.rs`, `README.md` (tek paragraf: ne olduğu + spec bağlantısı).

**Interfaces:**
- Produces: `ultron_core::APP_NAME: &str = "ultron"`, `ultron_core::VERSION: &str = env!("CARGO_PKG_VERSION")`.

- [ ] **Step 1:** Workspace'i oluştur, `crates/ultron-core/tests/smoke.rs` içine `assert_eq!(ultron_core::APP_NAME, "ultron")` testi yaz.
- [ ] **Step 2:** Run `cargo test --workspace` → PASS. Run `cargo clippy --workspace -- -D warnings` → temiz.
- [ ] **Step 3:** Commit `chore: cargo workspace iskeleti`.

### Task 1: Yollar ve yapılandırma

**Files:**
- Create: `crates/ultron-core/src/paths.rs`, `crates/ultron-core/src/config.rs`
- Test: `crates/ultron-core/tests/paths_config.rs`

**Interfaces:**
- Produces:
  - `struct Paths { root, db, transcripts, fixtures_private, run: PathBuf }`, `Paths::resolve() -> Result<Paths>` (`ULTRON_HOME` varsa onu, yoksa `%LOCALAPPDATA%\ultron`), `Paths::ensure() -> Result<()>` (dizinleri oluşturur).
  - `struct Config { engines: Vec<EngineConfig>, default_engine: EngineId, context_budget_chars: usize }`, `Config::load(paths: &Paths) -> Result<Config>` (`root\config.toml`; yoksa varsayılan).
  - `enum EngineId { Claude, Codex, Agy }` (serde `lowercase`), `struct EngineConfig { id: EngineId, enabled: bool, binary: String, model: Option<String>, effort: Option<String>, extra_args: Vec<String> }`.
  - `EngineConfig::validate(&self) -> Result<()>`.
- Varsayılanlar: claude `model=None`; codex `model="gpt-6.1-sol"`, `effort="medium"`; agy `model="gemini-3.8-flash-low"`; `context_budget_chars = 24000`.

- [ ] **Step 1: Failing tests**
  - `paths_respect_ultron_home`: `ULTRON_HOME=tmp` → `paths.db == tmp\state\ultron.db`.
  - `config_defaults_when_missing`: dosya yok → codex model `gpt-6.1-sol`, agy model `gemini-3.8-flash-low`, bütçe `24000`.
  - `validate_rejects_astra`: codex model `"gpt-6.1-astra"` → `Err`.
  - `validate_rejects_effort_above_high_for_sol`: `effort="max"` + sol → `Err`; luna + `effort="medium"` → `Err` (Luna yalnız `max`).
  - `validate_rejects_non_gemini_agy`: agy model `"claude-sonnet"` → `Err`.
- [ ] **Step 2:** Run `cargo test -p ultron-core --test paths_config` → FAIL.
- [ ] **Step 3:** Implement. Kurallar: model adında `astra` (büyük/küçük harf duyarsız) → hata; `gpt-6.1-sol|terra` için efor ∈ {low, medium, high}; `gpt-6-luna` için efor == `max`; agy modeli `gemini-` ile başlamalı.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): yollar ve yapılandırma`.

### Task 2: SQLite şeması ve depo

**Files:**
- Create: `crates/ultron-core/src/db/mod.rs`, `crates/ultron-core/src/db/migrations.rs`
- Test: `crates/ultron-core/tests/db.rs`

**Interfaces:**
- Produces:
  - `struct Db` (`rusqlite::Connection` + `Mutex`), `Db::open(path: &Path) -> Result<Db>` (WAL, `foreign_keys=ON`, migrasyonları uygular).
  - Tablolar: `projects(id TEXT PK, name, path, aliases_json)`, `chats(id TEXT PK, project_id NULL, title, created_at)`, `engine_sessions(id TEXT PK, chat_id, engine, native_session_id NULL, status, started_at, ended_at NULL)`, `events(id TEXT PK, chat_id, session_id NULL, seq INTEGER, ts, kind TEXT, payload_json, UNIQUE(chat_id, seq))`, `messages_fts` (FTS5: `chat_id UNINDEXED, event_id UNINDEXED, text`), `work_state(chat_id PK, json, updated_at)`.
  - `Db::insert_event(&self, ev: &StoredEvent) -> Result<bool>` (`INSERT OR IGNORE`; yeni satırsa `true`).
  - `Db::events_for_chat(&self, chat_id: &str, after_seq: Option<i64>) -> Result<Vec<StoredEvent>>`.
  - `Db::next_seq(&self, chat_id: &str) -> Result<i64>`.
  - `Db::upsert_session(&self, s: &SessionRow)`, `Db::open_sessions(&self) -> Result<Vec<SessionRow>>`.
  - `Db::put_work_state(&self, chat_id, json: &serde_json::Value)`, `Db::get_work_state(&self, chat_id) -> Result<Option<Value>>`.
  - `Db::search_messages(&self, chat_id: Option<&str>, query: &str, limit: usize) -> Result<Vec<SearchHit>>`.
  - `struct StoredEvent { id: String, chat_id: String, session_id: Option<String>, seq: i64, ts: DateTime<Utc>, kind: String, payload: Value }`.

- [ ] **Step 1: Failing tests**
  - `open_sets_wal`: `PRAGMA journal_mode` → `"wal"`.
  - `event_insert_is_idempotent`: aynı `id` ile iki kez ekle → ilki `true`, ikincisi `false`, `events_for_chat` uzunluğu 1.
  - `events_survive_reopen`: ekle, `Db`'yi düşür, yeniden aç → olay mevcut.
  - `fts_finds_turkish_text`: `"Ödeme ekranında buton kayıyor"` metnini indeksle; `search_messages(None, "ödeme", 5)` → 1 sonuç.
- [ ] **Step 2:** Run `cargo test -p ultron-core --test db` → FAIL.
- [ ] **Step 3:** Implement. FTS5 tokenizer: `unicode61 remove_diacritics 2` (Türkçe büyük/küçük harf ve şapka toleransı).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): sqlite şeması ve olay deposu`.

### Task 3: Olay modeli ve olay günlüğü

**Files:**
- Create: `crates/ultron-core/src/events.rs`, `crates/ultron-core/src/eventlog.rs`
- Test: `crates/ultron-core/tests/eventlog.rs`

**Interfaces:**
- Consumes: `Db`, `Paths` (Task 1–2).
- Produces:
  - `enum EngineEvent` (serde `tag="type"`, `snake_case`): `SessionStarted { native_session_id: Option<String> }`, `AssistantText { text: String }`, `UserText { text: String }`, `ToolCall { tool: String, input: Value }`, `ToolResult { tool: String, output_excerpt: String, is_error: bool }`, `Command { command: String, exit_code: Option<i32>, output_excerpt: String }`, `FileChanged { path: String, change: String }`, `Usage { input_tokens: Option<u64>, output_tokens: Option<u64>, cached_tokens: Option<u64> }`, `QuotaExhausted { message: String, resets_at: Option<DateTime<Utc>> }`, `TurnCompleted`, `Error { message: String }`, `Unknown { raw: Value }`, `EngineSwitched { from: EngineId, to: EngineId, reason: SwitchReason }`, `ExternalChanges { summary: String }`.
  - `enum SwitchReason { User, Quota, Error }`.
  - `EngineEvent::searchable_text(&self) -> Option<&str>` (yalnız `AssistantText`, `UserText`).
  - `struct EventLog`, `EventLog::new(db: Arc<Db>, paths: Paths) -> EventLog`, `EventLog::append(&self, chat_id: &str, session_id: Option<&str>, ev: EngineEvent) -> Result<StoredEvent>` (seq atar, DB'ye ve `transcripts\<chat_id>.jsonl`'ye yazar, FTS'i besler, yayınlar), `EventLog::subscribe(&self) -> broadcast::Receiver<StoredEvent>`.
  - `excerpt(s: &str, max_chars: usize) -> String`: baş 60% + `"\n…[N karakter kırpıldı]…\n"` + son 40%. Varsayılan `max_chars = 4000`.

- [ ] **Step 1: Failing tests**
  - `append_assigns_monotonic_seq`: üç ekleme → seq 1, 2, 3.
  - `append_writes_transcript_line`: JSONL dosyasında 1 satır, `type == "assistant_text"`.
  - `subscribers_receive_events`: abone ol, ekle → aynı `id` alınır.
  - `excerpt_marks_truncation`: 10.000 karakter → çıktı ≤ 4100 karakter ve `"kırpıldı"` içerir; 100 karakter → aynen döner.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): olay modeli ve olay günlüğü`.

### Task 4: Motor arayüzü, süreç çalıştırıcı ve sahte motor

**Files:**
- Create: `crates/ultron-core/src/engine/mod.rs`, `crates/ultron-core/src/engine/runner.rs`, `crates/ultron-core/src/engine/fake.rs`
- Test: `crates/ultron-core/tests/runner.rs`, `crates/ultron-core/tests/fixtures/fake/` (sentetik JSONL)

**Interfaces:**
- Consumes: `EngineEvent`, `EngineConfig`, `EngineId`.
- Produces:
  - `struct TurnRequest { chat_id: String, cwd: PathBuf, prompt: String, resume_native_id: Option<String>, tool_policy: ToolPolicy }`.
  - `enum ToolPolicy { ReadOnly, Workspace, Full }`.
  - `trait Engine: Send + Sync { fn id(&self) -> EngineId; fn capabilities(&self) -> Capabilities; fn command(&self, req: &TurnRequest) -> CommandSpec; fn parse_line(&self, line: &str) -> Vec<EngineEvent>; }`.
  - `struct Capabilities { streaming: bool, resume: bool, mid_turn_input: bool, tool_restriction: RestrictionLevel }`, `enum RestrictionLevel { Unknown, Partial, Full }`.
  - `struct CommandSpec { program: String, args: Vec<String>, cwd: PathBuf, stdin: Option<String> }`.
  - `async fn run_turn(engine: &dyn Engine, req: TurnRequest, sink: mpsc::Sender<EngineEvent>, cancel: CancellationToken) -> Result<TurnOutcome>`.
  - `enum TurnOutcome { Completed, Cancelled, Failed { exit_code: Option<i32>, stderr_excerpt: String }, QuotaExhausted }`.
  - `FakeEngine::from_fixture(path: &Path) -> FakeEngine` (`command` yerine fixture satırlarını okuyan bir yardımcı ikili: `cargo` test ikilisi olarak `tests/bin/fake_cli.rs`).
- Kurallar: stdout satır satır okunur; boş satır atlanır; süreç bitince ya da iptal edilince yarım satır atılır; stderr en fazla 64 KB tutulur ve `excerpt` ile kırpılır. Windows'ta iptal `TerminateProcess` ile süreç ağacını sonlandırır (`kill_on_drop` + iş nesnesi, Job Object).

- [ ] **Step 1: Failing tests**
  - `runner_streams_fixture_events`: fake fixture 3 olay → sink'e 3 olay, `Completed`.
  - `runner_child_killed_midstream_keeps_events`: fake CLI 2 satır yazıp yarım satırla `exit(137)` → sink'e 2 olay, `Failed { exit_code: Some(137), .. }`.
  - `runner_cancel_terminates_tree`: uzun süren fake CLI; iptal → `Cancelled`, 2 sn içinde süreç yok.
  - `runner_passes_unicode_and_spaced_cwd`: cwd `tmp\"roblox 2"`, prompt `"Ödeme ekranı çalışsın mı?"` → fake CLI argüman ve cwd'yi yankılar, aynen eşleşir.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): motor arayüzü ve süreç çalıştırıcı`.

### Task 5: Claude Code sürücüsü

**Files:**
- Create: `crates/ultron-core/src/engine/claude.rs`
- Test: `crates/ultron-core/tests/claude_driver.rs`, `crates/ultron-core/tests/fixtures/claude/*.jsonl` (sentetik; gerçek bir akıştan alan adları korunarak elle yazılmış)

**Interfaces:**
- Consumes: Task 4 `Engine`, `CommandSpec`, `ToolPolicy`.
- Produces: `ClaudeEngine::new(cfg: EngineConfig) -> ClaudeEngine`.
- Komut: `claude -p --output-format stream-json --verbose` + (`resume_native_id` varsa `--resume <id>`) + (`model` varsa `--model <m>`) + `--strict-mcp-config` + `--mcp-config <ultron-run\mcp-empty.json>` + izin politikası:
  - `ReadOnly` → `--tools Read,Grep,Glob` + `--permission-mode default`
  - `Workspace` → `--disallowedTools Bash(rm:*),Bash(del:*),Bash(Remove-Item:*),Bash(git push:*)` + `--permission-mode acceptEdits`
  - `Full` → kısıt yok (yalnız kullanıcı onaylı görevler; Aşama 3'te güvenlik kapısına bağlanır).
  - Prompt stdin'den verilir (komut satırı uzunluğu ve kaçış sorunlarını önlemek için).
- `capabilities()` → `streaming: true, resume: true, mid_turn_input: true, tool_restriction: Partial`.
- Aşama 1'de her tur ayrı bir `-p` süreci + `--resume` ile çalışır. Tur ortasında mesaj iletmek için kalıcı süreç (`--input-format stream-json`) Aşama 2'de canlı ajan penceresiyle birlikte eklenir (spec §11.2).
- Kesin bayrak adları Task 5 başında `claude --help` ile yeniden doğrulanır; farklıysa bu adım güncellenir.

- [ ] **Step 1: Failing tests**
  - `claude_command_includes_stream_json_and_resume`: `resume_native_id = Some("abc")` → args `--output-format`, `stream-json`, `--resume`, `abc` içerir; `--strict-mcp-config` içerir.
  - `claude_readonly_policy_limits_tools`: `ReadOnly` → args `--tools` ve `Read,Grep,Glob` içerir.
  - `claude_parses_init_assistant_tool_result`: fixture → sırasıyla `SessionStarted{native_session_id: Some(..)}`, `AssistantText`, `ToolCall`, `ToolResult`, `Usage`, `TurnCompleted`.
  - `claude_unknown_event_type_is_preserved`: `{"type":"brand_new"}` → `Unknown`.
  - `claude_quota_error_maps_to_quota_exhausted`: kullanım sınırı mesajı içeren result satırı (fixture `quota.jsonl`) → `QuotaExhausted`.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5 (canlı, onaylı):** `live_claude_roundtrip` (`#[ignore]`): `"Sadece TAMAM yaz"` → `AssistantText` "TAMAM" içerir ve `SessionStarted` native id verir. Gerçek akış `fixtures-private\claude\` altına kaydedilir.
- [ ] **Step 6:** Commit `feat(core): claude code sürücüsü`.

### Task 6: Codex sürücüsü

**Files:**
- Create: `crates/ultron-core/src/engine/codex.rs`
- Test: `crates/ultron-core/tests/codex_driver.rs`, `crates/ultron-core/tests/fixtures/codex/*.jsonl`

**Interfaces:**
- Produces: `CodexEngine::new(cfg: EngineConfig) -> CodexEngine`.
- Komut: yeni tur `codex exec --json -C <cwd> -m <model> -c model_reasoning_effort=<effort> --skip-git-repo-check -s <sandbox> -` (prompt stdin'den); devam `codex exec resume <id> --json ... -`.
  - `ReadOnly` → `-s read-only`, `Workspace` → `-s workspace-write`, `Full` → `-s workspace-write` (Aşama 1'de `danger-full-access` kullanılmaz).
- `capabilities()` → `streaming: true, resume: true, mid_turn_input: false, tool_restriction: Partial`.

- [ ] **Step 1: Failing tests**
  - `codex_command_new_and_resume`: yeni → `exec`, `--json`, `-m`, `gpt-6.1-sol`; resume → `exec`, `resume`, `<id>`.
  - `codex_full_policy_never_uses_danger`: `Full` → args `danger-full-access` içermez.
  - `codex_parses_session_message_command_events`: fixture → `SessionStarted`, `AssistantText`, `Command{exit_code: Some(0)}`, `FileChanged`, `Usage`, `TurnCompleted`.
  - `codex_unknown_event_type_is_preserved`, `codex_quota_error_maps_to_quota_exhausted`.
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5 (canlı, onaylı):** `live_codex_roundtrip` (`#[ignore]`), model `gpt-6-luna`, efor `max`. Codex kotası ≥%90 ise çalıştırılmaz.
- [ ] **Step 6:** Commit `feat(core): codex sürücüsü`.

### Task 7: agy sürücüsü

**Files:**
- Create: `crates/ultron-core/src/engine/agy.rs`
- Test: `crates/ultron-core/tests/agy_driver.rs`, `crates/ultron-core/tests/fixtures/agy/*.jsonl`

**Interfaces:**
- Produces: `AgyEngine::new(cfg: EngineConfig) -> AgyEngine`.
- Komut: `agy -p --output-format stream-json --model <model>` + (devam: `--conversation <id>`) + `ReadOnly` → `--mode plan`, `Workspace` → `--mode accept-edits --sandbox`, `Full` → `--mode accept-edits`. `--dangerously-skip-permissions` asla kullanılmaz. Prompt stdin'den.
- Referans davranış: `C:/Users/Alperen/.claude/lanes/lane.py` agy kolu (hata sınıflandırması, model kuralı). Kod kopyalanmaz.
- `capabilities()` → `streaming: true` (canlı testle doğrulanana kadar `RestrictionLevel::Unknown`), `resume: true`, `mid_turn_input: false`.

- [ ] **Step 1: Failing tests:** `agy_command_uses_stream_json_and_flash`, `agy_never_skips_permissions`, `agy_parses_fixture_events`, `agy_unknown_event_type_is_preserved`, `agy_quota_error_maps_to_quota_exhausted`, `agy_success_without_output_is_error` (lane.py'de belgelenen "SUCCESS döndü ama yazmadı" durumu: çıkış kodu 0 + hiç `AssistantText` yok → `Error`).
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5 (canlı, onaylı):** `live_agy_roundtrip` (`#[ignore]`). Sonuç `capabilities` değerlerine ve rapora işlenir.
- [ ] **Step 6:** Commit `feat(core): agy sürücüsü`.

### Task 8: Çalışma durumu

**Files:**
- Create: `crates/ultron-core/src/workstate.rs`, `crates/ultron-core/src/summarizer.rs`
- Test: `crates/ultron-core/tests/workstate.rs`

**Interfaces:**
- Consumes: `StoredEvent`, `Db::put_work_state/get_work_state`, Task 7 `AgyEngine` (özetleyici olarak).
- Produces:
  - `struct WorkState { goal: Option<String>, plan: Vec<String>, decisions: Vec<Decision>, files_touched: BTreeSet<String>, done: Vec<String>, next_step: Option<String>, open_questions: Vec<String>, engine_history: Vec<EngineSpan>, approvals_pending: Vec<String> }` (serde).
  - `struct Decision { what: String, why: Option<String>, ts: DateTime<Utc>, source_event: String }`, `struct EngineSpan { engine: EngineId, from_seq: i64, to_seq: Option<i64> }`.
  - `WorkState::apply_rules(&mut self, ev: &StoredEvent)`: `FileChanged` → `files_touched`; `EngineSwitched` → `engine_history`; `UserText` içinde satır başı `KARAR:` / `DECISION:` → `decisions`; `UserText` ilk mesaj → `goal` (boşsa).
  - `trait Summarizer { async fn refresh(&self, state: &WorkState, recent: &[StoredEvent]) -> Result<SummaryPatch>; }` → `struct SummaryPatch { goal, next_step, open_questions, done }` (hepsi `Option`).
  - `AgySummarizer` (agy + `--json-schema` ile yapılandırılmış çıktı), `NullSummarizer` (boş yama).
  - `WorkStateService::on_turn_completed(&self, chat_id: &str) -> Result<WorkState>`: kuralları uygular, özetleyiciyi çağırır, özetleyici hata verirse yalnızca kural tabanlı durumu kaydeder ve `Error` olayı yazmaz (sessizce düşmez: `tracing::warn` + `work_state.json` içinde `summary_stale: true`).

- [ ] **Step 1: Failing tests:** `rules_collect_files_and_engine_history`, `rules_capture_decision_markers` (`"KARAR: CSV kullanacağız"` → `decisions[0].what == "CSV kullanacağız"`), `summarizer_failure_keeps_rule_state_and_marks_stale` (hata döndüren sahte özetleyici), `state_persists_roundtrip`.
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5:** Commit `feat(core): çalışma durumu`.

### Task 9: Devir paketi ve geçmiş araması

**Files:**
- Create: `crates/ultron-core/src/handoff.rs`
- Test: `crates/ultron-core/tests/handoff.rs`

**Interfaces:**
- Consumes: `WorkState`, `Db::events_for_chat`, `Db::search_messages`.
- Produces:
  - `struct HandoffPack { text: String, truncated: bool, sections: Vec<(String, usize)> }`.
  - `build_handoff(db: &Db, chat_id: &str, budget_chars: usize, recent_verbatim: usize, external: Option<&str>) -> Result<HandoffPack>`.
  - Bölüm sırası (sabitten değişkene, önbellek dostu): `[ULTRON DEVİR]` başlığı → proje talimatları (cwd'de `AGENTS.md`/`CLAUDE.md` varsa yol olarak anılır, içerik basılmaz) → çalışma durumu (JSON değil, okunur madde listesi) → `[SON OTURUMDAN BERİ DEĞİŞENLER]` (varsa) → eski mesajların özeti (`AssistantText`/`UserText` için ilk 200 karakter) → son `recent_verbatim` mesaj (varsayılan 6) aynen → `[GEÇMİŞ]` notu: "Ayrıntı gerekirse `ultron history search "<sorgu>"` çalıştır."
  - Bütçe aşılırsa önce eski özet, sonra eski verbatim mesajlar kırpılır; çalışma durumu ve son 2 mesaj asla kırpılmaz. `truncated: true` ve pakette `[KIRPILDI: N karakter]` satırı.
- Not: Aşama 1'de motorlara MCP aracı verilmez; geçmiş araması CLI komutu olarak sunulur (Task 12). MCP araç sağlayıcı Aşama 3'te.

- [ ] **Step 1: Failing tests:** `pack_order_is_stable_first`, `pack_respects_budget_and_marks_truncation` (bütçe 2000 → `text.len() <= 2000`, `truncated`), `pack_never_drops_work_state_or_last_two_messages`, `pack_includes_external_changes_section`.
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5:** Commit `feat(core): devir paketi`.

### Task 10: Oturum yöneticisi ve motor değişimi

**Files:**
- Create: `crates/ultron-core/src/session.rs`, `crates/ultron-core/src/external.rs`
- Test: `crates/ultron-core/tests/session.rs`

**Interfaces:**
- Consumes: Task 3–9.
- Produces:
  - `struct SessionManager`, `SessionManager::new(db: Arc<Db>, log: Arc<EventLog>, cfg: Config, engines: HashMap<EngineId, Arc<dyn Engine>>) -> SessionManager`.
  - `async fn send(&self, chat_id: &str, cwd: &Path, engine: EngineId, text: &str, policy: ToolPolicy) -> Result<TurnOutcome>`: aynı motorda açık oturum varsa native resume; yoksa devir paketi + mesaj ile yeni tur. Her olay `EventLog`'a, tur sonunda `WorkStateService::on_turn_completed`.
  - `async fn switch(&self, chat_id: &str, to: EngineId, reason: SwitchReason) -> Result<()>`: `EngineSwitched` olayı yazar, eski oturumu `Closed` yapar; bir sonraki `send` yeni motorda devir paketiyle başlar.
  - `async fn cancel(&self, chat_id: &str) -> Result<()>`.
  - `TurnOutcome::QuotaExhausted` → oturum `WaitingQuota` durumuna geçer ve `QuotaExhausted` olayı yazılır (otomatik devam Aşama 4).
  - `external::changes_since(cwd: &Path, since: DateTime<Utc>, last_head: Option<&str>) -> Result<Option<String>>`: git deposuysa `git status --porcelain` + `git diff --stat <last_head>..HEAD`; değilse `mtime > since` olan dosyalar (en fazla 50, `target/`, `node_modules/`, `.git/` hariç). Ultron'un kendi tur içinde değiştirdiği dosyalar (`files_touched`) listeden çıkarılır.

- [ ] **Step 1: Failing tests (FakeEngine ile):**
  - `send_resumes_native_session_on_same_engine`: iki `send` → ikinci `CommandSpec` resume id içerir.
  - `switch_emits_event_and_next_send_uses_handoff`: `switch(Codex)` → `EngineSwitched{from: Claude, to: Codex}` olayı; sonraki codex isteğinin stdin'i `[ULTRON DEVİR]` ile başlar.
  - `quota_outcome_sets_waiting_quota`.
  - `external_changes_detects_user_edit`: geçici git deposunda dosya değiştir → özet dosya adını içerir; ultron'un dokunduğu dosya listede yok.
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5:** Commit `feat(core): oturum yöneticisi ve motor değişimi`.

### Task 11: IPC protokolü ve servis

**Files:**
- Create: `crates/ultron-core/src/ipc.rs`, `crates/ultrond/src/main.rs`, `crates/ultrond/src/server.rs`, `crates/ultrond/src/keepawake.rs`
- Test: `crates/ultrond/tests/ipc.rs`

**Interfaces:**
- Produces:
  - Pipe adı: `\\.\pipe\ultron-<kullanıcı-SID>`; ACL yalnızca mevcut kullanıcı. Ek olarak her istek `run\token` dosyasındaki (kullanıcı-özel ACL) rastgele 32 bayt hex token'ı taşır.
  - Satır başına bir JSON. `enum Request { Hello{token}, Send{chat_id: Option<String>, cwd, engine: Option<EngineId>, text, policy}, Switch{chat_id, to}, Cancel{chat_id}, Events{chat_id: Option<String>, follow: bool, after_seq: Option<i64>}, HistorySearch{chat_id: Option<String>, query, limit}, Status }`. `enum Response { Ok{data: Value}, Event(StoredEvent), Error{message} }`.
  - `chat_id` yoksa yeni sohbet oluşturulur ve `Ok{data: {"chat_id": ..}}` döner.
  - `ultrond` başlarken: `Paths::ensure`, `Db::open`, açık oturumları yükler (`Running` olanlar `Interrupted` yapılır ve `Error{message:"servis yeniden başladı"}` olayı yazılır), pipe'ı dinler. Tek örnek kilidi: `run\ultrond.lock`.
  - Keep-awake: herhangi bir tur çalışırken `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`, son tur bitince geri alınır.

- [ ] **Step 1: Failing tests:** `rejects_wrong_token`, `send_returns_chat_id_and_streams_events` (FakeEngine), `events_follow_receives_live_events`, `second_instance_refuses_to_start`.
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5:** Commit `feat(ultrond): ipc ve servis`.

### Task 12: `ultron` CLI istemcisi ve otomatik başlatma

**Files:**
- Create: `crates/ultron/src/main.rs`, `crates/ultron/src/commands.rs`
- Test: `crates/ultron/tests/cli.rs`, `crates/ultrond/tests/restart.rs`

**Interfaces:**
- Consumes: Task 11 protokolü.
- Produces (clap):
  - `ultron chat [--chat <id>] [--engine claude|codex|agy] [--cwd <dir>] [--policy readonly|workspace|full] "<mesaj>"` → olayları akıtarak yazdırır (`AssistantText` düz metin, `Command`/`ToolCall` tek satır özet, `EngineSwitched` belirgin uyarı satırı).
  - `ultron switch --chat <id> --to <engine>`
  - `ultron events --chat <id> [--follow]`
  - `ultron history search "<sorgu>" [--chat <id>] [--limit 5]`
  - `ultron status`
  - `ultron service install|uninstall` → Görev Zamanlayıcı'da oturum açılışında `ultrond`'u başlatan görev (`schtasks /Create /SC ONLOGON /RL LIMITED /TN Ultron\ultrond ...`). Çalıştırılması kullanıcı onayı ister (sistem ayarı).
  - İstemci servis çalışmıyorsa `ultrond`'u ayrık süreç olarak başlatır ve 5 sn bekler.

- [ ] **Step 1: Failing tests:** `chat_prints_assistant_text` (FakeEngine'li servis), `switch_prints_engine_switched_warning`, `history_search_finds_previous_message`, `daemon_restart_restores_sessions` (servisi öldür, yeniden başlat → `ultron events` olayları tekrarsız gösterir, açık oturum `Interrupted`).
- [ ] **Step 2–4:** FAIL → Implement → PASS.
- [ ] **Step 5:** Commit `feat(ultron): cli istemci ve otomatik başlatma`.

### Task 13: Kabul senaryosu ve Aşama 1 ölçüm raporu

**Files:**
- Create: `crates/ultrond/tests/live_acceptance.rs` (`#[ignore]`), `docs/reports/2026-xx-xx-asama1-olcum.md`

**Interfaces:**
- Consumes: hepsi.

- [ ] **Step 1: `live_claude_to_codex_handoff`** (onaylı, kota harcar): geçici bir proje klasöründe Claude'a: `"Bu projede rapor çıktısı için format seçiyoruz. KARAR: CSV kullanacağız çünkü Excel'de açılacak. Sıradaki adım: export fonksiyonunu yazmak. Sadece ANLADIM yaz."` → `switch(Codex)` → Codex'e (Luna, max, read-only): `"Hangi formatta karar verdik, neden, sıradaki adım ne? Tek satır."` Beklenen: cevap `CSV`, `Excel` ve `export` içerir; olay günlüğünde `EngineSwitched` var.
- [ ] **Step 2:** Aynı senaryoyu Claude→agy için tekrarla (`live_claude_to_agy_handoff`).
- [ ] **Step 3: Token ölçümü:** Aynı devir için (a) tüm döküm aynen, (b) devir paketi; her iki durumun karakter sayısı ve motorların bildirdiği `Usage` değerleri rapora yazılır.
- [ ] **Step 4: Araç kısıtlama ölçümü:** Her motor için `ReadOnly` politikasında `"geçici klasördeki test.txt dosyasını sil"` istenir (sentetik dosya). Dosyanın silinip silinmediği ve motorun ne yaptığı rapora `RestrictionLevel` önerisiyle yazılır. Silinirse motorun `capabilities.tool_restriction` değeri `Partial`/`Unknown` olarak bırakılır ve Aşama 3 kısıtı olarak raporlanır (Aşama 1'i bloklamaz, spec §11.3).
- [ ] **Step 5: agy canlı akış raporu:** stream-json'un tur içinde artımlı mı yoksa tur sonunda toplu mu geldiği.
- [ ] **Step 6:** `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` → temiz. Commit `test: aşama 1 kabul senaryosu ve ölçüm raporu`.

---

## Kapsam dışı (sonraki aşamalar)
Masaüstü arayüzü, kota paneli ve OAuth kota okuma (Aşama 2); MCP araç sağlayıcı, güvenlik kapısı, çöp kutusu, yerel onay modeli (Aşama 3); Telegram, otomatik devam (Aşama 4); hafıza kasası ve embedding (Aşama 5); yerel ve API motorları, orkestrasyon modları (Aşama 6); temsilci (Aşama 7).
