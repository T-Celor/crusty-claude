//! `crusty -p`: answer one prompt without the UI.

use std::io::{IsTerminal, Read};

use anyhow::{Result, bail};
use crusty_agent::run_print;
use crusty_core::{AppEvent, CancellationToken};
use serde_json::json;

use crate::wiring::{announce, build_session, index_graph, load_config};
use crate::{Cli, OutputFormat};

/// First Ctrl+C cancels `token` for a clean stop; a second one exits at once with 130.
pub(crate) fn cancel_on_ctrl_c(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        token.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
}

fn read_prompt(cli: &Cli) -> Result<String> {
    let mut prompt = cli.prompt.clone().unwrap_or_default();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut input = String::new();
        stdin.lock().read_to_string(&mut input)?;
        let input = input.trim_end();
        if !input.is_empty() {
            prompt = if prompt.trim().is_empty() { input.to_owned() } else { format!("{prompt}\n\n<stdin>\n{input}\n</stdin>") };
        }
    }
    if prompt.trim().is_empty() {
        bail!("no prompt: pass it as an argument or on stdin");
    }
    Ok(prompt)
}

/// One stderr line for an event, in verbose text mode.
fn describe(ev: &AppEvent) -> Option<String> {
    Some(match ev {
        AppEvent::AgentSpawned { agent_id, domain, provider, model, task, .. } => {
            format!("↳ {agent_id} [{domain}] {provider}:{model} — {task}")
        }
        AppEvent::AgentFinished { agent_id, ok, turns, cost_usd, .. } => {
            format!("✓ {agent_id} {} after {turns} turns (${cost_usd:.4})", if *ok { "done" } else { "stopped" })
        }
        AppEvent::ToolStarted { agent_id, summary, .. } => format!("⏺ {agent_id}: {summary}"),
        AppEvent::ToolFinished { agent_id, tool, ok, duration_ms, .. } => {
            format!("  {agent_id}: {tool} {} {duration_ms} ms", if *ok { "ok" } else { "failed" })
        }
        AppEvent::Gate(g) => format!("gate: {} ({}) est {}", g.route, g.reason, g.est),
        AppEvent::Optimizer(m) if m.status == crusty_core::StageStatus::Applied && m.saved_est() > 0 => {
            format!("{}: {} -{} tokens est.", m.stage.label(), m.transformer, m.saved_est())
        }
        AppEvent::ModelFallback { from, to, reason, .. } => format!("fallback {from} → {to}: {reason}"),
        AppEvent::Warning { message } => format!("warning: {message}"),
        AppEvent::Error { message } => format!("error: {message}"),
        _ => return None,
    })
}

pub async fn run(cli: &Cli) -> Result<i32> {
    let prompt = read_prompt(cli)?;
    let loaded = load_config(cli)?;
    let mut session = build_session(cli, loaded, false).await?;
    let rt = session.rt.clone();
    for w in &session.warnings {
        eprintln!("warning: {w}");
    }
    let format = cli.output_format;
    let verbose = cli.verbose;
    let mut rx = std::mem::replace(&mut session.events_rx, tokio::sync::mpsc::unbounded_channel().1);
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AppEvent::Shutdown) {
                break;
            }
            if format == OutputFormat::StreamJson {
                if let Ok(line) = serde_json::to_string(&ev) {
                    println!("{line}");
                }
            } else if verbose && let Some(line) = describe(&ev) {
                eprintln!("{line}");
            }
        }
    });
    announce(&rt, &[]).await;
    if let Some(g) = session.graph.clone() {
        index_graph(rt.clone(), g).await;
    }
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
    let mut messages = std::mem::take(&mut session.messages);
    let outcome = run_print(&rt, &mut messages, &prompt, &cancel).await;
    rt.events.emit(AppEvent::Shutdown);
    let _ = printer.await;
    match format {
        OutputFormat::Text => {
            println!("{}", outcome.text);
            if outcome.is_error() {
                eprintln!("crusty: {}", outcome.stop.label());
            }
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = json!({
                "type": "result",
                "subtype": if outcome.is_error() { "error" } else { "success" },
                "is_error": outcome.is_error(),
                "result": outcome.text,
                "stop": outcome.stop.label(),
                "session_id": outcome.session_id,
                "num_turns": outcome.turns,
                "duration_ms": outcome.duration_ms,
                "total_cost_usd": outcome.cost_usd,
                "usage": outcome.usage,
                "optimizer_saved_est_tokens": rt.events.metrics().snapshot().total_saved_est(),
            });
            println!("{result}");
        }
    }
    session.background.cancel();
    session.mcp.shutdown().await;
    Ok(if outcome.is_error() { 1 } else { 0 })
}
