//! `crusty proxy | graph | config | doctor | sessions | mcp | models`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crusty_agent::{display_path, find_in_path};
use crusty_core::config::Config;
use crusty_core::{AppEvent, CancellationToken, EventSink, OriginalStore, RequestOptimizer, StageStatus};
use crusty_graph::CodeGraph;
use crusty_pipeline::Pipeline;
use crusty_providers::{ProviderRegistry, http_client};

use crate::wiring::{Loaded, load_config, print_sessions};
use crate::{Cli, Command, ConfigAction, GraphAction};

/// Commented example written by `crusty config init`.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../config/config.example.toml");

pub async fn run(cli: &Cli, cmd: &Command) -> Result<i32> {
    match cmd {
        Command::Proxy { listen, forward, live } => proxy(cli, listen.as_deref(), forward.as_deref(), *live).await,
        Command::Graph { action } => graph(cli, action).await,
        Command::Config { action } => config(cli, action),
        Command::Doctor => doctor(cli),
        Command::Trust { remove } => {
            let workspace = match &cli.cwd {
                Some(p) => p.clone(),
                None => std::env::current_dir()?,
            };
            let workspace = workspace.canonicalize()?;
            crusty_core::trust::set_trusted(&workspace, !remove)?;
            if *remove {
                println!("{} is no longer trusted", display_path(&workspace));
            } else {
                println!(
                    "{} is trusted: its .crusty/config.toml may now set providers, proxies, hooks, MCP servers and permission settings, and its .mcp.json is read",
                    display_path(&workspace)
                );
            }
            Ok(0)
        }
        Command::Sessions => {
            print_sessions(&load_config(cli)?);
            Ok(0)
        }
        Command::Mcp => mcp(cli).await,
        Command::Models { provider } => models(cli, provider.as_deref()).await,
    }
}

fn fmt_k(n: i64) -> String {
    let a = n.unsigned_abs();
    let s = if a >= 1_000_000 {
        format!("{:.1}M", a as f64 / 1e6)
    } else if a >= 1_000 {
        format!("{:.1}k", a as f64 / 1e3)
    } else {
        a.to_string()
    };
    if n < 0 { format!("-{s}") } else { s }
}

async fn proxy(cli: &Cli, listen: Option<&str>, forward: Option<&str>, live: bool) -> Result<i32> {
    let loaded = load_config(cli)?;
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    let config = Arc::new(loaded.config);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = EventSink::new(tx);
    let pipeline = Arc::new(Pipeline::new(&config, events.clone(), Arc::new(OriginalStore::default())));
    let listen = listen.unwrap_or(&config.proxy.listen).to_owned();
    let forward = forward.unwrap_or(&config.proxy.forward_url).trim_end_matches('/').to_owned();
    let listener = crusty_proxy::bind(&listen).await.with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener.local_addr()?;
    eprintln!("crusty proxy listening on http://{addr} → {forward}");
    eprintln!("{}", pipeline.describe());
    eprintln!("use it from any Anthropic client, e.g.: ANTHROPIC_BASE_URL=http://{addr} claude");
    let cancel = CancellationToken::new();
    let upstreams = Arc::new(std::sync::RwLock::new(config.proxy.upstreams.clone()));
    crusty_proxy::spawn_health_monitor(
        upstreams,
        http_client(10),
        events.clone(),
        Duration::from_secs(config.proxy.health_interval_secs.max(1)),
        cancel.clone(),
    );
    let metrics = events.metrics().clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if !live {
                continue;
            }
            match ev {
                AppEvent::Gate(g) => eprintln!(
                    "→ gate {} ({}{}) est {}",
                    g.route,
                    g.reason,
                    g.zone.map(|z| format!(", zone {z}")).unwrap_or_default(),
                    fmt_k(g.est as i64)
                ),
                AppEvent::Optimizer(m) if m.status == StageStatus::Applied && m.saved_est() != 0 => {
                    eprintln!("  {} {} {} tokens (est.)", m.stage.label(), m.transformer, fmt_k(-m.saved_est()))
                }
                AppEvent::Usage { model, usage, cost_usd, .. } => {
                    let snap = metrics.snapshot();
                    eprintln!(
                        "← {model}: in {} · out {} · ${cost_usd:.4} | session saved ≈ {} tokens (est.) · ${:.4} total",
                        fmt_k(usage.total_input() as i64),
                        fmt_k(usage.output_tokens as i64),
                        fmt_k(snap.total_saved_est()),
                        snap.total_cost_usd
                    );
                }
                AppEvent::ProxyStatus { name, up, latency_ms, .. } => {
                    eprintln!("  upstream {name}: {}", if up { format!("up {}ms", latency_ms.unwrap_or(0)) } else { "down".into() })
                }
                AppEvent::Warning { message } | AppEvent::Error { message } => eprintln!("! {message}"),
                _ => {}
            }
        }
    });
    crate::print_mode::cancel_on_ctrl_c(cancel.clone());
    let state = crusty_proxy::ProxyState {
        optimizer: pipeline,
        config,
        events: events.clone(),
        http: http_client(600),
        forward_url: forward,
        started: Instant::now(),
    };
    crusty_proxy::serve(listener, state, cancel).await?;
    let snap = events.metrics().snapshot();
    eprintln!(
        "crusty proxy stopped: {} requests · in {} · out {} · saved ≈ {} tokens (est.) · ${:.4}",
        snap.usage_by_model.values().map(|u| u.calls).sum::<u64>(),
        fmt_k(snap.total_usage.total_input() as i64),
        fmt_k(snap.total_usage.output_tokens as i64),
        fmt_k(snap.total_saved_est()),
        snap.total_cost_usd
    );
    Ok(0)
}

