//! Codex kota: en yeni oturum JSONL'larındaki son `rate_limits` kaydı.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{QuotaSource, QuotaWindow};

const SCAN_LIMIT: usize = 40;

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
    for (_, path) in files.into_iter().take(SCAN_LIMIT) {
        let Ok(f) = std::fs::File::open(&path) else {
            continue;
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if !line.contains("\"rate_limits\"") {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
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
                best = Some((stamp, limits.clone()));
            }
        }
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
        let resets_at = w
            .get("resets_at")
            .and_then(Value::as_f64)
            .and_then(|t| Utc.timestamp_opt(t as i64, 0).single())
            .or_else(|| {
                w.get("resets_in_seconds")
                    .and_then(Value::as_f64)
                    .filter(|_| stamp > DateTime::<Utc>::MIN_UTC)
                    .map(|s| stamp + chrono::Duration::seconds(s as i64))
            });
        let used = w.get("used_percent").and_then(Value::as_f64);
        let expired = resets_at.is_some_and(|r| r <= now);
        let (used_pct, resets_at, source) = if expired || used.is_none() {
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
