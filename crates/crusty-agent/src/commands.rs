//! Built-in slash commands and custom commands from `.crusty/commands` / `.claude/commands`.

use std::fmt::Write as _;

use crusty_core::checkpoint::RewindScope;
use crusty_core::config::{ModelOverride, OptimizerMode, ProxyUpstream};
use crusty_core::event::{Stage, TodoStatus};
use crusty_core::tokens::{estimate_blocks, estimate_messages};
use crusty_core::{AppEvent, CancellationToken, ModelRef, PermissionMode};
use crusty_session::commands::CustomCommand;
use crusty_session::{memory, transcript};
use serde_json::json;

use crate::controller::Controller;
use crate::engine::{ORCHESTRATOR_ID, orchestrator_spec};
use crate::runtime::Runtime;
use crate::{compact, display_path, find_in_path, prompts, sessions_dir};

const BUILTIN: &[(&str, &str)] = &[
    ("help", "List commands and keys"),
    ("clear", "Start a new conversation"),
    ("compact", "Summarise older messages to free context [instructions]"),
    ("model", "Show or set the orchestrator model [provider:model] [effort]"),
    ("models", "Models of the orchestrator and every executor domain"),
    ("agents", "Executor domains, their models and tools"),
    ("domain", "Set a domain's model: <domain> <provider:model> [effort] | <domain> reset"),
    ("cost", "Tokens and cost so far, per model"),
    ("context", "What fills the orchestrator's context"),
    ("config", "Show the effective configuration"),
    ("permissions", "Show permission rules [allow <rule>]"),
    ("mode", "Show or set the permission mode [default|acceptEdits|plan|bypassPermissions]"),
    ("memory", "Memory files in use (add a note with #note)"),
    ("init", "Create a CRUSTY.md describing this project"),
    ("review", "Review the current changes [target]"),
    ("mcp", "MCP servers and their tools"),
    ("status", "Session, models, modes and integrations"),
    ("skills", "Skills the agents can load with the Skill tool"),
    ("plugins", "Loaded plugins"),
    ("rewind", "Checkpoints: list | <id> [code|conversation|both] (restore files and/or conversation)"),
    ("resume", "Resume a saved session [id]"),
    ("sessions", "List saved sessions"),
    ("export", "Export the conversation to Markdown [file]"),
    ("proxy", "Token-saving proxies: list | add <name> <url> [kind] | route <provider> <name|off> | remove <name>"),
    ("optimizer", "Optimizer status | <stage> on|off | observe | active"),
    ("gate", "Context gate decisions and statistics"),
    ("graph", "Code graph: rebuild | map | stats | find <q> | explain <x> | path <a> <b>"),
    ("ponytail", "Ponytail policy level [lite|full|ultra|off|on]"),
    ("todos", "The orchestrator's current plan"),
    ("doctor", "Check providers, keys, external tools and integrations"),
    ("exit", "Quit"),
    ("quit", "Quit"),
];

/// Built-in and custom commands, for completion in the UI.
pub fn command_list(custom: &[CustomCommand]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = BUILTIN.iter().map(|(n, d)| (n.to_string(), d.to_string())).collect();
    for c in custom {
        if !out.iter().any(|(n, _)| n == &c.name) {
            out.push((c.name.clone(), format!("{} ({})", c.description, c.scope)));
        }
    }
    out
}

/// Commands that change or read the conversation wait for the running turn.
pub(crate) fn needs_idle(name: &str, custom: &[CustomCommand]) -> bool {
    matches!(name, "clear" | "compact" | "resume" | "init" | "review" | "context" | "export" | "graph" | "rewind")
        || custom.iter().any(|c| c.name == name)
}

fn out(rt: &Runtime, command: &str, output: impl Into<String>) {
    rt.events.emit(AppEvent::CommandOutput { command: format!("/{command}"), output: output.into() });
}

/// Reloads memory files into the prompts.
pub fn reload_memory(rt: &Runtime) {
    let files = memory::load_memory(&rt.workspace, &rt.home, &rt.config.session.memory_files);
    let prompt = memory::memory_prompt(&files);
    rt.update_context(|c| {
        c.memory = prompt;
        c.memory_files = files.iter().map(|f| f.path.clone()).collect();
    });
}

fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub(crate) async fn handle(ctl: &mut Controller, name: &str, args: &str) {
    let rt = ctl.rt.clone();
    let words: Vec<&str> = args.split_whitespace().collect();
    match name {
        "help" => out(&rt, name, help(&ctl.custom_commands)),
        "clear" => {
            ctl.reset().await;
            rt.events.info("new conversation");
        }
        "compact" => {
            let (job_rt, convo, instructions) = (rt.clone(), ctl.convo.clone(), args.to_owned());
            ctl.spawn_job(move |cancel| async move {
                let spec = orchestrator_spec(&job_rt);
                let mut msgs = convo.lock().await;
                match compact::compact(&job_rt, &spec, &mut msgs, Some(&instructions), &cancel).await {
                    Ok(r) => {
                        out(&job_rt, "compact", format!("compacted {} messages into a {}-character summary", r.replaced, r.summary_chars))
                    }
                    Err(e) => job_rt.events.warn(format!("compact: {e}")),
                }
            });
        }
        "model" => set_model(&rt, &words),
        "models" | "agents" => out(&rt, name, models(&rt, name == "agents")),
        "domain" => set_domain(&rt, &words),
        "cost" => out(&rt, name, cost(&rt)),
        "context" => {
            let msgs = ctl.convo.lock().await;
            let spec = orchestrator_spec(&rt);
            let system = estimate_blocks(&[crusty_core::ContentBlock::text(&spec.system)], false);
            let policy = rt.policy_text().map(|(t, _)| crusty_core::tokens::estimate_text(&t)).unwrap_or(0);
            let tools: u64 = spec
                .tools
                .iter()
                .filter_map(|t| rt.tools.get(t))
                .map(|t| {
                    crusty_core::tokens::estimate_text(&t.spec().input_schema.to_string())
                        + crusty_core::tokens::estimate_text(&t.spec().description)
                })
                .sum();
            let messages = estimate_messages(&msgs, true);
            let window = rt.config.model_info(&spec.profile.model).context_window.max(1);
            let total = system + policy + tools + messages;
            out(
                &rt,
                name,
                format!(
                    "system {} · ponytail {} · tools {} · messages {} ({} msgs)\ntotal ≈ {} est. tokens of {} ({:.1}%)",
                    fmt_k(system),
                    fmt_k(policy),
                    fmt_k(tools),
                    fmt_k(messages),
                    msgs.len(),
                    fmt_k(total),
                    fmt_k(window),
                    total as f64 * 100.0 / window as f64
                ),
            );
        }
        "config" => {
            let s = rt.settings();
            let mut text = rt.config.to_toml();
            let _ = write!(
                text,
                "\n# runtime\n# orchestrator = {}:{}\n# domain overrides = {:?}\n# provider routes = {:?}\n# ponytail = {} ({})",
                s.orchestrator.provider,
                s.orchestrator.model,
                s.domain_overrides.keys().collect::<Vec<_>>(),
                s.provider_routes,
                s.ponytail.level,
                if s.ponytail.enabled { "on" } else { "off" }
            );
            out(&rt, name, text);
        }
        "permissions" => match words.as_slice() {
            ["allow", rule @ ..] if !rule.is_empty() => match rt.permissions.add_allow_rule(&rule.join(" ")) {
                Ok(()) => out(&rt, name, format!("allowed for this session: {}", rule.join(" "))),
                Err(e) => rt.events.error(e),
            },
            _ => out(&rt, name, rt.permissions.describe()),
        },
        "mode" => match words.first() {
            Some(m) => match PermissionMode::parse(m) {
                Some(mode) => {
                    rt.permissions.set_mode(mode);
                    rt.events.emit(AppEvent::PermissionModeChanged { mode });
                }
                None => rt.events.error(format!("unknown mode `{m}`")),
            },
            None => out(&rt, name, format!("permission mode: {}", rt.permissions.mode())),
        },
        "memory" => {
            reload_memory(&rt);
            let files = rt.context().memory_files;
            let mut text = if files.is_empty() { "no memory files (CRUSTY.md / CLAUDE.md)".to_owned() } else { String::new() };
            for f in files {
                let _ = writeln!(text, "- {}", display_path(&f));
            }
            text.push_str("\nAdd a note with `#your note`; edit the files directly to change them.");
            out(&rt, name, text);
        }
        "init" => ctl.start_turn(prompts::INIT_PROMPT.to_owned()).await,
        "review" => ctl.start_turn(prompts::review_prompt(args)).await,
        "mcp" => {
            let lines = rt.context().mcp_status;
            out(&rt, name, if lines.is_empty() { "no MCP servers configured".to_owned() } else { lines.join("\n") });
        }
        "status" => out(&rt, name, status(&rt)),
        "skills" => out(&rt, name, list_or(rt.context().skills, "no skills")),
        "plugins" => out(&rt, name, list_or(rt.context().plugins, "no plugins")),
        "rewind" => rewind(ctl, &words).await,
        "sessions" => out(&rt, name, list_sessions(&rt)),
        "resume" => match words.first() {
            None => out(&rt, name, list_sessions(&rt)),
            Some(id) => match transcript::find(&sessions_dir(&rt), id) {
                Some(path) => match ctl.resume(&path).await {
                    Ok(()) => rt.events.info(format!("resumed session {}", rt.session_id())),
                    Err(e) => rt.events.error(format!("cannot resume: {e}")),
                },
                None => rt.events.error(format!("no session starting with `{id}`")),
            },
        },
        "export" => {
            let msgs = ctl.convo.lock().await;
            let file = words.first().map(|f| rt.workspace.join(f)).unwrap_or_else(|| {
                let id = rt.session_id();
                rt.workspace.join(format!("crusty-{}.md", &id[..id.len().min(8)]))
            });
            match std::fs::write(&file, transcript::export_markdown(&msgs)) {
                Ok(()) => rt.events.info(format!("exported to {}", display_path(&file))),
                Err(e) => rt.events.error(format!("export failed: {e}")),
            }
        }
        "proxy" => proxy(&rt, &words),
        "optimizer" => optimizer(&rt, &words),
        "gate" => out(&rt, name, gate(&rt)),
        "graph" => {
            let (job_rt, owned): (_, Vec<String>) = (rt.clone(), words.iter().map(|w| w.to_string()).collect());
            ctl.spawn_job(move |cancel| async move {
                let words: Vec<&str> = owned.iter().map(String::as_str).collect();
                graph(&job_rt, &words, cancel).await;
            });
        }
        "ponytail" => match words.first().copied() {
            Some(level @ ("lite" | "full" | "ultra")) => {
                rt.update_settings(|s| {
                    s.ponytail.enabled = true;
                    s.ponytail.level = level.to_owned();
                });
                rt.events.info(format!("ponytail {level} for every agent from the next request"));
            }
            Some("off") => {
                rt.update_settings(|s| s.ponytail.enabled = false);
                rt.events.info("ponytail off");
            }
            Some("on") => rt.update_settings(|s| s.ponytail.enabled = true),
            _ => {
                let p = rt.settings().ponytail;
                out(
                    &rt,
                    name,
                    format!("{} {} — level {} ({})", rt.policy.name, rt.policy.version, p.level, if p.enabled { "on" } else { "off" }),
                );
            }
        },
        "todos" => out(&rt, name, todos(&rt)),
        "doctor" => out(&rt, name, doctor(&rt)),
        "exit" | "quit" => ctl.quit = true,
        other => match ctl.custom_commands.iter().find(|c| c.name == other).cloned() {
            Some(cmd) => {
                if cmd.model.is_some() {
                    rt.events.info(format!("/{other}: the command's `model` field is ignored; the orchestrator model is used"));
                }
                ctl.start_turn(cmd.render(args)).await;
            }
            None => rt.events.error(format!("unknown command /{other} — try /help")),
        },
    }
}

