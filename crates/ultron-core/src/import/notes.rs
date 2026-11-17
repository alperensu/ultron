//! Proje başına "Şimdiye kadar yapılanlar" notu.
//!
//! Girdi: projenin geçmiş Claude/Codex oturumlarının yalnız kullanıcı/asistan metni (araç
//! çıktısı yok), proje `AGENTS.md`/`CLAUDE.md` dosyaları ve Claude otomatik hafıza notları.
//! Her şey özetlemeden ÖNCE `mask_secrets` ile maskelenir. Girdi projeye göre sınırlıdır
//! (en yeni mesajlar). Özetleme mevcut [`Summarizer`] ile yapılır.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::discovery::{
    SUBAGENT_PREFIX, SessionFile, Source, bounded_line, content_text, is_injected, mtime_ms,
};
use super::safe_fs::{DOC_MAX_BYTES, is_plain_dir, is_plain_file, read_text};
use super::secrets::mask_secrets;
use crate::db::StoredEvent;
use crate::summarizer::Summarizer;
use crate::workstate::{WorkState, clip};

/// Projeye göre özetleyiciye giren toplam mesaj metni üst sınırı (karakter).
const MAX_INPUT_CHARS: usize = 160 * 1024;
/// Tek mesajın üst sınırı (karakter).
const MESSAGE_CHARS: usize = 1500;
/// Bir oturum dosyasından tutulan en yeni mesaj sayısı.
const PER_FILE_MESSAGES: usize = 80;
/// Çok büyük dosyalarda yalnız son bu kadar bayt okunur.
const MAX_FILE_TAIL_BYTES: u64 = 32 * 1024 * 1024;
/// Özetleyiciye en fazla bu kadar çağrı yapılır (çağrı başına 16 mesaj).
const MAX_BATCHES: usize = 4;
const BATCH_MESSAGES: usize = 16;
/// Hafıza/belge kırpmaları.
const MEMORY_FILES: usize = 12;
const DOC_CHARS: usize = 1500;
const MEMORY_CHARS: usize = 1200;
/// İstemde (prompt) tutulan not üst sınırı; agy komut satırı sınırına yer bırakır.
pub const NOTE_PROMPT_CHARS: usize = 6000;

/// Bir konuşma mesajı.
#[derive(Debug, Clone)]
pub struct Msg {
    pub ts: i64,
    pub user: bool,
    pub text: String,
    /// İşçi oturumundan özet (görev + son rapor); asistan metni gibi işlenir.
    pub worker: bool,
}

/// Bir proje için toplanmış (maskelenmiş) kaynaklar.
#[derive(Debug, Default)]
pub struct Collected {
    pub messages: Vec<Msg>,
    /// (dosya adı, metin): AGENTS.md / CLAUDE.md.
    pub docs: Vec<(String, String)>,
    /// (dosya adı, metin): Claude hafıza notları.
    pub memory: Vec<(String, String)>,
    pub masked: usize,
}

/// Çıkan not.
#[derive(Debug)]
pub struct NoteBuild {
    pub markdown: String,
}

/// Claude'un cwd -> klasör adı kodlaması: alfasayısal olmayan her karakter `-` olur.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn memory_dir(home: &Path, project_path: &str) -> PathBuf {
    home.join(".claude")
        .join("projects")
        .join(encode_cwd(project_path))
        .join("memory")
}

fn doc_paths(project_path: &str) -> Vec<PathBuf> {
    ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .map(|n| Path::new(project_path).join(n))
        .collect()
}

