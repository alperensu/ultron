# Aşama 4 Görev 12 düzeltmeleri

## Durum

**Completed** — İşçi kapsamındaki üç madde uygulandı ve yerel doğrulamalar geçti. İstekte `--out` değeri verilmediği için rapor `docs/reports/2026-11-18-asama4-gorev12-duzeltmeleri.md` yoluna yazıldı.

## Değişen dosyalar

- `crates/ultrond/src/telegram/notifications.rs` — Son yanıttaki kalın ve satır içi kod işaretleri, başlıklar ve maddeler düz metne çevrilir. Kod çitlerinin içeriği korunur. Kalıcı olaylardan okunan cevap, dönüşümden sonra 4096 UTF-16 birimine göre kesilir ve gerekiyorsa mevcut kısaltma notu eklenir.
- `crates/ultron-core/src/policy/mod.rs` ve `crates/ultron-core/src/policy/rules.rs` — Çözümlenemeyen komutta mevcut `Ask` ve `SystemConfig` kararı yeniden deneme ipucuyla birlikte döner.
- `crates/ultron-core/tests/policy.rs` — Kararın türü, kategorisi ve Türkçe ipucu sınanır.
- `crates/ultrond/src/approvals.rs` — Onay çıkmadığında yalnız bu gerekçe ret yanıtına taşınır. `crates/ultron/src/commands.rs:361` içindeki mevcut `hook_output`, gerekçeyi Claude’a `permissionDecisionReason` ve `systemMessage` olarak iletir. Test, süre dolduğunda ret gerekçesini ve eylemin Telegram’dan onaylanamadığını doğrular.
- `crates/ultrond/src/telegram/bridge.rs` — Başlangıç ve yeniden oynatma kuralları yorum ve sahte poller testiyle sabitlendi.

## C — Bekleyen Telegram güncellemeleri

- `bridge.rs:105` doğrulanan bot kimliğinin kalıcı `next_update_id` değeriyle yoklamaya başlar. Botun kaydı yoksa `None` kullanılır. Başka bir botun ofseti taşınmaz. `commands.rs:32` mesajın `date` alanını kabul eder ve yaş kontrolü yapmaz. Yetkili birikmiş mesaj Telegram tarafından döndürülürse ve kimliği mevcut ofsetin altına düşmüyorsa normal işleme girer.
- `crates/ultron-core/src/db/telegram.rs:219` ve `:248` kabul kaydını ve ofseti aynı işlemde yazar. `bridge.rs:268`, ofsetin altındaki güncellemeyi yeniden işlemeden geçer. Sunucu başlangıcındaki `server.rs:830` çağrısı, kota bekleme istisnası dışındaki yarım `accepted` veya `running` kayıtları `needs_attention` durumuna alır (`db/telegram.rs:438`). Bu kural, önceki süreç kabulden sonra ve yanıt ulaşmadan önce kapanmışsa yan etkilerin tekrar yürütülmesini önler. Kalıcı olarak gönderime hazır finallerin teslim yolu ayrıdır.
- `bridge.rs:577` testi, başka botun ofsetine rağmen ilk yoklamanın ofsetsiz yapıldığını, çok eski `date` taşıyan yeni güncellemelerin yanıt aldığını ve önceden kabul edilmiş yarım güncellemenin yeniden yanıt üretmediğini gösterir. Bu davranış korundu. 03.16 civarındaki mesajların hangi koşula düştüğü canlı güncelleme kimlikleri ve önceki süreç kayıtları olmadan doğrulanamadı.

## Kanıt

| Komut veya kontrol | Sonuç |
| --- | --- |
| `cargo test -j 4 -p ultron-core policy` | Passed |
| `cargo test -j 4 -p ultron-core --test policy` | Passed, 27 test |
| `cargo test -j 4 -p ultrond --lib telegram` | Passed, 87 test. Son kaynak değişikliğinden sonra `--quiet` ile tekrar geçti. |
| `cargo test -j 4 -p ultrond --lib policy_unparseable_command_retains_retry_hint_after_timeout` | Passed, 1 test |
| `cargo clippy -j 4 --workspace --all-targets -- -D warnings` | Passed |
| `git diff --check` | Passed |
| Değişen dosyaların UTF-8 kodlaması ve Türkçe ipucu | Passed |
| Canlı Telegram ve motor kabulü | Not run |
| Kod ve güvenlik incelemeleri | Not run — koordinatörün sorumluluğunda |

## Kalan işler

Koordinatörün bağımsız incelemeleri ve canlı kabulü bekliyor. 03.16 olayı ayrıca araştırılacaksa önceki süreçteki kabul durumunu ve bot ofsetini mesaj kimlikleriyle karşılaştırmak gerekir. Clippy sırasında Tauri’nin ürettiği masaüstü dosyalarının satır sonları başlangıç biçimine döndürüldü.
