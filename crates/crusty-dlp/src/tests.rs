//! Table-driven tests for detection, masking and request redaction.

use std::path::Path;

use async_trait::async_trait;
use crusty_core::advisor::{AdviceSource, Advisor, Intent, Judgement, McpServerSummary, NoAdvisor, Routing, Scores, SecretCandidate};
use crusty_core::config::DlpConfig;
use crusty_core::message::{ChatRequest, ContentBlock, Extra, Message, Role};
use crusty_core::permission::ApprovalRequest;
use crusty_core::tool::PlannedChange;
use crusty_core::vault::{RedactionKind, SecretVault};
use serde_json::json;

use crate::{mask_text, redact_request, scan};

fn cfg() -> DlpConfig {
    DlpConfig::default()
}

/// The text each finding covers, in order.
fn values(text: &str, cfg: &DlpConfig) -> Vec<String> {
    scan(text, cfg).iter().filter_map(|f| text.get(f.start..f.end)).map(str::to_owned).collect()
}

/// The (text, kind) pairs of each finding, in order.
fn kinded(text: &str, cfg: &DlpConfig) -> Vec<(String, RedactionKind)> {
    scan(text, cfg).iter().filter_map(|f| text.get(f.start..f.end).map(|v| (v.to_owned(), f.kind))).collect()
}

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";

#[test]
fn known_secret_formats_are_masked_exactly() {
    let cases: &[(&str, &str)] = &[
        ("use sk-ant-api03-Qx7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg9 now", "sk-ant-api03-Qx7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg9"),
        ("OPENAI=sk-proj-abcdefghijklmnopqrstuvwx12", "sk-proj-abcdefghijklmnopqrstuvwx12"),
        ("ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5", "ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5"),
        ("AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7EXAMPLE"),
        ("AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q", "AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q"),
        // Split so secret scanners do not flag this fake Stripe-style fixture.
        (concat!("sk_", "live_4eC39HqLyjWDarjtT1zdp7dc"), concat!("sk_", "live_4eC39HqLyjWDarjtT1zdp7dc")),
        ("glpat-xK3nP9qLr2Tv8Wz1Bm4Dc", "glpat-xK3nP9qLr2Tv8Wz1Bm4Dc"),
        ("xoxb-1234567890-abcdefghij", "xoxb-1234567890-abcdefghij"),
        ("SG.abcdefghijklmnop.qrstuvwxyzABCDEF", "SG.abcdefghijklmnop.qrstuvwxyzABCDEF"),
        ("hf_abcdefghijklmnopqrstuvwxyz1234", "hf_abcdefghijklmnopqrstuvwxyz1234"),
        ("npm_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5", "npm_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5"),
        ("pypi-AgEabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP", "pypi-AgEabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP"),
        (JWT, JWT),
        (PEM, PEM),
        ("postgres://admin:S3cretPass@db.internal:5432/app", "S3cretPass"),
        ("Authorization: Bearer abc123def456ghi789", "abc123def456ghi789"),
        ("Server=db;Password=Hunter22;Database=x", "Hunter22"),
        ("API_KEY=q8Zr4tYp2LmN", "q8Zr4tYp2LmN"),
        ("\"apiKey\": \"k9Lp2Qw7Zx4Vn8Rt\"", "k9Lp2Qw7Zx4Vn8Rt"),
    ];
    for (text, want) in cases {
        assert_eq!(values(text, &cfg()), vec![want.to_string()], "input: {text}");
    }
}

#[test]
fn near_miss_secret_formats_are_not_masked() {
    let cases = ["sk-short", "task-abcdefghijklmnopqrstuvwxyz", "ghp_short", "AKIA123", "xoxb-short", "hf_short"];
    for text in cases {
        assert!(scan(text, &cfg()).is_empty(), "input: {text}");
    }
}

#[test]
fn credential_assignments_skip_placeholders_and_references() {
    let cases = [
        "API_KEY=${API_KEY}",
        "password: $DB_PASS",
        "secret = <your-secret-here>",
        "token = process.env.TOKEN",
        "password=changeme",
        "password=\"xxxxxxxxxxxx\"",
        "secret=aaaaaaaaaaaa",
        "token=getToken()",
        "api_key = os.environ[\"KEY\"]",
        "password={{ pw }}",
        "token=placeholder_value",
        "secret=short",
        "password: your_password_here",
    ];
    for text in cases {
        assert!(scan(text, &cfg()).is_empty(), "input: {text}");
    }
}

