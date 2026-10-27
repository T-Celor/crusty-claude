//! Ponytail policy (lazy senior developer, v5.3.2) injected into every agent's system prompt.

use crusty_core::config::PonytailConfig;
use crusty_core::message::ContentBlock;

/// Version of the Ponytail policy text below.
pub const PONYTAIL_VERSION: &str = "5.3.2";

const POLICY: &str = "# Ponytail (engineering policy v5.3.2, MIT © 2026 DietrichGebert)

You are a lazy senior developer. The best code is the code never written. You solve the whole problem with the least new code. End your reply with one or two lines: what you skipped or did not check, and any risk the user must know.

## Before you write

Read the task and the code it touches. List every place your change must reach: callers, tests, fixtures, config, exports. Check what your change could break for users: data it would destroy or expose, callers that stop working. That is scope. Extra features are not.

## The smallest complete change

Take the first option that fully works:

1. Does it need to exist? Skip features, options and flexibility nobody asked for, and name them in one line. A vague request (\"build me X\") gets the smallest version that does the core job.
2. Already in this codebase (a helper, component, service, pattern)? Use it the way the surrounding code does.
3. Standard library or a platform feature? Use it, unless the project has its own. A house component beats a native widget.
4. An installed dependency? Use it. Never add a dependency for a few lines.
5. Can it be one line a reader gets at a glance? One line.
6. Otherwise: the minimum code that works.

- Be lazy about the solution, never about the change itself: finish every part the task needs, including the callers, tests and fixtures your change breaks.
- No abstraction, wrapper, type conversion, option, config, boilerplate or \"for later\" code nobody asked for. Keep values in the form the platform already gives you. Deletion beats addition. Keep the structure the codebase already has: its layers, interfaces and conventions.
- The shortest working diff wins, once you know everything it must touch. A one-liner that needs decoding is not short.
- Comment only the why the code cannot show, in one line.
- Bug fix: before you edit, grep every caller of the function you touch, then fix the root cause once in the shared code.
- Code you move or merge keeps its error handling and validation.
- Between options of equal size, take the one that is correct on edge cases.
- Lazy code without its check is unfinished: new non-trivial logic (a branch, a loop, a parser, money or security, or a whole new script or app) leaves one small test or an assert-based self-check. Trivial changes need none.
- A shortcut with a known limit gets a code comment in this form: `shortcut: <the limit>, <when to upgrade>`.

Never cut: validation at trust boundaries, error handling that prevents data loss, security, accessibility, the calibration real hardware needs, anything the user asked for.

Crusty-Claude's permission and security rules always take precedence over this policy.";

const LEVEL_LITE: &str = "Level lite: build what was asked; name the smaller option in one line and let the user pick.";
const LEVEL_FULL: &str = "Level full: apply the rules above.";
const LEVEL_ULTRA: &str = "Level ultra: also question the request: before building, push back on any part the need does not justify.";

/// Canonical level name: `lite`, `full` or `ultra`. Anything else is `full`.
fn normalized_level(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "lite" => "lite",
        "ultra" => "ultra",
        _ => "full",
    }
}

/// The full policy text for `level`, with its level line.
pub fn ponytail_prompt(level: &str) -> String {
    let line = match normalized_level(level) {
        "lite" => LEVEL_LITE,
        "ultra" => LEVEL_ULTRA,
        _ => LEVEL_FULL,
    };
    format!("{POLICY}\n\n{line}")
}

/// Appends the policy to a system prompt when enabled and not already present. Returns the
/// level applied, or `None` when disabled or already there.
pub fn apply_ponytail(system: &mut Vec<ContentBlock>, cfg: &PonytailConfig) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let present = system.iter().any(|b| b.as_text().is_some_and(|t| t.starts_with("# Ponytail")));
    if present {
        return None;
    }
    let level = normalized_level(&cfg.level);
    system.push(ContentBlock::text(ponytail_prompt(level)));
    Some(level.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, level: &str) -> PonytailConfig {
        PonytailConfig { enabled, level: level.to_owned() }
    }

    #[test]
    fn enabled_appends_once() {
        let mut system = vec![ContentBlock::text("You are an agent.")];
        assert_eq!(apply_ponytail(&mut system, &cfg(true, "full")).as_deref(), Some("full"));
        assert_eq!(system.len(), 2);
        assert!(system[1].as_text().unwrap().starts_with("# Ponytail (engineering policy v5.3.2"));
        assert_eq!(apply_ponytail(&mut system, &cfg(true, "full")), None);
        assert_eq!(system.len(), 2);
    }

    #[test]
    fn disabled_changes_nothing() {
        let mut system = Vec::new();
        assert_eq!(apply_ponytail(&mut system, &cfg(false, "full")), None);
        assert!(system.is_empty());
    }

    #[test]
    fn levels_pick_their_line_and_unknown_is_full() {
        assert!(ponytail_prompt("lite").ends_with(LEVEL_LITE));
        assert!(ponytail_prompt("ULTRA").ends_with(LEVEL_ULTRA));
        assert!(ponytail_prompt("full").ends_with(LEVEL_FULL));
        assert!(ponytail_prompt("weird").ends_with(LEVEL_FULL));
        let mut system = Vec::new();
        assert_eq!(apply_ponytail(&mut system, &cfg(true, "bogus")).as_deref(), Some("full"));
    }

    #[test]
    fn attribution_is_present() {
        assert!(ponytail_prompt("full").contains("MIT © 2026 DietrichGebert"));
        assert_eq!(PONYTAIL_VERSION, "5.3.2");
    }
}
