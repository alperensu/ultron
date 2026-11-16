//! Codex kota: en yeni oturum JSONL'larındaki son `rate_limits` kaydı.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{QuotaSource, QuotaWindow};

const SCAN_LIMIT: usize = 40;
/// Tek satır üst sınırı; daha uzun satırlar atlanır.
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Tüm dosyalar için toplam okuma bütçesi.
const SCAN_BUDGET: u64 = 8 * 1024 * 1024;
/// Dosya başına yalnız kuyruktan bu kadar okunur.
const PER_FILE_TAIL: u64 = 4 * 1024 * 1024;

/// Bir satırı en fazla `MAX_LINE_BYTES` biriktirerek okur. `Ok(None)` = EOF;
/// `Some((bytes, true))` = satır sınırı aştı (içerik atılır). İkinci değer: okunan ham bayt.
fn read_line_capped<R: BufRead>(r: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<(bool, u64)>> {
    buf.clear();
    let mut consumed = 0u64;
    let mut overflow = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok((consumed > 0).then_some((overflow, consumed)));
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !overflow {
            if buf.len() + take > MAX_LINE_BYTES {
                overflow = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        consumed += take as u64;
        if done {
            return Ok(Some((overflow, consumed)));
        }
    }
}

fn scan_file(path: &Path, budget: &mut u64, best: &mut Option<(DateTime<Utc>, Value)>) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let window = len.min(PER_FILE_TAIL).min(*budget);
    if window == 0 {
        return;
    }
    let start = len - window;
    if f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    *budget -= window;
    let mut r = BufReader::new(f.take(window));
    let mut buf = Vec::new();
    let mut first = start > 0; // kuyruk ortasından başladık: ilk satır kırpık olabilir
    while let Ok(Some((overflow, _))) = read_line_capped(&mut r, &mut buf) {
        if std::mem::take(&mut first) || overflow {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(limits) = ev.pointer("/payload/rate_limits").filter(|l| l.is_object()) else {
            continue;
        };
        if let Some(id) = limits.get("limit_id").and_then(Value::as_str)
            && id != "codex"
        {
            continue;
        }
        let stamp = ev
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map_or(DateTime::<Utc>::MIN_UTC, |d| d.with_timezone(&Utc));
        if best.as_ref().is_none_or(|(s, _)| stamp >= *s) {
            *best = Some((stamp, limits.clone()));
        }
    }
}

/// `resets_in_seconds` değerini taşma/panik olmadan mutlak zamana çevirir.
fn checked_reset(stamp: DateTime<Utc>, secs: f64) -> Option<DateTime<Utc>> {
    if !secs.is_finite() || secs.abs() > i64::MAX as f64 {
        return None;
    }
    let d = chrono::Duration::try_seconds(secs as i64)?;
    stamp.checked_add_signed(d)
}

fn collect(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>, depth: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() && depth < 6 {
            collect(&p, out, depth + 1);
        } else if ft.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
            let m = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((m, p));
        }
    }
}

/// `sessions` kökünden pencereleri okur; kaynak yoksa boş döner (Unknown).
pub fn read_codex_windows(sessions_root: &Path, now: DateTime<Utc>) -> Vec<QuotaWindow> {
    let mut files = Vec::new();
    collect(sessions_root, &mut files, 0);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut best: Option<(DateTime<Utc>, Value)> = None;
    let mut budget = SCAN_BUDGET;
    for (_, path) in files.into_iter().take(SCAN_LIMIT) {
        scan_file(&path, &mut budget, &mut best);
    }
    let Some((stamp, limits)) = best else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, label, default_secs) in [
        ("primary", "5 saat", 5 * 3600u64),
        ("secondary", "7 gün", 7 * 24 * 3600u64),
    ] {
        let Some(w) = limits.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let secs = w
            .get("window_minutes")
            .and_then(Value::as_f64)
            .filter(|m| *m > 0.0)
            .map_or(default_secs, |m| (m * 60.0) as u64);
        let mut invalid = false;
        let resets_at = w
            .get("resets_at")
            .and_then(Value::as_f64)
            .and_then(|t| {
                if t.is_finite() && t.abs() < 1e12 {
                    Utc.timestamp_opt(t as i64, 0).single()
                } else {
                    None
                }
            })
            .or_else(|| {
                let secs = w.get("resets_in_seconds").and_then(Value::as_f64)?;
                if stamp <= DateTime::<Utc>::MIN_UTC {
                    return None;
                }
                let r = checked_reset(stamp, secs);
                invalid = r.is_none();
                r
            });
        let used = w.get("used_percent").and_then(Value::as_f64);
        let expired = resets_at.is_some_and(|r| r <= now);
        let (used_pct, resets_at, source) = if expired || invalid || used.is_none() {
            (None, None, QuotaSource::Unknown)
        } else {
            (
                used.map(|u| u.clamp(0.0, 100.0) as f32),
                resets_at,
                QuotaSource::Estimated, // son oturum ölçümü; canlı değil
            )
        };
        out.push(QuotaWindow {
            label: label.to_string(),
            used_pct,
            resets_at,
            window_secs: secs,
            source,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, lines: &[String]) {
        std::fs::write(dir.join("s.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn absurd_reset_seconds_yields_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        for secs in ["1e300", "9.3e18", "-9.3e18", "1e15"] {
            write(
                tmp.path(),
                &[format!(
                    r#"{{"timestamp":"2026-01-01T00:00:00Z","payload":{{"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":10.0,"resets_in_seconds":{secs}}}}}}}}}"#
                )],
            );
            let w = read_codex_windows(tmp.path(), Utc::now());
            assert_eq!(w.len(), 1, "{secs}");
            assert_eq!(w[0].source, QuotaSource::Unknown, "{secs}");
            assert!(w[0].resets_at.is_none());
        }
    }

    #[test]
    fn oversized_session_line_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let good = r#"{"timestamp":"2099-01-01T00:00:00Z","payload":{"rate_limits":{"limit_id":"codex","primary":{"used_percent":33.0,"window_minutes":300,"resets_at":4102444800}}}}"#;
        // Sınırı aşan, "rate_limits" içeren dev satır; geçerli kayıt öncesinde ve sonrasında.
        let huge = format!(
            r#"{{"timestamp":"2099-02-01T00:00:00Z","pad":"{}","payload":{{"rate_limits":{{"primary":{{"used_percent":99.0}}}}}}}}"#,
            "x".repeat(MAX_LINE_BYTES + 10)
        );
        write(tmp.path(), &[good.to_string(), huge]);
        let w = read_codex_windows(tmp.path(), Utc::now());
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].used_pct, Some(33.0));
    }
}
