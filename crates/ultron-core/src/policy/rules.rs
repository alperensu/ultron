use std::path::Path;

use serde_json::Value;

use super::command::{
    PathKind, ambiguous_path, changes_working_directory, classified_path, executable, parse,
    path_arguments, without_device_prefix,
};
use super::{Action, Decision, PolicyConfig, PolicyContext, RiskCategory};

fn ask(category: RiskCategory) -> Decision {
    let reason = match category {
        RiskCategory::Delete => "Silme veya içerik temizleme işlemi onay gerektirir",
        RiskCategory::GitHistory => {
            "Git geçmişini veya uzak depoyu değiştirme işlemi onay gerektirir"
        }
        RiskCategory::Deploy => "Dağıtım veya uzak sistem işlemi onay gerektirir",
        RiskCategory::Finance => "Finans yolu erişimi onay gerektirir",
        RiskCategory::Secrets => "Gizli bilgi dosyasına erişim onay gerektirir",
        RiskCategory::Install => "Proje ortamı dışına kurulum onay gerektirir",
        RiskCategory::SystemConfig => "Korumalı yol veya sistem ayarı değişikliği onay gerektirir",
        RiskCategory::Publish => "Yayınlama veya görünürlük değişikliği onay gerektirir",
    };
    Decision::Ask {
        reason: reason.into(),
        category,
    }
}

fn uncertain() -> Decision {
    Decision::Ask {
        reason: "Komut güvenle çözümlenemedi; kullanıcı onayı gerekir".into(),
        category: RiskCategory::SystemConfig,
    }
}

/// Sabit retler önce değerlendirilir. Gerekçeler eylem metnini ve sırları içermez.
/// Yol eşlemesi yan etkisiz ve sözcükseldir; symlink çözümlemesi yapmaz.
pub fn evaluate(action: &Action, cfg: &PolicyConfig, ctx: &PolicyContext) -> Decision {
    match action {
        Action::Shell { command, cwd } => evaluate_shell(command, cwd, cfg, ctx),
        Action::FileWrite { path } => {
            path_risk(path, base(ctx), cfg, ctx).map_or(Decision::Allow, ask)
        }
        Action::FileDelete { .. } => ask(RiskCategory::Delete),
        Action::Network { url } => {
            if bypass(url) {
                denied()
            } else {
                Decision::Allow
            }
        }
        Action::ToolCall { tool, input } => evaluate_tool(tool, input, cfg, ctx),
    }
}

fn denied() -> Decision {
    Decision::Deny {
        reason: "İzin atlama veya geri döndürülemez disk işlemi yasaktır".into(),
    }
}

fn unescape(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, '`' | '^'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn bypass(text: &str) -> bool {
    let lower = unescape(text);
    [
        "--dangerously-skip-permissions",
        "bypasspermissions",
        "danger-full-access",
    ]
    .iter()
    .any(|flag| lower.contains(flag))
}

fn words(text: &str) -> Vec<String> {
    text.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '=' | ','
            )
    })
    .filter(|word| !word.is_empty())
    .map(str::to_owned)
    .collect()
}

fn forbidden(text: &str) -> bool {
    if bypass(text) {
        return true;
    }
    let words = words(&unescape(text));
    for (index, word) in words.iter().enumerate() {
        match executable(word).as_str() {
            "format" | "diskpart" => return true,
            "cipher" if words[index + 1..].iter().any(|arg| arg.starts_with("/w")) => return true,
            "vssadmin" if words.get(index + 1).is_some_and(|arg| arg == "delete") => return true,
            _ => {}
        }
    }
    false
}