fn list_or(items: Vec<String>, empty: &str) -> String {
    if items.is_empty() { empty.to_owned() } else { items.join("\n") }
}

/// `/rewind` lists the checkpoints (the UI shows a picker); `/rewind <id> [scope]` restores
/// the files, the conversation or both (default). Refused while a turn runs.
async fn rewind(ctl: &mut Controller, words: &[&str]) {
    let rt = ctl.rt.clone();
    let Some(&id_arg) = words.first() else {
        rt.events.emit(AppEvent::CheckpointList { items: rt.checkpoints.list() });
        return;
    };
    if ctl.busy() {
        rt.events.error("rewind refused while a turn is running");
        return;
    }
    let Ok(id) = id_arg.trim_start_matches('#').parse::<u32>() else {
        rt.events.error(format!("`{id_arg}` is not a checkpoint id (see /rewind)"));
        return;
    };
    let scope = match words.get(1) {
        None => RewindScope::Both,
        Some(s) => match RewindScope::parse(s) {
            Some(scope) => scope,
            None => {
                rt.events.error(format!("unknown scope `{s}` (code, conversation or both)"));
                return;
            }
        },
    };
    let Some(index) = rt.checkpoints.list().iter().find(|c| c.id == id).map(|c| c.message_index) else {
        rt.events.error(format!("no checkpoint {id} (see /rewind)"));
        return;
    };
    // The stored message index no longer matches the compacted conversation.
    if scope.restores_conversation() && id <= rt.compacted_through_checkpoint() {
        rt.events.error(format!(
            "checkpoint {id}: the conversation was compacted after this checkpoint; only `code` can be rewound (`/rewind {id} code`)"
        ));
        return;
    }
    let (mut files_restored, mut failures) = (0, Vec::new());
    if scope.restores_code() {
        match rt.checkpoints.rewind(id) {
            Ok(report) => {
                files_restored = report.files_restored;
                failures = report.failures;
            }
            Err(e) => {
                rt.events.error(format!("rewind failed: {e}"));
                return;
            }
        }
    }
    if scope.restores_conversation() {
        let mut convo = ctl.convo.lock().await;
        let keep = index.min(convo.len());
        convo.truncate(keep);
        rt.clear_plans();
        rt.forget_conversation(&[rt.session_id().as_str()]);
        rt.log(&transcript::Entry::Rewind { keep_messages: keep, ts: chrono::Utc::now() });
    }
    rt.events.emit(AppEvent::Rewound { id, scope, files_restored, failures });
}

