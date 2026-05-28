//! Belief extraction — pure prompt building and response parsing.
//!
//! The actual network call lives on NebiusClient::extract_beliefs.
//! Everything in this module is testable without a running API.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const EXTRACTION_TEMPLATE: &str = concat!(
    "You are a belief extractor for Palamedes, a personal AI memory system.\n\n",
    "Given a conversation turn between USER and ASSISTANT, extract zero or more\n",
    "beliefs about the USER that are clearly supported by what they said in the TURN.\n\n",
    "For each belief output:\n",
    "- statement: a short third-person claim about the user (under 20 words)\n",
    "- category: one of [preference, fact, skill, plan, context, other]\n",
    "- trust_class: one of \"asserted\" | \"inferred\" | \"hypothesized\".\n",
    "  - \"asserted\": the user explicitly states the claim about themselves\n",
    "    in this turn (\"I am X\", \"I like Y\", \"my name is Z\"). This REQUIRES a\n",
    "    verbatim evidence_quote copied exactly from the user's own words. If\n",
    "    you cannot quote them stating it, it is NOT asserted.\n",
    "  - \"inferred\": you derived it from what the user said this turn, but they\n",
    "    did not state it verbatim (e.g. they describe doing X → they do X).\n",
    "  - \"hypothesized\": a weak guess from mood, tone, an offhand quip, or\n",
    "    implication (\"ugh, again\" → maybe frustrated). Low-signal; use freely\n",
    "    instead of forcing a shaky claim up to \"inferred\".\n",
    "- evidence_quote: the exact user text span that supports this claim,\n",
    "  copied verbatim from the user. Always include it; it is mandatory for\n",
    "  \"asserted\" and verified against the user's message.\n\n",
    "Rules:\n",
    "- Do NOT extract beliefs from weak signals. When in doubt, extract nothing.\n",
    "- Do NOT extract the user's current request or task as a belief. \"Wants\n",
    "  to calculate CSV averages\" or \"is debugging a Rust function\" are\n",
    "  ephemeral — they describe the moment, not the person. Only extract\n",
    "  durable traits: preferences they hold, facts about their life, skills\n",
    "  they have, plans they are pursuing across turns.\n",
    "- A belief from a passing remark, hedge, or mood (\"sometimes I wish X\",\n",
    "  \"today seems like a nicer time\") is \"hypothesized\", never \"asserted\".\n",
    "- When the user pastes a document about themselves (CV, bio), claims\n",
    "  the document makes about the user are \"asserted\" — they are putting\n",
    "  the document forward as their own.\n",
    "- Prefer specific over general (\"learning Rust async\" beats \"knows Rust\").\n",
    "- Extract only beliefs about the USER — not the assistant, not third parties.\n",
    "- Skip trivia (\"asked a question\", \"said hello\").\n\n",
    "BLOCKLIST — DO NOT extract any belief matching these patterns, even if the\n",
    "turn seems to support them. They have been explicitly marked wrong:\n",
    "{blocklist}\n\n",
    "TURN:\n",
    "User: {user_content}\n\n",
    "Assistant: {assistant_content}\n\n",
    "Return strict JSON with a single top-level key \"beliefs\":\n",
    "{\"beliefs\": [...]}\n",
    "Use an empty array if nothing qualifies. No prose, no markdown fences.\n",
);