fn evaluate_shell(command: &str, cwd: &Path, cfg: &PolicyConfig, ctx: &PolicyContext) -> Decision {
    if forbidden(command) {
        return denied();
    }
    let parsed = parse(command);
    // Her zincir bileşenini tara; önceki Ask sonraki Deny'ı gizleyemez.
    if parsed.commands.iter().flatten().any(|arg| forbidden(arg))
        || parsed
            .commands
            .iter()
            .any(|args| forbidden(&args.join(" ")))
    {
        return denied();
    }
    if parsed.uncertain {
        return uncertain();
    }
    // Sabit sır sınırı allow_list ile kaldırılamaz. Genişletmeleri çalıştırmadan çözümleyemeyiz.
    if sensitive_runtime_access(&parsed.commands, &cwd.to_string_lossy())
        || ambiguous_path(&cwd.to_string_lossy())
        || protected_runtime_path(&cwd.to_string_lossy(), "")
        || parsed
            .commands
            .iter()
            .flat_map(|args| path_arguments(args))
            .any(|arg| {
                ambiguous_path(arg)
                    || arg.to_lowercase().contains("approver-token")
                    || protected_runtime_path(arg, &cwd.to_string_lossy())
            })
    {
        // Zaten onay gerektiren işlemin özgün risk kategorisini koru.
        let category = parsed.commands.iter().find_map(|args| {
            let lower: Vec<_> = args.iter().map(|arg| arg.to_lowercase()).collect();
            builtin(args, &lower, cwd, ctx)
        });
        return ask(category.unwrap_or(RiskCategory::Secrets));
    }
    let mut decision = Decision::Allow;
    for args in &parsed.commands {
        let category = command_risk(args, cwd, cfg, ctx);
        if let Some(category) = category
            && decision == Decision::Allow
        {
            decision = ask(category);
        }
    }
    if cfg
        .allow_list
        .iter()
        .any(|allowed| allowed.trim().eq_ignore_ascii_case(command.trim()))
    {
        return Decision::Allow;
    }
    decision
}

fn creates_link(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        let tail = &args[index + 1..];
        match executable(arg).as_str() {
            "mklink" | "ln" => true,
            "fsutil" => tail.iter().any(|arg| arg.eq_ignore_ascii_case("hardlink")),
            "new-item" | "ni" => tail.iter().any(|arg| {
                arg.to_lowercase()
                    .split(['=', ':'])
                    .any(|part| matches!(part, "junction" | "symboliclink" | "hardlink"))
            }),
            _ => false,
        }
    })
}

fn reads_content_or_recurses(args: &[String]) -> bool {
    args.iter().flat_map(|arg| words(arg)).any(|arg| {
        let lower = arg.to_lowercase();
        matches!(
            executable(&arg).as_str(),
            "get-content"
                | "gc"
                | "cat"
                | "type"
                | "more"
                | "select-string"
                | "sls"
                | "findstr"
                | "rg"
                | "grep"
                | "get-childitem"
                | "gci"
                | "ls"
                | "dir"
                | "find"
                | "copy-item"
                | "cpi"
                | "copy"
                | "cp"
                | "robocopy"
                | "xcopy"
                | "compress-archive"
                | "expand-archive"
                | "zip"
                | "unzip"
                | "7z"
                | "tar"
        ) || lower.starts_with("-recurse")
            || matches!(lower.as_str(), "/s" | "/e")
            || (lower.starts_with('-') && !lower.starts_with("--") && lower[1..].contains('r'))
            || lower == "--recursive"
    })
}

/// Üst dizin taramaları ve bağlantı oluşturma allow-list ile serbest bırakılamaz.
fn sensitive_runtime_access(commands: &[Vec<String>], cwd: &str) -> bool {
    let mut changed_directory = false;
    commands.iter().any(|args| {
        changed_directory |= changes_working_directory(args);
        if creates_link(args) {
            return true;
        }
        if !reads_content_or_recurses(args) {
            return false;
        }
        let paths = path_arguments(args);
        // Örtük cwd erişimleri (ör. gci | gc) de başlangıç cwd'sine bağlanamaz.
        (changed_directory
            && (paths.is_empty()
                || paths.iter().any(|path| {
                    !matches!(classified_path(path), Some((PathKind::FullyQualified, _)))
                })))
            || resolve(cwd, "").is_none()
            || runtime_path_or_ancestor(cwd, "")
            || paths
                .iter()
                .any(|path| resolve(path, cwd).is_none() || runtime_path_or_ancestor(path, cwd))
    })
}

