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
use crate::db::{Db, ImportCheckpoint, StoredEvent, auto_title};
use crate::ipc::ImportReport;

/// Tek satır üst sınırı; aşan satır atlanır (bellek koruması).
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
/// Araç özetindeki argüman uzunluğu (karakter).
const TOOL_ARG_CHARS: usize = 80;

/// Dosya başına işlenmiş bayt ofsetleri (kalıcı kopya `import_state` tablosundadır).
#[derive(Debug, Default)]
pub struct ImportState {
    checkpoints: HashMap<String, ImportCheckpoint>,
}

impl ImportState {
    /// Kalıcı ofsetleri yükler.
    pub fn load(db: &Db) -> Result<ImportState> {
        Ok(ImportState {
            checkpoints: db.import_checkpoints()?,
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
    crate::db::normalized_project_path(p)
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

// Süreç yeniden başlatıldığında değişmeyen 128 bit içerik parmak izi.
// İki bağımsız FNV-1a akışı; Rust'ın sürüme bağlı DefaultHasher'ı kullanılmaz.
fn fingerprint(parts: &[&[u8]]) -> String {
    let mut a = 0xcbf29ce484222325_u64;
    let mut b = 0x84222325cbf29ce4_u64;
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            a = (a ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            b = (b ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("{a:016x}{b:016x}")
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
    let mut checkpoint = match state.checkpoints.get(&key) {
        Some(saved) => saved.clone(),
        None => {
            // Ofset belleği sıfırlansa bile kalıcı nesli koru; kimlikler tekrar etmez.
            let mut saved = db.import_checkpoints()?.remove(&key).unwrap_or_default();
            saved.offset = 0;
            saved
        }
    };
    let mut reader = BufReader::new(File::open(file)?);
    let first_hash = read_line(&mut reader)?
        .filter(|(_, complete, _)| *complete)
        .map(|(line, _, _)| fingerprint(&[&line]));
    let rewritten = len < checkpoint.file_size
        || checkpoint
            .first_line_hash
            .as_ref()
            .zip(first_hash.as_ref())
            .is_some_and(|(old, new)| old != new);
    if rewritten {
        checkpoint.generation += 1;
        checkpoint.offset = 0;
        checkpoint.session_cwd = None;
    }
    checkpoint.file_size = len;
    checkpoint.first_line_hash = first_hash;
    reader.seek(SeekFrom::Start(checkpoint.offset))?;

    let stem = file
        .file_stem()
        .map(|s| sanitize_id(&s.to_string_lossy()))
        .unwrap_or_default();
    let chat_id = format!("claude-{stem}");
    let mut touched = false;
    let mut project = checkpoint.session_cwd.as_deref().and_then(|cwd| {
        projects
            .iter()
            .find(|(_, path)| path == cwd)
            .map(|(id, _)| id.clone())
    });

    while let Some((line, complete, consumed)) = read_line(&mut reader)? {
        if !complete {
            break; // yarım yazılmış son satır; sonraki çalıştırmada
        }
        let line_start = checkpoint.offset;
        checkpoint.offset += consumed;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            db.commit_import_record(&key, &checkpoint, None)?;
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            report.skipped += 1;
            db.commit_import_record(&key, &checkpoint, None)?;
            continue;
        };
        // cwd, mesaj olmayan metadata kayıtlarında da bulunabilir.
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            let cwd = norm_path(cwd);
            if let Some((id, _)) = projects.iter().find(|(_, path)| *path == cwd) {
                project = Some(id.clone());
                db.bind_chat_project(&chat_id, id)?;
            }
            checkpoint.session_cwd = Some(cwd);
        }
        let Some((kind, body, masked)) = extract_message(&v) else {
            db.commit_import_record(&key, &checkpoint, None)?;
            continue;
        };
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
        if db.get_chat(&chat_id)?.is_none() {
            let title = if kind == "user_text" {
                auto_title(&body)
            } else {
                String::new()
            };
            db.insert_chat(&chat_id, project.as_deref(), &title, ts)?;
            report.chats += 1;
        }
        if let Some(project) = &project {
            db.bind_chat_project(&chat_id, project)?;
        }
        let source_id = if let Some(uuid) = v
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            fingerprint(&[key.as_bytes(), b"uuid", uuid.as_bytes()])
        } else {
            fingerprint(&[
                key.as_bytes(),
                b"content",
                &checkpoint.generation.to_le_bytes(),
                &line_start.to_le_bytes(),
                text.as_bytes(),
            ])
        };
        let ev = StoredEvent {
            id: format!("imp-{source_id}"),
            chat_id: chat_id.clone(),
            session_id: None,
            seq: 0, // İşlem içinde tahsis edilir.
            ts,
            kind: kind.to_string(),
            payload: serde_json::json!({"text": body, "source": "claude-imported"}),
        };
        if db.commit_import_record(&key, &checkpoint, Some(&ev))? {
            report.messages += 1;
            report.masked += masked;
        }
    }

    db.commit_import_record(&key, &checkpoint, None)?;
    state.checkpoints.insert(key, checkpoint);
    Ok(())
}

/// Satırdan (olay türü, metin) çıkarır; metin içermeyen satırlar `None`.
fn extract_message(v: &Value) -> Option<(&'static str, String, usize)> {
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
    let mut masked = 0;
    match content {
        Value::String(s) => {
            let (text, count) = mask_secrets(s);
            parts.push(text);
            masked += count;
        }
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            let (text, count) = mask_secrets(t);
                            parts.push(text);
                            masked += count;
                        }
                    }
                    Some("tool_use") => {
                        let (summary, count) = tool_summary(b);
                        parts.push(summary);
                        masked += count;
                    }
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
        Some((kind, joined, masked))
    }
}

fn tool_summary(b: &Value) -> (String, usize) {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
    let (name, mut masked) = mask_secrets(name);
    let arg = b
        .get("input")
        .and_then(Value::as_object)
        .and_then(|o| o.values().find_map(Value::as_str))
        .map(|s| {
            let (s, count) = mask_secrets(s);
            masked += count;
            let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
            flat.chars().take(TOOL_ARG_CHARS).collect::<String>()
        })
        .unwrap_or_default();
    if arg.is_empty() {
        (format!("[araç: {name}]"), masked)
    } else {
        (format!("[araç: {name} {arg}]"), masked)
    }
}