/// What the router and the user decided about the orchestrator model.
fn route_note(rt: &Runtime) -> String {
    let s = rt.settings();
    if s.orchestrator_pinned {
        "pinned by /model (`/model auto` lets the router choose)".into()
    } else {
        match s.routed_tier {
            Some(tier) => format!("routed to the {tier} tier"),
            None => "not routed yet (the router picks at the next prompt)".into(),
        }
    }
}

fn help(custom: &[CustomCommand]) -> String {
    let mut text = String::from("Commands:\n");
    for (n, d) in BUILTIN {
        let _ = writeln!(text, "  /{n:<12} {d}");
    }
    if !custom.is_empty() {
        text.push_str("\nCustom commands:\n");
        for c in custom {
            let _ = writeln!(text, "  /{:<12} {} ({})", c.name, c.description, c.scope);
        }
    }
    text.push_str(
        "\nInput: `!cmd` runs a shell command, `#note` adds to memory.\n\
         Keys: Enter send · Shift+Enter newline · Esc interrupt · Shift+Tab permission mode · Ctrl+T side panel · Ctrl+O thinking · F1/F2/F3 tabs · Ctrl+C twice quit",
    );
    text
}

fn set_model(rt: &Runtime, words: &[&str]) {
    let current = rt.settings().orchestrator;
    let Some(arg) = words.first() else {
        out(
            rt,
            "model",
            format!(
                "orchestrator: {}:{} (effort {}) · {}",
                current.provider,
                current.model,
                current.effort.as_deref().unwrap_or("default"),
                route_note(rt)
            ),
        );
        return;
    };
    if *arg == "auto" {
        rt.update_settings(|s| {
            s.orchestrator_pinned = false;
            s.routed_tier = None;
        });
        out(rt, "model", "model routing on: the router picks the model at the next prompt");
        return;
    }
    let Some(mref) = ModelRef::parse(arg, &current.provider) else {
        rt.events.error(format!("bad model `{arg}` (use provider:model)"));
        return;
    };
    if rt.provider(&mref.provider).is_none() {
        rt.events.error(format!("unknown provider `{}`", mref.provider));
        return;
    }
    let effort = words.get(1).map(|e| e.to_string());
    if !valid_effort(rt, effort.as_deref()) {
        return;
    }
    rt.update_settings(|s| {
        s.orchestrator_pinned = true;
        s.orchestrator.provider.clone_from(&mref.provider);
        s.orchestrator.model.clone_from(&mref.model);
        if effort.is_some() {
            s.orchestrator.effort.clone_from(&effort);
        }
    });
    rt.events.emit(AppEvent::ModelChanged { target: ORCHESTRATOR_ID.into(), model: mref.to_string() });
}