fn memory_files(home: &Path, project_path: &str) -> Vec<PathBuf> {
    let dir = memory_dir(home, project_path);
    if !is_plain_dir(&dir) {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_plain_file(p) && p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files.truncate(MEMORY_FILES);
    files
}

/// Kaynakların en yeni değişiklik zamanı (ms); yalnız üst veriye bakar. Artımlılık damgası.
pub fn source_stamp(
    home: &Path,
    project_path: &str,
    claude: &[SessionFile],
    codex: &[SessionFile],
) -> i64 {
    let sessions = claude.iter().chain(codex).map(|f| f.mtime_ms);
    let files = doc_paths(project_path)
        .into_iter()
        .chain(memory_files(home, project_path))
        .filter(|p| is_plain_file(p))
        .map(|p| mtime_ms(&p));
    sessions.chain(files).max().unwrap_or(0)
}

/// Not üretmeye yetecek kaynak var mı?
pub fn has_sources(
    home: &Path,
    project_path: &str,
    claude: &[SessionFile],
    codex: &[SessionFile],
) -> bool {
    !claude.is_empty()
        || !codex.is_empty()
        || doc_paths(project_path).iter().any(|p| is_plain_file(p))
        || !memory_files(home, project_path).is_empty()
}

fn parse_ts(v: &Value, fallback: i64) -> i64 {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or(fallback)
}

/// İşçi mesajlarının toplam girdideki payı (ordinary konuşmalar baskın kalır).
const WORKER_BUDGET_CHARS: usize = MAX_INPUT_CHARS / 4;
const WORKER_SESSIONS_MAX: usize = 40;

/// Bir oturum dosyasından yalnız kullanıcı/asistan metnini (en yeni `PER_FILE_MESSAGES`) okur.
fn session_messages(file: &SessionFile, source: Source) -> Vec<Msg> {
    let mut keep: VecDeque<Msg> = VecDeque::new();
    read_messages(file, source, &mut |m| {
        if keep.len() == PER_FILE_MESSAGES {
            keep.pop_front();
        }
        keep.push_back(m);
    });
    keep.into()
}

/// Görev başlığındaki standart "You are a scoped worker..." paragrafını atar, kalanı kısaltır.
fn brief_text(brief: &str) -> String {
    let brief = brief.trim();
    let (first, rest) = match brief.split_once("\n\n") {
        Some((a, b)) => (a, b),
        None => (brief, ""),
    };
    let body = if first.trim_start().starts_with(SUBAGENT_PREFIX) && !rest.trim().is_empty() {
        rest
    } else {
        first
    };
    clip(&body.split_whitespace().collect::<Vec<_>>().join(" "), 300)
}

/// İşçi oturumundan tek mesaj: kısaltılmış görev + son asistan raporu.
fn worker_message(file: &SessionFile, source: Source) -> Option<Msg> {
    let mut first_user: Option<String> = None;
    let mut last_assistant: Option<Msg> = None;
    read_messages(file, source, &mut |m| {
        if m.user {
            if first_user.is_none() {
                first_user = Some(m.text);
            }
        } else {
            last_assistant = Some(m);
        }
    });
    let ts = last_assistant.as_ref().map_or(file.mtime_ms, |m| m.ts);
    // Önce maskele, sonra kısalt: kesilen bir sır parçası maskeyi atlatmasın.
    let brief = first_user
        .as_deref()
        .map(|t| brief_text(&mask_secrets(t).0))
        .unwrap_or_default();
    let report = last_assistant
        .map(|m| clip(&mask_secrets(&m.text).0, 1000))
        .unwrap_or_default();
    if brief.is_empty() && report.is_empty() {
        return None;
    }
    Some(Msg {
        ts,
        user: false,
        text: format!("[işçi] Görev: {brief} | Rapor: {report}"),
        worker: true,
    })
}

fn read_messages(file: &SessionFile, source: Source, on_msg: &mut dyn FnMut(Msg)) {
    if !is_plain_file(&file.path) {
        return;
    }
    let Ok(mut f) = File::open(&file.path) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut skip_first = false;
    if len > MAX_FILE_TAIL_BYTES && f.seek(SeekFrom::Start(len - MAX_FILE_TAIL_BYTES)).is_ok() {
        skip_first = true;
    }
    let mut reader = BufReader::new(f);
    if skip_first {
        let _ = bounded_line(&mut reader);
    }
    while let Some(line) = bounded_line(&mut reader) {
        let Some(v) = line.and_then(|l| serde_json::from_str::<Value>(&l).ok()) else {
            continue;
        };
        let (role, content) = match source {
            Source::Claude => {
                let role = v.get("type").and_then(Value::as_str);
                if !matches!(role, Some("user" | "assistant"))
                    || v.get("isMeta").and_then(Value::as_bool) == Some(true)
                    || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                (role, v.get("message").and_then(|m| m.get("content")))
            }
            Source::Codex => {
                if v.get("type").and_then(Value::as_str) != Some("response_item") {
                    continue;
                }
                let Some(p) = v.get("payload") else { continue };
                if p.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                (p.get("role").and_then(Value::as_str), p.get("content"))
            }
        };
        let Some(content) = content else { continue };
        let user = match role {
            Some("user") => true,
            Some("assistant") => false,
            _ => continue,
        };
        let text = content_text(content);
        let text = text.trim();
        if text.is_empty() || (user && is_injected(text)) {
            continue;
        }
        on_msg(Msg {
            ts: parse_ts(&v, file.mtime_ms),
            user,
            text: text.to_string(),
            worker: false,
        });
    }
}

/// Kaynakları toplar, maskeler ve sınırlar.
pub fn collect(
    home: &Path,
    project_path: &str,
    claude: &[SessionFile],
    codex: &[SessionFile],
) -> Collected {
    let mut out = Collected::default();
    let mask = |s: &str, max: usize, masked: &mut usize| {
        let (m, n) = mask_secrets(s);
        *masked += n;
        clip(m.trim(), max)
    };

    // En yeni oturumlar önce; toplam girdi sınırına kadar.
    let mut files: Vec<(&SessionFile, Source)> = claude
        .iter()
        .map(|f| (f, Source::Claude))
        .chain(codex.iter().map(|f| (f, Source::Codex)))
        .collect();
    files.sort_by_key(|(f, _)| std::cmp::Reverse(f.mtime_ms));
    let mut all = Vec::new();
    let mut workers = Vec::new();
    for (file, source) in files {
        if file.worker {
            if workers.len() < WORKER_SESSIONS_MAX {
                workers.extend(worker_message(file, source));
            }
        } else if all.len() <= MAX_BATCHES * BATCH_MESSAGES * 4 {
            all.extend(session_messages(file, source));
        }
    }
    // Normal konuşmalar ve işçi özetleri ayrı bütçelerle sınırlanır; en yeniler önce.
    let take = |mut list: Vec<Msg>, mut budget: usize, out: &mut Collected| {
        list.sort_by_key(|m| m.ts);
        let mut newest_first = Vec::new();
        for m in list.into_iter().rev() {
            let text = mask(&m.text, MESSAGE_CHARS, &mut out.masked);
            let n = text.chars().count();
            if n > budget {
                break;
            }
            budget -= n;
            newest_first.push(Msg { text, ..m });
        }
        newest_first
    };
    let mut kept = take(all, MAX_INPUT_CHARS - WORKER_BUDGET_CHARS, &mut out);
    kept.extend(take(workers, WORKER_BUDGET_CHARS, &mut out));
    kept.sort_by_key(|m| m.ts);
    out.messages = kept;

    for path in doc_paths(project_path) {
        if let Some(text) = read_text(&path, Some(Path::new(project_path)), DOC_MAX_BYTES) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            out.docs.push((name, mask(&text, DOC_CHARS, &mut out.masked)));
        }
    }
    let mem_dir = memory_dir(home, project_path);
    for path in memory_files(home, project_path) {
        if let Some(text) = read_text(&path, Some(&mem_dir), DOC_MAX_BYTES) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            out.memory.push((name, mask(&text, MEMORY_CHARS, &mut out.masked)));
        }
    }
    out
}