async fn graph(cli: &Cli, action: &GraphAction) -> Result<i32> {
    let loaded = load_config(cli)?;
    if !loaded.config.optimizer.graphify.enabled {
        bail!("the code graph is disabled (optimizer.graphify.enabled = false)");
    }
    let graph = Arc::new(CodeGraph::new(&loaded.workspace, loaded.config.optimizer.graphify.clone()));
    let budget = loaded.config.optimizer.graphify.repo_map_tokens;
    let g = graph.clone();
    // Only `build` rewrites the index; queries reuse the stored graph and build it once if missing.
    if !matches!(action, GraphAction::Build) {
        tokio::task::spawn_blocking(move || g.ensure_built()).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    match action {
        GraphAction::Build => {
            let g = graph.clone();
            let stats = tokio::task::spawn_blocking(move || g.refresh(&EventSink::noop())).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{} files · {} symbols · {} edges · {} communities · {} files reused · {} ms\n{}",
                stats.files,
                stats.symbols,
                stats.edges,
                stats.communities,
                stats.reused_files,
                stats.duration_ms,
                display_path(&graph.graph_path())
            );
        }
        GraphAction::Map => println!("{}", graph.repo_map(budget).unwrap_or_default()),
        GraphAction::Find { query } => {
            let lines = graph
                .with_graph(|g| {
                    crusty_graph::query::find(g, query, 50).iter().map(|n| crusty_graph::query::label(g, &n.id)).collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if lines.is_empty() {
                println!("nothing matches `{query}`");
            }
            for l in lines {
                println!("{l}");
            }
        }
        GraphAction::Explain { target } => match graph.with_graph(|g| crusty_graph::query::explain(g, target)).flatten() {
            Some(text) => println!("{text}"),
            None => bail!("`{target}` is not a file or symbol of the graph"),
        },
        GraphAction::Report => println!("{}", std::fs::read_to_string(graph.report_path()).context("no report yet")?),
    }
    Ok(0)
}

fn config(cli: &Cli, action: &ConfigAction) -> Result<i32> {
    match action {
        ConfigAction::Path => {
            let workspace = match &cli.cwd {
                Some(p) => p.clone(),
                None => std::env::current_dir()?,
            };
            for p in Config::layer_paths(&workspace, cli.config.as_deref()) {
                println!("{} {}", if p.exists() { "✓" } else { "·" }, p.display());
            }
        }
        ConfigAction::Init => {
            let workspace = match &cli.cwd {
                Some(p) => p.clone(),
                None => std::env::current_dir()?,
            };
            let path = workspace.join(".crusty").join("config.toml");
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            std::fs::create_dir_all(path.parent().context("no parent directory")?)?;
            std::fs::write(&path, EXAMPLE_CONFIG)?;
            println!("wrote {}", path.display());
        }
        ConfigAction::Show => println!("{}", load_config(cli)?.config.to_toml()),
        ConfigAction::Validate => {
            let loaded = load_config(cli)?;
            for s in &loaded.sources {
                println!("loaded {}", s.display());
            }
            for w in &loaded.warnings {
                println!("warning: {w}");
            }
            println!("configuration is valid");
        }
    }
    Ok(0)
}

fn doctor(cli: &Cli) -> Result<i32> {
    let Loaded { config, workspace, sources, warnings, trusted, .. } = load_config(cli)?;
    println!(
        "Crusty-Claude {} · project {} ({})",
        env!("CARGO_PKG_VERSION"),
        display_path(&workspace),
        if trusted { "trusted" } else { "not trusted: run `crusty trust` to use its own providers, hooks, MCP and permission settings" }
    );
    println!(
        "config layers: {}",
        if sources.is_empty() {
            "defaults only".into()
        } else {
            sources.iter().map(|s| s.display().to_string()).collect::<Vec<_>>().join(", ")
        }
    );
    for w in &warnings {
        println!("warning: {w}");
    }
    println!(
        "orchestrator: {}:{} (effort {})",
        config.orchestrator.model.provider,
        config.orchestrator.model.model,
        config.orchestrator.model.effort.as_deref().unwrap_or("default")
    );
    println!("providers:");
    let mut missing_key = false;
    for (id, p) in &config.providers {
        let key = match (&p.api_key_env, &p.auth_token_env) {
            (Some(k), _) if std::env::var_os(k).is_some() => format!("{k} set"),
            (_, Some(t)) if std::env::var_os(t).is_some() => format!("{t} set"),
            (Some(k), _) | (None, Some(k)) => {
                missing_key |= *id == config.orchestrator.model.provider;
                format!("{k} not set")
            }
            _ => "no key needed".into(),
        };
        println!("  {id:<11} {:?} {} · {key}", p.kind, if p.base_url.is_empty() { "-" } else { &p.base_url });
    }
    println!("external tools (optional, native versions are built in):");
    for bin in ["rtk", "headroom", "pxpipe", "graphify", "bash", "git"] {
        println!("  {bin:<9} {}", find_in_path(bin).map(|p| display_path(&p)).unwrap_or_else(|| "not found".into()));
    }
    println!("MCP servers in config: {}", config.mcp.servers.len());
    println!("proxies: {}", config.proxy.upstreams.iter().map(|u| format!("{} {}", u.name, u.url)).collect::<Vec<_>>().join(", "));
    println!("transcripts: {}", display_path(&crusty_core::paths::sessions_dir(&workspace, config.session.dir.as_deref())));
    if missing_key {
        println!("\nThe orchestrator's provider has no API key: export it, or pick another provider with --model provider:model.");
        return Ok(1);
    }
    Ok(0)
}

async fn mcp(cli: &Cli) -> Result<i32> {
    let loaded = load_config(cli)?;
    let servers = crusty_mcp::merged_servers(&loaded.config.mcp, &loaded.workspace, cli.mcp_config.as_deref())?;
    if servers.is_empty() {
        println!("no MCP servers configured (config [mcp.servers], .mcp.json or --mcp-config)");
        return Ok(0);
    }
    let manager = crusty_mcp::McpManager::connect_all(servers, &EventSink::noop(), Duration::from_secs(30)).await;
    for s in manager.status() {
        match &s.error {
            Some(e) => println!("✕ {} ({}): {e}", s.name, s.transport),
            None => println!("✓ {} ({}): {} tools", s.name, s.transport, s.tools),
        }
    }
    for t in manager.tools() {
        println!("  {}", t.name());
    }
    manager.shutdown().await;
    Ok(0)
}

async fn models(cli: &Cli, provider: Option<&str>) -> Result<i32> {
    let loaded = load_config(cli)?;
    let id = provider.unwrap_or(&loaded.config.orchestrator.model.provider).to_owned();
    let registry = ProviderRegistry::from_config(&loaded.config);
    let p = registry.get(&id).with_context(|| format!("unknown provider `{id}`"))?;
    for m in p.list_models().await.map_err(|e| anyhow::anyhow!("{e}"))? {
        println!("{m}");
    }
    Ok(0)
}
