//! Summarization — pure prompt building and response parsing.
//!
//! Given a category and a list of level-0 beliefs (id + statement), the LLM
//! groups them into a small number of summary statements. The actual network
//! call lives on `NebiusClient::summarize`.
//!
//! Embeddings would let us cluster *before* asking the LLM. Phase 4 doesn't
//! have embeddings yet (those land in Phase 5), so we let the LLM do the
//! grouping itself: it sees all of a category's beliefs at once and decides
//! how to cluster + summarize in one shot. Simple, no extra dependencies.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const SUMMARIZATION_TEMPLATE: &str = concat!(
    "You are a summarization layer for Palamedes, a personal AI memory system.\n\n",
    "You will be given a CATEGORY and a list of low-level beliefs about a USER.\n",
    "Group related beliefs into a small number (1–5) of higher-level summary\n",
    "statements that capture the gist without losing nuance.\n\n",
    "For each summary output:\n",
    "- statement: a short third-person summary (under 25 words)\n",
    "- child_ids: list of belief ids from the input that this summary covers\n\n",
    "HARD RULES (violating these makes the output worthless):\n",
    "1. NO OVERLAP. Each input belief id appears in AT MOST ONE summary's\n",
    "   child_ids. Two summaries claiming the same child is the most common\n",
    "   failure mode — do not do it. If two clusters share a belief, they\n",
    "   are the same cluster: merge them.\n",
    "2. NO NEAR-DUPLICATE summaries. If two summaries say the same thing\n",
    "   in slightly different words, they are one summary — merge them.\n",
    "3. NO INVENTION. Never assert a fact not supported by at least one\n",
    "   child belief.\n",
    "4. EVERY input belief should appear in exactly one summary, unless it\n",
    "   is a true outlier (then omit it entirely — do not force a fit).\n\n",
    "Soft preferences:\n",
    "- Fewer summaries when beliefs are coherent; more when they genuinely fragment.\n",
    "- Avoid trivial 1-child summaries — prefer 2+ children per summary.\n",
    "- A category with 3 obviously-related beliefs should usually produce ONE summary.\n\n",
    "WRONG (overlap — both summaries claim id-1):\n",
    "  [{\"statement\":\"Likes Rust\",\"child_ids\":[\"id-1\",\"id-2\"]},\n",
    "   {\"statement\":\"Builds with Rust\",\"child_ids\":[\"id-1\",\"id-3\"]}]\n",
    "RIGHT (merged):\n",
    "  [{\"statement\":\"Likes and builds with Rust\",\"child_ids\":[\"id-1\",\"id-2\",\"id-3\"]}]\n\n",
    "CATEGORY: {category}\n\n",
    "BELIEFS:\n",
    "{beliefs}\n\n",
    "Return strict JSON with a single top-level key \"summaries\":\n",
    "{\"summaries\": [...]}\n",
    "Use an empty array if no sensible summary exists. No prose, no markdown fences.\n",
);

#[derive(Debug, Clone)]
pub struct SummarizationContext {
    pub category: String,
    /// (id, statement) pairs for the beliefs to be summarized.
    pub beliefs: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SummaryDraft {
    pub statement: String,
    // No confidence: a summary's confidence is computed in Rust as the
    // aggregate of its children's structural scores (see confidence.rs),
    // not invented by the model.
    pub child_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SummarizationOutput {
    summaries: Vec<SummaryDraft>,
}

pub fn build_prompt(ctx: &SummarizationContext) -> String {
    let beliefs = ctx
        .beliefs
        .iter()
        .map(|(id, statement)| format!("- [{}] {}", id, statement))
        .collect::<Vec<_>>()
        .join("\n");
    SUMMARIZATION_TEMPLATE
        .replace("{category}", &ctx.category)
        .replace("{beliefs}", &beliefs)
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

pub fn parse_response(raw: &str) -> Result<Vec<SummaryDraft>> {
    let cleaned = strip_fences(raw);
    if cleaned.is_empty() {
        return Ok(vec![]);
    }
    let parsed: SummarizationOutput = serde_json::from_str(cleaned)
        .with_context(|| format!("summarization response was not valid JSON: {:?}", cleaned))?;

    for s in &parsed.summaries {
        if s.statement.trim().is_empty() {
            return Err(anyhow!("empty summary statement"));
        }
        if s.child_ids.is_empty() {
            return Err(anyhow!("summary has no child_ids"));
        }
    }

    Ok(parsed.summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_category_and_beliefs() {
        let ctx = SummarizationContext {
            category: "skill".into(),
            beliefs: vec![
                ("id-1".into(), "Knows Rust".into()),
                ("id-2".into(), "Knows Tauri".into()),
            ],
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("CATEGORY: skill"));
        assert!(prompt.contains("- [id-1] Knows Rust"));
        assert!(prompt.contains("- [id-2] Knows Tauri"));
        assert!(!prompt.contains("{category}"));
        assert!(!prompt.contains("{beliefs}"));
    }

    #[test]
    fn parse_valid_summary() {
        let raw = r#"{"summaries": [
            {"statement": "Builds Rust+Tauri apps",
             "confidence": 0.85,
             "child_ids": ["id-1", "id-2"]}
        ]}"#;
        let r = parse_response(raw).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].child_ids, vec!["id-1", "id-2"]);
    }

    #[test]
    fn parse_empty() {
        assert!(parse_response(r#"{"summaries": []}"#).unwrap().is_empty());
    }

    #[test]
    fn parse_strips_fences() {
        let raw = "```json\n{\"summaries\": []}\n```";
        assert!(parse_response(raw).unwrap().is_empty());
    }

    #[test]
    fn parse_rejects_empty_statement() {
        let raw = r#"{"summaries":[{"statement":"","confidence":0.5,"child_ids":["a"]}]}"#;
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn parse_rejects_no_children() {
        let raw = r#"{"summaries":[{"statement":"x","confidence":0.5,"child_ids":[]}]}"#;
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_response("not json").is_err());
    }
}
