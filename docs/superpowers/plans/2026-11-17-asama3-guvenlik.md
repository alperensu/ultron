# Ultron Aşama 3 — Güvenlik ve Onay: Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Motorların yaptığı riskli işlemleri (silme, deploy, git geçmişi, finans, gizli bilgi, kurulum) Ultron'un kendi sabit kurallarıyla durduran; gerekirse kullanıcıya soran (masaüstü, sonra Telegram), silmeleri geri alınabilir yapan bir güvenlik katmanı.

**Architecture:** Kural motoru `ultron-core::policy` saf fonksiyondur: bir eylem tanımı alır, `Allow | Ask | Deny` + gerekçe döndürür. Claude'da zorlama, Claude Code'un `PreToolUse` hook'uyla yapılır: hook `ultron policy check` komutunu çağırır, o da servise sorar; `Ask` kararında servis onay kuyruğuna kayıt açar ve kullanıcı cevap verene kadar bekler. Codex ve agy'de zorlama sandbox/mod ile yapılır, ek olarak olay akışı sonradan denetlenir. Silmeler `ultron trash` ile çöp kutusuna taşınır.

**Tech Stack:** Mevcut Rust workspace, Tauri/React arayüzü. Yeni harici bağımlılık yok (yerel onay modeli Task 7'de, isteğe bağlı).

**Spec:** `docs/superpowers/specs/2026-11-15-harness-design.md` §6.8, §11; Aşama 2 raporu `docs/reports/2026-11-17-asama2-kabul.md`.

## Global Constraints

- Sabit kurallar Rust servisinde/çekirdekte durur; motorların değiştirebileceği bir dosyada değil. Kullanıcı ayarları `ULTRON_HOME\policy.toml` içinde (yalnızca arayüzden ve elle), motorların cwd'sinin dışında.
- Varsayılan güvenli: kural motoru tanımadığı riskli bir şeyde `Ask` döner; hook ya da servis ulaşılamazsa Claude'da işlem **reddedilir** (fail-closed), kullanıcıya bildirilir.
- İzin atlama bayrakları asla kullanılmaz. Gizli bilgi loglanmaz; onay kayıtlarında komut metni gizli bilgi maskelemesinden (`import::secrets::mask_secrets`) geçirilerek saklanır.
- Codex/agy için onay ortası bekleme yok (CLI'ları desteklemiyor): sandbox `read-only`/`workspace-write`, agy `--mode plan|accept-edits`; `Full` politika yalnızca kullanıcı onaylı görevlerde.
- Repoda gerçek veri yok; testler sentetik.

## Review Focus

1. **Komut gizleme:** `cmd /c "del x"`, `powershell -c Remove-Item`, `bash -c 'rm -rf'`, `git -C repo push -f`, büyük/küçük harf ve tırnak varyasyonları kuraldan kaçmamalı. Test: Task 1 `obfuscated_destructive_commands_are_caught`.
2. **Servis kapalı/hook zaman aşımı:** Claude işlemi reddedilir, tur `Error` olayı alır, kullanıcı bilgilendirilir. Test: Task 3 `hook_fails_closed_when_daemon_unreachable`.
3. **Onay yarışı:** Aynı onaya iki cevap (masaüstü + ileride Telegram) gelirse ilk cevap geçerli, ikincisi reddedilir. Test: Task 2 `approval_resolves_once`.
4. **Çöpten geri alma çakışması:** Geri alınan yolda dosya varsa üzerine yazılmaz, yeni ad önerilir. Test: Task 4 `restore_never_overwrites`.
5. **Onay beklerken servis yeniden başlar:** Bekleyen onay kalıcıdır; hook zaman aşımına düşer ve işlem reddedilir, kayıt `expired` olur. Test: Task 2 `pending_approval_expires_on_restart`.

---

### Task 1: Kural motoru

**Files:** Create `crates/ultron-core/src/policy/{mod.rs,command.rs,rules.rs}`, `crates/ultron-core/tests/policy.rs`.

**Interfaces — Produces:**
- `enum Action { Shell { command: String, cwd: PathBuf }, FileWrite { path: PathBuf }, FileDelete { path: PathBuf }, Network { url: String }, ToolCall { tool: String, input: Value } }`
- `enum Decision { Allow, Ask { reason: String, category: RiskCategory }, Deny { reason: String } }`
- `enum RiskCategory { Delete, GitHistory, Deploy, Finance, Secrets, Install, SystemConfig, Publish }`
- `struct PolicyConfig { finance_paths: Vec<PathBuf>, deploy_patterns: Vec<String>, protected_paths: Vec<PathBuf>, allow_list: Vec<String> }` (`policy.toml`'dan; varsayılan boş listeler + yerleşik desenler)
- `fn evaluate(action: &Action, cfg: &PolicyConfig, ctx: &PolicyContext) -> Decision` (`PolicyContext { project_root: Option<PathBuf>, workspace_root: PathBuf }`)
- `fn normalize_shell(command: &str) -> Vec<Vec<String>>`: `cmd /c`, `powershell -c/-Command`, `pwsh`, `bash -c`, `sh -c`, `&&`, `;`, `|` zincirlerini açar, tırnakları çözer, küçük harfe indirir.
- Yerleşik kurallar (Ask): `rm`, `del`, `erase`, `rmdir`, `rd`, `Remove-Item`, `ri`, `Clear-Content`, `format`; `git push --force|-f|--force-with-lease`, `git reset --hard`, `git rebase`, `git filter-repo|filter-branch`, `git clean -f`, `git branch -D`; deploy: `vercel`, `wrangler deploy|publish`, `netlify deploy`, `ssh`, `scp`, `rsync`, `systemctl`, `kubectl apply|delete`, `docker push`; kurulum: `winget install|uninstall`, `npm i -g`, `pip install` (proje venv dışı), `cargo install`, `choco`; sistem: `reg add|delete`, `schtasks /create|/delete`, `netsh`, `Set-ExecutionPolicy`; yayın: `gh repo edit --visibility`, `gh release create`, `npm publish`, `cargo publish`; gizli: `.env`, `*.key`, `id_rsa*`, `credentials*`, `.credentials.json` okuma/yazma.
- Deny (kullanıcı ayarıyla bile açılmaz): izin atlama bayrakları (`--dangerously-skip-permissions`, `bypassPermissions`, `danger-full-access`), `format` disk komutları, `diskpart`, `cipher /w`, `vssadmin delete`.
- `FileDelete` → her zaman Ask (Task 4'teki `ultron trash` hariç: o Allow). `FileWrite` → `protected_paths` ya da finans yolu altındaysa Ask, aksi Allow.

- [ ] Testler: `destructive_commands_ask` (tablo), `obfuscated_destructive_commands_are_caught`, `bypass_flags_are_denied_even_if_allowlisted`, `finance_paths_require_approval`, `secret_files_require_approval`, `safe_commands_allow` (`git status`, `cargo test`, `npm run lint`, `ls`, `Get-ChildItem`), `ultron_trash_is_allowed` → uygula → commit `feat(core): güvenlik kural motoru`.

### Task 2: Onay kuyruğu (servis)

**Files:** Modify `crates/ultron-core/src/db/{mod.rs,migrations.rs}`, `crates/ultron-core/src/events.rs`, `crates/ultron-core/src/ipc.rs`, `crates/ultrond/src/server.rs`; Create `crates/ultrond/src/approvals.rs`; Test `crates/ultrond/tests/approvals.rs`.

**Interfaces — Produces:**
- Tablo `approvals(id, chat_id NULL, engine, category, reason, action_json(maskeli), status: pending|approved|denied|expired, created_at, resolved_at, resolved_by: desktop|telegram|timeout|restart)`.
- Olaylar: `ApprovalRequested { approval_id, category, reason, summary }`, `ApprovalResolved { approval_id, status, by }` (sohbete ve küresel akışa).
- IPC: `PolicyCheck { action: Action, chat_id: Option<String>, engine: EngineId, wait_secs: u32 }` → `Ok{decision}`; `Ask` ise kayıt açılır ve en fazla `wait_secs` (varsayılan 300) cevap beklenir: onay → `Allow`, red/zaman aşımı → `Deny`. `ListApprovals { status }`, `ResolveApproval { id, approve: bool, by }`.
- İlk cevap geçerli; sonraki cevaplar `Error{"onay zaten çözüldü"}`. Servis açılışında `pending` kayıtlar `expired/restart` olur.

- [ ] Testler: `ask_creates_pending_and_approve_allows`, `deny_or_timeout_denies`, `approval_resolves_once`, `pending_approval_expires_on_restart`, `action_text_is_masked_in_db` → commit `feat(ultrond): onay kuyruğu`.

### Task 3: Claude PreToolUse hook entegrasyonu

**Files:** Modify `crates/ultron/src/commands.rs` (yeni alt komut `ultron policy check`), `crates/ultron-core/src/engine/claude.rs` (hook ayarı), Test `crates/ultron/tests/policy_hook.rs`, `crates/ultron-core/tests/claude_driver.rs`.

**Interfaces — Produces:**
- `ultron policy check`: stdin'den Claude Code hook JSON'unu okur (`tool_name`, `tool_input`, `session_id`, `cwd`), `Action`'a çevirir (`Bash`→Shell, `Write|Edit|MultiEdit|NotebookEdit`→FileWrite, diğer araçlar→ToolCall), `PolicyCheck` gönderir; çıktısı Claude Code'un PreToolUse karar biçimi (`permissionDecision: allow|deny` + `permissionDecisionReason`). Servise ulaşılamazsa ya da hata olursa **deny** (çıkış kodu ve JSON biçimi Claude Code belgesine göre; sürüm 2.1.325 ile canlı doğrulanır).
- ClaudeEngine her turda `--settings <ULTRON_HOME\run\claude-settings-<chat>.json>` geçirir; içinde `hooks.PreToolUse` → `ultron.exe policy check` (mutlak yol). Kullanıcının kendi `~/.claude` ayarlarına yazılmaz.
- Chat bağlamı: hook ortam değişkeni `ULTRON_CHAT_ID` (ClaudeEngine süreç ortamına ekler).

- [ ] Testler: `hook_maps_bash_to_shell_action`, `hook_fails_closed_when_daemon_unreachable`, `claude_command_includes_policy_settings`; canlı `#[ignore] live_claude_hook_blocks_delete` (geçici klasörde `test.txt` silme isteği → onay kaydı açılır, test reddeder, dosya yerinde) → commit `feat: claude için kural kancası`.

### Task 4: Çöp kutusu

**Files:** Create `crates/ultron-core/src/trash.rs`; Modify `crates/ultron/src/commands.rs` (`ultron trash <yol>...`, `ultron trash list`, `ultron trash restore <id> [--to <yol>]`), `crates/ultrond/src/server.rs` (`TrashList`, `TrashRestore`); Test `crates/ultron-core/tests/trash.rs`.

**Interfaces — Produces:**
- `ULTRON_HOME\trash\<id>\` + `meta.json { original_path, deleted_at, chat_id, size }`; aynı birim üzerinde taşıma (rename), farklı birimde kopyala+doğrula+sil.
- `restore` hedefte dosya varsa üzerine yazmaz, `<ad> (geri yüklendi N)` önerir. Saklama süresi `policy.toml` `trash_retention_days` (varsayılan 30); süresi geçenler servis açılışında temizlenir (gerçek silme yalnız çöp kutusu içinde).
- `prompts/core.md`'ye kural: "Dosya silmek için `ultron trash <yol>` kullan."

- [ ] Testler: `trash_moves_and_restores`, `restore_never_overwrites`, `cross_volume_trash_copies_then_deletes` (geçici iki dizin ile simüle), `retention_purges_only_inside_trash` → commit `feat: çöp kutusu`.

### Task 5: Codex ve agy için koruma ve sonradan denetim

**Files:** Modify `crates/ultron-core/src/session.rs`, `crates/ultron-core/src/engine/{codex.rs,agy.rs}`; Test `crates/ultron-core/tests/session.rs`.

**Davranış:**
- Codex: `ToolPolicy::Full` yalnızca kullanıcının onayladığı turda (yeni `Request::Send` alanı `elevated: bool` + onay kaydı); varsayılan `workspace-write`.
- Tur sonunda olay akışındaki `Command` olayları kural motorundan geçirilir; `Ask/Deny` sınıfına giren ve **gerçekleşmiş** bir komut varsa sohbete `PolicyViolation { command(maskeli), category }` olayı ve masaüstü bildirimi.
- agy: `--mode plan` varsayılan, `accept-edits` yalnız Workspace politikasında.

- [ ] Testler: `codex_full_policy_requires_approved_elevation`, `post_turn_audit_flags_executed_risky_command` → commit `feat(core): codex ve agy koruması`.

### Task 6: Arayüz — onaylar ve kurallar

**Files:** `apps/desktop/src/features/approvals/**`, `src/screens/Today.tsx` ("Seni bekleyen" bölümü artık gerçek), sohbet içinde satır içi onay kartı, `src/screens/Settings.tsx` (Kurallar bölümü: finans yolları, korumalı yollar, saklama süresi — `policy.toml`'u servis üzerinden okur/yazar; yeni IPC `GetPolicyConfig`/`SetPolicyConfig`), Tauri komutları + sözleşme testi.

**Davranış:** Onay kartı: kategori, gerekçe, maskeli komut özeti, sohbet bağlantısı, kalan süre; **Onayla** (mavi kapsül) / **Reddet** (kırmızı metin). Çözülünce kart kayarak kaybolur. Bekleyen onay sayısı kenar çubuğunda rozet. Çöp kutusu: Ayarlar'da liste ve **Geri yükle**.

- [ ] Testler: Vitest `approval_card_resolves_and_disappears`, `policy_settings_roundtrip`; Playwright `today_shows_pending_approvals`; Rust sözleşme testi yeni komutlar için → commit `feat(desktop): onaylar, kurallar ve çöp kutusu`.

### Task 7 (isteğe bağlı): Yerel onay modeli

Gri alan (kuralların `Allow` dediği ama alışılmadık görünen komutlar) için Ollama + küçük bir model (≤ 3B) ile "kullanıcının istediği işin parçası mı?" sınıflandırması. Önce sentetik 100 örneklik değerlendirme seti (`crates/ultron-core/tests/fixtures/policy_eval.jsonl`), ölçüm raporu; yalnızca isabet ≥ %90 ve ortalama < 300 ms ise varsayılan açılır, aksi kapalı kalır. Ollama kurulumu kullanıcı onayı ister.

### Task 8: Kabul ve rapor

- [ ] Canlı (onaylı): Claude'a geçici klasörde dosya sildirme isteği → masaüstünde onay kartı çıkar → Reddet → dosya yerinde, sohbette ret görünür; aynısı Onayla ile → dosya çöpte, `ultron trash restore` ile geri gelir.
- [ ] `ultrond` kapalıyken Claude'un silme denemesi reddedilir.
- [ ] Rapor `docs/reports/<tarih>-asama3-kabul.md`; tam test kapısı; commit `test: aşama 3 kabul`.

## Kapsam dışı
Telegram üzerinden onay (Aşama 4 bu kuyruğu kullanacak), temsilci yetki seviyeleri (Aşama 7), hafızada `private/remote_allowed` filtreleri (Aşama 5).