#[derive(Debug, Clone)]
pub struct TurnContext {
    pub user_content: String,
    pub assistant_content: String,
    pub blocklist_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeliefDraft {
    pub statement: String,
    pub category: String,
    // No confidence field: the model is never asked to rate its own certainty
    // (self-reported LLM confidence is badly miscalibrated). Confidence is
    // derived structurally at read time — see `confidence.rs`. Any `confidence`
    // key a model emits anyway is ignored on deserialize.
    pub trust_class: String, // "asserted" | "inferred" | "hypothesized"
    #[serde(default)]
    pub evidence_quote: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtractionOutput {
    beliefs: Vec<BeliefDraft>,
}

pub fn build_prompt(ctx: &TurnContext) -> String {
    let blocklist = if ctx.blocklist_patterns.is_empty() {
        "(none)".to_string()
    } else {
        ctx.blocklist_patterns
            .iter()
            .map(|p| format!("- {}", p))
            .collect::<Vec<_>>()
            .join("\n")
    };
    EXTRACTION_TEMPLATE
        .replace("{blocklist}", &blocklist)
        .replace("{user_content}", &ctx.user_content)
        .replace("{assistant_content}", &ctx.assistant_content)
}

/// Strip common LLM-output wrappers (markdown code fences) before parsing.
fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

pub fn parse_response(raw: &str) -> Result<Vec<BeliefDraft>> {
    let cleaned = strip_fences(raw);
    if cleaned.is_empty() {
        return Ok(vec![]);
    }
    let parsed: ExtractionOutput = serde_json::from_str(cleaned)
        .with_context(|| format!("extraction response was not valid JSON: {:?}", cleaned))?;

    for b in &parsed.beliefs {
        if b.trust_class != "asserted"
            && b.trust_class != "inferred"
            && b.trust_class != "hypothesized"
        {
            return Err(anyhow!("invalid trust_class: {}", b.trust_class));
        }
        if b.statement.trim().is_empty() {
            return Err(anyhow!("empty belief statement"));
        }
    }

    Ok(parsed.beliefs)
}

/// Is `quote` actually grounded in `user_text`? Used to verify an "asserted"
/// trust class: a directly-stated belief must be backed by the user's own
/// words, not a paraphrase the model invented. Deterministic and cheap —
/// normalizes case/punctuation/whitespace, then checks verbatim containment,
/// with an 80%-token-overlap fallback for trivial reorderings. This is the
/// grounding check the audit thesis rests on: "asserted" you can verify.
pub fn is_grounded(quote: &str, user_text: &str) -> bool {
    fn norm(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    let q = norm(quote);
    if q.is_empty() {
        return false;
    }
    let u = norm(user_text);
    if u.contains(&q) {
        return true;
    }
    // Fallback: most of the quote's tokens appear in the user's text.
    let utoks: std::collections::HashSet<&str> = u.split(' ').collect();
    let qtoks: Vec<&str> = q.split(' ').filter(|t| !t.is_empty()).collect();
    if qtoks.is_empty() {
        return false;
    }
    let hits = qtoks.iter().filter(|t| utoks.contains(*t)).count();
    (hits as f64 / qtoks.len() as f64) >= 0.8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grounded_accepts_verbatim_and_case_punct_variants() {
        let user = "I'm building Palamedes in Rust and Tauri.";
        assert!(is_grounded("I'm building Palamedes in Rust", user));
        assert!(is_grounded("building palamedes in rust", user));
        assert!(is_grounded("Rust and Tauri", user));
    }

    #[test]
    fn grounded_rejects_hallucinated_or_empty_quote() {
        let user = "I prefer dark mode and terse answers.";
        assert!(!is_grounded("the user is a professional chef", user));
        assert!(!is_grounded("", user));
        assert!(!is_grounded("   ", user));
    }

    #[test]
    fn prompt_substitutes_blocklist_and_content() {
        let ctx = TurnContext {
            user_content: "I'm learning Rust".into(),
            assistant_content: "Cool, what are you building?".into(),
            blocklist_patterns: vec!["user is vegan".into(), "user lives in Paris".into()],
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("I'm learning Rust"));
        assert!(prompt.contains("Cool, what are you building?"));
        assert!(prompt.contains("- user is vegan"));
        assert!(prompt.contains("- user lives in Paris"));
        assert!(!prompt.contains("{blocklist}"));
        assert!(!prompt.contains("{user_content}"));
    }

    #[test]
    fn prompt_handles_empty_blocklist() {
        let ctx = TurnContext {
            user_content: "hi".into(),
            assistant_content: "hello".into(),
            blocklist_patterns: vec![],
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("(none)"));
    }

    #[test]
    fn parse_empty_array() {
        let r = parse_response(r#"{"beliefs": []}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_one_belief() {
        let raw = r#"{"beliefs": [
            {"statement": "Prefers Rust", "category": "preference",
             "confidence": 0.8, "trust_class": "inferred",
             "evidence_quote": "I prefer Rust"}
        ]}"#;
        let r = parse_response(raw).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].statement, "Prefers Rust");
        assert_eq!(r[0].trust_class, "inferred");
        assert_eq!(r[0].evidence_quote.as_deref(), Some("I prefer Rust"));
    }

    #[test]
    fn parse_strips_json_fences() {
        let raw = "```json\n{\"beliefs\": []}\n```";
        assert!(parse_response(raw).unwrap().is_empty());
    }

    #[test]
    fn parse_strips_generic_fences() {
        let raw = "```\n{\"beliefs\": []}\n```";
        assert!(parse_response(raw).unwrap().is_empty());
    }

    #[test]
    fn parse_ignores_any_model_emitted_confidence() {
        // The model is no longer asked for confidence, but if it emits one
        // anyway it must be silently ignored, not stored or validated.
        let raw = r#"{"beliefs": [{"statement": "x", "category": "fact",
            "confidence": 1.5, "trust_class": "inferred"}]}"#;
        let r = parse_response(raw).unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn parse_accepts_hypothesized_trust_class() {
        let raw = r#"{"beliefs": [{"statement": "Seems frustrated lately",
            "category": "context", "trust_class": "hypothesized",
            "evidence_quote": "ugh, again"}]}"#;
        let r = parse_response(raw).unwrap();
        assert_eq!(r[0].trust_class, "hypothesized");
    }

    #[test]
    fn parse_rejects_invalid_trust_class() {
        let raw = r#"{"beliefs": [{"statement": "x", "category": "fact",
            "confidence": 0.5, "trust_class": "maybe"}]}"#;
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn parse_rejects_empty_statement() {
        let raw = r#"{"beliefs": [{"statement": "", "category": "fact",
            "confidence": 0.5, "trust_class": "inferred"}]}"#;
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn parse_handles_garbage_gracefully() {
        assert!(parse_response("not json at all").is_err());
    }
}
