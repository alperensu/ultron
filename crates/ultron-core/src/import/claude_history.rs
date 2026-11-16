//! `~/.claude/projects/**/*.jsonl` oturum dosyalarını sohbetlere dönüştürür.
//!
//! Yalnız kullanıcı/asistan metni alınır; `thinking` atlanır, `tool_use` tek satıra
//! indirgenir, `tool_result` alınmaz. Artımlı: dosya başına işlenen bayt ofseti saklanır.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::secrets::mask_secrets;
use crate::db::{Db, StoredEvent, auto_title};
use crate::ipc::ImportReport;

/// Tek satır üst sınırı; aşan satır atlanır (bellek koruması).
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
/// Araç özetindeki argüman uzunluğu (karakter).
const TOOL_ARG_CHARS: usize = 80;

/// Dosya başına işlenmiş bayt ofsetleri (kalıcı kopya `import_state` tablosundadır).
#[derive(Debug, Default)]
pub struct ImportState {
    offsets: HashMap<String, u64>,
}

impl ImportState {
    /// Kalıcı ofsetleri yükler.
    pub fn load(db: &Db) -> Result<ImportState> {
        Ok(ImportState {
            offsets: db.import_offsets()?,
        })
    }
}

/// `root` altındaki tüm `.jsonl` dosyalarını içe aktarır.
pub fn import_claude_history(
    db: &Db,
    root: &Path,
    state: &mut ImportState,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let projects: Vec<(String, String)> = db
        .list_projects()?
        .into_iter()
        .map(|p| (p.id, norm_path(&p.path)))
        .collect();
    let mut files = Vec::new();
    collect_jsonl(root, &mut files);
    files.sort();
    for file in files {
        import_file(db, &file, &projects, state, &mut report)?;
    }
    Ok(report)
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        // Sembolik bağlantılar izlenmez.
        if ft.is_dir() {
            collect_jsonl(&path, out);
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Bir satırı okur. `None`: dosya sonu. `Some((bytes, tam_mı))`; aşırı uzun satır
/// sonuna kadar tüketilir ve boş (atlanacak) döner.
fn read_line(r: &mut BufReader<File>) -> Result<Option<(Vec<u8>, bool, u64)>> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE_BYTES)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let mut consumed = n as u64;
    let mut complete = buf.ends_with(b"\n");
    if !complete && n as u64 == MAX_LINE_BYTES {
        // Aşırı uzun satır: sonuna kadar at.
        let mut sink = Vec::new();
        loop {
            sink.clear();
            let m = r
                .by_ref()
                .take(MAX_LINE_BYTES)
                .read_until(b'\n', &mut sink)?;
            consumed += m as u64;
            if m == 0 {
                break;
            }
            if sink.ends_with(b"\n") {
                complete = true;
                break;
            }
        }
        buf.clear();
        buf.extend_from_slice(b"\0TOOLONG");
    }
    Ok(Some((buf, complete, consumed)))
}

fn import_file(
    db: &Db,
    file: &Path,
    projects: &[(String, String)],
    state: &mut ImportState,
    report: &mut ImportReport,
) -> Result<()> {
    let key = file.to_string_lossy().to_string();
    let len = std::fs::metadata(file)?.len();
    let mut offset = state.offsets.get(&key).copied().unwrap_or(0);
    if offset > len {
        offset = 0; // dosya kısalmış/yeniden yazılmış
    }
    if offset == len {
        return Ok(());
    }
    let mut f = File::open(file)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(f);

    let stem = file
        .file_stem()
        .map(|s| sanitize_id(&s.to_string_lossy()))
        .unwrap_or_default();
    let chat_id = format!("claude-{stem}");
    let mut seq: Option<i64> = None;
    let mut touched = false;
    let mut last_ts: Option<DateTime<Utc>> = None;

    while let Some((line, complete, consumed)) = read_line(&mut reader)? {
        if !complete {
            break; // yarım yazılmış son satır; sonraki çalıştırmada
        }
        let line_start = offset;
        offset += consumed;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            report.skipped += 1;
            continue;
        };
        let Some((kind, body)) = extract_message(&v) else {
            continue;
        };
        let (body, masked) = mask_secrets(&body);
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        if !touched {
            touched = true;
            report.files += 1;
        }
        if seq.is_none() {
            if db.get_chat(&chat_id)?.is_none() {
                let cwd = v.get("cwd").and_then(Value::as_str).map(norm_path);
                let project = cwd.and_then(|c| {
                    projects
                        .iter()
                        .find(|(_, p)| *p == c)
                        .map(|(id, _)| id.as_str())
                });
                let title = if kind == "user_text" {
                    auto_title(&body)
                } else {
                    String::new()
                };
                db.insert_chat(&chat_id, project, &title, ts)?;
                report.chats += 1;
            }
            seq = Some(db.next_seq(&chat_id)?);
        }
        let n = seq.unwrap_or(1);
        let ev = StoredEvent {
            id: format!("imp-{stem}-{line_start}"),
            chat_id: chat_id.clone(),
            session_id: None,
            seq: n,
            ts,
            kind: kind.to_string(),
            payload: serde_json::json!({"text": body, "source": "claude-imported"}),
        };
        if db.insert_event(&ev)? {
            seq = Some(n + 1);
            report.messages += 1;
            report.masked += masked;
            last_ts = Some(ts);
        }
    }

    if let Some(ts) = last_ts {
        db.set_chat_updated_at(&chat_id, ts)?;
    }
    db.set_import_offset(&key, offset)?;
    state.offsets.insert(key, offset);
    Ok(())
}

/// Satırdan (olay türü, metin) çıkarır; metin içermeyen satırlar `None`.
fn extract_message(v: &Value) -> Option<(&'static str, String)> {
    let kind = match v.get("type").and_then(Value::as_str)? {
        "user" => "user_text",
        "assistant" => "assistant_text",
        _ => return None,
    };
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let mut parts: Vec<String> = Vec::new();
    match content {
        Value::String(s) => parts.push(s.clone()),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => parts.push(tool_summary(b)),
                    // thinking, tool_result, görseller: alınmaz
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    let joined = parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some((kind, joined))
    }
}

fn tool_summary(b: &Value) -> String {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
    let arg = b
        .get("input")
        .and_then(Value::as_object)
        .and_then(|o| o.values().find_map(Value::as_str))
        .map(|s| {
            let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
            flat.chars().take(TOOL_ARG_CHARS).collect::<String>()
        })
        .unwrap_or_default();
    if arg.is_empty() {
        format!("[araç: {name}]")
    } else {
        format!("[araç: {name} {arg}]")
    }
}
