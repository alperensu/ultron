//! Türkçe-duyarlı normalizasyon ve güvenli FTS5 sorgu üretimi.
//!
//! İndekse yazılan metin ve kullanıcı sorgusu aynı [`fold_text`] ile katlanır; FTS5 `unicode61`
//! tokenizer'ı `ı`/`İ` harflerini katlamadığı için bu adım zorunludur. Kullanıcı girdisi FTS
//! operatörü veya SQL olarak yorumlanmaz: yalnız harf/rakam dizileri tırnaklı sözcük olur.

/// Bir sorgudaki en fazla sözcük sayısı (aşırı uzun AND zincirlerini sınırlar).
pub const MAX_QUERY_TOKENS: usize = 16;

/// Türkçe harfleri ve büyük/küçük harf farkını sabit bir biçime indirger.
///
/// `İ`, `I`, `ı`, `i` → `i`; `Ş/ş` → `s`; `Ğ/ğ` → `g`; `Ü/ü` → `u`; `Ö/ö` → `o`; `Ç/ç` → `c`.
/// Diğer karakterler Unicode küçük harfine çevrilir.
pub fn fold_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            // Birleştirici işaretler (NFD girdisi, ör. "cafe" + U+0301): atılır; ayrışık ve
            // bileşik biçim aynı katlanır ve ayraç sayılmaz.
            '\u{0300}'..='\u{036F}' => {}
            'İ' | 'I' | 'ı' | 'i' => out.push('i'),
            'Ş' | 'ş' => out.push('s'),
            'Ğ' | 'ğ' => out.push('g'),
            'Ü' | 'ü' => out.push('u'),
            'Ö' | 'ö' => out.push('o'),
            'Ç' | 'ç' => out.push('c'),
            other => out.extend(other.to_lowercase()),
        }
    }
    out
}

/// Kullanıcı sorgusundan güvenli bir FTS5 `MATCH` ifadesi üretir.
///
/// Harf/rakam dışındaki her karakter sözcük ayracı sayılır; her sözcük çift tırnağa alınır ve
/// AND ile birleştirilir. Sözcük yoksa (boş sorgu, yalnız noktalama/emoji) `None` döner.
pub fn fts_query(query: &str) -> Option<String> {
    let folded = fold_text(query);
    let tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .take(MAX_QUERY_TOKENS)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_turkish_letters() {
        assert_eq!(fold_text("İstanbul ıı Şişli Ğüneş ÖÇ"), "istanbul ii sisli gunes oc");
    }

    #[test]
    fn drops_combining_marks() {
        assert_eq!(fold_text("cafe\u{301}s"), fold_text("cafes"));
        assert_eq!(fold_text("I\u{307}stanbul"), "istanbul");
        assert_eq!(fts_query("cafe\u{301}s"), Some("\"cafes\"".to_string()));
    }

    #[test]
    fn query_is_literal_words_only() {
        assert_eq!(fts_query("a\"b OR c*"), Some("\"a\" \"b\" \"or\" \"c\"".to_string()));
        assert_eq!(fts_query("  \u{1F600}  -- "), None);
        assert_eq!(fts_query(""), None);
    }
}
