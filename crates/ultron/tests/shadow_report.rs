//! Gölge raporu testleri: saf hesap ve metin. Veriler sentetiktir.

use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use ultron::shadow_report::{compute, format_text, window_for, ShadowWindow};
use ultron_core::Db;
use ultron_core::config::EngineId;
use ultron_core::db::approval_shadow::{ShadowAskRow, ShadowScoreRow};
use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
use ultron_core::memory::contracts::Sha256Hash;
use ultron_core::policy::{Action, RiskCategory};

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
}

fn window() -> ShadowWindow {
    ShadowWindow {
        since: at(11, 0),
        until: at(18, 0),
    }
}

fn score(p_safe: f64, measured: DateTime<Utc>) -> Option<ShadowScoreRow> {
    Some(ShadowScoreRow {
        p_safe,
        model: "jeb:4b".into(),
        prompt_version: "shadow-b-v1".into(),
        measured_at: measured,
    })
}

fn row(
    id: &str,
    created: DateTime<Utc>,
    status: ApprovalStatus,
    resolved_by: Option<ResolvedBy>,
    score: Option<ShadowScoreRow>,
) -> ShadowAskRow {
    ShadowAskRow {
        approval_id: id.into(),
        created_at: created,
        status,
        resolved_by,
        resolved_at: resolved_by.map(|_| created + Duration::minutes(1)),
        score,
    }
}

#[test]
fn default_window_is_last_seven_days() {
    let now = at(18, 12);
    let w = window_for(None, None, now).unwrap();
    assert_eq!(w.until, now);
    assert_eq!(w.since, now - Duration::days(7));
}

#[test]
fn explicit_istanbul_days_map_to_utc_storage() {
    // Europe/Istanbul 2026-11-11 00:00 = 2026-11-10 21:00 UTC; bitiş günü dahil.
    let w = window_for(Some("2026-11-11"), Some("2026-11-17"), at(18, 0)).unwrap();
    assert_eq!(w.since, Utc.with_ymd_and_hms(2026, 11, 10, 21, 0, 0).unwrap());
    assert_eq!(w.until, Utc.with_ymd_and_hms(2026, 11, 17, 21, 0, 0).unwrap());
}

#[test]
fn invalid_ranges_are_rejected() {
    assert!(window_for(Some("2026-11-18"), Some("2026-11-11"), at(18, 0)).is_err());
    assert!(window_for(Some("18.11.2026"), None, at(18, 0)).is_err());
}

#[test]
fn agreement_and_false_safe_are_classified_with_comparable_sample() {
    let rows = vec![
        // p >= 0.5 ve insan onayladı: uyum.
        row("a", at(12, 1), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.9, at(12, 1))),
        // p >= 0.5 ama insan reddetti: yanlış-güvenli.
        row("b", at(12, 2), ApprovalStatus::Denied, Some(ResolvedBy::Telegram), score(0.95, at(12, 2))),
        // p < 0.5 ve insan reddetti: uyum.
        row("c", at(13, 1), ApprovalStatus::Denied, Some(ResolvedBy::Desktop), score(0.1, at(13, 1))),
        // p < 0.5 ama insan onayladı: yanlış-güvensiz.
        row("d", at(13, 2), ApprovalStatus::Approved, Some(ResolvedBy::Approver), score(0.2, at(13, 2))),
    ];
    let s = compute(window(), &rows);
    assert_eq!(s.total_asks, 4);
    assert_eq!(s.scored, 4);
    assert_eq!(s.resolved_scored, 4);
    assert_eq!(s.agree, 2);
    assert_eq!(s.disagree, 2);
    assert_eq!(s.false_safe, 1);
    assert_eq!(s.false_unsafe, 1);
    assert_eq!(s.agreement_ratio(), Some(0.5));
    assert_eq!(s.models, vec!["jeb:4b".to_string()]);
    assert_eq!(s.prompt_versions, vec!["shadow-b-v1".to_string()]);
}

#[test]
fn pending_timeout_and_restart_are_separate_and_not_comparable() {
    let rows = vec![
        row("p", at(14, 1), ApprovalStatus::Pending, None, score(0.9, at(14, 1))),
        row("t", at(14, 2), ApprovalStatus::Denied, Some(ResolvedBy::Timeout), score(0.9, at(14, 2))),
        row("r", at(14, 3), ApprovalStatus::Expired, Some(ResolvedBy::Restart), None),
    ];
    let s = compute(window(), &rows);
    assert_eq!(s.total_asks, 3);
    assert_eq!(s.pending, 1);
    assert_eq!(s.timeout, 1);
    assert_eq!(s.restart, 1);
    assert_eq!(s.human_decided, 0);
    assert_eq!(s.resolved_scored, 0);
    assert_eq!(s.false_safe, 0, "zaman aşımı insan kararı sayılmaz");
    assert_eq!(s.unscored, 1);
    assert_eq!(s.agreement_ratio(), None);
}

