//! Çalışma durumu: sohbetin hedef, karar, dosya ve motor geçmişini tutar.
//!
//! Kural tabanlı alanlar (dosyalar, kararlar, motor geçmişi, ilk hedef) olay günlüğünden
//! her seferinde yeniden türetilir. Özet alanları (hedef, sonraki adım, açık sorular,
//! tamamlananlar) özetleyiciden gelir; özetleyici başarısız olursa önceki değerler
//! korunur ve `summary_stale` işaretlenir.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::EngineId;
use crate::db::{Db, RECENT_EVENTS_WINDOW, StoredEvent};
use crate::events::EngineEvent;
use crate::summarizer::{Summarizer, SummaryPatch};

/// Çalışma durumu oku-değiştir-yaz işlemlerini (kural güncellemesi ve geç dönen özet) süreç
/// içinde sıralar; böylece CAS karşılaştırması ile yazma arasında başka yazıcı giremez.
static STATE_LOCK: Mutex<()> = Mutex::new(());

fn state_guard() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Özet durum kodları (`WorkState::summary_status`); ham sağlayıcı metni taşımaz.
pub mod summary_status {
    pub const PENDING: &str = "pending";
    pub const OK: &str = "ok";
    pub const NO_ENGINE: &str = "no_engine";
    pub const QUEUE_FULL: &str = "queue_full";
    pub const ENQUEUE_FAILED: &str = "enqueue_failed";
    pub const TURN_INCOMPLETE: &str = "turn_incomplete";
    /// Özet işi sonuçlandı ama kural alanları turdan ileri gitmişti; yama atıldı.
    pub const SUPERSEDED: &str = "superseded";
    pub const FAILED_PREFIX: &str = "failed:";
}

