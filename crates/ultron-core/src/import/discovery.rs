//! Kullanıcının gerçek proje klasörlerini Claude Code ve Codex oturum günlüklerinden bulur.
//!
//! Kaynaklar: `~/.claude/projects/*/*.jsonl` (cwd alanı), `~/.codex/sessions/**/*.jsonl` ve
//! `~/.codex/archived_sessions` (`session_meta.cwd`), isteğe bağlı `~/.claude/projects-catalog.md`.
//! Alt ajan oturumları, worktree/geçici/ev/sürücü kökü yolları ve artık var olmayan klasörler elenir.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::safe_fs::{
    CATALOG_MAX_BYTES, GIT_FILE_MAX_BYTES, is_plain_dir, is_plain_file, read_text,
};
use crate::db::normalized_project_path;

/// Alt ajan (işçi) oturumlarının ilk kullanıcı mesajı bu önekle başlar.
pub const SUBAGENT_PREFIX: &str = "You are a scoped worker";

/// Başlık taramasında okunan en fazla satır ve satır başına en fazla bayt.
const PEEK_LINES: usize = 400;
const PEEK_LINE_BYTES: u64 = 256 * 1024;

/// Elemede kullanılan sistem bilgisi; testler sentetik değerler verir.
#[derive(Debug, Clone)]
pub struct Filters {
    /// Kullanıcının ev dizini (kendisi proje sayılmaz).
    pub home: PathBuf,
    /// Geçici/scratchpad kökleri; altındaki yollar elenir.
    pub temp_dirs: Vec<PathBuf>,
}

impl Filters {
    /// Gerçek sistem geçici dizinini kullanır.
    pub fn system(home: &Path) -> Filters {
        let mut temp_dirs = vec![std::env::temp_dir(), home.join("AppData").join("Local").join("Temp")];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            temp_dirs.push(PathBuf::from(local).join("Temp"));
        }
        // Ultron'un kendi verisi (fikstür, çalışma alanı) proje değildir.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            temp_dirs.push(PathBuf::from(local).join("ultron"));
        }
        if let Some(uh) = std::env::var_os("ULTRON_HOME") {
            temp_dirs.push(PathBuf::from(uh));
        }
        Filters {
            home: home.to_path_buf(),
            temp_dirs,
        }
    }
}

/// Bir oturum günlüğü dosyası.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub path: PathBuf,
    pub mtime_ms: i64,
    /// İşçi / alt ajan oturumu (ilk mesajı "You are a scoped worker" ile başlar vb.).
    pub worker: bool,
}

/// Keşif sonucu: projeler ve hiçbir projeye atfedilemeyen oturum sayısı.
#[derive(Debug, Default)]
pub struct Discovery {
    pub projects: Vec<FoundProject>,
    /// Kapsayıcı klasör (Documents, ev dizini...) veya çözülemeyen worktree oturumları.
    pub unmatched_sessions: usize,
}

/// Bulunan proje klasörü ve ona ait oturumlar (işçi oturumları dahil, `worker` ile işaretli).
#[derive(Debug, Clone, Default)]
pub struct FoundProject {
    pub path: String,
    /// Katalogdan gelen ad/açıklama.
    pub catalog: Option<(String, String)>,
    pub claude: Vec<SessionFile>,
    pub codex: Vec<SessionFile>,
}

impl FoundProject {
    /// (claude, codex, işçi) oturum sayıları; işçi sayısı iki kaynağın toplamıdır.
    pub fn counts(&self) -> (usize, usize, usize) {
        let normal = |v: &[SessionFile]| v.iter().filter(|f| !f.worker).count();
        let workers = self
            .claude
            .iter()
            .chain(&self.codex)
            .filter(|f| f.worker)
            .count();
        (normal(&self.claude), normal(&self.codex), workers)
    }
}

/// Oturum kaynağı.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

/// Oturum başlığından çıkarılan bilgi.
#[derive(Debug, Default)]
pub(super) struct Peek {
    pub cwd: Option<String>,
    pub subagent: bool,
}

