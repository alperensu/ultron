//! Credential Manager uygulaması Task 2 sahibine aittir.

/// Serialize/Clone/Debug/Display uygulanmaz; dar erişim Task 2'de tanımlanır.
pub struct SecretToken {
    #[allow(dead_code)] // Task 2'nin credential/HTTP sınırında kullanılacak tek sır alanı.
    pub(crate) value: String,
}
