# Ultron

Ultron; Claude, Codex ve Antigravity motorlarını koordine eden yerel bir harness kütüphanesi ve CLI aracıdır.
Geliştirici oturumlarını, olay günlüğünü (eventlog), durum özetlerini ve motorlar arası handoff mekanizmasını yönetir.

## Talimat Haritası (Instruction Map)
- `crates/ultron-core/AGENTS.md`: Kütüphane çekirdeği, modül haritası, runner limitleri, olay ve motor değişmezleri.
- `crates/ultrond/AGENTS.md`: Arka plan servisi (daemon), adlandırılmış boru (named pipe) güvenliği, el sıkışma ve yaşam döngüsü.
- `crates/ultron/AGENTS.md`: CLI istemcisi, komutlar, servis doğrulama ve başlatma kuralları.
- `docs/AGENTS.md`: Dokümantasyon düzeni, tasarım özellikleri, planlar ve raporlama standartları.

## Genel Kurallar
- Derleme ve test: `cargo test --workspace` ve `cargo clippy --workspace --all-targets -- -D warnings`.
- Toolchain yerel rustup override ile `stable-x86_64-pc-windows-msvc` olarak ayarlanmıştır; toolchain dosyalarını değiştirmeyin.
- Canlı claude/codex/agy testlerini (`#[ignore] live_*`) kullanıcı onayı olmadan asla çalıştırmayın (kota harcar).
- Test fikstürleri sentetik olmalıdır; gerçek yakalamalar depo dışında `%LOCALAPPDATA%\ultron\fixtures-private` altında tutulur.
- Depoda hiçbir sır (secret), belirteç (token), kişisel veri veya mutlak kullanıcı yolu bulunamaz.
- İzin atlama bayraklarını (`--dangerously-skip-permissions`, `bypassPermissions`, `danger-full-access`) asla kullanmayın.
- Değişiklikleri cerrahi tutun; önce en basit çözümü uygulayın, varsayımları belirtin, başarıyı bir teste bağlayın.
- Commit mesajları Türkçe conventional commit stilinde olmalıdır: `feat/fix/test/docs(kapsam): açıklama`.