fn set_domain(rt: &Runtime, words: &[&str]) {
    let (Some(domain), Some(arg)) = (words.first(), words.get(1)) else {
        rt.events.error("usage: /domain <domain> <provider:model> [effort] | /domain <domain> reset");
        return;
    };
    let Some(current) = rt.domain(domain) else {
        rt.events.error(format!("unknown domain `{domain}`; available: {}", rt.domain_names().join(", ")));
        return;
    };
    if *arg == "reset" {
        rt.update_settings(|s| {
            s.domain_overrides.remove(*domain);
        });
        rt.events.info(format!("{domain}: back to the configured model"));
        return;
    }
    let Some(mref) = ModelRef::parse(arg, &current.model.provider) else {
        rt.events.error(format!("bad model `{arg}`"));
        return;
    };
    if rt.provider(&mref.provider).is_none() {
        rt.events.error(format!("unknown provider `{}`", mref.provider));
        return;
    }
    let effort = words.get(2).map(|e| e.to_string());
    if !valid_effort(rt, effort.as_deref()) {
        return;
    }
    let o = ModelOverride { provider: Some(mref.provider.clone()), model: Some(mref.model.clone()), effort, ..ModelOverride::default() };
    rt.update_settings(|s| {
        s.domain_overrides.insert(domain.to_string(), o);
    });
    rt.events.emit(AppEvent::ModelChanged { target: domain.to_string(), model: mref.to_string() });
}

/// Reports an error and returns false when `effort` is not an accepted level.
fn valid_effort(rt: &Runtime, effort: Option<&str>) -> bool {
    match effort {
        Some(e) if !["low", "medium", "high", "xhigh", "max"].contains(&e) => {
            rt.events.error(format!("effort must be low|medium|high|xhigh|max, got `{e}`"));
            false
        }
        _ => true,
    }
}

fn models(rt: &Runtime, with_tools: bool) -> String {
    let s = rt.settings();
    let mut text = format!(
        "orchestrator  {}:{} (effort {}) — tools: {}\n",
        s.orchestrator.provider,
        s.orchestrator.model,
        s.orchestrator.effort.as_deref().unwrap_or("default"),
        rt.config.orchestrator.direct_tools.join(", ")
    );
    for name in rt.domain_names() {
        if let Some(d) = rt.domain(&name) {
            let _ = write!(text, "{}", prompts::domain_line(&d));
            if s.domain_overrides.contains_key(&name) {
                text.push_str(" [runtime override]");
            }
            if with_tools {
                let _ = write!(text, "\n    tools: {}", d.tools.as_ref().map(|t| t.join(", ")).unwrap_or_else(|| "all".into()));
            }
            text.push('\n');
        }
    }
    text
}

