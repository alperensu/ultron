//! Gölge onay raporu: saf hesap ve Türkçe metin. Ham eylem, not veya model çıktısı içermez;
//! yalnız toplamlar. Puan yalnız sınıflandırma içindir, onay kararı değildir.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone, Utc};
use ultron_core::db::approval_shadow::ShadowAskRow;
use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
use ultron_core::memory::contracts::ShadowReport;

/// Yerel modelin "güvenli" dediği eşik. Yalnız rapor sınıflandırmasıdır.
pub const SAFE_THRESHOLD: f64 = 0.5;
pub const DEFAULT_WINDOW_DAYS: i64 = 7;

/// Europe/Istanbul: UTC+03:00, 2016'dan beri yaz saati uygulaması yok; sabit ofset yeterlidir.
pub fn display_offset() -> FixedOffset {
    FixedOffset::east_opt(3 * 3600).expect("sabit ofset geçerlidir")
}

/// Yarı açık pencere `[since, until)`, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowWindow {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

fn istanbul_midnight(date: NaiveDate) -> Result<DateTime<Utc>> {
    let local = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("geçersiz tarih"))?;
    let zoned = display_offset()
        .from_local_datetime(&local)
        .single()
        .ok_or_else(|| anyhow!("geçersiz tarih"))?;
    Ok(zoned.with_timezone(&Utc))
}

fn parse_date(text: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| anyhow!("tarih YYYY-AA-GG biçiminde olmalı"))
}

/// Varsayılan: son 7 gün. `from`/`to` `YYYY-AA-GG` olarak Europe/Istanbul gününe göre yorumlanır;
/// `to` o günü de dahil eder. Yalnız `from` verilirse bitiş şimdi, yalnız `to` verilirse başlangıç
/// bitişten 7 gün önce olur.
pub fn window_for(from: Option<&str>, to: Option<&str>, now: DateTime<Utc>) -> Result<ShadowWindow> {
    let until = match to {
        Some(text) => istanbul_midnight(parse_date(text)? + Duration::days(1))?,
        None => now,
    };
    let since = match from {
        Some(text) => istanbul_midnight(parse_date(text)?)?,
        None => until - Duration::days(DEFAULT_WINDOW_DAYS),
    };
    if since >= until {
        bail!("başlangıç bitişten önce olmalı");
    }
    Ok(ShadowWindow { since, until })
}

/// Pencere toplamları. Oranlar yalnız karşılaştırılabilir örnek varsa vardır.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowSummary {
    pub window: ShadowWindow,
    /// Pencerede oluşturulan `Ask` onayları.
    pub total_asks: u64,
    pub scored: u64,
    pub unscored: u64,
    pub pending: u64,
    pub timeout: u64,
    pub restart: u64,
    /// Masaüstü, Telegram veya onaylayıcı tarafından çözülen onaylar.
    pub human_decided: u64,
    /// Ölçülmüş ve insan kararı olan onaylar (karşılaştırılabilir örnek).
    pub resolved_scored: u64,
    pub agree: u64,
    pub disagree: u64,
    /// Model güvenli dedi (p >= eşik), insan reddetti.
    pub false_safe: u64,
    /// Model güvensiz dedi (p < eşik), insan onayladı.
    pub false_unsafe: u64,
    /// Ölçüm, onayın çözülme zamanından sonra alınanlar.
    pub late_scored: u64,
    pub mean_p_safe: Option<f64>,
    pub models: Vec<String>,
    pub prompt_versions: Vec<String>,
}

impl ShadowSummary {
    /// Uyum oranı `agree / resolved_scored`. Payda sıfırsa oran yoktur.
    pub fn agreement_ratio(&self) -> Option<f64> {
        if self.resolved_scored == 0 {
            None
        } else {
            Some(self.agree as f64 / self.resolved_scored as f64)
        }
    }

    /// Uyum yüzdesi, tamsayı tabanı: `agree * 100 / resolved_scored` (kayan nokta yok).
    pub fn agreement_percent_floor(&self) -> Option<u64> {
        (self.resolved_scored != 0).then(|| self.agree.saturating_mul(100) / self.resolved_scored)
    }

    /// Mevcut tel biçimi (`ShadowReport`). Bekleyen/zaman aşımı/yeniden başlatma alanları
    /// bu sözleşmede yoktur; yalnız rapor metninde gösterilir.
    pub fn to_wire(&self) -> ShadowReport {
        ShadowReport {
            since: self.window.since,
            total_asks: self.total_asks,
            scored: self.scored,
            resolved_scored: self.resolved_scored,
            agree: self.agree,
            disagree: self.disagree,
            false_safe: self.false_safe,
            false_unsafe: self.false_unsafe,
            mean_p_safe: self.mean_p_safe,
            models: self.models.clone(),
            prompt_versions: self.prompt_versions.clone(),
        }
    }
}

