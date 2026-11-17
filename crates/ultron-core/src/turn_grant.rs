//! Tur kapsamlı yetki: daemon bir motor turu başlatırken (sohbet, oturum dizini) a bağlı rastgele
//! bir belirteç üretir, motor ortamına `ULTRON_TURN_TOKEN` olarak verir; tur bitince geçersizdir.
//! Motorlar daemon sürecinde çalıştırıldığı için kayıt süreç içidir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub chat_id: String,
    pub cwd: PathBuf,
}

#[derive(Default)]
struct Registry {
    by_token: HashMap<String, Grant>,
    by_chat: HashMap<String, String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Düştüğünde (tur bitince) belirteci geçersiz kılar.
pub struct TurnGrant {
    token: String,
    chat_id: String,
}

impl TurnGrant {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for TurnGrant {
    fn drop(&mut self) {
        if let Ok(mut reg) = registry().lock() {
            reg.by_token.remove(&self.token);
            if reg.by_chat.get(&self.chat_id) == Some(&self.token) {
                reg.by_chat.remove(&self.chat_id);
            }
        }
    }
}

/// Sohbet için yeni tur belirteci üretir (aynı sohbetin eskisi geçersiz olur).
pub fn issue(chat_id: &str, cwd: &Path) -> TurnGrant {
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    if let Ok(mut reg) = registry().lock() {
        if let Some(old) = reg.by_chat.insert(chat_id.to_owned(), token.clone()) {
            reg.by_token.remove(&old);
        }
        reg.by_token.insert(
            token.clone(),
            Grant {
                chat_id: chat_id.to_owned(),
                cwd: cwd.to_path_buf(),
            },
        );
    }
    TurnGrant {
        token,
        chat_id: chat_id.to_owned(),
    }
}

/// Etkin tur belirteçlerini metinde `[MASKED]` yapar (günlüğe/yayına gitmeden önce).
pub fn mask_live(text: &str) -> String {
    let Ok(reg) = registry().lock() else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for token in reg.by_token.keys() {
        if out.contains(token.as_str()) {
            out = out.replace(token.as_str(), "[MASKED]");
        }
    }
    out
}

/// JSON değerindeki tüm metin alanlarını ve nesne anahtarlarını maskeler.
pub fn mask_live_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = mask_live(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(mask_live_value),
        serde_json::Value::Object(map) => {
            let old = std::mem::take(map);
            for (key, mut item) in old {
                mask_live_value(&mut item);
                map.insert(mask_live(&key), item);
            }
        }
        _ => {}
    }
}

/// Etkin tüm tur belirteçleri.
pub fn live_tokens() -> Vec<String> {
    registry()
        .lock()
        .map(|reg| reg.by_token.keys().cloned().collect())
        .unwrap_or_default()
}

/// Bu uzunluktan kısa belirteç önekleri parça sınırında açıkta kalabilir (kabul edilen sızıntı).
const MIN_HOLD: usize = 8;

/// Akış halindeki metin olaylarında olay sınırlarına yayılan belirteçleri maskeler.
/// Bir olayın sonu etkin bir belirtecin (en az `MIN_HOLD` karakterlik) öneki ise o kısım bir
/// sonraki olaya kadar tutulur; önceki ham metnin kuyruğu sonraki olayla birlikte taranır.
#[derive(Default)]
pub struct StreamMasker {
    carry: Vec<char>,
    held: usize,
}

impl StreamMasker {
    /// Yeni metin parçasını işler; yayınlanacak (maskeli) metni döner (boş olabilir).
    pub fn push(&mut self, text: &str, tokens: &[String]) -> String {
        let tokens: Vec<Vec<char>> = tokens
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| t.chars().collect())
            .collect();
        let max_len = tokens.iter().map(Vec::len).max().unwrap_or(0);
        let new: Vec<char> = text.chars().collect();
        let mut window = std::mem::take(&mut self.carry);
        window.extend(new.iter());
        let region_len = (self.held + new.len()).min(window.len());
        let region_start = window.len() - region_len;
        let mut masked = vec![false; window.len()];
        for token in &tokens {
            if window.len() >= token.len() {
                for i in 0..=window.len() - token.len() {
                    if window[i..i + token.len()] == token[..] {
                        masked[i..i + token.len()].fill(true);
                    }
                }
            }
        }
        let mut hold = 0;
        for token in &tokens {
            for k in (MIN_HOLD..token.len()).rev() {
                if k <= window.len()
                    && window.ends_with(&token[..k])
                    && !masked[window.len() - k..].iter().any(|m| *m)
                {
                    // Önekin yayınlanmış kısmı geri alınamaz (en çok MIN_HOLD-1 karakter);
                    // yayınlanmamış kısmın tamamı tutulur.
                    hold = hold.max(k.min(region_len));
                    break;
                }
            }
        }
        let emit_end = window.len() - hold;
        let mut out = String::new();
        for i in region_start..emit_end {
            if masked[i] {
                if i == region_start || !masked[i - 1] {
                    out.push_str("[MASKED]");
                }
            } else {
                out.push(window[i]);
            }
        }
        let keep = max_len.saturating_sub(1).min(window.len());
        self.carry = window[window.len() - keep..].to_vec();
        self.held = hold.min(self.carry.len());
        out
    }

