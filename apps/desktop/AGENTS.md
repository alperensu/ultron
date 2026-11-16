# apps/desktop

Ultron masaüstü uygulaması: Tauri 2 kabuğu (`src-tauri`, crate `ultron-desktop`) + Vite/React 18/TypeScript (strict) arayüzü.

## Komutlar
- `npm --prefix apps/desktop run dev|build|test|lint|typecheck|e2e`; bağımlılıklar tam sürümle sabit, `package-lock.json` commit edilir.
- Masaüstü derlemesi: `npx tauri build --debug` (apps/desktop içinde). Rust tarafı: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- Bileşen kataloğu: `/__ui` rotası (`src/KitchenSink.tsx`). Yeni bileşen buraya da eklenir; görsel kontrol `e2e/ui.spec.ts` ekran görüntüsüyle yapılır.

## Tasarım sistemi
- Yalnızca koyu tema; renk, tipografi, yarıçap ve hareket değerleri `src/design/tokens.css` token'larından gelir, bileşenlerde sabit renk yazılmaz. Serif yazı yok.
- Bileşenler `src/ui/` altında, stil `src/ui/ui.css`. Ekranlar bileşenleri birleştirir; tek seferlik stil yerine bileşen/token genişletilir.
- Animasyon yalnızca `transform`/`opacity` (renk geçişi hariç); yay eğrisi `cubic-bezier(0.32,0.72,0,1)`; süre ve gecikme `src/design/motion.ts` üzerinden. `prefers-reduced-motion` (Windows "animasyonları azalt") kapatır: `reducedMotion()` doğruysa transform yok, süre sıfır.
- Uzun listeler sanallaştırılır; kare bütçesi 60 FPS.
- Sheet ve diyaloglar odağı içeride tutar, Escape ile kapanır, odağı geri verir.

## Güvenlik ve dürüstlük
- Sahte buton yok: arka ucu olmayan özellik gösterilmez; boş durum "Aşama N'de" der. Örnek veri yalnızca testlerde ve `/__ui` içinde, "örnek" etiketiyle.
- Webview `ultrond`'a doğrudan bağlanmaz; yalnızca Tauri komutları ve `ultron://event` kullanılır (Task 8). Token webview'e verilmez, loglanmaz.
- Motor çıktısı/markdown sanitize edilir (`rehype-sanitize`); ham HTML ve `javascript:` bağlantıları çalışmaz.
- Tauri capabilities en az düzeyde: `core:default` + özel başlık çubuğu için pencere düğmeleri. fs/shell/http/dialog eklentisi eklenmez; CSP gevşetilmez (`default-src 'self'`, harici script yok).
- Repoda gerçek veri, kişisel yol veya sır yok.
