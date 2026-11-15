//! Devir paketi: bir sohbeti başka bir motora bağlam kaybı olmadan aktarmak için metin üretir.
//!
//! Bölüm sırası sabitten değişkene doğrudur (önbellek dostu): başlık, çalışma durumu,
//! dış değişiklikler, eski mesaj özeti, son mesajlar, geçmiş ipucu. Bütçe karakter ile
//! ölçülür; kırpma önce eski özete, sonra kırpılabilir son mesajlara uygulanır. Çalışma
//! durumu ve son iki mesaj hiçbir zaman kırpılmaz.

use std::fmt::Write as _;

use anyhow::Result;

use crate::config::EngineId;
use crate::db::Db;
use crate::workstate::{WorkState, clip};

/// Son mesajlardan asla kırpılmayan adet.
const PROTECTED_MESSAGES: usize = 2;

/// Eski mesaj özetinde her mesaj için en fazla karakter.
const SUMMARY_CLIP_CHARS: usize = 200;

/// Üretilen devir paketi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPack {
    pub text: String,
    pub truncated: bool,
    /// Bölüm adı ve paketteki karakter sayısı, paketteki sırayla.
    pub sections: Vec<(String, usize)>,
}

/// Bir sohbetin devir paketini üretir.
///
/// `recent_verbatim` son mesajları aynen koyar (en az 2). `external` verilirse
/// `[SON OTURUMDAN BERİ DEĞİŞENLER]` bölümü olarak eklenir. Bütçe aşılırsa eski özet,
/// ardından kırpılabilir son mesajlar en eskiden başlayarak atılır ve `[KIRPILDI: N karakter]`
/// eklenir. Çalışma durumu ve son iki mesaj bütçe aşılsa da korunur.
pub fn build_handoff(
    db: &Db,
    chat_id: &str,
    budget_chars: usize,
    recent_verbatim: usize,
    external: Option<&str>,
) -> Result<HandoffPack> {
    let state: WorkState = match db.get_work_state(chat_id)? {
        Some(value) => serde_json::from_value(value)?,
        None => WorkState::default(),
    };
    let messages: Vec<(&'static str, String)> = db
        .events_for_chat(chat_id, None)?
        .into_iter()
        .filter_map(|ev| {
            let role = match ev.kind.as_str() {
                "user_text" => "kullanıcı",
                "assistant_text" => "asistan",
                _ => return None,
            };
            let text = ev.payload.get("text")?.as_str()?.to_string();
            Some((role, text))
        })
        .collect();

    let window = recent_verbatim.max(PROTECTED_MESSAGES);
    let split = messages.len().saturating_sub(window);
    let (older, recent) = messages.split_at(split);
    let protected_from = recent.len().saturating_sub(PROTECTED_MESSAGES);
    let (cuttable, protected) = recent.split_at(protected_from);

    let parts = Parts {
        header: "[ULTRON DEVİR]\nBu sohbet bir motordan diğerine devredildi; aşağıdaki çalışma durumu ve mesajlar bağlamdır.\n\n".to_string(),
        state: render_state(&state),
        external: external
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| format!("[SON OTURUMDAN BERİ DEĞİŞENLER]\n{e}\n\n")),
        summary: render_summary(older),
        cuttable: cuttable.iter().map(message_line).collect(),
        protected: protected.iter().map(message_line).collect(),
        history: "[GEÇMİŞ]\nAyrıntı gerekirse `ultron history search \"<sorgu>\"` çalıştır.\n"
            .to_string(),
    };

    let mut cut_summary = false;
    let mut cut_messages = 0usize;
    let mut assembled = assemble(&parts, cut_summary, cut_messages);
    while assembled.text.chars().count() > budget_chars {
        if !cut_summary && !parts.summary.is_empty() {
            cut_summary = true;
        } else if cut_messages < parts.cuttable.len() {
            cut_messages += 1;
        } else {
            // Kırpılabilecek her şey gitti; zorunlu bölümler bütçeyi aşıyorsa olduğu gibi döner.
            break;
        }
        assembled = assemble(&parts, cut_summary, cut_messages);
    }

    Ok(HandoffPack {
        truncated: cut_summary || cut_messages > 0,
        text: assembled.text,
        sections: assembled.sections,
    })
}