#[test]
fn ambiguous_assignment_and_token_have_low_confidence() {
    let found = scan("\"apiKey\": \"hunter2hunter2\"", &cfg());
    assert_eq!(found.len(), 1);
    assert!((found[0].confidence - 0.6).abs() < 1e-9);
    assert_eq!(found[0].rule, "credential_assignment");

    let found = scan("blob Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg end", &cfg());
    assert_eq!(found.len(), 1);
    assert!((found[0].confidence - 0.5).abs() < 1e-9);
}

#[test]
fn high_entropy_rule_skips_hashes_ids_paths_and_identifiers() {
    let cases = [
        "commit 9fceb02d0ae598e95dc970b74767f19372d61af8",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "550e8400-e29b-41d4-a716-446655440000",
        "getUserAccountSettingsFromDatabase",
        "this_is_a_very_long_identifier_name",
        "/var/lib/app/cache/v2/build/Release/x64",
        "https://example.com/path/to/some/resource",
    ];
    for text in cases {
        assert!(scan(text, &cfg()).is_empty(), "input: {text}");
    }
}

#[test]
fn pii_is_detected_with_kind() {
    let cases: &[(&str, &str, RedactionKind)] = &[
        ("jane.doe@acme.io", "jane.doe@acme.io", RedactionKind::Email),
        ("+33 6 12 34 56 78", "+33 6 12 34 56 78", RedactionKind::Phone),
        ("06 12 34 56 78", "06 12 34 56 78", RedactionKind::Phone),
        ("(555) 123-4567", "(555) 123-4567", RedactionKind::Phone),
        ("call +44 20 7946 0958 now", "+44 20 7946 0958", RedactionKind::Phone),
        ("4111 1111 1111 1111", "4111 1111 1111 1111", RedactionKind::Card),
        ("4111111111111111", "4111111111111111", RedactionKind::Card),
        ("378282246310005", "378282246310005", RedactionKind::Card),
        ("DE89 3704 0044 0532 0130 00", "DE89 3704 0044 0532 0130 00", RedactionKind::Iban),
        ("DE89370400440532013000", "DE89370400440532013000", RedactionKind::Iban),
    ];
    for (text, want, kind) in cases {
        assert_eq!(kinded(text, &cfg()), vec![(want.to_string(), *kind)], "input: {text}");
    }
}

#[test]
fn pii_negatives_versions_dates_ips_and_bad_checksums() {
    let cases = [
        "version 1.22.333",
        "v2.10.3",
        "10.20.30.40",
        "192.168.100.200",
        "2026-10-27",
        "12345678901234",
        "x+12345678",
        "4111-1111-1111-1112",
    ];
    for text in cases {
        assert!(scan(text, &cfg()).is_empty(), "input: {text}");
    }
    let bad_iban = scan("DE89 3704 0044 0532 0130 01", &cfg());
    assert!(bad_iban.iter().all(|f| f.kind != RedactionKind::Iban));
}

#[test]
fn allowed_emails_are_not_masked() {
    let cases =
        ["user@example.com", "ops@example.org", "noreply@anthropic.com", "123+bot@users.noreply.github.com", "git@github.com:org/repo.git"];
    for text in cases {
        assert!(scan(text, &cfg()).is_empty(), "input: {text}");
    }
}

#[test]
fn config_flags_and_allow_values_are_honoured() {
    let text = "jane.doe@acme.io and ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5";
    let only_secrets = DlpConfig { pii: false, ..cfg() };
    assert_eq!(values(text, &only_secrets), vec!["ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5".to_string()]);
    let only_pii = DlpConfig { secrets: false, ..cfg() };
    assert_eq!(values(text, &only_pii), vec!["jane.doe@acme.io".to_string()]);
    let allowed = DlpConfig { allow_values: vec!["jane.doe@acme.io".into()], ..cfg() };
    assert_eq!(values(text, &allowed), vec!["ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5".to_string()]);
    let disabled = DlpConfig { enabled: false, ..cfg() };
    assert!(scan(text, &disabled).is_empty());
}

#[test]
fn placeholders_are_never_masked_again() {
    let text = "see ⟦EMAIL_1⟧ and ⟦SECRET_2⟧ and password=⟦SECRET_3⟧ and ⟦PHONE_1⟧";
    assert!(scan(text, &cfg()).is_empty());
}

#[test]
fn overlapping_jwt_in_header_yields_one_finding() {
    let text = format!("Authorization: Bearer {JWT}");
    assert_eq!(values(&text, &cfg()), vec![JWT.to_string()]);
}

#[test]
fn findings_are_sorted_and_non_overlapping() {
    let text = "a jane.doe@acme.io b 4111 1111 1111 1111 c Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg";
    let found = scan(text, &cfg());
    assert_eq!(found.len(), 3);
    for pair in found.windows(2) {
        assert!(pair[0].end <= pair[1].start);
    }
}