fn base(ctx: &PolicyContext) -> &Path {
    ctx.project_root.as_deref().unwrap_or(&ctx.workspace_root)
}

fn command_risk(
    args: &[String],
    cwd: &Path,
    cfg: &PolicyConfig,
    ctx: &PolicyContext,
) -> Option<RiskCategory> {
    let first = args.first()?;
    let program = executable(first);
    // Yalnız bu komut geri alınabilir silme arabirimidir; zincirin kalanı ayrı taranır.
    if program == "ultron"
        && args
            .get(1)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("trash"))
    {
        return None;
    }
    let lower: Vec<_> = args.iter().map(|arg| arg.to_lowercase()).collect();
    if let Some(risk) = builtin(args, &lower, cwd, ctx) {
        return Some(risk);
    }
    for arg in path_arguments(args) {
        if let Some(risk) = path_risk(Path::new(arg), cwd, cfg, ctx) {
            return Some(risk);
        }
    }
    if cfg
        .deploy_patterns
        .iter()
        .filter(|pattern| !pattern.trim().is_empty())
        .any(|pattern| lower.join(" ").contains(&pattern.to_lowercase()))
    {
        return Some(RiskCategory::Deploy);
    }
    // Tanınmayan sarmalayıcılar veya argüman içine gizlenen komutlar güvenli sayılmaz.
    for (index, arg) in args.iter().enumerate().skip(1) {
        if matches!(program.as_str(), "python" | "python3" | "py")
            && arg.eq_ignore_ascii_case("pip")
            && args[index - 1] == "-m"
            && project_venv(first, cwd, ctx)
        {
            continue;
        }
        let tail_lower: Vec<_> = args[index..].iter().map(|arg| arg.to_lowercase()).collect();
        if let Some(risk) = builtin(&args[index..], &tail_lower, cwd, ctx) {
            return Some(risk);
        }
        let tokens = words(arg);
        for (index, token) in tokens.iter().enumerate() {
            let lower: Vec<_> = tokens[index..]
                .iter()
                .map(|arg| arg.to_lowercase())
                .collect();
            if let Some(risk) = builtin(&tokens[index..], &lower, cwd, ctx) {
                return Some(risk);
            }
            if is_delete(&executable(token)) {
                return Some(RiskCategory::Delete);
            }
        }
    }
    None
}

fn is_delete(program: &str) -> bool {
    matches!(
        program,
        "rm" | "del" | "erase" | "rmdir" | "rd" | "remove-item" | "ri" | "clear-content"
    )
}

fn builtin(
    args: &[String],
    lower: &[String],
    cwd: &Path,
    ctx: &PolicyContext,
) -> Option<RiskCategory> {
    let program = executable(args.first()?);
    let rest = &lower[1..];
    let has = |value: &str| {
        rest.iter()
            .any(|arg| arg == value || arg.starts_with(&format!("{value}=")))
    };
    let verb = rest.first().map(String::as_str).unwrap_or("");
    if is_delete(&program) {
        return Some(RiskCategory::Delete);
    }
    match program.as_str() {
        // Ultron yönetim komutları (`import`, `service`, `switch`...) motor kabuğundan asla
        // sessizce çalışmaz; yalnız salt okunur `policy check`, `history search`, `status` ve
        // geri alınabilir `trash` serbesttir.
        "ultron"
            if !matches!(
                (verb, rest.get(1).map(String::as_str)),
                ("policy", Some("check")) | ("history", Some("search")) | ("status", _) | ("trash", _)
            ) =>
        {
            return Some(RiskCategory::SystemConfig);
        }
        "git" => return git_risk(args),
        "vercel" | "ssh" | "scp" | "rsync" | "systemctl" => return Some(RiskCategory::Deploy),
        "wrangler" if has("deploy") || has("publish") => return Some(RiskCategory::Deploy),
        "netlify" if has("deploy") => return Some(RiskCategory::Deploy),
        "kubectl" if has("apply") || has("delete") => return Some(RiskCategory::Deploy),
        "docker" if has("push") => return Some(RiskCategory::Deploy),
        "winget" if has("install") || has("uninstall") => return Some(RiskCategory::Install),
        "choco" => return Some(RiskCategory::Install),
        "npm" if has("publish") => return Some(RiskCategory::Publish),
        "npm" if (has("i") || has("install")) && (has("-g") || has("--global")) => {
            return Some(RiskCategory::Install);
        }
        "pip" | "pip3" if verb == "install" => {
            if !project_venv(&args[0], cwd, ctx) {
                return Some(RiskCategory::Install);
            }
        }
        "python" | "python3" | "py"
            if rest.windows(3).any(|args| args == ["-m", "pip", "install"]) =>
        {
            if !project_venv(&args[0], cwd, ctx) {
                return Some(RiskCategory::Install);
            }
        }
        "cargo" if has("install") => return Some(RiskCategory::Install),
        "cargo" if has("publish") => return Some(RiskCategory::Publish),
        "reg" if matches!(verb, "add" | "delete") => return Some(RiskCategory::SystemConfig),
        "schtasks" if has("/create") || has("/delete") => return Some(RiskCategory::SystemConfig),
        "netsh" | "set-executionpolicy" => return Some(RiskCategory::SystemConfig),
        "gh" if (rest.windows(2).any(|args| args == ["repo", "edit"]) && has("--visibility"))
            || rest.windows(2).any(|args| args == ["release", "create"]) =>
        {
            return Some(RiskCategory::Publish);
        }
        _ => {}
    }
    None
}

