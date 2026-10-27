//! Native routing, intent detection and auto-approval: keyword heuristics, English and French.

use std::collections::HashSet;

use crusty_core::advisor::{AdviceSource, Intent, IntentKind, Judgement, McpServerSummary, Routing, Tier, Verdict};
use crusty_core::permission::{ApprovalRequest, RiskLevel};

use crate::rank::tokenize;

const RESEARCH_WORDS: &[&str] = &["research", "investigate", "compare", "survey"];
const RESEARCH_PHRASES: &[&str] = &["state of the art", "evaluate options"];
const HARD_WORDS: &[&str] = &[
    "architecture",
    "architectural",
    "refactor",
    "refactoring",
    "concurrency",
    "concurrent",
    "race",
    "deadlock",
    "security",
    "vulnerability",
    "vulnerabilities",
    "migrate",
    "migration",
    "performance",
    "optimize",
    "optimise",
    "distributed",
];
const HARD_PHRASES: &[&str] = &["multi file", "race condition"];
const TRIVIAL_WORDS: &[&str] = &["typo", "rename", "format", "version", "list"];
const TRIVIAL_PHRASES: &[&str] = &["what is"];

const STARTERS: &[&str] =
    &["what", "why", "how", "when", "where", "which", "who", "explain", "describe", "est-ce", "pourquoi", "comment", "quoi"];
const IMPERATIVES: &[&str] = &[
    "add",
    "fix",
    "create",
    "implement",
    "run",
    "refactor",
    "delete",
    "write",
    "build",
    "deploy",
    "install",
    "update",
    "remove",
    "rename",
    "ajoute",
    "ajouter",
    "corrige",
    "corriger",
    "crée",
    "créer",
    "lance",
    "lancer",
    "supprime",
    "supprimer",
    "écris",
    "écrire",
];

/// Number of distinct words and phrases of a list that occur in the prompt.
fn hits(words: &HashSet<String>, joined: &str, word_list: &[&str], phrase_list: &[&str]) -> usize {
    word_list.iter().filter(|w| words.contains(**w)).count() + phrase_list.iter().filter(|p| joined.contains(**p)).count()
}

/// Confidence from the amount of evidence: 0.55 with none, up to 0.75.
fn confidence(evidence: usize) -> f64 {
    (0.55 + 0.05 * evidence as f64).min(0.75)
}

/// Model tier for a prompt, from keywords, length and code blocks.
///
/// Precedence: `Research`, then `Hard` (keywords, over 400 words or over 3 code blocks),
/// then `Trivial` (under 12 words, no code, a trivial keyword), else `Routine`.
pub fn route(prompt: &str) -> Routing {
    let words: HashSet<String> = tokenize(prompt).into_iter().collect();
    let joined = tokenize(prompt).join(" ");
    let word_count = prompt.split_whitespace().count();
    let code_blocks = prompt.matches("```").count() / 2;

    let research = hits(&words, &joined, RESEARCH_WORDS, RESEARCH_PHRASES);
    if research > 0 {
        return routing(Tier::Research, research);
    }
    let hard = hits(&words, &joined, HARD_WORDS, HARD_PHRASES) + usize::from(word_count > 400) + usize::from(code_blocks > 3);
    if hard > 0 {
        return routing(Tier::Hard, hard);
    }
    if word_count < 12 && code_blocks == 0 {
        let trivial = hits(&words, &joined, TRIVIAL_WORDS, TRIVIAL_PHRASES);
        if trivial > 0 {
            return routing(Tier::Trivial, trivial);
        }
    }
    routing(Tier::Routine, 0)
}

fn routing(tier: Tier, evidence: usize) -> Routing {
    Routing { tier, confidence: confidence(evidence), source: AdviceSource::Native }
}

/// Intent of a prompt, and relevance of each MCP server to it.
///
/// `Question` when the prompt starts with a question word or ends with `?`, and has no
/// imperative verb. `Action` when it has an imperative verb and no question signal. `Mixed`
/// when both are present, or when neither is (with low confidence).
pub fn intent(prompt: &str, servers: &[McpServerSummary]) -> Intent {
    let words = tokenize(prompt);
    let first = prompt.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric() && c != '-').to_lowercase();
    let starter = STARTERS.contains(&first.as_str());
    let ends_question = prompt.trim_end().ends_with(['?', '？']);
    let question = starter || ends_question;
    let action_hits = words.iter().filter(|w| IMPERATIVES.contains(&w.as_str())).count();
    let action = action_hits > 0;

    let (kind, confidence) = match (question, action) {
        (true, false) if starter && ends_question => (IntentKind::Question, 0.85),
        (true, false) => (IntentKind::Question, 0.7),
        (false, true) => (IntentKind::Action, (0.65 + 0.05 * action_hits as f64).min(0.9)),
        (true, true) => (IntentKind::Mixed, 0.55),
        (false, false) => (IntentKind::Mixed, 0.3),
    };

    let prompt_terms: HashSet<String> = words.into_iter().filter(|w| w.chars().count() >= 3).collect();
    let raw: Vec<f64> = servers
        .iter()
        .map(|s| {
            let mut terms: HashSet<String> = tokenize(&s.name).into_iter().collect();
            for t in &s.tools {
                terms.extend(tokenize(t));
            }
            terms.retain(|t| t.chars().count() >= 3);
            prompt_terms.intersection(&terms).count() as f64
        })
        .collect();
    let max = raw.iter().copied().fold(0.0_f64, f64::max);
    let server_relevance = raw.iter().map(|r| if max > 0.0 { r / max } else { 0.0 }).collect();

    Intent { kind, confidence, server_relevance, source: AdviceSource::Native }
}