fn event(i: usize, user: bool, ts: i64, text: String) -> StoredEvent {
    let kind = if user { "user_text" } else { "assistant_text" };
    StoredEvent {
        id: format!("note-{i}"),
        chat_id: "project-note".into(),
        session_id: None,
        seq: i as i64,
        ts: Utc.timestamp_millis_opt(ts).single().unwrap_or_else(Utc::now),
        kind: kind.into(),
        payload: serde_json::json!({ "type": kind, "text": text }),
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty() && *l != "---")
        .unwrap_or("");
    clip(line, max)
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "- (belirtilmedi)\n".into()
    } else {
        items.iter().map(|i| format!("- {}\n", i.trim())).collect()
    }
}

/// Önceki notun yapılandırılmış içeriği (kaybolmaması gereken geçmiş).
#[derive(Debug, Default, Clone)]
pub struct PrevNote {
    pub state: WorkState,
    pub excerpts: Vec<String>,
}

/// Tek satırlık, denetim karakterlerinden arındırılmış, maskelenmiş ve kısaltılmış alan.
/// Başlık/alıntı işaretiyle başlayan satırlar etkisizleştirilir.
pub fn one_line(text: &str, max: usize) -> String {
    let (masked, _) = mask_secrets(text);
    let flat = masked
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let flat = flat.trim_start_matches(['#', '>', '`', '<', '-', '*']).trim_start();
    clip(flat, max)
}

