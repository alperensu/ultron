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
            // Başlığın tamamını kaldır; karmaşık/bozuk tırnaklarda da sır saklanmaz.
            (
                r#"(?im)\b(?:proxy-)?authorization\s*[\"']?\s*[:=][^\r\n]*"#,
                MASK,
            ),
            (r#"(?i)\bbearer[ \t]+[^\r\n]*"#, MASK),
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
            (
                r"([A-Za-z][A-Za-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@",
                "${1}[MASKED]@",
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

/// Shell seçilmediği için kaçış/alıntı birleştirme semantiği tahmin edilmez.
/// Açık alıntı satır sınırını aşar. Kapanış yoksa veya kaçış/birleştirme
/// belirsizse kalan giriş tüketilir; düz atama değeri satır sonuna uzanır.
fn secret_value_end(text: &str, start: usize, cli: bool) -> usize {
    let mut quote = None;
    let line_start = text[..start]
        .rfind(['\r', '\n'])
        .map_or(0, |index| index + 1);
    for ch in text[line_start..start].chars() {
        match ch {
            '\\' | '`' | '^' => return text.len(),
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            _ => {}
        }
    }
    let mut closed = false;
    let mut chars = text[start..].char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        let index = start + offset;
        if matches!(ch, '\\' | '`' | '^') {
            return text.len();
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                // PowerShell '' ve bitişik bash alıntıları tek bir değer olabilir.
                if chars
                    .peek()
                    .is_some_and(|(_, next)| matches!(next, '\'' | '"'))
                {
                    return text.len();
                }
                quote = None;
                closed = true;
            } else if quote.is_none() && index == start && !closed {
                quote = Some(ch);
            } else {
                return text.len();
            }
        } else if quote.is_none() {
            if matches!(ch, '\r' | '\n')
                || ((cli || closed)
                    && (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>')))
            {
                return index;
            }
            if closed {
                return text.len();
            }
        }
    }
    text.len()
}

/// Değer regex ile kesilmez: anahtar tespitinden sonra bütün argüman tüketilir.
fn mask_named_values(s: &str) -> (String, usize) {
    static KEY: OnceLock<Regex> = OnceLock::new();
    let key = KEY.get_or_init(|| {
        Regex::new(
            r#"(?im)(^|[^a-z0-9_])((?:"[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key)"|'[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key)'|[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key))[ \t]*[:=][ \t]*|(?:--?|/)[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key)[ \t]+)"#,
        ).expect("sabit anahtar deseni geçerli")
    });
    let mut out = String::with_capacity(s.len());
    let mut count = 0;
    let mut cursor = 0;
    for caps in key.captures_iter(s) {
        let prefix = caps.get(2).expect("anahtar ve ayırıcı");
        if prefix.start() < cursor || prefix.end() == s.len() {
            continue;
        }
        let line_start = s[..prefix.start()]
            .rfind(['\r', '\n'])
            .map_or(0, |index| index + 1);
        let lead = s[line_start..prefix.start()].trim_end_matches(['\'', '"']);
        // Satırdaki YAML/atama değeri boşluk içerebilir. Açıklama metnine
        // gömülü bir argümanın ardından gelen normal sözcükler korunur.
        let assignment =
            lead.trim().is_empty() || lead.trim() == "-" || lead.trim_end().ends_with(';');
        let cli =
            prefix.as_str().starts_with(['-', '/']) || lead.ends_with(['-', '/']) || !assignment;
        let end = secret_value_end(s, prefix.end(), cli);
        if end == prefix.end() || &s[prefix.end()..end] == MASK {
            continue;
        }
        let boundary = caps.get(1).expect("anahtar sınırı");
        // Seçeneği de saran tırnak açık bırakılmaz; anahtarla birlikte kaldırılır.
        let start = if matches!(boundary.as_str(), "'" | "\"") {
            boundary.start()
        } else {
            prefix.start()
        };
        out.push_str(&s[cursor..start]);
        out.push_str(MASK);
        cursor = end;
        count += 1;
    }
    out.push_str(&s[cursor..]);
    (out, count)
}

/// YAML bloklarını girinti sınırına kadar tüketir; regex ile girinti karşılaştırılmaz.
fn mask_yaml_blocks(s: &str) -> (String, usize) {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let header = HEADER.get_or_init(|| {
        Regex::new(
            r#"(?i)^([ \t]*)(?:-[ \t]+)?(?:"[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key)"|'[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key)'|[a-z0-9_-]*(?:password|passwd|token|secret|api[-_]?key))[ \t]*:[ \t]*([|>])(?:[1-9][+-]?|[+-][1-9]?)?[ \t]*(?:#[^\r\n]*)?\r?$"#,
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
    let live = crate::turn_grant::mask_live(s);
    let (out, mut count) = mask_yaml_blocks(&live);
    let (mut out, named_count) = mask_named_values(&out);
    count += named_count;
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

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn ambiguous_secret_arguments_mask_to_line_end() {
        let parts =
            ["head", "first", "second"].map(|part| [part, "synthetic", "runtime"].join("_"));
        for name in [
            "api-key",
            "api_key",
            "apikey",
            "token",
            "password",
            "passwd",
            "secret",
            "access-token",
            "access_token",
            "auth-token",
            "auth_token",
            "client-secret",
            "client_secret",
        ] {
            for separator in ["=", ":", " "] {
                for argument in [
                    format!(
                        "'--{name}{separator}{}''{} {}'",
                        parts[0], parts[1], parts[2]
                    ),
                    format!(
                        "--{name}{separator}'{}''{} {}'",
                        parts[0], parts[1], parts[2]
                    ),
                    format!(
                        r"--{name}{separator}'{}\'{} {}'",
                        parts[0], parts[1], parts[2]
                    ),
                    format!(
                        r"--{name}{separator}'{}'\''{} {}'",
                        parts[0], parts[1], parts[2]
                    ),
                    format!(
                        r"--{name}{separator}{}\'{}\ {}",
                        parts[0], parts[1], parts[2]
                    ),
                    format!("--{name}{separator}'{} {} {}", parts[0], parts[1], parts[2]),
                    format!(
                        "--{name}{separator}\"{}`\"{} {}\"",
                        parts[0], parts[1], parts[2]
                    ),
                    format!(
                        "--{name}{separator}'{}'\"{} {}\"",
                        parts[0], parts[1], parts[2]
                    ),
                ] {
                    let command =
                        format!("example {argument}; trailing_argument\nnext line retained");
                    let masked = mask_secrets(&command).0;
                    assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                    assert!(!masked.contains("trailing_argument"), "{masked}");
                    assert_eq!(mask_secrets(&masked), (masked.clone(), 0));
                }
            }
        }
    }

    #[test]
    fn quoted_multiline_and_plain_assignment_values_are_fully_masked() {
        let parts = ["head", "middle", "tail"].map(|part| [part, "synthetic", "runtime"].join("_"));
        for newline in ["\n", "\r\n", "\r"] {
            for quote in ["'", "\""] {
                for prefix in ["--api-key=", "--password ", "password: ", "token="] {
                    let value = parts.join(newline);
                    for closed in [true, false] {
                        let suffix = if closed {
                            format!("{quote}; echo retained{newline}safe: retained")
                        } else {
                            String::new()
                        };
                        let input = format!("example {prefix}{quote}{value}{suffix}");
                        let (masked, count) = mask_secrets(&input);
                        assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                        assert_eq!(count, 1, "{masked}");
                        if closed {
                            assert!(masked.ends_with("safe: retained"), "{masked}");
                        }
                        assert_eq!(mask_secrets(&masked), (masked.clone(), 0));
                    }
                }
                let input = format!(
                    "example {quote}--api-key={}{newline}{}{quote}; echo retained",
                    parts[0], parts[2]
                );
                let masked = mask_secrets(&input).0;
                assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                assert!(masked.ends_with("; echo retained"), "{masked}");
            }
            for key in ["password: ", "token=", "client_secret = ", "\"api_key\": "] {
                let input = format!("{key}{}{newline}safe: retained", parts.join(" "));
                let masked = mask_secrets(&input).0;
                assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                assert!(masked.ends_with("safe: retained"), "{masked}");
            }
        }
    }

    #[test]
    fn named_values_consume_complete_simple_shell_arguments() {
        let parts = ["head", "tail"].map(|part| [part, "synthetic", "runtime"].join("_"));
        for separator in ["=", ":", " "] {
            for argument in [
                format!("--api-key{separator}{},{}", parts[0], parts[1]),
                format!("--api-key{separator}'{} {}'", parts[0], parts[1]),
                format!("'--api-key{separator}{} {}'", parts[0], parts[1]),
            ] {
                let masked = mask_secrets(&format!("example {argument}; echo retained")).0;
                assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                assert!(masked.ends_with("; echo retained"), "{masked}");
            }
        }
    }

    #[test]
    fn whole_quoted_cli_argument_masks_every_value_word() {
        let parts = ["synthetic", "first", "second"].map(|part| [part, "runtime"].join("_"));
        let value = parts.join(" ");
        for name in [
            "api-key",
            "api_key",
            "apikey",
            "access-token",
            "client_secret",
        ] {
            for quote in ["'", "\""] {
                let command =
                    format!("rm synthetic.txt; example {quote}--{name}={value}{quote}; echo safe");
                let masked = mask_secrets(&command).0;
                assert!(masked.contains(MASK));
                assert!(masked.ends_with("; echo safe"));
                assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
            }
        }
    }

    #[test]
    fn yaml_secret_separator_variants_mask_entire_indented_block() {
        let parts = ["synthetic", "first", "second"].map(|part| [part, "runtime"].join("_"));
        for name in [
            "api-key",
            "api_key",
            "apikey",
            "access-token",
            "access_token",
            "auth-token",
            "auth_token",
            "client-secret",
            "client_secret",
            "password",
            "passwd",
            "secret",
        ] {
            for quote in ["", "'", "\""] {
                for marker in ["|", ">-", "|2+", ">+2"] {
                    let input = format!(
                        "config:\n  {quote}{name}{quote}: {marker} # synthetic\n    {}\n      {}\n\n    {}\n  safe: retained\n",
                        parts[0], parts[1], parts[2]
                    );
                    let masked = mask_secrets(&input).0;
                    assert!(parts.iter().all(|part| !masked.contains(part)), "{masked}");
                    assert!(masked.contains("  safe: retained"));
                    assert!(masked.contains(MASK));
                }
            }
        }
    }

    #[test]
    fn cli_secret_names_mask_space_equals_and_separator_variants() {
        let value = ["synthetic", "runtime", "value"].join("_");
        for name in [
            "api-key",
            "api_key",
            "apikey",
            "token",
            "password",
            "passwd",
            "secret",
            "access-token",
            "access_token",
            "auth-token",
            "auth_token",
            "client-secret",
            "client_secret",
        ] {
            for separator in [" ", "="] {
                for quote in ["", "'", "\""] {
                    let command = format!("example --{name}{separator}{quote}{value}{quote}");
                    let masked = mask_secrets(&command).0;
                    assert!(!masked.contains(&value), "{name}{separator}{quote}");
                    assert!(masked.contains(MASK));
                }
            }
            let input = serde_json::json!({name: value}).to_string();
            assert!(!mask_secrets(&input).0.contains(&value), "{name}");
        }
    }
}
