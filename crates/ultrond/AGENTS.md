# Ultron Daemon (`crates/ultrond`)

- Adlandırılmış boru (named pipe) adı kullanıcı SID'si baz alınarak ve sadece kullanıcıya izin veren DACL ile oluşturulur.
- İlk örnek (first-instance) güvencesi sağlanır; uzak istemciler reddedilir (reject remote clients).
- Her başlangıçta `run\token` içinde sadece kullanıcı izinli ACL ile rastgele güvenlik belirteci üretilir.
- İstemci iletişimi Hello-ilk (Hello-first) el sıkışması ve sabit zamanlı (constant-time) belirteç karşılaştırmasıyla başlar.
- İletişimde 1 MiB çerçeve limiti (frame limit) uygulanır.
- Tek örnek kilidi (single-instance lock) ile çakışmalar önlenir.
- Yeniden başlatmalarda tur ortasında kalan oturumlar kesintiye uğramış (`interrupted`) olarak işaretlenir.
- Uyanık tutma (keep-awake) özelliği yalnızca bir tur (turn) aktifken çalıştırılır.
- Testlerde geçici `ULTRON_HOME` ve `ULTRON_PIPE_NAME` geçersiz kılmaları (override) kullanılmalı, asla zamanlanmış görev (scheduled task) kaydedilmemelidir.
