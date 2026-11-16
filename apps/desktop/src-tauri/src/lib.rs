//! Ultron masaüstü kabuğu. Şimdilik yalnızca web arayüzünü gösterir; köprü komutları Task 8'de.

pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}
