//! Geçmiş metnindeki gizli bilgi desenlerini maskeleme. Muhafazakâr: yalnız bilinen biçimler.

use std::sync::OnceLock;

use regex::{Captures, Regex};

const MASK: &str = "[MASKED]";

struct Rule {
    re: Regex,
    /// Değiştirme şablonu (`$1` ile ön ek korunabilir).
    repl: &'static str,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let defs: &[(&str, &str)] = &[
            // PEM özel anahtar bloğu (bitişi yoksa metnin sonuna kadar).
            (
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
                MASK,
            ),
            (r"\bsk-ant-[A-Za-z0-9_-]{10,}", MASK),
            (r"\bsk-[A-Za-z0-9_-]{20,}", MASK),
            (
                r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,})",
                MASK,
            ),
            (r"\bAKIA[0-9A-Z]{16}\b", MASK),
            (r"\bxox[baprs]-[A-Za-z0-9-]{10,}", MASK),
            (r"\bAIza[0-9A-Za-z_-]{35}", MASK),
            (
                r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*",
                MASK,
            ),
            // URL içi kullanıcı:parola@
            (r"([A-Za-z][A-Za-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@", "${1}[MASKED]@"),
            // Atama, JSON ve YAML; kaçışlı tırnaklar değerin tamamıyla maskelenir.
            (
                r#"(?im)(^|[^a-z0-9_"'])((?:"[a-z0-9_]*(?:password|passwd|token|secret|api_key)"|'[a-z0-9_]*(?:password|passwd|token|secret|api_key)'|[a-z0-9_]*(?:password|passwd|token|secret|api_key))\s*[:=][ \t]*)(\[MASKED\]|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;}&\]"']+)"#,
                "${1}${2}[MASKED]",
            ),
        ];
        defs.iter()
            .map(|(p, r)| Rule {
                re: Regex::new(p).expect("sabit desen geçerli"),
                repl: r,
            })
            .collect()
    })
}

/// YAML bloklarını girinti sınırına kadar tüketir; regex ile girinti karşılaştırılmaz.
fn mask_yaml_blocks(s: &str) -> (String, usize) {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let header = HEADER.get_or_init(|| {
        Regex::new(
            r#"(?i)^([ \t]*)(?:-[ \t]+)?(?:"[a-z0-9_]*(?:password|passwd|token|secret|api_key)"|'[a-z0-9_]*(?:password|passwd|token|secret|api_key)'|[a-z0-9_]*(?:password|passwd|token|secret|api_key))[ \t]*:[ \t]*([|>])(?:[1-9][+-]?|[+-][1-9]?)?[ \t]*(?:#[^\r\n]*)?\r?$"#,
        ).expect("sabit YAML deseni geçerli")
    });
    let mut lines = s.split_inclusive('\n').peekable();
    let mut out = String::with_capacity(s.len());
    let mut count = 0;
    while let Some(line) = lines.next() {
        let Some(caps) = header.captures(line.trim_end_matches('\n')) else {
            out.push_str(line);
            continue;
        };
        let indent = caps[1].len();
        let marker = caps.get(2).expect("blok işareti");
        out.push_str(&line[..marker.start()]);
        out.push_str(MASK);
        if line.ends_with('\n') {
            out.push('\n');
        }
        while let Some(next) = lines.peek() {
            let next_indent = next.len() - next.trim_start_matches([' ', '\t']).len();
            if !next.trim().is_empty() && next_indent <= indent {
                break;
            }
            lines.next();
        }
        count += 1;
    }
    (out, count)
}

/// Gizli bilgi desenlerini `[MASKED]` yapar; (yeni metin, maskelenen sayısı) döner.
pub fn mask_secrets(s: &str) -> (String, usize) {
    let (mut out, mut count) = mask_yaml_blocks(s);
    for rule in rules() {
        let mut n = 0;
        let replaced = rule
            .re
            .replace_all(&out, |caps: &Captures<'_>| {
                if caps.get(3).is_some_and(|value| value.as_str() == MASK) {
                    return caps[0].to_string();
                }
                n += 1;
                let mut dst = String::new();
                caps.expand(rule.repl, &mut dst);
                dst
            })
            .into_owned();
        count += n;
        out = replaced;
    }
    (out, count)
}
