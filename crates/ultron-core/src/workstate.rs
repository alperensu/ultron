//! Çalışma durumu: sohbetin hedef, karar, dosya ve motor geçmişini tutar.
//!
//! Kural tabanlı alanlar (dosyalar, kararlar, motor geçmişi, ilk hedef) olay günlüğünden
//! her seferinde yeniden türetilir. Özet alanları (hedef, sonraki adım, açık sorular,
//! tamamlananlar) özetleyiciden gelir; özetleyici başarısız olursa önceki değerler
//! korunur ve `summary_stale` işaretlenir.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::EngineId;
use crate::db::{Db, StoredEvent};
use crate::events::EngineEvent;
use crate::summarizer::{Summarizer, SummaryPatch};

/// Hedef metninin üst sınırı (karakter).
const GOAL_CLIP_CHARS: usize = 300;

/// Bir sohbetin çalışma durumu.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkState {
    pub goal: Option<String>,
    pub plan: Vec<String>,
    pub decisions: Vec<Decision>,
    pub files_touched: BTreeSet<String>,
    pub done: Vec<String>,
    pub next_step: Option<String>,
    pub open_questions: Vec<String>,
    pub engine_history: Vec<EngineSpan>,
    pub approvals_pending: Vec<String>,
    /// Son özet yenilemesi başarısız olduysa `true`; özet alanları eski olabilir.
    pub summary_stale: bool,
}

/// Bir karar kaydı. Kaynak olayın kimliği izlenebilirlik için tutulur.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub what: String,
    pub why: Option<String>,
    pub ts: DateTime<Utc>,
    pub source_event: String,
}

/// Bir motorun sohbette aktif olduğu sıra aralığı. `to_seq` `None` ise hâlâ aktif.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSpan {
    pub engine: EngineId,
    pub from_seq: i64,
    pub to_seq: Option<i64>,
}

impl WorkState {
    /// Tek bir olayı kural tabanlı alanlara uygular.
    ///
    /// - `FileChanged` → `files_touched`
    /// - `EngineSwitched` → `engine_history`
    /// - `UserText` satır başı `KARAR:` / `DECISION:` → `decisions`
    /// - İlk `UserText` (hedef boşsa) → `goal`
    pub fn apply_rules(&mut self, ev: &StoredEvent) {
        let Ok(parsed) = serde_json::from_value::<EngineEvent>(ev.payload.clone()) else {
            return;
        };
        match parsed {
            EngineEvent::FileChanged { path, .. } => {
                self.files_touched.insert(path);
            }
            EngineEvent::EngineSwitched { from, to, .. } => {
                match self.engine_history.last_mut() {
                    Some(last) => last.to_seq = Some(ev.seq),
                    None => self.engine_history.push(EngineSpan {
                        engine: from,
                        from_seq: 0,
                        to_seq: Some(ev.seq),
                    }),
                }
                self.engine_history.push(EngineSpan {
                    engine: to,
                    from_seq: ev.seq,
                    to_seq: None,
                });
            }
            EngineEvent::UserText { text } => {
                if self.goal.is_none() {
                    let goal = text.trim();
                    if !goal.is_empty() {
                        self.goal = Some(clip(goal, GOAL_CLIP_CHARS));
                    }
                }
                for line in text.lines() {
                    let line = line.trim_start();
                    let what = line
                        .strip_prefix("KARAR:")
                        .or_else(|| line.strip_prefix("DECISION:"))
                        .map(str::trim)
                        .filter(|w| !w.is_empty());
                    if let Some(what) = what {
                        self.decisions.push(Decision {
                            what: what.to_string(),
                            why: None,
                            ts: ev.ts,
                            source_event: ev.id.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Özetleyiciden gelen yamayı uygular. `None` alanlar değişmez.
    pub fn apply_summary(&mut self, patch: SummaryPatch) {
        if let Some(goal) = patch.goal.filter(|g| !g.trim().is_empty()) {
            self.goal = Some(goal);
        }
        if let Some(next) = patch.next_step {
            self.next_step = Some(next);
        }
        if let Some(questions) = patch.open_questions {
            self.open_questions = questions;
        }
        if let Some(done) = patch.done {
            self.done = done;
        }
    }
}

/// Metni en fazla `max_chars` karaktere kırpar; kırpılırsa sona `…` ekler.
pub(crate) fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Çalışma durumunu hesaplayıp kaydeden servis.
pub struct WorkStateService {
    db: Arc<Db>,
    summarizer: Box<dyn Summarizer>,
}

impl WorkStateService {
    pub fn new(db: Arc<Db>, summarizer: Box<dyn Summarizer>) -> Self {
        Self { db, summarizer }
    }

    /// Bir tur bittiğinde çağrılır: kuralları olay günlüğünün tamamına uygular, özetleyiciyi
    /// çalıştırır ve sonucu kaydeder. Özetleyici hatası durumunda kural durumu yine kaydedilir
    /// ve `summary_stale` doğru olarak işaretlenir; olay yazılmaz.
    pub async fn on_turn_completed(&self, chat_id: &str) -> Result<WorkState> {
        let prev: WorkState = match self.db.get_work_state(chat_id)? {
            Some(value) => serde_json::from_value(value)?,
            None => WorkState::default(),
        };
        let events = self.db.events_for_chat(chat_id, None)?;

        // Özet alanları önceki durumdan taşınır; kural alanları sıfırdan türetilir.
        let mut state = WorkState {
            goal: prev.goal,
            plan: prev.plan,
            done: prev.done,
            next_step: prev.next_step,
            open_questions: prev.open_questions,
            approvals_pending: prev.approvals_pending,
            ..WorkState::default()
        };
        for ev in &events {
            state.apply_rules(ev);
        }

        match self.summarizer.refresh(&state, &events).await {
            Ok(patch) => {
                state.apply_summary(patch);
                state.summary_stale = false;
            }
            Err(_) => state.summary_stale = true,
        }

        self.db
            .put_work_state(chat_id, &serde_json::to_value(&state)?)?;
        Ok(state)
    }
}
