//! Shell başlatmadan, sınırlandırılmış komut normalleştirmesi.

const MAX_DEPTH: usize = 16;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_COMMANDS: usize = 1024;

/// Shell türüne göre değişen joker ve değişken genişletmeleri güvenle çözülemez.
pub(super) fn ambiguous_path(text: &str) -> bool {
    let text = without_device_prefix(text);
    // 8.3 adları dosya sistemine bakmadan güvenle genişletilemez; onaya düşer.
    text.contains(['*', '?', '[', ']', '$', '%', '~', '`', '^', '{', '}'])
}

pub(super) fn without_device_prefix(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 4
        && matches!(bytes[0], b'/' | b'\\')
        && matches!(bytes[1], b'/' | b'\\')
        && matches!(bytes[2], b'?' | b'.')
        && matches!(bytes[3], b'/' | b'\\')
    {
        &text[4..]
    } else {
        text
    }
}

#[derive(Clone, Copy)]
pub(super) enum PathKind {
    FullyQualified,
    Relative,
}

/// Yalnız sürücü+kök veya köksüz/sürücüsüz yollar çözümlenebilir.
/// Diğer Windows ad alanları ve shell genişletmeleri tahmin edilmez.
pub(super) fn classified_path(text: &str) -> Option<(PathKind, String)> {
    let plain = without_device_prefix(text);
    let path = plain.replace('\\', "/").to_lowercase();
    if path.is_empty()
        || ambiguous_path(text)
        || path
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '|' | '"' | '\''))
    {
        return None;
    }
    let bytes = path.as_bytes();
    let (kind, tail) = if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'/'
    {
        (PathKind::FullyQualified, &path[3..])
    } else if plain.len() == text.len() && !path.starts_with('/') && !path.contains(':') {
        (PathKind::Relative, path.as_str())
    } else {
        return None;
    };
    if tail.contains(':')
        || tail.split('/').any(|part| {
            (!matches!(part, "." | "..") && part.ends_with(['.', ' ']))
                || matches!(
                    part.split('.').next().unwrap_or(""),
                    "con" | "prn" | "aux" | "nul"
                )
                || part
                    .strip_prefix("com")
                    .or_else(|| part.strip_prefix("lpt"))
                    .is_some_and(|suffix| {
                        matches!(
                            suffix.split('.').next(),
                            Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                        )
                    })
        })
    {
        return None;
    }
    Some((kind, path))
}

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

/// Shell durumu izlenmez: dizin değiştiren bir bileşenden sonra göreli yol
/// başlangıç cwd'sine bağlanamaz. Dinamik kod da aynı belirsizliği taşır.
pub(super) fn changes_working_directory(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            executable(arg).as_str(),
            "cd" | "chdir"
                | "set-location"
                | "sl"
                | "push-location"
                | "pushd"
                | "pop-location"
                | "popd"
                | "invoke-expression"
                | "iex"
        ) || {
            let lower = arg.to_lowercase();
            lower.contains("[environment]::currentdirectory")
                || lower.contains("[system.environment]::currentdirectory")
                || lower.contains("[io.directory]::setcurrentdirectory")
                || lower.contains("[system.io.directory]::setcurrentdirectory")
                || lower.starts_with("-workingdirectory")
        }
    })
}

/// Seçenek adlarını, alt komutları ve filtreleri yol diye çözümleme. Bilinmeyen
/// araçların konumsal argümanları muhafazakâr olarak olası yol kabul edilir.
pub(super) fn path_arguments(args: &[String]) -> Vec<&str> {
    let Some(first) = args.first() else {
        return Vec::new();
    };
    let program = executable(first);
    let build_tool = matches!(program.as_str(), "cargo" | "npm");
    let git = program == "git";
    let git_objects = git
        && !args
            .iter()
            .any(|arg| arg.eq_ignore_ascii_case("--no-index"));
    let mut git_verb = None;
    let mut literal_paths = false;
    let mut expect_path = false;
    let mut skip_value = false;
    let mut paths = Vec::new();
    for arg in args.iter().skip(1) {
        if expect_path {
            paths.push(arg.as_str());
            expect_path = false;
            continue;
        }
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg == "--" {
            literal_paths = true;
            continue;
        }
        if !literal_paths && arg.starts_with('-') {
            let (name, value) = arg
                .split_once([':', '='])
                .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
            let lower = name.to_lowercase();
            let path_option = matches!(
                lower.as_str(),
                "-path"
                    | "-literalpath"
                    | "-destination"
                    | "-destinationpath"
                    | "-target"
                    | "-workingdirectory"
                    | "-filepath"
                    | "--manifest-path"
                    | "--target-dir"
                    | "--prefix"
                    | "--cache"
                    | "--git-dir"
                    | "--work-tree"
                    | "--exec-path"
                    | "--output"
                    | "-output"
                    | "-o"
            ) || (git && name == "-C");
            if path_option {
                if let Some(value) = value {
                    paths.push(value);
                } else {
                    expect_path = true;
                }
            } else if git && name.starts_with("-C") {
                paths.push(&arg[2..]);
            } else if git && matches!(lower.as_str(), "-c" | "--namespace" | "--config-env") {
                skip_value = value.is_none();
            } else if !build_tool
                && !git
                && let Some(value) = value
            {
                paths.push(value);
            }
            continue;
        }
        // cmd/Windows dosya araçlarının anahtarları birer yol değildir.
        if !literal_paths
            && matches!(
                arg.to_ascii_lowercase().as_str(),
                "/s" | "/e" | "/b" | "/a" | "/y" | "/i" | "/q" | "/f"
            )
        {
            continue;
        }
        if build_tool {
            continue;
        }
        if git && git_verb.is_none() {
            git_verb = Some(arg.as_str());
            continue;
        }
        // revision:path Git nesnesidir; yalnız salt okunur komutlarda ve
        // dosya pathspec sınırından önce bu sözdizimine izin verilir.
        if git_objects
            && !literal_paths
            && git_verb.is_some_and(|verb| {
                matches!(
                    verb.to_ascii_lowercase().as_str(),
                    "show" | "log" | "diff" | "cat-file" | "ls-tree" | "rev-parse"
                )
            })
            && arg.contains(':')
            && !arg.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        {
            continue;
        }
        paths.push(arg.as_str());
    }
    paths
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