/// Daha önce üretilmiş notu (kendi biçimimiz) yapılandırılmış duruma çevirir.
pub fn parse_note(md: &str) -> PrevNote {
    let mut prev = PrevNote::default();
    let mut section = "";
    for line in md.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("## ") {
            section = match h.trim() {
                "Hedef" => "goal",
                "Tamamlananlar" => "done",
                "Sonraki adım" => "next",
                "Açık sorular" => "questions",
                "Son konuşmalardan alıntılar" => "excerpts",
                _ => "",
            };
            continue;
        }
        if t.is_empty() || t == "(belirtilmedi)" || t == "- (belirtilmedi)" {
            continue;
        }
        let item = t.strip_prefix("- ").map(|s| one_line(s, 300));
        match (section, item) {
            ("goal", _) if prev.state.goal.is_none() => prev.state.goal = Some(one_line(t, 400)),
            ("next", _) if prev.state.next_step.is_none() => {
                prev.state.next_step = Some(one_line(t, 300));
            }
            ("done", Some(i)) if !i.is_empty() => prev.state.done.push(i),
            ("questions", Some(i)) if !i.is_empty() => prev.state.open_questions.push(i),
            ("excerpts", Some(i)) if !i.is_empty() => prev.excerpts.push(i),
            _ => {}
        }
    }
    prev
}

fn union_keep_order(mut base: Vec<String>, extra: Vec<String>, cap: usize) -> Vec<String> {
    for e in extra {
        if !base.contains(&e) {
            base.push(e);
        }
    }
    let skip = base.len().saturating_sub(cap);
    base.split_off(skip)
}

/// Not üretiminin ek bağlamı.
pub struct NoteCtx<'a> {
    pub claude_sessions: usize,
    pub codex_sessions: usize,
    pub worker_sessions: usize,
    /// Önceki not (varsa): özetleyiciye başlangıç durumu olarak verilir, bilgi kaybolmaz.
    pub prev: Option<&'a PrevNote>,
    /// Oturum sayısı azaldı (eski dökümler silinmiş): eski hedef/alıntılar korunur.
    pub shrunk: bool,
    pub now: DateTime<Utc>,
}

