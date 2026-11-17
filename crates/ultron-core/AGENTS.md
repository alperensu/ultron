# Ultron Core (`crates/ultron-core`)

## Modül Haritası
- `paths`: Sistem yolları ve ULTRON_HOME çözümleme mantığı.
- `config`: Yapılandırma modelleri ve yükleme kuralları.
- `db`: SQLite veritabanı bağlantısı ve şema yönetimi.
- `events`: Olay günlüğü (eventlog), tipleri ve serileştirme.
- `engine`: Claude, Codex, AGY sürücüleri (drivers) ve runner altyapısı.
- `workstate`: Çalışma durumu, bağlam takibi ve dosya durumları.
- `summarizer`: Oturum ve bağlam özetleme motoru.
- `handoff`: Motorlar arası bağlam aktarımı (handoff) mantığı.
- `session`: Oturum yaşam döngüsü ve kimliklendirme.
- `external`: Dış araçlar ve sistem etkileşimleri.
- `ipc`: ultrond ile iletişim için çerçeveleme ve protokol tipleri.

## Değişmezler (Invariants)
- Olaylar idempotenttir (`INSERT OR IGNORE`, chat başına ardışık seq).
- Motor geçişlerinde her zaman `EngineSwitched` olayı yazılır (sessiz geçiş yasaktır).
- Sürücüler bilinmeyen/bozuk satırları `EngineEvent::Unknown` olarak eşler; asla paniklemez (panic-free).
- Kota tespiti sadece hata veya sonuç yollarında (error/result paths) yapılır.
- Runner limitleri: 1 MiB satır sınırı, tur başına 64 MiB sınır, alt süreç askıya alınmış (suspended) halde Job Object içine başlatılır.
- Git çağrılarında fsmonitor/hooks/ext-diff/filters devre dışı bırakılır ve `GIT_*` ortam değişkenleri temizlenir.
- Handoff sıralaması stabil-önce (stable-first) kuralına uyar, bütçe karakter (char) bazlı hesaplanır.

## Testler
- Entegrasyon testleri `crates/ultron-core/tests` altında, sentetik fikstürler `tests/fixtures` altındadır.
- Sahte CLI yardımcı ikili dosyası `tests/bin/fake_cli.rs` konumundadır.

## Kalan risk
Politika sözcüksel bir ek savunma katmanıdır; politika yolunun dışındaki aynı kullanıcı süreçleri onaylayıcı belirtecini hâlâ okuyabilir. Motorların işletim sistemi düzeyinde izolasyonu (kısıtlı belirteç veya ayrı hesap) Faz 3 Görev 5 için planlanmıştır. Tur sonu denetimi yalnız dosya metaverisine dayalı, mümkün olan ölçüde çalışan bir ek savunmadır; içerik bütünlüğünü garanti etmez.