fn git_risk(args: &[String]) -> Option<RiskCategory> {
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        let lower = arg.to_lowercase();
        if !lower.starts_with('-') {
            break;
        }
        match lower.as_str() {
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" | "--exec-path" => {
                if args.get(index + 1).is_none() {
                    return Some(RiskCategory::GitHistory);
                }
                index += 2;
            }
            "--no-pager"
            | "--paginate"
            | "-p"
            | "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
            | "--no-lazy-fetch"
            | "--no-advice"
            | "--no-suppress-advice" => index += 1,
            _ if arg.starts_with("-C") || lower.starts_with("-c") || lower.contains('=') => {
                index += 1
            }
            _ => return Some(RiskCategory::GitHistory),
        }
    }
    let verb = args.get(index)?.to_lowercase();
    let tail = &args[index + 1..];
    let flags: Vec<_> = tail.iter().map(|arg| arg.to_lowercase()).collect();
    let risk = match verb.as_str() {
        "push" => flags.iter().any(|arg| {
            arg == "--force"
                || arg.starts_with("--force=")
                || arg.starts_with("--force-with-lease")
                || arg == "--mirror"
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
                || arg.starts_with('+')
        }),
        "reset" => flags.iter().any(|arg| arg == "--hard"),
        "rebase" | "filter-repo" | "filter-branch" => true,
        "clean" => flags.iter().any(|arg| {
            arg == "--force"
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
        }),
        // Küçük -d güvenli dal silmedir; büyük -D zorla silmedir.
        "branch" => tail.iter().any(|arg| {
            arg == "--delete" && tail.iter().any(|other| other == "--force")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('D'))
        }),
        _ => false,
    };
    risk.then_some(RiskCategory::GitHistory)
}

fn project_venv(program: &str, cwd: &Path, ctx: &PolicyContext) -> bool {
    let Some(resolved) = resolve(program, &cwd.to_string_lossy()) else {
        return false;
    };
    let project = normalized(&base(ctx).to_string_lossy());
    [".venv/scripts", ".venv/bin", "venv/scripts", "venv/bin"]
        .iter()
        .any(|folder| under(&resolved, &format!("{project}/{folder}")))
}

fn secret_path(path: &str) -> bool {
    path.replace('\\', "/").split('/').any(|component| {
        let name = component
            .trim_end_matches(['.', ' '])
            .split(':')
            .next()
            .unwrap_or("")
            .to_lowercase();
        name == "approver-token"
            || name == ".env"
            || name.starts_with(".env.")
            || name.ends_with(".key")
            || name.starts_with("id_rsa")
            || name.starts_with("credentials")
            || name == ".credentials.json"
    })
}