/// UNC (`\\sunucu\paylaşım`) veya `\\?\` / `\\.\` biçimli mi? Bu yollar kabul edilmez.
pub fn is_remote_or_verbatim(path: &str) -> bool {
    let t = path.trim_start();
    t.starts_with(r"\\") || t.starts_with("//")
}

/// `\\?\` ön ekini ve sondaki ayırıcıları atar.
pub(super) fn clean_cwd(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix(r"\\?\").unwrap_or(s);
    s.trim_end_matches(['\\', '/']).to_string()
}

/// Sistem tarafından enjekte edilen (kullanıcının yazmadığı) mesaj mı?
pub(super) fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<') || t.starts_with("# AGENTS.md") || t.starts_with("Caveat:")
}

pub(super) fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Sınırlı uzunlukta bir satır okur; aşan satırlar atlanır (`Some(None)`). Dosya sonunda `None`.
pub(super) fn bounded_line(r: &mut impl BufRead) -> Option<Option<String>> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(PEEK_LINE_BYTES)
        .read_until(b'\n', &mut buf)
        .ok()?;
    if n == 0 {
        return None;
    }
    if !buf.ends_with(b"\n") && n as u64 == PEEK_LINE_BYTES {
        let mut sink = Vec::new();
        loop {
            sink.clear();
            let m = r
                .by_ref()
                .take(PEEK_LINE_BYTES)
                .read_until(b'\n', &mut sink)
                .ok()?;
            if m == 0 || sink.ends_with(b"\n") {
                break;
            }
        }
        return Some(None);
    }
    Some(Some(String::from_utf8_lossy(&buf).trim().to_string()))
}