/// Notu üretir. Özetleyici hata verirse `Err` döner (not yazılmaz, sonraki çalıştırmada
/// yeniden denenir); `null` özetleyicide yalnız ham alıntılı not üretilir. Çıktı yeniden
/// maskelenir.
pub async fn build_note(
    summarizer: &dyn Summarizer,
    project_name: &str,
    data: &Collected,
    ctx: NoteCtx<'_>,
) -> Result<NoteBuild> {
    let NoteCtx {
        claude_sessions,
        codex_sessions,
        worker_sessions,
        prev,
        shrunk,
        now,
    } = ctx;
    let kind = summarizer.kind();
    let prev_state = prev.map(|p| p.state.clone()).unwrap_or_default();
    let mut state = prev_state.clone();
    if kind != "null" && (!data.messages.is_empty() || !data.docs.is_empty()) {
        // Belgeler ilk gruba bağlam olarak girer; mesajlar eskiden yeniye gruplanır.
        let context: Vec<(bool, String)> = data
            .docs
            .iter()
            .map(|(n, t)| (true, format!("[{n}] {}", clip(t, 450))))
            .chain(
                data.memory
                    .iter()
                    .take(2)
                    .map(|(n, t)| (true, format!("[hafıza {n}] {}", clip(t, 450)))),
            )
            .take(4)
            .collect();
        let start = data
            .messages
            .len()
            .saturating_sub(MAX_BATCHES * BATCH_MESSAGES);
        let recent = &data.messages[start..];
        let mut batches: Vec<Vec<(bool, i64, String)>> = recent
            .chunks(BATCH_MESSAGES)
            .map(|c| c.iter().map(|m| (m.user, m.ts, m.text.clone())).collect())
            .collect();
        if batches.is_empty() {
            batches.push(Vec::new());
        }
        for (bi, batch) in batches.into_iter().enumerate() {
            let mut events = Vec::new();
            if bi == 0 {
                for (user, text) in &context {
                    events.push(event(events.len(), *user, now.timestamp_millis(), text.clone()));
                }
            }
            for (user, ts, text) in batch {
                events.push(event(events.len(), user, ts, text));
            }
            let patch = summarizer.refresh(&state, &events).await?;
            state.apply_summary(patch);
        }
        // Tamamlananlar yalnız genişler; eski bilgi kaybolmaz.
        state.done = union_keep_order(prev_state.done.clone(), state.done.clone(), 40);
        if shrunk && prev_state.goal.is_some() {
            state.goal = prev_state.goal.clone();
        }
    }

    let date = now.format("%Y-%m-%d").to_string();
    let yes = |b: bool| if b { "var" } else { "yok" };
    let has_doc = |n: &str| data.docs.iter().any(|(d, _)| d == n);
    let name = one_line(project_name, 80);
    let mut md = format!(
        "# Şimdiye kadar yapılanlar: {name}\n\n\
         > Kaynaklar: {claude_sessions} Claude oturumu, {codex_sessions} Codex oturumu, \
         {worker_sessions} işçi oturumu, AGENTS.md {}, CLAUDE.md {}, {} hafıza notu. Son güncelleme: {date}. Özetleyici: {kind}.\n\
         > Otomatik üretildi; gizli bilgi desenleri maskelendi. Güncel durum için depoyu doğrula.\n\n",
        yes(has_doc("AGENTS.md")),
        yes(has_doc("CLAUDE.md")),
        data.memory.len(),
    );
    let items = |v: &[String], n: usize| -> String {
        let v: Vec<String> = v.iter().take(n).map(|i| one_line(i, 300)).collect();
        list(&v)
    };
    let has_structure = kind != "null"
        || state.goal.is_some()
        || !state.done.is_empty()
        || state.next_step.is_some();
    if has_structure {
        md.push_str(&format!(
            "## Hedef\n{}\n\n## Tamamlananlar\n{}\n## Sonraki adım\n{}\n\n## Açık sorular\n{}\n",
            state.goal.as_deref().map_or("(belirtilmedi)".into(), |g| one_line(g, 400)),
            items(&state.done, 40),
            state.next_step.as_deref().map_or("(belirtilmedi)".into(), |n| one_line(n, 300)),
            items(&state.open_questions, 12),
        ));
    } else {
        md.push_str("_Özetleyici yapılandırılmadı; aşağıda yalnız ham alıntılar var._\n");
    }
    md.push_str("\n## Son konuşmalardan alıntılar\n");
    let users: Vec<&Msg> = data.messages.iter().filter(|m| m.user).collect();
    let mut excerpts: Vec<String> = users
        .iter()
        .rev()
        .take(6)
        .rev()
        .map(|m| {
            let day = Utc
                .timestamp_millis_opt(m.ts)
                .single()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            format!("[{day}] {}", one_line(&m.text, 240))
        })
        .collect();
    if shrunk && let Some(p) = prev {
        // Silinmiş eski dökümlerin alıntıları kaybolmasın.
        for old in &p.excerpts {
            if !excerpts.contains(old) && excerpts.len() < 10 {
                excerpts.push(old.clone());
            }
        }
    }
    if excerpts.is_empty() {
        md.push_str("- (oturum metni yok)\n");
    }
    for e in &excerpts {
        md.push_str(&format!("- {e}\n"));
    }
    if !data.memory.is_empty() {
        md.push_str("\n## Hafıza notları\n");
        for (n, text) in &data.memory {
            md.push_str(&format!("- **{}**: {}\n", one_line(n, 80), first_line(text, 300)));
        }
    }
    // Son savunma: üretilen notun tamamı yeniden maskelenir.
    let (md, _) = mask_secrets(&md);
    Ok(NoteBuild { markdown: md })
}