fn protected_runtime_path(path: &str, cwd: &str) -> bool {
    let Some(resolved) = resolve(path, cwd) else {
        return false;
    };
    // Varsayılan Ultron dizini, farklı Windows sürücü/önek yazımlarıyla da korunur.
    if resolved
        .split('/')
        .collect::<Vec<_>>()
        .windows(2)
        .any(|parts| parts == ["ultron", "run"])
    {
        return true;
    }
    crate::Paths::resolve()
        .is_ok_and(|paths| under(&resolved, &normalized(&paths.run.to_string_lossy())))
}

fn runtime_path_or_ancestor(path: &str, cwd: &str) -> bool {
    // PowerShell adlandırılmış parametreleri yolu aynı argümanda taşıyabilir.
    let path = path_argument(path);
    let Some(resolved) = resolve(path, cwd) else {
        return false;
    };
    protected_runtime_path(path, cwd)
        || crate::Paths::resolve()
            .is_ok_and(|paths| under(&normalized(&paths.run.to_string_lossy()), &resolved))
}

fn path_argument(arg: &str) -> &str {
    if arg.starts_with('-') {
        arg.split_once([':', '=']).map_or(arg, |(_, value)| value)
    } else {
        arg
    }
}

fn path_risk(
    path: &Path,
    cwd: &Path,
    cfg: &PolicyConfig,
    ctx: &PolicyContext,
) -> Option<RiskCategory> {
    let text = path.to_string_lossy();
    if secret_path(&text)
        || ambiguous_path(&text)
        || protected_runtime_path(&text, &cwd.to_string_lossy())
        || protected_runtime_path(&cwd.to_string_lossy(), "")
    {
        return Some(RiskCategory::Secrets);
    }
    let Some(resolved) = resolve(&text, &cwd.to_string_lossy()) else {
        return Some(RiskCategory::Secrets);
    };
    let root = base(ctx).to_string_lossy();
    if cfg.finance_paths.iter().any(|path| {
        resolve(&path.to_string_lossy(), &root).is_none_or(|parent| under(&resolved, &parent))
    }) {
        return Some(RiskCategory::Finance);
    }
    if cfg.protected_paths.iter().any(|path| {
        resolve(&path.to_string_lossy(), &root).is_none_or(|parent| under(&resolved, &parent))
    }) {
        return Some(RiskCategory::SystemConfig);
    }
    None
}

fn under(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/') || parent.ends_with('/'))
}

fn resolve(path: &str, cwd: &str) -> Option<String> {
    let (kind, path) = classified_path(path)?;
    match kind {
        PathKind::FullyQualified => Some(normalized(&path)),
        PathKind::Relative if cwd.is_empty() => Some(normalized(&path)),
        PathKind::Relative => {
            let (_, cwd) = classified_path(cwd)?;
            Some(normalized(&format!("{cwd}/{path}")))
        }
    }
}

fn normalized(path: &str) -> String {
    let path = path.replace('\\', "/").to_lowercase();
    let path = without_device_prefix(&path);
    // Win32 aygıt önekli UNC, aynı normal UNC yoluyla karşılaştırılır.
    let unc;
    let path = if let Some(tail) = path.strip_prefix("unc/") {
        unc = format!("//{tail}");
        unc.as_str()
    } else {
        path
    };
    let absolute = path.starts_with('/');
    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if components
                    .last()
                    .is_some_and(|last: &&str| !last.ends_with(':') && *last != "..")
                {
                    components.pop();
                } else if !absolute && components.is_empty() {
                    components.push("..");
                }
            }
            part => components.push(part.trim_end_matches(['.', ' '])),
        }
    }
    format!(
        "{}{}",
        if absolute { "/" } else { "" },
        components.join("/")
    )
}