    /// Akış başka bir olayla kesilince tutulan kısmı bırakır: hâlâ etkin bir belirtecin
    /// önekiyse `[MASKED]`, değilse ham metin. Karşılaştırma kuyruğu korunur.
    pub fn flush(&mut self, tokens: &[String]) -> Option<String> {
        if self.held == 0 {
            return None;
        }
        let held: String = self.carry[self.carry.len() - self.held..].iter().collect();
        self.held = 0;
        if tokens.iter().any(|t| t.starts_with(&held)) {
            Some("[MASKED]".to_owned())
        } else {
            Some(held)
        }
    }
}

/// Motor ortamına verilecek belirteç (tur sürerken).
pub fn token_for_chat(chat_id: &str) -> Option<String> {
    registry().lock().ok()?.by_chat.get(chat_id).cloned()
}

/// Belirteçten sohbet ve oturum dizinini çözer; geçersiz/bitmişse `None`.
pub fn resolve(token: &str) -> Option<Grant> {
    registry().lock().ok()?.by_token.get(token).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_resolves_only_while_turn_lasts() {
        let grant = issue("grant-test-a", Path::new("C:/a"));
        let token = grant.token().to_owned();
        assert_eq!(resolve(&token).unwrap().chat_id, "grant-test-a");
        assert_eq!(token_for_chat("grant-test-a").as_deref(), Some(token.as_str()));
        drop(grant);
        assert!(resolve(&token).is_none());
        assert!(token_for_chat("grant-test-a").is_none());
    }

    #[test]
    fn live_tokens_are_masked_in_text_and_json() {
        let grant = issue("grant-test-c", Path::new("C:/c"));
        let token = grant.token().to_owned();
        let masked = mask_live(&format!("a {token} b"));
        assert!(!masked.contains(&token) && masked.contains("[MASKED]"));
        let mut value = serde_json::json!({"x": [format!("t={token}")], "y": {"z": token.clone()}});
        mask_live_value(&mut value);
        assert!(!value.to_string().contains(&token));
        drop(grant);
        assert_eq!(mask_live(&token), token, "düşen belirteç artık yetki değil");
    }

    #[test]
    fn json_keys_are_masked_too() {
        let grant = issue("grant-test-d", Path::new("C:/d"));
        let token = grant.token().to_owned();
        let mut value = serde_json::json!({ token.clone(): 1, "iç": { format!("k{token}"): [2] } });
        mask_live_value(&mut value);
        assert!(!value.to_string().contains(&token), "{value}");
        assert!(value.to_string().contains("[MASKED]"));
    }

    fn feed(parts: &[&str], tokens: &[String]) -> String {
        let mut masker = StreamMasker::default();
        let mut out = String::new();
        for part in parts {
            out.push_str(&masker.push(part, tokens));
        }
        out.push_str(&masker.flush(tokens).unwrap_or_default());
        out
    }

    #[test]
    fn stream_masker_masks_tokens_split_across_events() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned();
        let tokens = vec![token.clone()];
        let two = format!("önce {} sonra", token);
        let (a, b) = two.split_at(20);
        assert_eq!(feed(&[a, b], &tokens), "önce [MASKED] sonra");
        let three = format!("x {} y", token);
        let (a, rest) = three.split_at(9);
        let (b, c) = rest.split_at(30);
        // İlk parçadaki 7 karakterlik önek (MIN_HOLD altı) açıkta kalabilir; gerisi maskelenir.
        let got = feed(&[a, b, c], &tokens);
        assert_eq!(got, "x 0123456[MASKED] y");
        assert!(!got.contains(&token[..8]));
        // Parçalar tek tek de belirteç içermez.
        let mut masker = StreamMasker::default();
        for part in [&token[..32], &token[32..]] {
            let out = masker.push(part, &tokens);
            assert!(!out.contains(&token[..8]) || out.contains("[MASKED]"));
        }
        // Belirteçsiz metin değişmez ve gecikmez.
        let mut plain = StreamMasker::default();
        assert_eq!(plain.push("merhaba ", &tokens), "merhaba ");
        assert_eq!(plain.push("dünya", &tokens), "dünya");
        assert_eq!(plain.flush(&tokens), None);
    }

    #[test]
    fn newer_turn_replaces_older_token() {
        let first = issue("grant-test-b", Path::new("C:/b"));
        let old = first.token().to_owned();
        let second = issue("grant-test-b", Path::new("C:/b"));
        assert!(resolve(&old).is_none());
        assert!(resolve(second.token()).is_some());
        drop(first);
        assert!(resolve(second.token()).is_some());
    }
}
