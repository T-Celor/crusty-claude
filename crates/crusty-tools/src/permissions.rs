//! Permission rules and the decision order that applies them.

use std::fmt;
use std::path::Path;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crusty_core::config::PermissionsConfig;
use crusty_core::permission::{PermissionDecision, PermissionMode, PermissionPolicy, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext};
use serde_json::Value;

use crate::sandbox::{absolute_path, compile_glob, display_path};

/// One parsed rule: `Bash(cargo test:*)`, `Edit(src/**)`, `WebFetch(domain:docs.rs)`,
/// `mcp__github` (a bare tool name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub tool: String,
    pub specifier: Option<String>,
}

impl Rule {
    /// Parses `Tool` or `Tool(specifier)`. Parentheses must balance and nothing may follow
    /// the closing one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (tool, specifier) = match text.find('(') {
            None => (text, None),
            Some(open) => {
                let close = matching_close(text, open).ok_or_else(|| format!("unbalanced parentheses in `{text}`"))?;
                if close != text.len() - 1 {
                    return Err(format!("unexpected text after `)` in `{text}`"));
                }
                let inner = text[open + 1..close].trim();
                if inner.is_empty() {
                    return Err(format!("empty specifier in `{text}`"));
                }
                (text[..open].trim(), Some(inner.to_owned()))
            }
        };
        if tool.is_empty() {
            return Err(format!("missing tool name in `{text}`"));
        }
        if tool.contains(char::is_whitespace) || tool.contains(')') {
            return Err(format!("invalid tool name in `{text}`"));
        }
        Ok(Rule { tool: tool.to_owned(), specifier })
    }
}

/// Index of the parenthesis closing the one at `open`.
fn matching_close(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.specifier {
            Some(spec) => write!(f, "{}({spec})", self.tool),
            None => f.write_str(&self.tool),
        }
    }
}

#[derive(Debug, Default)]
struct RuleSets {
    mode: PermissionMode,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    deny: Vec<Rule>,
}

/// Decides whether a tool call runs, asks the user, or is refused.
#[derive(Debug)]
pub struct PermissionEngine {
    state: RwLock<RuleSets>,
}

/// How a rule's specifier is compared with a call's subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Command,
    Path,
    Domain,
    Exact,
}

fn kind_of(tool_name: &str) -> Kind {
    match tool_name {
        "Bash" => Kind::Command,
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Glob" | "Grep" | "LS" => Kind::Path,
        "WebFetch" => Kind::Domain,
        _ => Kind::Exact,
    }
}