fn evaluate_tool(tool: &str, input: &Value, cfg: &PolicyConfig, ctx: &PolicyContext) -> Decision {
    fn has_denied(value: &Value) -> bool {
        match value {
            Value::String(text) => {
                forbidden(text)
                    || parse(text)
                        .commands
                        .iter()
                        .any(|args| forbidden(&args.join(" ")))
            }
            Value::Array(values) => values.iter().any(has_denied),
            Value::Object(values) => values.values().any(has_denied),
            _ => false,
        }
    }
    if forbidden(tool) || bypass(&input.to_string()) || has_denied(input) {
        return denied();
    }
    let name = tool.to_lowercase();
    if name.contains("link") || name.contains("junction") {
        return ask(RiskCategory::Secrets);
    }
    if matches!(
        name.as_str(),
        "bash" | "shell" | "powershell" | "run_command"
    ) {
        if let Some(command) = input.get("command").and_then(Value::as_str) {
            let cwd = input
                .get("cwd")
                .and_then(Value::as_str)
                .map(Path::new)
                .unwrap_or(base(ctx));
            return evaluate_shell(command, cwd, cfg, ctx);
        }
        return uncertain();
    }
    if is_delete(&name) || name.contains("delete") {
        return ask(RiskCategory::Delete);
    }
    fn visit(value: &Value, cfg: &PolicyConfig, ctx: &PolicyContext) -> Option<RiskCategory> {
        match value {
            Value::String(text) => {
                if runtime_path_or_ancestor(text, &base(ctx).to_string_lossy())
                    || matches!(
                        text.to_lowercase().as_str(),
                        "junction" | "symboliclink" | "hardlink"
                    )
                {
                    return Some(RiskCategory::Secrets);
                }
                if let Some(risk) = path_risk(Path::new(text), base(ctx), cfg, ctx) {
                    return Some(risk);
                }
                let parsed = parse(text);
                if parsed.uncertain {
                    return Some(RiskCategory::SystemConfig);
                }
                if sensitive_runtime_access(&parsed.commands, &base(ctx).to_string_lossy()) {
                    return Some(RiskCategory::Secrets);
                }
                parsed
                    .commands
                    .iter()
                    .find_map(|args| command_risk(args, base(ctx), cfg, ctx))
            }
            Value::Array(values) => values.iter().find_map(|value| visit(value, cfg, ctx)),
            Value::Object(values) => values.values().find_map(|value| visit(value, cfg, ctx)),
            _ => None,
        }
    }
    visit(input, cfg, ctx).map_or(Decision::Allow, ask)
}

#[cfg(test)]
mod approval_secret_tests {
    use super::*;

    #[test]
    fn runtime_path_normalization_removes_device_prefixes_and_preserves_boundaries() {
        for path in [
            r"C:\SANDBOX\Ultron\\",
            "c:/sandbox/ultron///",
            r"\\?\C:\sandbox\ultron",
            r"\\.\C:\sandbox\ultron",
        ] {
            assert_eq!(normalized(path), "c:/sandbox/ultron");
            assert!(under("c:/sandbox/ultron/run", &normalized(path)));
        }
        assert_eq!(
            normalized(r"\\?\UNC\server\share\ultron"),
            normalized(r"\\server\share\ultron")
        );
        assert!(under("c:/sandbox/ultron/run", &normalized("C:/")));
        assert!(!under("c:/sandbox/ultron/run", "c:/sandbox/ultron-old"));
        assert!(ambiguous_path(r"C:\SYNTH~1\ULTRON~1"));
    }

    #[test]
    fn approver_secret_is_a_protected_path_for_shell_and_tools() {
        let ctx = PolicyContext {
            project_root: None,
            workspace_root: "workspace".into(),
        };
        let cfg = PolicyConfig::default();
        for action in [
            Action::Shell {
                command: "cat run/approver-token".into(),
                cwd: "workspace".into(),
            },
            Action::FileWrite {
                path: "run/approver-token".into(),
            },
            Action::ToolCall {
                tool: "Read".into(),
                input: serde_json::json!({"path": "run/approver-token"}),
            },
        ] {
            assert!(matches!(
                evaluate(&action, &cfg, &ctx),
                Decision::Ask {
                    category: RiskCategory::Secrets,
                    ..
                }
            ));
        }
    }
}