fn cost(rt: &Runtime) -> String {
    let m = rt.events.metrics().snapshot();
    let mut text = String::new();
    for (model, u) in &m.usage_by_model {
        let _ = writeln!(
            text,
            "{model}: {} calls · in {} · out {} · cache read {} · write {} · ${:.4}",
            u.calls,
            fmt_k(u.usage.input_tokens),
            fmt_k(u.usage.output_tokens),
            fmt_k(u.usage.cache_read_input_tokens),
            fmt_k(u.usage.cache_creation_input_tokens),
            u.cost_usd
        );
    }
    let _ = writeln!(
        text,
        "total: in {} · out {} · ${:.4} (estimated from list prices)",
        fmt_k(m.total_usage.total_input()),
        fmt_k(m.total_usage.output_tokens),
        m.total_cost_usd
    );
    let _ = write!(text, "optimizer saved ≈ {} tokens (est.)", fmt_k(m.total_saved_est().max(0) as u64));
    text
}

fn status(rt: &Runtime) -> String {
    let s = rt.settings();
    let ctx = rt.context();
    let deferred = rt.deferred_servers();
    format!(
        "Crusty-Claude {}\nsession: {}\nworkspace: {}\norchestrator: {}:{} ({})\npermission mode: {}\n{}\nponytail: {} ({})\nmemory files: {}\nMCP servers: {}\ntools: {}\nadvisor: {}\ndeferred MCP servers: {}",
        env!("CARGO_PKG_VERSION"),
        rt.session_id(),
        display_path(&rt.workspace),
        s.orchestrator.provider,
        s.orchestrator.model,
        route_note(rt),
        rt.permissions.mode(),
        rt.optimizer.describe(),
        s.ponytail.level,
        if s.ponytail.enabled { "on" } else { "off" },
        ctx.memory_files.len(),
        ctx.mcp_status.len(),
        rt.tools.len(),
        rt.advisor.name(),
        if deferred.is_empty() { "none".to_owned() } else { deferred.into_iter().collect::<Vec<_>>().join(", ") }
    )
}

fn list_sessions(rt: &Runtime) -> String {
    let sessions = transcript::list_sessions(&sessions_dir(rt));
    if sessions.is_empty() {
        return "no saved sessions for this project".into();
    }
    let mut text = String::new();
    for s in sessions.iter().take(20) {
        let _ = writeln!(text, "{}  {:>4} msgs  {}", &s.id[..s.id.len().min(8)], s.messages, s.title_or_first_prompt);
    }
    text.push_str("resume with /resume <id prefix>");
    text
}