#[test]
fn mask_text_uses_stable_placeholders() {
    let vault = SecretVault::default();
    let text = "mail jane.doe@acme.io and jane.doe@acme.io";
    let found = scan(text, &cfg());
    let (masked, counts) = mask_text(text, &found, &vault);
    assert_eq!(masked, "mail ⟦EMAIL_1⟧ and ⟦EMAIL_1⟧");
    assert_eq!(counts.pii.len(), 1);
    assert!(counts.secrets.is_empty());
    assert_eq!(vault.rehydrate(&masked), text);
    assert_eq!(vault.value_of("⟦EMAIL_1⟧").as_deref(), Some("jane.doe@acme.io"));
}

/// Advisor that returns fixed scores, or abstains when `values` is `None`.
struct Scripted {
    values: Option<Vec<f64>>,
}

#[async_trait]
impl Advisor for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn judge_command(&self, command: &str, cwd: &Path, goal: &str) -> Judgement {
        NoAdvisor.judge_command(command, cwd, goal).await
    }
    async fn judge_write(&self, change: &PlannedChange, goal: &str) -> Judgement {
        NoAdvisor.judge_write(change, goal).await
    }
    async fn secret_probability(&self, _candidates: &[SecretCandidate]) -> Scores {
        match &self.values {
            Some(v) => Scores { values: v.clone(), source: AdviceSource::Laya },
            None => Scores::none(),
        }
    }
    async fn relevance(&self, query: &str, items: &[String]) -> Scores {
        NoAdvisor.relevance(query, items).await
    }
    async fn error_scores(&self, blocks: &[String]) -> Scores {
        NoAdvisor.error_scores(blocks).await
    }
    async fn route(&self, prompt: &str) -> Routing {
        NoAdvisor.route(prompt).await
    }
    async fn intent(&self, prompt: &str, servers: &[McpServerSummary]) -> Intent {
        NoAdvisor.intent(prompt, servers).await
    }
    async fn auto_approve(&self, request: &ApprovalRequest, goal: &str) -> Judgement {
        NoAdvisor.auto_approve(request, goal).await
    }
}

const AMBIGUOUS_TEXT: &str = "blob Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg end";

fn request_with_user_text(text: &str) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        max_tokens: 1,
        system: Vec::new(),
        messages: vec![Message { role: Role::User, content: vec![ContentBlock::text(text)] }],
        tools: Vec::new(),
        temperature: None,
        stream: None,
        extra: Extra::new(),
    }
}

fn user_text(req: &ChatRequest) -> String {
    match req.messages.first().and_then(|m| m.content.first()) {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn advisor_decides_ambiguous_findings() {
    let vault = SecretVault::default();
    let cases: &[(Option<Vec<f64>>, bool, AdviceSource)] = &[
        (Some(vec![0.9]), true, AdviceSource::Laya),
        (Some(vec![0.1]), false, AdviceSource::Laya),
        // Abstaining: masked when at or above cfg.ambiguous_threshold (0.5 here).
        (None, true, AdviceSource::None),
        // Unusable scores fall back to the threshold rule, as an abstention does.
        (Some(vec![]), true, AdviceSource::Laya),
    ];
    for (values, masked, source) in cases {
        let mut req = request_with_user_text(AMBIGUOUS_TEXT);
        let advisor = Scripted { values: values.clone() };
        let report = redact_request(&mut req, &vault, &advisor, &cfg()).await;
        assert_eq!(report.ambiguous_checked, 1, "{values:?}");
        assert_eq!(report.ambiguous_masked, usize::from(*masked), "{values:?}");
        assert_eq!(report.source, *source, "{values:?}");
        assert_eq!(user_text(&req).contains("⟦SECRET_1⟧"), *masked, "{values:?}");
    }
}

#[tokio::test]
async fn abstaining_advisor_masks_ambiguous_findings_at_the_threshold() {
    let vault = SecretVault::default();
    let mut req = request_with_user_text("\"apiKey\": \"hunter2hunter2\" and blob Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg end");
    let report = redact_request(&mut req, &vault, &NoAdvisor, &cfg()).await;
    assert_eq!(report.ambiguous_checked, 2);
    assert_eq!(report.ambiguous_masked, 2, "0.6 and 0.5 are both at or above the 0.5 threshold");
    let text = user_text(&req);
    assert!(!text.contains("hunter2hunter2"));
    assert!(!text.contains("Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg"));
}

#[tokio::test]
async fn redact_request_covers_blocks_and_skips_thinking_and_images() {
    let vault = SecretVault::default();
    let key = "sk-ant-api03-Qx7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg9";
    let thinking = format!("reasoning mentions {key}");
    let image_data = "AKIAIOSFODNN7EXAMPLE";
    let mut req = ChatRequest {
        model: "m".into(),
        max_tokens: 1,
        system: vec![ContentBlock::text(format!("Use key {key}."))],
        messages: vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::text("Mail jane.doe@acme.io"), ContentBlock::image_png_base64(image_data)],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking { thinking: thinking.clone(), signature: Some("sig".into()), extra: Extra::new() },
                    ContentBlock::tool_use(
                        "t1",
                        "bash",
                        json!({"cmd": "curl -H 'Authorization: Bearer abc123def456ghi789' x", "list": ["4111 1111 1111 1111"], "n": 3}),
                    ),
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", vec![ContentBlock::text("password=Hunter22abc")], false)],
            },
        ],
        tools: Vec::new(),
        temperature: None,
        stream: None,
        extra: Extra::new(),
    };
    let report = redact_request(&mut req, &vault, &NoAdvisor, &cfg()).await;
    assert_eq!(report.secrets, 3);
    assert_eq!(report.pii, 2);
    let wire = serde_json::to_string(&req).expect("serializes");
    assert!(!wire.contains("jane.doe@acme.io"));
    assert!(!wire.contains("abc123def456ghi789"));
    assert!(!wire.contains("Hunter22abc"));
    assert!(!wire.contains("4111 1111 1111 1111"));
    assert!(wire.contains(key) && wire.contains(&thinking), "thinking must be untouched");
    assert!(wire.contains(image_data), "images must be untouched");
    assert!(wire.contains("\"n\":3"), "non-string leaves are kept");
}