/// Hedef metninin üst sınırı (karakter).
/// Saklanan en fazla karar sayısı (en yeniler kalır).
pub const MAX_DECISIONS: usize = 50;

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
    /// Kural alanlarına uygulanan en yüksek olay `seq` değeri. Yenilemede yalnız bundan
    /// sonraki olaylar işlenir; böylece eski dosya ve karar kayıtları korunur.
    #[serde(default)]
    pub rules_seq: i64,
    /// Modelden gelen özetin uygulandığı en yüksek tur `end_seq` değeri (CAS jetonu).
    pub summary_seq: i64,
    /// Son uygulanan özeti üreten motor (`agy`, `claude-haiku`); yoksa `None`.
    pub summary_engine: Option<String>,
    /// Son özet yedek motorla (kota nedeniyle) üretildiyse `true`.
    pub summary_fallback: bool,
    /// Güvenli özet durum kodu (bkz. [`summary_status`]).
    pub summary_status: Option<String>,
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
    /// Özetleyici istemine konacak kopya: bütün metin alanları temizlenir, maskelenir ve
    /// kırpılır (kalıcı durum zaten temizdir; bu, eski/bozuk kayıtlara karşı ikinci kapıdır).
    pub fn sanitized_for_prompt(&self) -> WorkState {
        let mut out = self.clone();
        out.goal = self.goal.as_deref().map(clean_text);
        out.next_step = self.next_step.as_deref().map(clean_text);
        out.plan = clean_list(self.plan.clone());
        out.done = clean_list(self.done.clone());
        out.open_questions = clean_list(self.open_questions.clone());
        out.approvals_pending = clean_list(self.approvals_pending.clone());
        out.files_touched = self.files_touched.iter().take(200).map(|f| clean_text(f)).collect();
        let skip = self.decisions.len().saturating_sub(MAX_DECISIONS);
        out.decisions = self
            .decisions
            .iter()
            .skip(skip)
            .map(|d| Decision {
                what: clean_text(&d.what),
                why: d.why.as_deref().map(clean_text),
                ts: d.ts,
                source_event: d.source_event.clone(),
            })
            .collect();
        out
    }

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
                self.files_touched.insert(clean_text(&path));
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
                    // Ham kullanıcı metni: görünmez karakter temizliği + sır maskeleme + kırpma.
                    let goal = clean_text(text.trim());
                    if !goal.is_empty() {
                        self.goal = Some(goal);
                    }
                }
                for line in text.lines() {
                    let line = line.trim_start();
                    let what = line
                        .strip_prefix("KARAR:")
                        .or_else(|| line.strip_prefix("DECISION:"))
                        .map(str::trim)
                        .filter(|w| !w.is_empty());
                    let what = what.map(clean_text).filter(|w| !w.is_empty());
                    if let Some(what) = what {
                        self.decisions.push(Decision {
                            what,
                            why: None,
                            ts: ev.ts,
                            source_event: ev.id.clone(),
                        });
                        if self.decisions.len() > MAX_DECISIONS {
                            let excess = self.decisions.len() - MAX_DECISIONS;
                            self.decisions.drain(..excess);
                        }
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

/// Görünmez biçim (Unicode Cf) karakteri mi? Bidi geçersiz kılma, sıfır genişlik, BOM vb.;
/// ayrıca satır/paragraf ayırıcıları U+2028/2029.
pub(crate) fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
        | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// Denetim/biçim karakterlerini atar. `keep_newlines` ise `\n`, `\r` ve `\t` korunur; değilse
/// bunlar boşluğa çevrilir.
pub(crate) fn strip_unsafe_chars(text: &str, keep_newlines: bool) -> String {
    text.chars()
        .filter_map(|c| {
            if is_format_char(c) {
                None
            } else if !c.is_control() {
                Some(c)
            } else if matches!(c, '\n' | '\t' | '\r') {
                // Tek satırlı alanlarda satır/sekme boşluğa dönüşür (sözcükler birleşmesin).
                Some(if keep_newlines { c } else { ' ' })
            } else {
                None
            }
        })
        .collect()
}

const PATCH_TEXT_CHARS: usize = 300;
const PATCH_LIST_ITEMS: usize = 20;
/// Maskelemeden önce taranacak en fazla karakter (regex maliyeti sınırı).
const PATCH_SCAN_CHARS: usize = 4 * PATCH_TEXT_CHARS;

fn clean_text(text: &str) -> String {
    let cropped: String = text.chars().take(PATCH_SCAN_CHARS).collect();
    let single = strip_unsafe_chars(&cropped, false);
    let masked = crate::import::mask_secrets(&single).0;
    clip(&masked.split_whitespace().collect::<Vec<_>>().join(" "), PATCH_TEXT_CHARS)
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    items
        .iter()
        .take(PATCH_LIST_ITEMS)
        .map(|i| clean_text(i))
        .filter(|i| !i.is_empty())
        .collect()
}

/// Model yamasını güvenli hale getirir: Cc+Cf temizliği, sır maskeleme, boyut sınırı
/// (hedef/sonraki adım 300 karakter; listeler en çok 20 x 300).
pub fn sanitize_patch(patch: SummaryPatch) -> SummaryPatch {
    SummaryPatch {
        goal: patch.goal.map(|g| clean_text(&g)),
        next_step: patch.next_step.map(|n| clean_text(&n)),
        open_questions: patch.open_questions.map(clean_list),
        done: patch.done.map(clean_list),
    }
}

/// Geç dönen özetin uygulanma sonucu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// Daha yeni bir özet veya kural güncellemesi var; yama atıldı.
    Stale,
}

/// Çalışma durumunu hesaplayıp kaydeden servis.
pub struct WorkStateService {
    db: Arc<Db>,
    summarizer: Arc<dyn Summarizer>,
}

impl WorkStateService {
    pub fn new(db: Arc<Db>, summarizer: Box<dyn Summarizer>) -> Self {
        Self {
            db,
            summarizer: Arc::from(summarizer),
        }
    }

    /// Arka plan işçisinin kullandığı özetleyici.
    pub fn summarizer(&self) -> Arc<dyn Summarizer> {
        self.summarizer.clone()
    }

    /// Doğrudan (bloklayan) yol: kuralları günceller, özetleyiciyi bekler ve sonucu kaydeder.
    /// `SessionManager` bunu KULLANMAZ; tur tamamlanması modeli beklemez
    /// ([`WorkStateService::complete_rules`] + arka plan işi). Özetleyici hatası durumunda
    /// kural durumu yine kaydedilir ve `summary_stale` doğru olarak işaretlenir.
    pub async fn on_turn_completed(&self, chat_id: &str) -> Result<WorkState> {
        let (state, events) = {
            let _guard = state_guard();
            self.apply_new_rules(chat_id)?
        };
        let result = self.summarizer.refresh(&state, &events).await;
        let _guard = state_guard();
        // Model beklenirken kural alanları ilerlemiş olabilir; yamayı en güncel duruma uygula.
        let (mut latest, _) = self.apply_new_rules(chat_id)?;
        drop(state);
        match result {
            Ok(patch) => {
                latest.apply_summary(sanitize_patch(patch));
                latest.summary_stale = false;
            }
            Err(err) => {
                tracing::warn!(chat_id, error = %err, "çalışma durumu özetleyicisi başarısız");
                latest.summary_stale = true;
            }
        }
        self.store(chat_id, &latest)?;
        Ok(latest)
    }

    /// Yalnız kural tabanlı yenileme; özetleyici çağrılmaz. Tur tamamlanmadan bittiğinde
    /// (kota, hata, iptal) kullanılır. Özet alanları eski kabul edilir ve `summary_stale`
    /// işaretlenir.
    pub fn refresh_rules(&self, chat_id: &str) -> Result<WorkState> {
        self.complete_rules(chat_id, true, Some(summary_status::TURN_INCOMPLETE))
    }

    /// Tamamlanan turun senkron, ucuz kural güncellemesi (model beklenmez). `summary_pending`
    /// `true` ise özet işi kuyruğa alınacaktır: özet eski işaretlenir ve `status` kaydedilir.
    /// `false` ise (özetleyici yok) özet bayat sayılmaz.
    pub fn complete_rules(
        &self,
        chat_id: &str,
        summary_pending: bool,
        status: Option<&str>,
    ) -> Result<WorkState> {
        let _guard = state_guard();
        let (mut state, _events) = self.apply_new_rules(chat_id)?;
        state.summary_stale = summary_pending;
        state.summary_status = status.map(str::to_string);
        self.store(chat_id, &state)?;
        Ok(state)
    }

    /// Yalnız güvenli özet durum kodunu günceller (kuyruğa alınamadı, başarısız vb.). Daha yeni
    /// bir özet uygulanmışsa (`summary_seq >= end_seq`) veya kural alanları turdan ilerlemişse
    /// (`rules_seq > end_seq`; yeni tur kendi durumunu yazar) yok sayılır.
    pub fn set_summary_status(&self, chat_id: &str, status: &str, end_seq: i64) -> Result<()> {
        let _guard = state_guard();
        let mut state = self.load(chat_id)?;
        if state.summary_seq >= end_seq || state.rules_seq > end_seq {
            return Ok(());
        }
        state.summary_status = Some(status.to_string());
        state.summary_stale = true;
        self.store(chat_id, &state)
    }

    /// Kayıtlı çalışma durumu (yoksa varsayılan).
    pub fn load(&self, chat_id: &str) -> Result<WorkState> {
        Ok(match self.db.get_work_state(chat_id)? {
            Some(value) => serde_json::from_value(value)?,
            None => WorkState::default(),
        })
    }

    fn store(&self, chat_id: &str, state: &WorkState) -> Result<()> {
        self.db
            .put_work_state(chat_id, &serde_json::to_value(state)?)
    }

    /// Geç dönen model yamasını CAS ile uygular. Yama yalnız (a) daha yeni bir özet
    /// uygulanmadıysa (`summary_seq < end_seq`) ve (b) kural alanları turdan ileri gitmediyse
    /// (`rules_seq <= end_seq`) yazılır; aksi halde atılır ve hedef/tamamlananlar/`rules_seq`
    /// geriye gitmez.
    pub fn apply_digest_patch(
        &self,
        chat_id: &str,
        end_seq: i64,
        patch: SummaryPatch,
        engine: &str,
        fell_back: bool,
    ) -> Result<ApplyOutcome> {
        let _guard = state_guard();
        let mut state = self.load(chat_id)?;
        if state.summary_seq >= end_seq {
            return Ok(ApplyOutcome::Stale);
        }
        if state.rules_seq > end_seq {
            // Eski yama atılır; durum sonsuza dek `pending` kalmasın.
            if state.summary_status.as_deref() == Some(summary_status::PENDING) {
                state.summary_status = Some(summary_status::SUPERSEDED.to_string());
                state.summary_stale = true;
                self.store(chat_id, &state)?;
            }
            return Ok(ApplyOutcome::Stale);
        }
        let patch = sanitize_patch(patch);
        state.apply_summary(patch);
        state.summary_seq = end_seq;
        state.summary_stale = false;
        state.summary_engine = Some(engine.to_string());
        state.summary_fallback = fell_back;
        state.summary_status = Some(summary_status::OK.to_string());
        self.store(chat_id, &state)?;
        Ok(ApplyOutcome::Applied)
    }

    /// Kayıtlı durumu okur, son [`RECENT_EVENTS_WINDOW`] olaydan `rules_seq`'ten yeni olanları
    /// kural alanlarına uygular. Döndürülen olaylar özetleyiciye verilir. Çağıran
    /// `state_guard` tutar.
    fn apply_new_rules(&self, chat_id: &str) -> Result<(WorkState, Vec<StoredEvent>)> {
        let mut state = self.load(chat_id)?;
        let events = self.db.events_tail(chat_id, RECENT_EVENTS_WINDOW)?;
        let base = state.rules_seq;
        for ev in events.iter().filter(|ev| ev.seq > base) {
            state.apply_rules(ev);
            state.rules_seq = state.rules_seq.max(ev.seq);
        }
        Ok((state, events))
    }
}