fn proxy(rt: &Runtime, words: &[&str]) {
    match words {
        ["add", name, url, rest @ ..] => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                rt.events.error("proxy url must start with http:// or https://");
                return;
            }
            let kind = rest.first().copied().unwrap_or("generic").to_owned();
            let new_url = url.trim_end_matches('/').to_owned();
            let old_url = {
                let mut list = rt.proxy_upstreams.write().unwrap_or_else(|e| e.into_inner());
                let old = list.iter().find(|u| u.name == *name).map(|u| u.url.clone());
                list.retain(|u| u.name != *name);
                list.push(ProxyUpstream { name: name.to_string(), url: new_url.clone(), kind, ..ProxyUpstream::default() });
                old
            };
            // Providers routed through the replaced proxy follow it to the new URL.
            if let Some(old) = old_url.filter(|o| *o != new_url) {
                rt.update_settings(|s| {
                    for u in s.provider_routes.values_mut().filter(|u| **u == old) {
                        u.clone_from(&new_url);
                    }
                });
            }
            rt.events.info(format!("proxy {name} added; route a provider through it with /proxy route <provider> {name}"));
        }
        ["route", provider, target] => {
            if rt.provider(provider).is_none() {
                rt.events.error(format!("unknown provider `{provider}`"));
                return;
            }
            if *target == "off" {
                rt.update_settings(|s| {
                    s.provider_routes.remove(*provider);
                });
                rt.events.info(format!("{provider}: direct"));
                return;
            }
            let url =
                rt.proxy_upstreams.read().unwrap_or_else(|e| e.into_inner()).iter().find(|u| u.name == *target).map(|u| u.url.clone());
            let Some(url) = url.or_else(|| target.starts_with("http").then(|| target.to_string())) else {
                rt.events.error(format!("unknown proxy `{target}` (add it first)"));
                return;
            };
            rt.update_settings(|s| {
                s.provider_routes.insert(provider.to_string(), url.clone());
            });
            rt.events.info(format!("{provider} now goes through {url}"));
        }
        ["remove", name] => {
            let removed = {
                let mut list = rt.proxy_upstreams.write().unwrap_or_else(|e| e.into_inner());
                let url = list.iter().find(|u| u.name == *name).map(|u| u.url.clone());
                list.retain(|u| u.name != *name);
                url
            };
            match removed {
                Some(url) => {
                    rt.update_settings(|s| s.provider_routes.retain(|_, u| *u != url));
                    rt.events.info(format!("proxy {name} removed"));
                }
                None => rt.events.error(format!("no proxy named `{name}`")),
            }
        }
        _ => {
            let list = rt.proxy_upstreams.read().unwrap_or_else(|e| e.into_inner()).clone();
            let snap = rt.events.metrics().snapshot();
            let mut text = String::new();
            if list.is_empty() {
                text.push_str("no proxies; add one with /proxy add <name> <url> [kind]\n");
            }
            for u in &list {
                let health = snap
                    .proxies
                    .get(&u.name)
                    .map(|p| if p.up { format!("up {}ms", p.latency_ms.unwrap_or(0)) } else { "down".into() })
                    .unwrap_or_else(|| "not probed yet".into());
                let _ = writeln!(text, "{} [{}] {} — {}", u.name, u.kind, u.url, health);
            }
            for (p, url) in rt.settings().provider_routes {
                let _ = writeln!(text, "route: {p} → {url}");
            }
            text.push_str("built-in optimizing proxy: run `crusty proxy` (listens on proxy.listen)");
            out(rt, "proxy", text);
        }
    }
}

fn parse_stage(s: &str) -> Option<Stage> {
    Stage::ALL.into_iter().find(|st| {
        let l = st.label().to_ascii_lowercase();
        l == s
            || format!("{st:?}").to_ascii_lowercase() == s
            || (s == "html" && *st == Stage::HtmlToMarkdown)
            || (s == "gate" && *st == Stage::ContextGate)
    })
}

fn optimizer(rt: &Runtime, words: &[&str]) {
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let result = match lower.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["observe"] => rt.optimizer.set_mode(OptimizerMode::Observe),
        ["active"] => rt.optimizer.set_mode(OptimizerMode::Active),
        [stage, state @ ("on" | "off")] => match parse_stage(stage) {
            Some(st) => rt.optimizer.set_stage_enabled(st, *state == "on"),
            None => Err(format!("unknown stage `{stage}`")),
        },
        _ => {
            let snap = rt.events.metrics().snapshot();
            let mut text = rt.optimizer.describe();
            text.push('\n');
            for st in Stage::ALL {
                if let Some(t) = snap.stages.get(&st) {
                    let _ = writeln!(
                        text,
                        "{:<9} runs {:>4} · applied {:>4} · saved ≈ {} ({:.0}%, est.){}",
                        st.label(),
                        t.runs,
                        t.applied,
                        fmt_k(t.saved_est().max(0) as u64),
                        t.ratio_est() * 100.0,
                        t.ratio_real().map(|r| format!(" · real {:.0}%", r * 100.0)).unwrap_or_default()
                    );
                }
            }
            out(rt, "optimizer", text);
            return;
        }
    };
    match result {
        Ok(()) => rt.events.info(format!("optimizer: {}", words.join(" "))),
        Err(e) => rt.events.error(e),
    }
}