#[tokio::test]
async fn redact_request_is_idempotent_and_keeps_placeholders_stable() {
    let vault = SecretVault::default();
    let mut first = request_with_user_text("Mail jane.doe@acme.io key ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5");
    redact_request(&mut first, &vault, &NoAdvisor, &cfg()).await;
    let once = serde_json::to_string(&first).expect("serializes");
    let second_report = redact_request(&mut first, &vault, &NoAdvisor, &cfg()).await;
    assert_eq!(second_report.secrets + second_report.pii, 0);
    assert_eq!(serde_json::to_string(&first).expect("serializes"), once);

    let mut other = request_with_user_text("Again jane.doe@acme.io");
    redact_request(&mut other, &vault, &NoAdvisor, &cfg()).await;
    assert_eq!(user_text(&other), "Again ⟦EMAIL_1⟧");
}

#[tokio::test]
async fn disabled_config_changes_nothing() {
    let vault = SecretVault::default();
    let mut req = request_with_user_text("Mail jane.doe@acme.io");
    let before = serde_json::to_string(&req).expect("serializes");
    let disabled = DlpConfig { enabled: false, ..cfg() };
    let report = redact_request(&mut req, &vault, &NoAdvisor, &disabled).await;
    assert_eq!(report, Default::default());
    assert_eq!(serde_json::to_string(&req).expect("serializes"), before);
}

#[tokio::test]
async fn abstaining_advisor_masks_ambiguous_findings_at_the_configured_threshold() {
    use crusty_core::advisor::NoAdvisor;
    let text = "blob Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg end";
    // Confidence 0.5 is masked at the default threshold (0.5) and kept below a higher one.
    let mut masked = ChatRequest::new("m", 100);
    masked.messages.push(Message::user_text(text));
    let report = redact_request(&mut masked, &SecretVault::default(), &NoAdvisor, &cfg()).await;
    assert_eq!(report.ambiguous_masked, 1);
    assert!(!serde_json::to_string(&masked).unwrap().contains("Zx9Qa7Lm2Rt8Vb4Nc6Hj1Kp3Ws5Dg"));

    let strict = DlpConfig { ambiguous_threshold: 0.55, ..cfg() };
    let mut kept = ChatRequest::new("m", 100);
    kept.messages.push(Message::user_text(text));
    let report = redact_request(&mut kept, &SecretVault::default(), &NoAdvisor, &strict).await;
    assert_eq!(report.ambiguous_masked, 0);
}

#[tokio::test]
async fn tool_input_object_keys_are_masked_too() {
    use crusty_core::advisor::NoAdvisor;
    let mut req = ChatRequest::new("m", 100);
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use("t1", "Write", json!({"AKIAIOSFODNN7EXAMPLE": "value"}))],
    });
    redact_request(&mut req, &SecretVault::default(), &NoAdvisor, &cfg()).await;
    let text = serde_json::to_string(&req).unwrap();
    assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "secret in a tool input key is masked: {text}");
    assert!(text.contains("value"), "values without secrets are kept");
}
