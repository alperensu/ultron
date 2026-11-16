//! Shell başlatmadan, sınırlandırılmış komut normalleştirmesi.

const MAX_DEPTH: usize = 16;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_COMMANDS: usize = 1024;

#[derive(Default)]
pub(super) struct Parsed {
    // İç kurallarda Git'in -D/-d ayrımı için özgün harfler korunur.
    pub commands: Vec<Vec<String>>,
    pub uncertain: bool,
}

/// Sarmalayıcıları ve zincirleri açar; bağımsız değişken sınırlarını korur.
/// Sonuç bir shell'e geri verilmemelidir. Hata bilgisi `evaluate` içinde tutulur.
pub fn normalize_shell(command: &str) -> Vec<Vec<String>> {
    parse(command)
        .commands
        .into_iter()
        .map(|args| args.into_iter().map(|arg| arg.to_lowercase()).collect())
        .collect()
}

pub(super) fn executable(token: &str) -> String {
    let leaf = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let lower = leaf.to_lowercase();
    for suffix in [".exe", ".com", ".cmd", ".bat"] {
        if let Some(name) = lower.strip_suffix(suffix) {
            return name.to_owned();
        }
    }
    lower
}

pub(super) fn parse(command: &str) -> Parsed {
    let mut parsed = Parsed::default();
    expand_script(command, 0, &mut parsed);
    parsed
}

fn expand_script(script: &str, depth: usize, parsed: &mut Parsed) {
    if depth > MAX_DEPTH || script.len() > MAX_COMMAND_BYTES {
        parsed.uncertain = true;
        return;
    }
    let (commands, uncertain) = tokenize(script);
    parsed.uncertain |= uncertain;
    for args in commands {
        expand_args(args, depth, parsed);
    }
}

fn expand_args(mut args: Vec<String>, depth: usize, parsed: &mut Parsed) {
    if args.is_empty() {
        return;
    }
    if depth > MAX_DEPTH || parsed.commands.len() >= MAX_COMMANDS {
        parsed.uncertain = true;
        return;
    }
    // PowerShell çağrı işleci ve POSIX exec, komutun güvenlik sınıfını değiştirmez.
    if args[0] == "&" || args[0].eq_ignore_ascii_case("exec") {
        args.remove(0);
        expand_args(args, depth + 1, parsed);
        return;
    }
    let program = executable(&args[0]);
    if matches!(
        program.as_str(),
        "cmd" | "powershell" | "pwsh" | "bash" | "sh"
    ) {
        let switch = args.iter().enumerate().skip(1).find(|(_, arg)| {
            let arg = arg.to_ascii_lowercase();
            match program.as_str() {
                "cmd" => matches!(arg.as_str(), "/c" | "/k"),
                "bash" | "sh" => arg == "-c" || (arg.starts_with('-') && arg[1..].contains('c')),
                _ => matches!(
                    arg.as_str(),
                    "-c" | "-command" | "-e" | "-ec" | "-enc" | "-encodedcommand"
                ),
            }
        });
        if let Some((index, flag)) = switch {
            let encoded = matches!(
                flag.to_ascii_lowercase().as_str(),
                "-e" | "-ec" | "-enc" | "-encodedcommand"
            );
            let payload = &args[index + 1..];
            if encoded {
                if payload.len() == 1
                    && let Some(script) = decode_powershell(&payload[0])
                {
                    expand_script(&script, depth + 1, parsed);
                    return;
                }
                parsed.uncertain = true;
            } else if let Some(script) = payload.first() {
                if payload.len() == 1 || matches!(program.as_str(), "bash" | "sh") {
                    expand_script(script, depth + 1, parsed);
                } else if script.contains(char::is_whitespace) {
                    // cmd/PowerShell vararg semantiği tam çözülemiyorsa izin verme.
                    parsed.uncertain = true;
                    expand_script(script, depth + 1, parsed);
                    expand_args(payload[1..].to_vec(), depth + 1, parsed);
                } else {
                    expand_args(payload.to_vec(), depth + 1, parsed);
                }
                return;
            } else {
                parsed.uncertain = true;
            }
        } else {
            // -File ve etkileşimli shell'in çalıştıracağı içerik bu eylemde yoktur.
            parsed.uncertain = true;
        }
    }
    parsed.commands.push(args);
}

fn tokenize(script: &str) -> (Vec<Vec<String>>, bool) {
    let mut commands = Vec::new();
    let mut args = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote = None;
    let mut uncertain = false;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        if quote == Some('\'') {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    // PowerShell tek tırnak kaçışı.
                    chars.next();
                    token.push('\'');
                } else {
                    quote = None;
                }
            } else {
                token.push(ch);
            }
            continue;
        }
        if matches!(ch, '`' | '^') {
            started = true;
            match chars.next() {
                Some('\r') if chars.peek() == Some(&'\n') => {
                    chars.next();
                }
                Some('\n') => {}
                Some(next) => token.push(next),
                None => uncertain = true,
            }
            continue;
        }
        if ch == '\\'
            && chars
                .peek()
                .is_some_and(|next| matches!(next, '"' | '\'' | ' ' | ';' | '|' | '&'))
        {
            token.push(chars.next().expect("peek ile doğrulandı"));
            started = true;
            continue;
        }
        if quote == Some('"') {
            if ch == '"' {
                quote = None;
            } else {
                if ch == '$' {
                    uncertain = true;
                }
                token.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                started = true;
            }
            ';' | '|' | '&' | '\n' | '\r' => {
                if started {
                    args.push(std::mem::take(&mut token));
                    started = false;
                }
                let double = chars.peek() == Some(&ch) && matches!(ch, '|' | '&');
                if double {
                    chars.next();
                }
                if ch == '&' && !double && args.is_empty() {
                    // & 'Remove-Item' x: PowerShell çağrı işleci.
                    continue;
                }
                if !args.is_empty() {
                    commands.push(std::mem::take(&mut args));
                }
            }
            '>' | '<' => {
                if started {
                    args.push(std::mem::take(&mut token));
                    started = false;
                }
                args.push(ch.to_string());
            }
            ch if ch.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut token));
                    started = false;
                }
            }
            ch => {
                if matches!(ch, '$' | '(' | ')' | '{' | '}' | '\0') {
                    uncertain = true;
                }
                token.push(ch);
                started = true;
            }
        }
        if commands.len() >= MAX_COMMANDS {
            uncertain = true;
            break;
        }
    }
    if quote.is_some() {
        uncertain = true;
    }
    if started {
        args.push(token);
    }
    if !args.is_empty() {
        commands.push(args);
    }
    (commands, uncertain)
}

/// Strict Base64 + UTF-16LE. Hatalı dolgu, tek bayt ve bozuk surrogate reddedilir.
fn decode_powershell(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let last = (index + 1) * 4 == bytes.len();
        let value = |byte| match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let a = value(chunk[0])?;
        let b = value(chunk[1])?;
        decoded.push((a << 2) | (b >> 4));
        if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' || b & 15 != 0 {
                return None;
            }
        } else {
            let c = value(chunk[2])?;
            decoded.push((b << 4) | (c >> 2));
            if chunk[3] == b'=' {
                if !last || c & 3 != 0 {
                    return None;
                }
            } else {
                decoded.push((c << 6) | value(chunk[3])?);
            }
        }
    }
    if !decoded.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<_> = decoded
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let script = String::from_utf16(&units).ok()?;
    Some(script.trim_start_matches('\u{feff}').to_owned())
}