fn gate(rt: &Runtime) -> String {
    let g = rt.events.metrics().snapshot().gate;
    let mut text = format!(
        "routes: direct {} · pxpipe {} · observed pxpipe {}\ndry runs {} ({} ms total) · predictions {} (avg error {} tokens)\nreal context: direct {} · pxpipe {}",
        g.direct,
        g.pxpipe,
        g.observed_pxpipe,
        g.dryruns,
        g.dryrun_ms_sum,
        g.predictions,
        g.prediction_abs_error_sum.checked_div(g.predictions).unwrap_or(0),
        fmt_k(g.real_context_direct_sum),
        fmt_k(g.real_context_pxpipe_sum)
    );
    if let Some(last) = g.last {
        let _ = write!(
            text,
            "\nlast: {} ({}) zone {} · measure {} · saving {} · delta {}",
            last.route,
            last.reason,
            last.zone.unwrap_or_else(|| "-".into()),
            fmt_k(last.measure),
            last.saving.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            last.delta.map(|d| format!("{d:.0}")).unwrap_or_else(|| "-".into())
        );
    }
    text
}

async fn graph(rt: &Runtime, words: &[&str], cancel: CancellationToken) {
    let Some(tool) = rt.tools.get("Graph").cloned() else {
        rt.events.error("the code graph is disabled (optimizer.graphify.enabled = false)");
        return;
    };
    let input = match words {
        [] | ["stats"] => json!({"action": "stats"}),
        ["rebuild"] => json!({"action": "rebuild"}),
        ["map"] => json!({"action": "map"}),
        ["find", q @ ..] => json!({"action": "find", "query": q.join(" ")}),
        ["explain", t @ ..] => json!({"action": "explain", "target": t.join(" ")}),
        ["path", a, b] => json!({"action": "path", "query": a, "target": b}),
        _ => {
            rt.events.error("usage: /graph rebuild | map | stats | find <q> | explain <x> | path <a> <b>");
            return;
        }
    };
    let ctx = rt.tool_context("user", cancel);
    match tool.call(input, &ctx).await {
        Ok(o) => {
            if words.first() == Some(&"rebuild")
                && let Ok(map) = tool.call(json!({"action": "map"}), &ctx).await
            {
                rt.update_context(|c| c.repo_map = Some(map.text_content()));
            }
            out(rt, "graph", o.text_content());
        }
        Err(e) => rt.events.error(format!("graph: {e}")),
    }
}

fn todos(rt: &Runtime) -> String {
    match rt.plan(ORCHESTRATOR_ID) {
        Some(items) if !items.is_empty() => items
            .iter()
            .map(|t| {
                let mark = match t.status {
                    TodoStatus::Pending => "☐",
                    TodoStatus::InProgress => "◐",
                    TodoStatus::Completed => "☑",
                };
                format!("{mark} {}", t.content)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "no plan yet".into(),
    }
}

fn doctor(rt: &Runtime) -> String {
    let mut text = String::from("Providers:\n");
    for (id, p) in &rt.config.providers {
        let key = match (&p.api_key_env, &p.auth_token_env) {
            (Some(k), _) if std::env::var_os(k).is_some() => format!("{k} set"),
            (_, Some(t)) if std::env::var_os(t).is_some() => format!("{t} set"),
            (Some(k), _) | (None, Some(k)) => format!("{k} NOT set"),
            _ => "no key needed".into(),
        };
        let route = rt.provider_route(id).map(|u| format!(" · via {u}")).unwrap_or_default();
        let _ = writeln!(text, "  {id:<11} {:?} {} · {key}{route}", p.kind, if p.base_url.is_empty() { "-" } else { &p.base_url });
    }
    text.push_str("External tools (optional):\n");
    for bin in ["rtk", "headroom", "pxpipe", "graphify", "bash", "git"] {
        let found = find_in_path(bin).map(|p| display_path(&p)).unwrap_or_else(|| "not found".into());
        let _ = writeln!(text, "  {bin:<9} {found}");
    }
    let ctx = rt.context();
    let _ = writeln!(text, "MCP: {}", if ctx.mcp_status.is_empty() { "none".into() } else { ctx.mcp_status.join("; ") });
    let snap = rt.events.metrics().snapshot();
    for (name, p) in &snap.proxies {
        let _ = writeln!(text, "proxy {name}: {} {}", p.url, if p.up { "up" } else { "down" });
    }
    let _ = write!(text, "Transcripts: {}", display_path(&sessions_dir(rt)));
    text
}