/// Whether a rule naming `rule_tool` covers a call to `tool_name`.
fn covers(rule_tool: &str, tool_name: &str) -> bool {
    if rule_tool == tool_name {
        return true;
    }
    match rule_tool {
        "Edit" => matches!(tool_name, "Write" | "MultiEdit" | "NotebookEdit"),
        prefix if prefix.starts_with("mcp__") => tool_name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with("__")),
        _ => false,
    }
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits a shell line into simple commands. Separators (`&&`, `||`, `;`, `|`, `&`, newline,
/// parentheses) end a command outside quotes. The bodies of `$( … )`, `<( … )`, `>( … )` and
/// backticks become commands of their own, also inside double quotes. Single quotes are literal.
/// Each part is normalised; empty parts are dropped.
fn split_compound(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut parts = Vec::new();
    scan_shell(&chars, 0, false, &mut parts);
    parts.iter().map(|part| normalize_ws(part)).filter(|part| !part.is_empty()).collect()
}

/// Scans `chars` from `i`, pushing each simple command into `parts`. With `in_subst`, stops
/// after the `)` that closes a `$(` or `<(` and returns the index just past it.
fn scan_shell(chars: &[char], mut i: usize, in_subst: bool, parts: &mut Vec<String>) -> usize {
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let next_is = |i: usize, want: char| chars.get(i) == Some(&want);
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
            current.push(c);
            continue;
        }
        let in_double = quote == Some('"');
        match c {
            '\\' => {
                current.push(c);
                if let Some(&escaped) = chars.get(i) {
                    current.push(escaped);
                    i += 1;
                }
            }
            '"' => {
                quote = if in_double { None } else { Some('"') };
                current.push(c);
            }
            '\'' if !in_double => {
                quote = Some('\'');
                current.push(c);
            }
            '`' => i = scan_backtick(chars, i, parts),
            '$' | '<' | '>' if next_is(i, '(') && (c == '$' || !in_double) => {
                i = scan_shell(chars, i + 1, true, parts);
            }
            _ if in_double => current.push(c),
            '(' => {
                depth += 1;
                parts.push(std::mem::take(&mut current));
            }
            ')' if depth > 0 => {
                depth -= 1;
                parts.push(std::mem::take(&mut current));
            }
            ')' => {
                parts.push(std::mem::take(&mut current));
                if in_subst {
                    return i;
                }
            }
            ';' | '\n' | '|' => parts.push(std::mem::take(&mut current)),
            // `>&` and `&>` are redirections, not separators.
            '&' if !current.ends_with(['<', '>']) && !next_is(i, '>') => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    i
}

/// Scans a backtick substitution whose body starts at `start` and returns the index just past
/// the closing backtick.
fn scan_backtick(chars: &[char], start: usize, parts: &mut Vec<String>) -> usize {
    let mut end = start;
    while end < chars.len() && chars[end] != '`' {
        end += if chars[end] == '\\' { 2 } else { 1 };
    }
    let end = end.min(chars.len());
    scan_shell(&chars[start..end], 0, false, parts);
    (end + 1).min(chars.len())
}

/// What a rule's specifier is compared with: each sub-command of a shell line, or the one
/// subject of any other tool.
fn subjects(tool: &dyn Tool, input: &Value) -> Vec<String> {
    let Some(subject) = tool.permission_subject(input) else {
        return Vec::new();
    };
    if tool.name() == "Bash" {
        let parts = split_compound(&subject);
        if !parts.is_empty() {
            return parts;
        }
    }
    vec![subject]
}

fn specifier_matches(spec: &str, kind: Kind, subject: &str, ctx: &ToolContext) -> bool {
    match kind {
        Kind::Command => command_matches(spec, subject),
        Kind::Path => path_matches(spec, subject, ctx),
        Kind::Domain => domain_matches(spec, subject),
        Kind::Exact => spec == subject,
    }
}

/// `prefix:*` matches the command or any command starting with `prefix` followed by a space.
/// Anything else must equal the normalised command.
fn command_matches(spec: &str, command: &str) -> bool {
    let command = normalize_ws(command);
    match spec.strip_suffix(":*") {
        Some(prefix) => {
            let prefix = normalize_ws(prefix);
            command == prefix || command.strip_prefix(prefix.as_str()).is_some_and(|rest| rest.starts_with(' '))
        }
        None => command == normalize_ws(spec),
    }
}

fn path_matches(spec: &str, subject: &str, ctx: &ToolContext) -> bool {
    let spec = expand_home(spec);
    let Ok(matcher) = compile_glob(&spec) else {
        return false;
    };
    let absolute = absolute_path(ctx, subject);
    let candidate = if Path::new(&spec).is_absolute() || spec.starts_with('/') {
        Some(display_path(&absolute).replace('\\', "/"))
    } else {
        absolute.strip_prefix(&ctx.workspace).ok().map(|rel| rel.to_string_lossy().replace('\\', "/"))
    };
    candidate.is_some_and(|candidate| matcher.is_match(&candidate))
}

/// `~/x` becomes the home directory from the environment.
fn expand_home(spec: &str) -> String {
    let Some(rest) = spec.strip_prefix("~/") else {
        return spec.to_owned();
    };
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    format!("{}/{rest}", home.replace('\\', "/"))
}

/// `domain:example.com` matches the host itself and every subdomain of it.
fn domain_matches(spec: &str, subject: &str) -> bool {
    let (Some(wanted), Some(host)) = (spec.strip_prefix("domain:"), subject.strip_prefix("domain:")) else {
        return spec == subject;
    };
    let wanted = wanted.trim().trim_start_matches('.').to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    host == wanted || host.strip_suffix(wanted.as_str()).is_some_and(|rest| rest.ends_with('.'))
}

/// Shells, interpreters and wrappers: a prefix rule for them would allow any program they run.
const UNSAFE_PREFIXES: [&str; 26] = [
    "sh",
    "bash",
    "zsh",
    "fish",
    "cmd",
    "pwsh",
    "powershell",
    "python",
    "python3",
    "node",
    "deno",
    "ruby",
    "perl",
    "php",
    "env",
    "xargs",
    "sudo",
    "eval",
    "exec",
    "nohup",
    "timeout",
    "watch",
    "busybox",
    "dash",
    "ksh",
    "tcsh",
];

/// Whether `cmd` (possibly a path, possibly `.exe`) names a shell, interpreter or wrapper.
fn is_unsafe_prefix(cmd: &str) -> bool {
    UNSAFE_PREFIXES.contains(&program_name(cmd).as_str())
}

/// Programs that run another program named by their arguments.
const WRAPPERS: [&str; 11] = ["env", "command", "builtin", "exec", "nice", "nohup", "timeout", "ionice", "busybox", "stdbuf", "unbuffer"];

/// A command word normalised for comparison: quotes and a leading `\` removed, the directory
/// removed, lowercase, and a trailing `.exe` removed.
fn program_name(word: &str) -> String {
    let word = word.trim_matches(['"', '\'']).trim_start_matches('\\');
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_owned()
}

/// A word that is safe to put in a `<cmd> <word>:*` rule.
fn is_plain_word(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The command with leading wrappers (`env VAR=x`, `nice -n 5`, `timeout 5s`, ...) removed and
/// its program normalised, so `env rm x` is compared as `rm x`. Wrapper options that take a
/// separate argument are not parsed, so `timeout -s KILL 5 rm` is not unwrapped.
fn unwrapped_command(command: &str) -> String {
    let mut words: Vec<&str> = command.split_whitespace().collect();
    while !words.is_empty() && WRAPPERS.contains(&program_name(words[0]).as_str()) {
        let name = program_name(words.remove(0));
        while let Some(next) = words.first() {
            let numeric =
                matches!(name.as_str(), "timeout" | "nice" | "ionice") && next.chars().all(|c| c.is_ascii_digit() || "smhd.".contains(c));
            let assignment = name == "env" && next.contains('=');
            if next.starts_with('-') || assignment || numeric {
                words.remove(0);
            } else {
                break;
            }
        }
    }
    let Some((first, rest)) = words.split_first() else {
        return String::new();
    };
    std::iter::once(program_name(first)).chain(rest.iter().map(|w| (*w).to_owned())).collect::<Vec<_>>().join(" ")
}

/// Programs whose prefix rule would also cover a wide range of unrelated invocations, so only
/// an exact command or a `<cmd> <plain-subcommand>:*` rule is suggested for them.
const BROAD_PROGRAMS: [&str; 18] = [
    "git", "rg", "grep", "sed", "awk", "find", "sort", "tar", "tree", "uniq", "xargs", "perl", "make", "npm", "npx", "cargo", "docker",
    "kubectl",
];

/// The permission an `Ask` decision would add to the allow list. `level` is the call's risk
/// from `Tool::assess`: a high-risk call gets only its exact command.
fn suggest_rule(tool: &dyn Tool, input: &Value, ctx: &ToolContext, level: RiskLevel) -> Option<String> {
    let name = tool.name();
    if name == "Bash" {
        let raw = tool.permission_subject(input).unwrap_or_default();
        let command = normalize_ws(&raw);
        let parts = split_compound(&command);
        let first = parts.first().cloned().unwrap_or_default();
        let words: Vec<&str> = first.split_whitespace().collect();
        let exact = || if command.is_empty() { "Bash".to_owned() } else { format!("Bash({command})") };
        // A prefix rule would also allow whatever a shell or wrapper runs, and a redirect or
        // separator changes what the command does, so only the exact command is suggested.
        let risky = parts.len() > 1
            || raw.contains(['<', '>', '`', '$', '(', ')', ';', '&', '|', '\n'])
            || words.first().is_some_and(|cmd| is_unsafe_prefix(cmd));
        let dash_option = words.get(1).is_some_and(|arg| arg.starts_with('-'));
        if risky || level >= RiskLevel::High || dash_option {
            return Some(exact()).filter(|rule| Rule::parse(rule).is_ok());
        }
        let broad = |cmd: &str| BROAD_PROGRAMS.contains(&program_name(cmd).as_str());
        return Some(match words.as_slice() {
            [] => "Bash".to_owned(),
            [cmd] if broad(cmd) => exact(),
            [cmd] => format!("Bash({cmd}:*)"),
            [cmd, arg, ..] if broad(cmd) && !is_plain_word(arg) => exact(),
            [cmd, arg, ..] => format!("Bash({cmd} {arg}:*)"),
        });
    }
    if name == "WebFetch" {
        return Some(match tool.permission_subject(input) {
            Some(subject) => format!("WebFetch({subject})"),
            None => name.to_owned(),
        });
    }
    if kind_of(name) == Kind::Path {
        let prefix = match tool.category() {
            ToolCategory::Edit => "Edit",
            ToolCategory::Read => "Read",
            _ => return Some(name.to_owned()),
        };
        let subject = tool.permission_subject(input)?;
        let absolute = absolute_path(ctx, &subject);
        let dir = if absolute.is_dir() { absolute.clone() } else { absolute.parent().map(Path::to_path_buf).unwrap_or(absolute.clone()) };
        let scope = match dir.strip_prefix(&ctx.workspace) {
            Ok(rel) if rel.as_os_str().is_empty() => "**".to_owned(),
            Ok(rel) => format!("{}/**", rel.to_string_lossy().replace('\\', "/")),
            Err(_) => format!("{}/**", display_path(&dir).replace('\\', "/")),
        };
        return Some(format!("{prefix}({scope})"));
    }
    Some(name.to_owned())
}

fn parse_all(rules: &[String], list: &str) -> Result<Vec<Rule>, String> {
    rules.iter().map(|rule| Rule::parse(rule).map_err(|e| format!("permissions.{list}: {e}"))).collect()
}

impl PermissionEngine {
    /// Builds the engine from the `[permissions]` configuration. Malformed rules are errors.
    pub fn from_config(cfg: &PermissionsConfig) -> Result<Self, String> {
        Ok(PermissionEngine {
            state: RwLock::new(RuleSets {
                mode: cfg.mode,
                allow: parse_all(&cfg.allow, "allow")?,
                ask: parse_all(&cfg.ask, "ask")?,
                deny: parse_all(&cfg.deny, "deny")?,
            }),
        })
    }

    fn read(&self) -> RwLockReadGuard<'_, RuleSets> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, RuleSets> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `rule` applies to this call. With `every`, an allow rule must match each
    /// sub-command of a compound shell line; otherwise one match is enough. With `unwrap`, a
    /// shell sub-command also matches in its unwrapped form (deny and ask rules only, so an
    /// allow rule never covers more than its literal text).
    fn matches(&self, rule: &Rule, tool: &dyn Tool, input: &Value, ctx: &ToolContext, every: bool, unwrap: bool) -> bool {
        if !covers(&rule.tool, tool.name()) {
            return false;
        }
        let Some(spec) = rule.specifier.as_deref() else {
            return true;
        };
        let kind = kind_of(tool.name());
        let mut subjects = subjects(tool, input);
        if unwrap && kind == Kind::Command {
            let unwrapped: Vec<String> = subjects.iter().map(|s| unwrapped_command(s)).collect();
            subjects.extend(unwrapped);
        }
        if every {
            !subjects.is_empty() && subjects.iter().all(|s| specifier_matches(spec, kind, s, ctx))
        } else {
            subjects.iter().any(|s| specifier_matches(spec, kind, s, ctx))
        }
    }
}

impl PermissionPolicy for PermissionEngine {
    fn evaluate(&self, tool: &dyn Tool, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        let state = self.read();
        if let Some(rule) = state.deny.iter().find(|r| self.matches(r, tool, input, ctx, false, true)) {
            return PermissionDecision::Deny { reason: format!("denied by rule `{rule}`") };
        }

        if state.mode == PermissionMode::Plan && matches!(tool.category(), ToolCategory::Edit | ToolCategory::Execute) {
            return PermissionDecision::Deny { reason: "plan mode is read-only".to_owned() };
        }

        let risk = tool.assess(input, ctx);
        let allowed = state.allow.iter().any(|r| self.matches(r, tool, input, ctx, true, false));
        let level = risk.level;
        let suggested = || suggest_rule(tool, input, ctx, level);

        if risk.level == RiskLevel::Critical {
            return if allowed {
                PermissionDecision::Ask { reason: format!("CRITICAL: {}", risk.reason), suggested_rule: suggested() }
            } else {
                PermissionDecision::Deny {
                    reason: format!("critical action denied by default: {}; add an allow rule to be asked", risk.reason),
                }
            };
        }

        if let Some(rule) = state.ask.iter().find(|r| self.matches(r, tool, input, ctx, false, true)) {
            return PermissionDecision::Ask { reason: format!("matches ask rule `{rule}`"), suggested_rule: suggested() };
        }

        if risk.level == RiskLevel::High {
            return if allowed {
                PermissionDecision::Allow { reason: "allowed by an allow rule".to_owned() }
            } else {
                PermissionDecision::Ask { reason: risk.reason, suggested_rule: suggested() }
            };
        }

        if allowed {
            return PermissionDecision::Allow { reason: "allowed by an allow rule".to_owned() };
        }
        if state.mode == PermissionMode::BypassPermissions {
            return PermissionDecision::Allow { reason: "bypassPermissions mode".to_owned() };
        }
        if state.mode == PermissionMode::AcceptEdits && tool.category() == ToolCategory::Edit && risk.level <= RiskLevel::Moderate {
            return PermissionDecision::Allow { reason: "acceptEdits mode".to_owned() };
        }
        if risk.level == RiskLevel::Low {
            return PermissionDecision::Allow { reason: "low risk".to_owned() };
        }
        PermissionDecision::Ask { reason: risk.reason, suggested_rule: suggested() }
    }

    fn mode(&self) -> PermissionMode {
        self.read().mode
    }

    fn set_mode(&self, mode: PermissionMode) {
        self.write().mode = mode;
    }

    fn add_allow_rule(&self, rule: &str) -> Result<(), String> {
        let rule = Rule::parse(rule)?;
        let mut state = self.write();
        if !state.allow.contains(&rule) {
            state.allow.push(rule);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        let state = self.read();
        let list = |rules: &[Rule]| {
            if rules.is_empty() { "none".to_owned() } else { rules.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ") }
        };
        format!("mode: {}\nallow: {}\nask: {}\ndeny: {}", state.mode, list(&state.allow), list(&state.ask), list(&state.deny))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use async_trait::async_trait;
    use crusty_core::config::PermissionsConfig;
    use crusty_core::message::ToolSpec;
    use crusty_core::permission::{PermissionDecision::*, RiskAssessment};
    use crusty_core::tool::{ToolError, ToolOutput};
    use serde_json::json;
    use tempfile::tempdir;

    /// A tool whose name, category, risk and subject field are chosen by the test.
    struct FakeTool {
        name: &'static str,
        category: ToolCategory,
        risk: RiskLevel,
        key: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name, "", Value::Null)
        }
        fn category(&self) -> ToolCategory {
            self.category
        }
        fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
            RiskAssessment::new(self.risk, "test risk")
        }
        fn permission_subject(&self, input: &Value) -> Option<String> {
            input.get(self.key)?.as_str().map(str::to_owned)
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(""))
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    fn engine(mode: PermissionMode, allow: &[&str], ask: &[&str], deny: &[&str]) -> PermissionEngine {
        let cfg = PermissionsConfig { mode, allow: strings(allow), ask: strings(ask), deny: strings(deny), ..PermissionsConfig::default() };
        PermissionEngine::from_config(&cfg).unwrap()
    }

    fn bash(risk: RiskLevel) -> FakeTool {
        FakeTool { name: "Bash", category: ToolCategory::Execute, risk, key: "command" }
    }

    fn write_tool() -> FakeTool {
        FakeTool { name: "Write", category: ToolCategory::Edit, risk: RiskLevel::Moderate, key: "file_path" }
    }

    #[test]
    fn rule_parsing() {
        assert_eq!(Rule::parse("Bash(cargo test:*)").unwrap(), Rule { tool: "Bash".into(), specifier: Some("cargo test:*".into()) });
        assert_eq!(Rule::parse("Edit").unwrap(), Rule { tool: "Edit".into(), specifier: None });
        assert_eq!(Rule::parse("Bash(echo (a))").unwrap().specifier.as_deref(), Some("echo (a)"));
        for bad in ["", "Bash(", "Bash(a))", "Bash()", "(x)", "Bash(a)(b)", "Bash x"] {
            assert!(Rule::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn bash_prefix_rules_and_compound_commands() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &["Bash(cargo test:*)", "Bash(git status)"], &[], &["Bash(rm:*)"]);
        let tool = bash(RiskLevel::Moderate);
        let decide = |cmd: &str| e.evaluate(&tool, &json!({ "command": cmd }), &ctx);
        assert!(matches!(decide("cargo test --all"), Allow { .. }));
        assert!(matches!(decide("git   status"), Allow { .. }));
        assert!(matches!(decide("cargo test && cargo test -p x"), Allow { .. }));
        assert!(matches!(decide("cargo build"), Ask { .. }));
        assert!(matches!(decide("cargo test && cargo build"), Ask { .. }));
        assert!(matches!(decide("cargo test; rm -rf x"), Deny { .. }));
    }

    #[test]
    fn shell_separators_and_substitutions_are_separate_commands() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let allow = engine(PermissionMode::Default, &["Bash(ls:*)"], &[], &[]);
        let deny = engine(PermissionMode::Default, &[], &[], &["Bash(rm:*)"]);
        let tool = bash(RiskLevel::Moderate);
        let hidden = ["ls & rm -rf build", "ls; rm x", "ls $(rm x)", "ls `rm x`", "(rm x)", "ls |rm x", "ls\nrm x", "ls <(rm x)"];
        for cmd in hidden {
            let input = json!({ "command": cmd });
            assert!(!matches!(allow.evaluate(&tool, &input, &ctx), Allow { .. }), "{cmd} must not be allowed");
            assert!(matches!(deny.evaluate(&tool, &input, &ctx), Deny { .. }), "{cmd} must be denied");
        }
        assert!(matches!(allow.evaluate(&tool, &json!({"command": "ls -la 2>&1"}), &ctx), Allow { .. }));
        assert!(matches!(allow.evaluate(&tool, &json!({"command": "ls \"a;b\""}), &ctx), Allow { .. }));
        assert!(matches!(allow.evaluate(&tool, &json!({"command": "ls 'x $(rm y)'"}), &ctx), Allow { .. }));
        assert_eq!(split_compound("echo \"$(date)\" `id`"), ["date", "id", "echo \"\""]);
    }

    #[test]
    fn plan_mode_is_checked_before_critical() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Plan, &["Bash(sudo:*)"], &[], &[]);
        let critical = FakeTool { name: "Bash", category: ToolCategory::Execute, risk: RiskLevel::Critical, key: "command" };
        match e.evaluate(&critical, &json!({"command": "sudo x"}), &ctx) {
            Deny { reason } => assert!(reason.contains("plan")),
            other => panic!("expected plan-mode Deny, got {other:?}"),
        }
    }

    #[test]
    fn no_prefix_rule_for_shells_or_redirects() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &[], &[], &[]);
        let tool = bash(RiskLevel::Moderate);
        let suggest = |cmd: &str| match e.evaluate(&tool, &json!({ "command": cmd }), &ctx) {
            Ask { suggested_rule, .. } => suggested_rule,
            other => panic!("expected Ask, got {other:?}"),
        };
        assert_eq!(suggest("python3 x.py").as_deref(), Some("Bash(python3 x.py)"));
        assert_eq!(suggest("/usr/bin/bash -c id").as_deref(), Some("Bash(/usr/bin/bash -c id)"));
        assert_eq!(suggest("ls > out.txt").as_deref(), Some("Bash(ls > out.txt)"));
        assert_eq!(suggest("cat < in.txt").as_deref(), Some("Bash(cat < in.txt)"));
        assert_eq!(suggest("ls; rm x").as_deref(), Some("Bash(ls; rm x)"));
        assert_eq!(suggest("cargo build").as_deref(), Some("Bash(cargo build:*)"));
    }

    #[test]
    fn prefix_rule_respects_word_boundary() {
        assert!(command_matches("cargo test:*", "cargo test"));
        assert!(!command_matches("cargo test:*", "cargo testing"));
    }

    #[test]
    fn edit_rule_covers_write() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &["Edit(src/**)"], &[], &[]);
        let write = write_tool();
        assert!(matches!(e.evaluate(&write, &json!({"file_path": "src/main.rs"}), &ctx), Allow { .. }));
        assert!(matches!(e.evaluate(&write, &json!({"file_path": "docs/a.md"}), &ctx), Ask { .. }));
    }

    #[test]
    fn domain_rule_matches_subdomains() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &["WebFetch(domain:example.com)"], &[], &[]);
        let fetch = FakeTool { name: "WebFetch", category: ToolCategory::Network, risk: RiskLevel::High, key: "url" };
        assert!(matches!(e.evaluate(&fetch, &json!({"url": "domain:docs.example.com"}), &ctx), Allow { .. }));
        assert!(matches!(e.evaluate(&fetch, &json!({"url": "domain:notexample.com"}), &ctx), Ask { .. }));
    }

    #[test]
    fn mcp_server_rule_covers_its_tools() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &["mcp__github"], &[], &[]);
        let github = FakeTool { name: "mcp__github__create_issue", category: ToolCategory::Other, risk: RiskLevel::High, key: "x" };
        let gitlab = FakeTool { name: "mcp__gitlab__create_issue", category: ToolCategory::Other, risk: RiskLevel::High, key: "x" };
        assert!(matches!(e.evaluate(&github, &json!({}), &ctx), Allow { .. }));
        assert!(matches!(e.evaluate(&gitlab, &json!({}), &ctx), Ask { .. }));
    }

    #[test]
    fn deny_wins_over_allow() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &["Bash(rm:*)"], &[], &["Bash(rm:*)"]);
        assert!(matches!(e.evaluate(&bash(RiskLevel::Low), &json!({"command": "rm x"}), &ctx), Deny { .. }));
    }

    #[test]
    fn plan_mode_denies_edits_and_execution() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Plan, &["Edit"], &[], &[]);
        assert!(matches!(e.evaluate(&write_tool(), &json!({"file_path": "a.txt"}), &ctx), Deny { reason } if reason.contains("plan")));
        assert!(matches!(e.evaluate(&bash(RiskLevel::Low), &json!({"command": "ls"}), &ctx), Deny { .. }));
    }

    #[test]
    fn accept_edits_allows_moderate_edits_but_not_execution() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::AcceptEdits, &[], &[], &[]);
        assert!(matches!(e.evaluate(&write_tool(), &json!({"file_path": "a.txt"}), &ctx), Allow { .. }));
        assert!(matches!(e.evaluate(&bash(RiskLevel::Moderate), &json!({"command": "cargo build"}), &ctx), Ask { .. }));
    }

    #[test]
    fn bypass_mode_still_asks_high_and_denies_critical() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::BypassPermissions, &["Bash(deploy:*)"], &[], &[]);
        let high = FakeTool { name: "Delete", category: ToolCategory::Execute, risk: RiskLevel::High, key: "path" };
        assert!(matches!(e.evaluate(&high, &json!({"path": "x"}), &ctx), Ask { .. }));
        assert!(matches!(e.evaluate(&bash(RiskLevel::Low), &json!({"command": "ls"}), &ctx), Allow { .. }));
        assert!(matches!(e.evaluate(&bash(RiskLevel::Critical), &json!({"command": "sudo x"}), &ctx), Deny { .. }));
        match e.evaluate(&bash(RiskLevel::Critical), &json!({"command": "deploy prod"}), &ctx) {
            Ask { reason, .. } => assert!(reason.starts_with("CRITICAL: ")),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn ask_carries_suggested_rule() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &[], &[], &[]);
        match e.evaluate(&bash(RiskLevel::Moderate), &json!({"command": "cargo build --release"}), &ctx) {
            Ask { suggested_rule, .. } => assert_eq!(suggested_rule.as_deref(), Some("Bash(cargo build:*)")),
            other => panic!("expected Ask, got {other:?}"),
        }
        match e.evaluate(&write_tool(), &json!({"file_path": "src/main.rs"}), &ctx) {
            Ask { suggested_rule, .. } => assert_eq!(suggested_rule.as_deref(), Some("Edit(src/**)")),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn add_allow_rule_validates_and_takes_effect() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &[], &[], &[]);
        assert!(e.add_allow_rule("Bash(").is_err());
        e.add_allow_rule("Bash(make:*)").unwrap();
        assert!(matches!(e.evaluate(&bash(RiskLevel::Moderate), &json!({"command": "make all"}), &ctx), Allow { .. }));
        assert!(e.describe().contains("Bash(make:*)"));
    }

    #[test]
    fn suggestions_never_widen_broad_programs() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::Default, &[], &[], &[]);
        let suggest = |tool: &FakeTool, cmd: &str| match e.evaluate(tool, &json!({ "command": cmd }), &ctx) {
            Ask { suggested_rule, .. } => suggested_rule,
            other => panic!("expected Ask, got {other:?}"),
        };
        let moderate = bash(RiskLevel::Moderate);
        assert_eq!(suggest(&moderate, "git status").as_deref(), Some("Bash(git status:*)"));
        assert_eq!(suggest(&moderate, "git").as_deref(), Some("Bash(git)"));
        assert_eq!(suggest(&moderate, "cargo -V").as_deref(), Some("Bash(cargo -V)"));
        assert_eq!(suggest(&moderate, "git --no-pager log").as_deref(), Some("Bash(git --no-pager log)"));
        assert_eq!(suggest(&moderate, "ls -la").as_deref(), Some("Bash(ls -la)"));
        assert_eq!(suggest(&bash(RiskLevel::High), "make all").as_deref(), Some("Bash(make all)"));
    }

    #[test]
    fn deny_matches_unwrapped_program() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let e = engine(PermissionMode::BypassPermissions, &["Bash(rm:*)"], &[], &["Bash(rm:*)"]);
        let tool = bash(RiskLevel::Low);
        for cmd in
            ["/bin/rm x", "rm.exe x", "\"rm\" x", "\\rm x", "env rm x", "busybox rm x", "env FOO=1 nice -n 5 RM x", "timeout 5s rm x"]
        {
            assert!(matches!(e.evaluate(&tool, &json!({ "command": cmd }), &ctx), Deny { .. }), "{cmd} must be denied");
        }
        // Allow rules keep matching the literal text only.
        let allow_only = engine(PermissionMode::Default, &["Bash(ls:*)"], &[], &[]);
        assert!(matches!(allow_only.evaluate(&bash(RiskLevel::Moderate), &json!({ "command": "env ls x" }), &ctx), Ask { .. }));
    }

    #[test]
    fn malformed_config_rule_is_an_error() {
        let cfg = PermissionsConfig { deny: strings(&["Bash(x"]), ..PermissionsConfig::default() };
        assert!(PermissionEngine::from_config(&cfg).is_err());
    }
}
