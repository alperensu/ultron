# Ultron CLI (`crates/ultron`)

## Komutlar
- `chat`: Bir sohbete (yeni veya `--chat <id>`) tek bir mesaj gönderir ve ortaya çıkan olayları akıtır (stream eder); etkileşimli bir oturum değildir.
- `switch`: Aktif motoru değiştirir ve handoff gerçekleştirir.
- `events`: Olay günlüğünü (eventlog) listeler ve filtreler.
- `history search`: Oturum geçmişi içinde arama yapar.
- `import projects [--dry-run]`: Claude/Codex oturumlarından proje klasörlerini bulur, projeleri oluşturur, "Şimdiye kadar yapılanlar" notunu yazar (sohbet içe aktarmaz). `import remove-chats [--dry-run]`: eski içe aktarıcının sohbetlerini yedek alarak siler. `--dry-run` motor çağırmaz, yazmaz.
- `status`: Daemon ve sistem durumunu görüntüler.
- `service install`: ultrond arka plan servisini zamanlanmış görev olarak kurar.

## Kurallar
- İstemci, belirteci (token) göndermeden önce boru sunucusunun (pipe server) SID doğrulamasını yapar.
- Çalışan bir `ultrond` bulunamazsa, `ultron.exe` yanında bulunan `ultrond` otomatik başlatılır.
- `service install` yalnızca kullanıcı açıkça bu komutu çalıştırdığında kurulur; testler yalnızca üretilen schtasks argümanlarını doğrular.