#[test]
fn no_ratio_when_denominator_is_zero_and_text_says_so() {
    let rows = vec![row("u", at(15, 1), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), None)];
    let s = compute(window(), &rows);
    assert_eq!(s.agreement_ratio(), None);
    assert_eq!(s.mean_p_safe, None);
    let text = format_text(&s);
    assert!(text.contains("Uyum: yok (karşılaştırılabilir örnek yok)"));
    assert!(text.contains("Ortalama P(güvenli): yok"));
}

#[test]
fn rows_outside_window_are_ignored() {
    let rows = vec![
        row("eski", at(10, 23), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.9, at(10, 23))),
        row("yeni", at(18, 0), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.9, at(18, 0))),
    ];
    let s = compute(window(), &rows);
    assert_eq!(s.total_asks, 0);
}

#[test]
fn late_measurement_is_counted_but_still_comparable() {
    let created = at(16, 1);
    let mut late = row("l", created, ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.8, created + Duration::minutes(5)));
    late.resolved_at = Some(created + Duration::minutes(1));
    let s = compute(window(), &[late]);
    assert_eq!(s.late_scored, 1);
    assert_eq!(s.resolved_scored, 1);
}

#[test]
fn text_is_masked_aggregate_only_and_states_observation_limit() {
    let rows = vec![row("a", at(12, 1), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.9, at(12, 1)))];
    let text = format_text(&compute(window(), &rows));
    assert!(text.contains("Europe/Istanbul"));
    assert!(text.contains("Uyum: %100"));
    assert!(text.contains("puan onay kararını değiştirmez"));
    assert!(!text.contains("cargo"));
    assert!(!text.contains("sk-"));
    assert!(text.contains("Yanlış-güvenli (p >= 0.50, insan reddetti): 0"));
}

#[test]
fn wire_conversion_keeps_contract_fields() {
    let rows = vec![row("a", at(12, 1), ApprovalStatus::Denied, Some(ResolvedBy::Desktop), score(0.9, at(12, 1)))];
    let wire = compute(window(), &rows).to_wire();
    assert_eq!(wire.false_safe, 1);
    assert_eq!(wire.disagree, 1);
    assert_eq!(wire.since, window().since);
}

#[test]
fn database_rows_feed_the_report_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("report.db")).unwrap();
    let action = Action::Shell {
        command: "cargo test".into(),
        cwd: PathBuf::from("C:/synthetic/workspace"),
    };
    let id = db
        .create_approval(None, EngineId::Claude, RiskCategory::Delete, "sentetik", &action)
        .unwrap()
        .id;
    db.insert_shadow_score(&ultron_core::memory::contracts::ApprovalShadowScore {
        approval_id: id.clone(),
        model: "jeb:4b".into(),
        prompt_version: "shadow-b-v1".into(),
        p_safe: 0.9,
        measured_at: Utc::now(),
        duration_ms: 10,
        action_fingerprint: Sha256Hash::of(b"synthetic"),
    })
    .unwrap();
    db.resolve_approval(&id, false, ResolvedBy::Desktop).unwrap();

    let now = Utc::now();
    let w = window_for(None, None, now + Duration::hours(1)).unwrap();
    let rows = db.shadow_ask_rows(w.since, w.until).unwrap();
    // Pencere SQL'de sınırlıdır: gelecekteki bir pencere satır döndürmez.
    assert!(db.shadow_ask_rows(now + Duration::days(1), now + Duration::days(2)).unwrap().is_empty());
    let s = compute(w, &rows);
    assert_eq!(s.total_asks, 1);
    assert_eq!(s.false_safe, 1);
    assert_eq!(s.resolved_scored, 1);
}

#[test]
fn agreement_percentage_floors_instead_of_rounding() {
    let mut rows = Vec::new();
    for i in 0..199 {
        rows.push(row(&format!("a{i}"), at(12, 1), ApprovalStatus::Approved, Some(ResolvedBy::Desktop), score(0.9, at(12, 1))));
    }
    rows.push(row("d", at(12, 1), ApprovalStatus::Denied, Some(ResolvedBy::Desktop), score(0.9, at(12, 1))));
    let text = format_text(&compute(window(), &rows));
    assert!(text.contains("Uyum: %99"), "{text}");
    assert!(!text.contains("Uyum: %100"));
}

#[test]
fn agreement_percent_uses_integer_floor_without_float_error() {
    let summary = |agree: u64, total: u64| {
        let mut s = compute(window(), &[]);
        s.agree = agree;
        s.resolved_scored = total;
        s
    };
    for (agree, total, want) in [(29, 100, 29), (57, 100, 57), (58, 100, 58), (29, 50, 58), (114, 200, 57), (199, 200, 99)] {
        assert_eq!(summary(agree, total).agreement_percent_floor(), Some(want), "{agree}/{total}");
        assert!(format_text(&summary(agree, total)).contains(&format!("Uyum: %{want}")), "{agree}/{total}");
    }
    assert_eq!(summary(0, 0).agreement_percent_floor(), None);
}