/// `auto` mode: low risk is allowed, moderate is allowed with less confidence, and high or
/// critical risk always needs a human.
pub fn auto_approve(request: &ApprovalRequest) -> Judgement {
    match request.risk {
        RiskLevel::Low => Judgement::new(Verdict::Allow, 0.9, "low risk", AdviceSource::Native),
        RiskLevel::Moderate => Judgement::new(Verdict::Allow, 0.65, "moderate risk", AdviceSource::Native),
        RiskLevel::High | RiskLevel::Critical => {
            Judgement::new(Verdict::Review, 1.0, format!("{} risk needs a human", request.risk), AdviceSource::Native)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(prompt: &str) -> Tier {
        route(prompt).tier
    }

    #[test]
    fn routes_by_keywords_and_size() {
        let long = "word ".repeat(401);
        let cases: Vec<(String, Tier)> = vec![
            ("Research the state of the art in vector databases".into(), Tier::Research),
            ("Please investigate why the cache misses".into(), Tier::Research),
            ("Compare tokio and async-std".into(), Tier::Research),
            ("Research the security model".into(), Tier::Research),
            ("Refactor the parser across all crates".into(), Tier::Hard),
            ("Fix the race condition in the scheduler".into(), Tier::Hard),
            ("Design the distributed architecture for sync".into(), Tier::Hard),
            ("Make the change across multi-file modules".into(), Tier::Hard),
            (long, Tier::Hard),
            ("Rename this variable".into(), Tier::Trivial),
            ("What is the version of rustc?".into(), Tier::Trivial),
            ("Quelle est la version de rustc ?".into(), Tier::Trivial),
            ("Fix a typo in the README".into(), Tier::Trivial),
            ("Add a function that parses dates and handles the edge cases for timezones correctly please thanks".into(), Tier::Routine),
            ("Ajoute un test pour le parser des dates".into(), Tier::Routine),
        ];
        for (prompt, expected) in cases {
            let short: String = prompt.chars().take(60).collect();
            assert_eq!(tier(&prompt), expected, "{short}");
        }
    }

    #[test]
    fn code_blocks_push_to_hard_and_block_trivial() {
        let code = "```rs\nfn a() {}\n```\n";
        let four = code.repeat(4);
        assert_eq!(tier(&four), Tier::Hard);
        let one = format!("rename this {code}");
        assert_eq!(tier(&one), Tier::Routine, "code blocks rule out trivial");
    }

    #[test]
    fn confidence_grows_with_evidence() {
        assert!((route("hello there friend").confidence - 0.55).abs() < 1e-12);
        assert!(route("architecture security performance").confidence > route("architecture").confidence);
        assert!(route("architecture security performance").confidence <= 0.75);
    }

    #[test]
    fn intent_table() {
        let cases: Vec<(&str, IntentKind)> = vec![
            ("Pourquoi le cache échoue ?", IntentKind::Question),
            ("How does the router pick a tier?", IntentKind::Question),
            ("Comment fonctionne le cache", IntentKind::Question),
            ("Ajoute un test pour le parser", IntentKind::Action),
            ("Écris la documentation", IntentKind::Action),
            ("Fix the bug", IntentKind::Action),
            ("Explain this and then fix it", IntentKind::Mixed),
            ("Why did you delete the file?", IntentKind::Mixed),
            ("hello there", IntentKind::Mixed),
        ];
        for (prompt, expected) in cases {
            assert_eq!(intent(prompt, &[]).kind, expected, "{prompt}");
        }
        assert!(intent("hello there", &[]).confidence < 0.5);
    }

    #[test]
    fn server_relevance_by_token_overlap() {
        let servers = vec![
            McpServerSummary { name: "github".into(), tools: vec!["create_issue".into(), "list_pull_requests".into()] },
            McpServerSummary { name: "filesystem".into(), tools: vec!["read_file".into(), "write_file".into()] },
        ];
        let i = intent("create an issue on github", &servers);
        assert_eq!(i.server_relevance, vec![1.0, 0.0]);
        assert_eq!(i.source, AdviceSource::Native);
        let none = intent("unrelated words here", &servers);
        assert_eq!(none.server_relevance, vec![0.0, 0.0]);
        assert!(intent("x", &[]).server_relevance.is_empty());
    }

    #[test]
    fn auto_approve_per_risk() {
        let request = |risk| ApprovalRequest {
            id: "1".into(),
            agent_id: "a".into(),
            tool: "Bash".into(),
            summary: "Bash(x)".into(),
            risk,
            reason: String::new(),
            suggested_rule: None,
            preview: String::new(),
        };
        let low = auto_approve(&request(RiskLevel::Low));
        assert_eq!((low.verdict, low.confidence), (Verdict::Allow, 0.9));
        let moderate = auto_approve(&request(RiskLevel::Moderate));
        assert_eq!((moderate.verdict, moderate.confidence), (Verdict::Allow, 0.65));
        for risk in [RiskLevel::High, RiskLevel::Critical] {
            let j = auto_approve(&request(risk));
            assert_eq!((j.verdict, j.confidence), (Verdict::Review, 1.0));
        }
    }
}
