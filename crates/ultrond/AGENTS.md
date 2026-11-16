# Ultron Daemon (`crates/ultrond`)

- Adlandırılmış boru (named pipe) adı kullanıcı SID'si baz alınarak ve sadece kullanıcıya izin veren DACL ile oluşturulur.
- İlk örnek (first-instance) güvencesi sağlanır; uzak istemciler reddedilir (reject remote clients).
- Her başlangıçta `run\token` içinde sadece kullanıcı izinli ACL ile rastgele güvenlik belirteci üretilir.
- İstemci iletişimi Hello-ilk (Hello-first) el sıkışması ve sabit zamanlı (constant-time) belirteç karşılaştırmasıyla başlar.
- İletişimde 1 MiB çerçeve limiti (frame limit) uygulanır.
- Tek örnek kilidi (single-instance lock) ile çakışmalar önlenir.
- Yeniden başlatmalarda tur ortasında kalan oturumlar kesintiye uğramış (`interrupted`) olarak işaretlenir.
- Uyanık tutma (keep-awake) özelliği yalnızca bir tur (turn) aktifken çalıştırılır.
- Projesiz `Send` (project_id yok) ve verilen cwd kullanıcının ev dizini ya da boşsa, motor `<ULTRON_HOME>\workspaces\<chat_id>` çalışma alanında (istek üzerine oluşturulur) çalışır; ev dizininde asla.
- `Send` kabulden (`{chat_id}` yanıtı) önce motor kaydını ve sohbet başına tur kilidini doğrular; kabulden sonraki hatalar sohbet günlüğüne `Error` olayı olarak yazılır.
- `ImportCatalog` yalnız düzenli `.md`/`.txt` dosyası ve en çok 1 MiB kabul eder; `config.toml` yazımları daemon mutex'i + benzersiz geçici dosya ile yapılır.
- Kota kaynak yolları `ULTRON_QUOTA_HOME` ile geçersiz kılınabilir (testlerde gerçek ev dizini okunmaz).
- Testlerde geçici `ULTRON_HOME` ve `ULTRON_PIPE_NAME` geçersiz kılmaları (override) kullanılmalı, asla zamanlanmış görev (scheduled task) kaydedilmemelidir.