/// İçerikten (string veya metin blokları) yalnız düz metni alır; araç blokları atlanır.
pub(super) fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn peek(path: &Path, source: Source) -> Peek {
    let mut out = Peek::default();
    let Ok(file) = File::open(path) else {
        return out;
    };
    let mut reader = BufReader::new(file);
    let mut first_user: Option<String> = None;
    for _ in 0..PEEK_LINES {
        let Some(line) = bounded_line(&mut reader) else {
            break;
        };
        let Some(v) = line.and_then(|l| serde_json::from_str::<Value>(&l).ok()) else {
            continue;
        };
        match source {
            Source::Claude => {
                if out.cwd.is_none()
                    && let Some(cwd) = v.get("cwd").and_then(Value::as_str)
                {
                    out.cwd = Some(clean_cwd(cwd));
                }
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    out.subagent = true;
                }
                if first_user.is_none()
                    && v.get("type").and_then(Value::as_str) == Some("user")
                    && v.get("isMeta").and_then(Value::as_bool) != Some(true)
                    && let Some(c) = v.get("message").and_then(|m| m.get("content"))
                {
                    let text = content_text(c);
                    if !text.trim().is_empty() && !is_injected(&text) {
                        first_user = Some(text);
                    }
                }
            }
            Source::Codex => {
                let payload = v.get("payload");
                match v.get("type").and_then(Value::as_str) {
                    Some("session_meta") => {
                        if let Some(cwd) = payload
                            .and_then(|p| p.get("cwd"))
                            .and_then(Value::as_str)
                        {
                            out.cwd = Some(clean_cwd(cwd));
                        }
                        if payload
                            .and_then(|p| p.get("source"))
                            .is_some_and(|s| s.get("subagent").is_some())
                        {
                            out.subagent = true;
                        }
                    }
                    Some("response_item") => {
                        if first_user.is_none()
                            && let Some(p) = payload
                            && p.get("type").and_then(Value::as_str) == Some("message")
                            && p.get("role").and_then(Value::as_str) == Some("user")
                        {
                            let text = content_text(p.get("content").unwrap_or(&Value::Null));
                            if !text.trim().is_empty() && !is_injected(&text) {
                                first_user = Some(text);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if out.cwd.is_some() && first_user.is_some() {
            break;
        }
    }
    if first_user
        .as_deref()
        .is_some_and(|t| t.trim_start().starts_with(SUBAGENT_PREFIX))
    {
        out.subagent = true;
    }
    out
}

fn collect_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            if depth > 0 && entry.file_name() != "subagents" {
                collect_jsonl(&path, depth - 1, out);
            }
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Klasör proje olarak alınabilir mi? Elenme nedenleri modül belgesindedir.
pub fn is_project_dir(path: &str, filters: &Filters) -> bool {
    if is_remote_or_verbatim(path) {
        return false;
    }
    let p = Path::new(path);
    if !p.is_absolute() || p.parent().is_none() {
        return false; // göreli yol veya sürücü kökü
    }
    let key = normalized_project_path(path);
    if key.len() <= 3 || key == normalized_project_path(&filters.home.to_string_lossy()) {
        return false;
    }
    let slashed = format!("{key}/");
    if slashed.contains("/.claude/worktrees/")
        || slashed.contains("/.codex/worktrees/")
        || slashed.contains("/.local/bin/")
    {
        return false;
    }
    for temp in &filters.temp_dirs {
        let t = normalized_project_path(&temp.to_string_lossy());
        if !t.is_empty() && (key == t || slashed.starts_with(&format!("{t}/"))) {
            return false;
        }
    }
    if !p.is_dir() {
        return false;
    }
    // Bağlı git worktree'sinde `.git` bir dosyadır ve gitdir `worktrees/` altını gösterir.
    let git = p.join(".git");
    !(is_plain_file(&git)
        && read_text(&git, None, GIT_FILE_MAX_BYTES)
            .map(|t| t.replace('\\', "/").contains("/worktrees/"))
            .unwrap_or(false))
}

/// Tek biçimli (ters eğik çizgili) görüntü yolu; diskte varsa gerçek harf büyüklüğüyle.
pub fn display_path(raw: &str) -> String {
    let win = clean_cwd(raw).replace('/', "\\");
    if let Ok(canon) = std::fs::canonicalize(&win) {
        let c = clean_cwd(&canon.to_string_lossy());
        if normalized_project_path(&c) == normalized_project_path(&win) {
            return c;
        }
    }
    win
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Bağlı git worktree'sinin ana checkout'unu `.git` dosyasından (gitdir -> commondir) çözer.
fn linked_main(dir: &Path) -> Option<PathBuf> {
    let text = read_text(&dir.join(".git"), None, GIT_FILE_MAX_BYTES)?;
    let gd = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let mut gitdir = PathBuf::from(gd);
    if gitdir.is_relative() {
        gitdir = dir.join(gitdir);
    }
    let common = match read_text(&gitdir.join("commondir"), None, GIT_FILE_MAX_BYTES) {
        Some(c) => lexical_normalize(&gitdir.join(c.trim())),
        None => {
            if !gd.replace('\\', "/").contains("/worktrees/") {
                return None;
            }
            gitdir.parent()?.parent()?.to_path_buf()
        }
    };
    if common.file_name()? != ".git" {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

enum Target {
    /// cwd zaten ana klasör.
    Direct,
    /// Ana checkout (veya en yakın git kökü) çözüldü.
    Path(String),
    /// Yalnız worktree adı biliniyor; keşfedilen ana klasörlerle eşleştirilir.
    ByName(String),
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_ascii_lowercase().find(needle)
}

fn folder_of(path: &str) -> String {
    path.replace('\\', "/")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// cwd'den yukarı doğru (ev dizinine kadar, ev hariç) en yakın git kökünü/worktree'yi arar.
/// Alt klasör cwd'leri böylece depo köküne iner; bağlı worktree'ler ana checkout'a,
/// submodule'ler kendi köküne eşlenir.
fn resolve_worktree(cwd: &str, home_key: &str) -> Target {
    let slashed = cwd.replace('\\', "/");
    for d in Path::new(cwd).ancestors().take(16) {
        let key = normalized_project_path(&d.to_string_lossy());
        if key == home_key || d.parent().is_none() {
            break;
        }
        let git = d.join(".git");
        if is_plain_dir(&git) {
            return Target::Path(d.to_string_lossy().into_owned());
        }
        if is_plain_file(&git) {
            let text = read_text(&git, None, GIT_FILE_MAX_BYTES).unwrap_or_default();
            // Submodule: gitdir `/modules/` altını gösterir; kendi başına bir projedir.
            if text.replace('\\', "/").contains("/modules/") {
                return Target::Path(d.to_string_lossy().into_owned());
            }
            return match linked_main(d) {
                Some(m) => Target::Path(m.to_string_lossy().into_owned()),
                None => Target::ByName(folder_of(&d.to_string_lossy())),
            };
        }
    }
    if let Some(i) = find_ci(&slashed, "/.claude/worktrees/") {
        return Target::Path(slashed[..i].to_string());
    }
    if let Some(i) = find_ci(&slashed, "/.codex/worktrees/") {
        let rest = &slashed[i + "/.codex/worktrees/".len()..];
        let mut parts = rest.split('/');
        let _id = parts.next();
        if let Some(name) = parts.next().filter(|n| !n.is_empty()) {
            return Target::ByName(name.to_string());
        }
    }
    Target::Direct
}

struct Raw {
    cwd: String,
    source: Source,
    file: SessionFile,
}

fn scan_sessions(home: &Path) -> Vec<Raw> {
    let mut raws = Vec::new();
    let mut push = |path: PathBuf, source: Source, force_worker: bool| {
        if !is_plain_file(&path) {
            return;
        }
        let info = peek(&path, source);
        if let Some(cwd) = info.cwd {
            let mtime_ms = mtime_ms(&path);
            raws.push(Raw {
                cwd,
                source,
                file: SessionFile {
                    path,
                    mtime_ms,
                    worker: info.subagent || force_worker,
                },
            });
        }
    };
    if let Ok(rd) = std::fs::read_dir(home.join(".claude").join("projects")) {
        for dir in rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        {
            let Ok(entries) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
                    push(path, Source::Claude, false);
                } else if ft.is_dir() {
                    // `subagents/` yalnız üst oturum dosyası yoksa (yinelenmiyorsa) işçi sayılır.
                    let parent = dir
                        .path()
                        .join(format!("{}.jsonl", e.file_name().to_string_lossy()));
                    if parent.is_file() {
                        continue;
                    }
                    if let Ok(subs) = std::fs::read_dir(path.join("subagents")) {
                        for f in subs.flatten() {
                            let fp = f.path();
                            if f.file_type().is_ok_and(|t| t.is_file())
                                && fp.extension().is_some_and(|x| x == "jsonl")
                            {
                                push(fp, Source::Claude, true);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut codex = Vec::new();
    collect_jsonl(&home.join(".codex").join("sessions"), 6, &mut codex);
    collect_jsonl(&home.join(".codex").join("archived_sessions"), 6, &mut codex);
    codex.sort();
    for path in codex {
        push(path, Source::Codex, false);
    }
    raws
}

/// Kanonik yolun anahtarı (var olmayan yol için `None`).
fn canonical_key(path: &str) -> Option<String> {
    let canon = std::fs::canonicalize(path).ok()?;
    Some(normalized_project_path(&clean_cwd(&canon.to_string_lossy())))
}

/// Home altındaki tüm kaynaklardan proje klasörlerini bulur (yola göre sıralı).
///
/// İşçi oturumları da proje geçmişidir ve sayılır. Worktree oturumları ana checkout'a eşlenir.
/// Kapsayıcı klasörler (başka projelerin atası olan, git kökü olmayan) proje sayılmaz.
/// Yalnız kullanıcı profili altındaki (kanonik yola göre) veya katalogda listelenen
/// klasörler kabul edilir.
pub fn discover(home: &Path, filters: &Filters) -> Discovery {
    let raws = scan_sessions(home);
    let home_key = normalized_project_path(&home.to_string_lossy());
    let canon_home = canonical_key(&home.to_string_lossy()).unwrap_or_else(|| home_key.clone());
    let mut unmatched = 0usize;

    // Katalog (sınırlı, sembolik bağlantısız okunur).
    let mut seed_catalog: Vec<(String, String, String)> = Vec::new();
    let claude_dir = home.join(".claude");
    if let Some(md) = read_text(
        &claude_dir.join("projects-catalog.md"),
        Some(&claude_dir),
        CATALOG_MAX_BYTES,
    ) {
        for seed in crate::projects::parse_catalog(&md) {
            let cwd = clean_cwd(&seed.path);
            if is_project_dir(&cwd, filters) {
                seed_catalog.push((cwd, seed.name, seed.description));
            }
        }
    }
    let catalog_keys: Vec<String> = seed_catalog
        .iter()
        .flat_map(|(c, _, _)| [Some(normalized_project_path(c)), canonical_key(c)])
        .flatten()
        .collect();

    // 1. her oturum için hedef.
    let resolved: Vec<(Raw, Target)> = raws
        .into_iter()
        .map(|raw| {
            let t = resolve_worktree(&raw.cwd, &home_key);
            (raw, t)
        })
        .collect();

    // 2. aday ana klasörler (anahtar -> görüntü yolu).
    let mut cand: BTreeMap<String, String> = BTreeMap::new();
    let allowed = |cwd: &str| {
        let key = normalized_project_path(cwd);
        let canon = canonical_key(cwd);
        canon
            .as_ref()
            .is_some_and(|c| c.starts_with(&format!("{canon_home}/")))
            || catalog_keys.contains(&key)
            || canon.is_some_and(|c| catalog_keys.contains(&c))
    };
    let consider = |cwd: &str, cand: &mut BTreeMap<String, String>| {
        if is_project_dir(cwd, filters) && allowed(cwd) {
            cand.entry(normalized_project_path(cwd))
                .or_insert_with(|| display_path(cwd));
        }
    };
    for (raw, t) in &resolved {
        match t {
            Target::Direct => consider(&raw.cwd, &mut cand),
            Target::Path(p) => consider(p, &mut cand),
            Target::ByName(_) => {}
        }
    }
    for (cwd, _, _) in &seed_catalog {
        consider(cwd, &mut cand);
    }

    // 3. kapsayıcılar: başka adayın atası olan ve git kökü olmayan klasörler.
    let keys: Vec<String> = cand.keys().cloned().collect();
    let containers: Vec<String> = keys
        .iter()
        .filter(|k| {
            keys.iter()
                .any(|o| o != *k && o.starts_with(&format!("{k}/")))
                && !Path::new(&cand[*k]).join(".git").exists()
        })
        .cloned()
        .collect();
    for c in &containers {
        cand.remove(c);
    }

    // 4. ad -> anahtarlar (ByName eşleşmesi için; yalnız tekil eşleşme kabul edilir).
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, disp) in &cand {
        by_name
            .entry(folder_of(disp).to_lowercase())
            .or_default()
            .push(k.clone());
    }

    // 5. atama.
    let mut out: BTreeMap<String, FoundProject> = BTreeMap::new();
    for (raw, t) in resolved {
        let key = match &t {
            Target::Direct => normalized_project_path(&raw.cwd),
            Target::Path(p) => normalized_project_path(p),
            Target::ByName(n) => match by_name.get(&n.to_lowercase()).map(Vec::as_slice) {
                Some([only]) => only.clone(),
                _ => {
                    unmatched += 1;
                    continue;
                }
            },
        };
        if key == home_key || containers.contains(&key) {
            unmatched += 1;
            continue;
        }
        let Some(disp) = cand.get(&key) else { continue };
        let entry = out.entry(key).or_insert_with(|| FoundProject {
            path: disp.clone(),
            ..FoundProject::default()
        });
        match raw.source {
            Source::Claude => entry.claude.push(raw.file),
            Source::Codex => entry.codex.push(raw.file),
        }
    }
    for (cwd, name, desc) in seed_catalog {
        let key = normalized_project_path(&cwd);
        if let Some(disp) = cand.get(&key) {
            out.entry(key)
                .or_insert_with(|| FoundProject {
                    path: disp.clone(),
                    ..FoundProject::default()
                })
                .catalog = Some((name, desc));
        }
    }
    Discovery {
        projects: out.into_values().collect(),
        unmatched_sessions: unmatched,
    }
}