/// Pakete girecek ham bölümler; kırpma kararları `assemble` içinde verilir.
struct Parts {
    header: String,
    state: String,
    external: Option<String>,
    summary: String,
    cuttable: Vec<String>,
    protected: Vec<String>,
    history: String,
}

struct Assembled {
    text: String,
    sections: Vec<(String, usize)>,
}

/// Verilen kırpma kararlarıyla paketi birleştirir. `cut_messages` en eski kırpılabilir
/// mesajlardan başlayarak atılır.
fn assemble(parts: &Parts, cut_summary: bool, cut_messages: usize) -> Assembled {
    let removed_summary = if cut_summary {
        parts.summary.chars().count()
    } else {
        0
    };
    let removed_messages: usize = parts.cuttable[..cut_messages]
        .iter()
        .map(|line| line.chars().count())
        .sum();
    let removed = removed_summary + removed_messages;

    let mut blocks: Vec<(&'static str, String)> = vec![
        ("header", parts.header.clone()),
        ("work_state", parts.state.clone()),
    ];
    if let Some(external) = &parts.external {
        blocks.push(("external_changes", external.clone()));
    }
    if !cut_summary && !parts.summary.is_empty() {
        blocks.push(("older_summary", parts.summary.clone()));
    }
    if removed > 0 {
        blocks.push(("truncation", format!("[KIRPILDI: {removed} karakter]\n\n")));
    }
    let mut messages = String::new();
    for line in parts.cuttable[cut_messages..].iter().chain(&parts.protected) {
        messages.push_str(line);
    }
    if !messages.is_empty() {
        blocks.push(("recent_messages", format!("[SON MESAJLAR]\n{messages}\n")));
    }
    blocks.push(("history_hint", parts.history.clone()));

    let mut text = String::new();
    let mut sections = Vec::with_capacity(blocks.len());
    for (name, block) in blocks {
        sections.push((name.to_string(), block.chars().count()));
        text.push_str(&block);
    }
    Assembled { text, sections }
}

fn render_state(state: &WorkState) -> String {
    let mut out = String::from("[ÇALIŞMA DURUMU]\n");
    if state.summary_stale {
        out.push_str("(özet güncel değil; kural tabanlı bilgiler geçerlidir)\n");
    }
    if let Some(goal) = &state.goal {
        let _ = writeln!(out, "Hedef: {goal}");
    }
    if let Some(next) = &state.next_step {
        let _ = writeln!(out, "Sonraki adım: {next}");
    }
    push_list(&mut out, "Plan", &state.plan);
    if !state.decisions.is_empty() {
        out.push_str("Kararlar:\n");
        for decision in &state.decisions {
            match &decision.why {
                Some(why) => {
                    let _ = writeln!(out, "- {} ({why})", decision.what);
                }
                None => {
                    let _ = writeln!(out, "- {}", decision.what);
                }
            }
        }
    }
    if !state.files_touched.is_empty() {
        out.push_str("Değiştirilen dosyalar:\n");
        for file in &state.files_touched {
            let _ = writeln!(out, "- {file}");
        }
    }
    push_list(&mut out, "Tamamlanan", &state.done);
    push_list(&mut out, "Açık sorular", &state.open_questions);
    push_list(&mut out, "Onay bekleyen", &state.approvals_pending);
    if !state.engine_history.is_empty() {
        out.push_str("Motor geçmişi:\n");
        for span in &state.engine_history {
            let to = span
                .to_seq
                .map_or_else(|| "şimdi".to_string(), |s| s.to_string());
            let _ = writeln!(
                out,
                "- {} (seq {}-{to})",
                engine_name(span.engine),
                span.from_seq
            );
        }
    }
    out.push('\n');
    out
}

fn push_list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "{title}:");
    for item in items {
        let _ = writeln!(out, "- {item}");
    }
}

fn render_summary(older: &[(&'static str, String)]) -> String {
    if older.is_empty() {
        return String::new();
    }
    let mut out = String::from("[ESKİ MESAJLAR ÖZETİ]\n");
    for (role, text) in older {
        let _ = writeln!(out, "- {role}: {}", clip(text, SUMMARY_CLIP_CHARS));
    }
    out.push('\n');
    out
}

fn message_line((role, text): &(&'static str, String)) -> String {
    format!("{role}: {text}\n")
}

fn engine_name(engine: EngineId) -> &'static str {
    match engine {
        EngineId::Claude => "claude",
        EngineId::Codex => "codex",
        EngineId::Agy => "agy",
    }
}