/// Saf hesap. Pencere dışındaki satırlar yok sayılır. Eylem metni girdide bulunmaz.
pub fn compute(window: ShadowWindow, rows: &[ShadowAskRow]) -> ShadowSummary {
    let mut summary = ShadowSummary {
        window,
        total_asks: 0,
        scored: 0,
        unscored: 0,
        pending: 0,
        timeout: 0,
        restart: 0,
        human_decided: 0,
        resolved_scored: 0,
        agree: 0,
        disagree: 0,
        false_safe: 0,
        false_unsafe: 0,
        late_scored: 0,
        mean_p_safe: None,
        models: Vec::new(),
        prompt_versions: Vec::new(),
    };
    let mut models = BTreeSet::new();
    let mut prompts = BTreeSet::new();
    let mut p_sum = 0.0_f64;

    for row in rows {
        if row.created_at < window.since || row.created_at >= window.until {
            continue;
        }
        summary.total_asks += 1;
        match row.status {
            ApprovalStatus::Pending => summary.pending += 1,
            _ => match row.resolved_by {
                Some(ResolvedBy::Timeout) => summary.timeout += 1,
                Some(ResolvedBy::Restart) => summary.restart += 1,
                _ => {}
            },
        }
        // İnsan kararı: yalnız gerçek kişi kaynakları ve açık onay/ret durumları.
        let human = match (row.resolved_by, row.status) {
            (Some(ResolvedBy::Desktop | ResolvedBy::Telegram | ResolvedBy::Approver), status) => {
                match status {
                    ApprovalStatus::Approved => Some(true),
                    ApprovalStatus::Denied => Some(false),
                    _ => None,
                }
            }
            _ => None,
        };
        if human.is_some() {
            summary.human_decided += 1;
        }
        let Some(score) = &row.score else {
            summary.unscored += 1;
            continue;
        };
        summary.scored += 1;
        p_sum += score.p_safe;
        models.insert(score.model.clone());
        prompts.insert(score.prompt_version.clone());
        if row.resolved_at.is_some_and(|resolved_at| score.measured_at > resolved_at) {
            summary.late_scored += 1;
        }
        if let Some(human_approved) = human {
            summary.resolved_scored += 1;
            let predicted_safe = score.p_safe >= SAFE_THRESHOLD;
            if predicted_safe == human_approved {
                summary.agree += 1;
            } else {
                summary.disagree += 1;
            }
            if predicted_safe && !human_approved {
                summary.false_safe += 1;
            }
            if !predicted_safe && human_approved {
                summary.false_unsafe += 1;
            }
        }
    }

    if summary.scored > 0 {
        summary.mean_p_safe = Some(p_sum / summary.scored as f64);
    }
    summary.models = models.into_iter().collect();
    summary.prompt_versions = prompts.into_iter().collect();
    summary
}

fn local_label(time: DateTime<Utc>) -> String {
    time.with_timezone(&display_offset())
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Türkçe rapor metni. Yalnız toplamlar ve etiketler yazılır.
pub fn format_text(summary: &ShadowSummary) -> String {
    let ratio = match summary.agreement_percent_floor() {
        // Taban: 199/200 uyum "%100" görünmesin (yanıltıcı yuvarlama yok).
        Some(p) => format!("%{p}"),
        None => "yok (karşılaştırılabilir örnek yok)".to_string(),
    };
    let mean = match summary.mean_p_safe {
        Some(m) => format!("{m:.2}"),
        None => "yok".to_string(),
    };
    let list = |items: &[String]| {
        if items.is_empty() {
            "yok".to_string()
        } else {
            items.join(", ")
        }
    };
    [
        "Gölge onay raporu (Europe/Istanbul, UTC+03:00)".to_string(),
        format!(
            "Dönem: {} — {} (yerel)",
            local_label(summary.window.since),
            local_label(summary.window.until)
        ),
        format!("Ask onayı: {}", summary.total_asks),
        format!(
            "  ölçülen: {} (ölçülmeyen: {}, geç ölçüm: {})",
            summary.scored, summary.unscored, summary.late_scored
        ),
        format!(
            "  bekleyen: {}, zaman aşımı: {}, yeniden başlatma: {}",
            summary.pending, summary.timeout, summary.restart
        ),
        format!(
            "  insan kararı: {} (karşılaştırılabilir örnek: {})",
            summary.human_decided, summary.resolved_scored
        ),
        format!("Uyum: {ratio}"),
        format!("Uyumsuz: {}", summary.disagree),
        format!(
            "Yanlış-güvenli (p >= {SAFE_THRESHOLD:.2}, insan reddetti): {}",
            summary.false_safe
        ),
        format!(
            "Yanlış-güvensiz (p < {SAFE_THRESHOLD:.2}, insan onayladı): {}",
            summary.false_unsafe
        ),
        format!("Ortalama P(güvenli): {mean}"),
        format!("Modeller: {}", list(&summary.models)),
        format!("Prompt sürümleri: {}", list(&summary.prompt_versions)),
        "Not: Bu rapor yalnız gözlemdir; puan onay kararını değiştirmez. İnsan kararı güvenliğin kanıtı değildir."
            .to_string(),
    ]
    .join("\n")
}
