//! Daily recap — pure rendering. The Tauri command in lib.rs handles I/O.
//!
//! Produces two markdown sections: "What I did" (chat activity) and
//! "What the AI learned about me" (new beliefs). Inferred beliefs are
//! the actionable ones — the user is expected to skim them daily and
//! confirm or correct via the audit panel.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationActivity {
    pub title: String,
    pub messages_today: i64,
    pub branches_today: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeliefRow {
    pub statement: String,
    pub category: Option<String>,
    /// Coarse structural-confidence bucket: "strong" | "moderate" | "tentative".
    /// (Confidence is never a self-reported number — see confidence.rs.)
    pub confidence_bucket: String,
    pub status: String,
    pub trust_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecapData {
    pub date: String,
    pub user_messages_today: i64,
    pub assistant_messages_today: i64,
    pub conversations: Vec<ConversationActivity>,
    pub beliefs: Vec<BeliefRow>,
}

pub fn render_markdown(d: &RecapData) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Recap — {}\n\n", d.date));

    // ----- What I did -----
    out.push_str("## What I did\n\n");
    if d.user_messages_today == 0 && d.conversations.is_empty() {
        out.push_str("_No chat activity today._\n\n");
    } else {
        out.push_str(&format!(
            "- {} user messages · {} assistant messages\n",
            d.user_messages_today, d.assistant_messages_today
        ));
        if !d.conversations.is_empty() {
            out.push_str(&format!("- {} conversation(s) touched:\n", d.conversations.len()));
            for c in &d.conversations {
                let mut line = format!("  - **{}** — {} message(s)", c.title, c.messages_today);
                if c.branches_today > 0 {
                    line.push_str(&format!(", {} new branch(es)", c.branches_today));
                }
                line.push('\n');
                out.push_str(&line);
            }
        }
        out.push('\n');
    }

    // ----- What the AI learned -----
    out.push_str("## What the AI learned about me\n\n");
    if d.beliefs.is_empty() {
        out.push_str("_No new beliefs today._\n\n");
        return out;
    }

    // Group by category for skimmability. Inferred-status beliefs are the
    // actionable ones (need user review). Everything else (asserted, summary,
    // corrected) just shows context.
    let inferred: Vec<&BeliefRow> = d
        .beliefs
        .iter()
        .filter(|b| b.status == "inferred")
        .collect();
    let other: Vec<&BeliefRow> = d
        .beliefs
        .iter()
        .filter(|b| b.status != "inferred")
        .collect();

    if !inferred.is_empty() {
        out.push_str(&format!(
            "### New inferences ({}) — review in the audit panel (Ctrl+M)\n\n",
            inferred.len()
        ));
        for b in &inferred {
            out.push_str(&format!(
                "- {} `{}` · _{}_ — {}\n",
                trust_badge(&b.trust_class),
                b.confidence_bucket,
                b.category.as_deref().unwrap_or("other"),
                b.statement
            ));
        }
        out.push('\n');
    }

    if !other.is_empty() {
        out.push_str(&format!("### Other belief activity ({})\n\n", other.len()));
        for b in &other {
            out.push_str(&format!(
                "- {} `{}` · {} · _{}_ — {}\n",
                trust_badge(&b.trust_class),
                b.confidence_bucket,
                b.status,
                b.category.as_deref().unwrap_or("other"),
                b.statement
            ));
        }
        out.push('\n');
    }

    out
}

fn trust_badge(tc: &str) -> &'static str {
    match tc {
        "asserted" => "🔒",
        "inferred" => "🧠",
        "hypothesized" => "❓",
        "summary" => "Σ",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_day_renders_quiet() {
        let r = RecapData {
            date: "2026-04-27".into(),
            user_messages_today: 0,
            assistant_messages_today: 0,
            conversations: vec![],
            beliefs: vec![],
        };
        let md = render_markdown(&r);
        assert!(md.contains("# Recap — 2026-04-27"));
        assert!(md.contains("_No chat activity today._"));
        assert!(md.contains("_No new beliefs today._"));
    }

    #[test]
    fn busy_day_groups_by_status() {
        let r = RecapData {
            date: "2026-04-27".into(),
            user_messages_today: 12,
            assistant_messages_today: 12,
            conversations: vec![ConversationActivity {
                title: "Phase 6 work".into(),
                messages_today: 8,
                branches_today: 1,
            }],
            beliefs: vec![
                BeliefRow {
                    statement: "Prefers terse summaries".into(),
                    category: Some("preference".into()),
                    confidence_bucket: "moderate".into(),
                    status: "inferred".into(),
                    trust_class: "inferred".into(),
                },
                BeliefRow {
                    statement: "Builds Palamedes in Rust + Tauri".into(),
                    category: Some("skill".into()),
                    confidence_bucket: "strong".into(),
                    status: "asserted".into(),
                    trust_class: "asserted".into(),
                },
            ],
        };
        let md = render_markdown(&r);
        assert!(md.contains("12 user messages"));
        assert!(md.contains("Phase 6 work"));
        assert!(md.contains("New inferences (1)"));
        assert!(md.contains("Other belief activity (1)"));
        assert!(md.contains("Prefers terse summaries"));
        assert!(md.contains("Builds Palamedes"));
    }
}
