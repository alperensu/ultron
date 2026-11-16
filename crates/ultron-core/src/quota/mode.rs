//! Bütçe modu: pencerelerin harcama hızından Tam/Dengeli/Eko kararı.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::QuotaWindow;

/// Bütçe modu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetMode {
    Tam,
    Dengeli,
    Eko,
}

/// Her pencere için `pace = (1 - used/100) / (kalan_süre / pencere_süresi)`; en kötü pencere belirler.
/// pace >= 1.0 → Tam; 0.5 <= pace < 1.0 → Dengeli; pace < 0.5 ya da used >= 90 → Eko.
/// Ölçülebilir pencere yoksa Dengeli.
pub fn budget_mode(windows: &[QuotaWindow], now: DateTime<Utc>) -> BudgetMode {
    let mut worst: Option<f64> = None;
    for w in windows {
        let (Some(used), Some(resets_at)) = (w.used_pct, w.resets_at) else {
            continue;
        };
        if w.window_secs == 0 {
            continue;
        }
        let used = f64::from(used);
        if used >= 90.0 {
            return BudgetMode::Eko;
        }
        let remaining = (resets_at - now).num_milliseconds() as f64 / 1000.0;
        let fraction = (remaining / w.window_secs as f64).min(1.0);
        if fraction <= 0.0 {
            continue; // süresi dolmuş: ölçüm geçersiz
        }
        let pace = (1.0 - used / 100.0) / fraction;
        worst = Some(worst.map_or(pace, |p| p.min(pace)));
    }
    match worst {
        None => BudgetMode::Dengeli,
        Some(p) if p >= 1.0 => BudgetMode::Tam,
        Some(p) if p >= 0.5 => BudgetMode::Dengeli,
        Some(_) => BudgetMode::Eko,
    }
}
