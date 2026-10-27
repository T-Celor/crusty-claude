//! `crusty`: the Crusty-Claude command line.

mod print_mode;
mod subcommands;
mod wiring;

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    Active,
    Observe,
}

/// Crusty-Claude: a multi-provider, multi-agent coding agent for the terminal.
/// The main agent is always an orchestrator that delegates to executor agents.
#[derive(Debug, Parser)]
#[command(name = "crusty", version, about, long_about = None)]
pub struct Cli {
    /// Prompt: submitted at start in interactive mode, or answered with --print.
    pub prompt: Option<String>,

    /// Answer the prompt (and stdin, if piped) without the interactive UI, then exit.
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Output of --print.
    #[arg(long, value_enum, default_value = "text")]
    pub output_format: OutputFormat,

    /// Continue the most recent session of this project.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a session by id (prefix); without a value, list sessions.
    #[arg(short = 'r', long, num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Orchestrator model, `provider:model` (or a bare model of the configured provider).
    #[arg(long)]
    pub model: Option<String>,

    /// Orchestrator effort: low, medium, high, xhigh, max.
    #[arg(long)]
    pub effort: Option<String>,

    /// Executor model per domain, e.g. `--domain-model coder=anthropic:claude-opus-5-5`.
    #[arg(long, value_name = "DOMAIN=PROVIDER:MODEL")]
    pub domain_model: Vec<String>,

    /// default, acceptEdits, plan or bypassPermissions.
    #[arg(long)]
    pub permission_mode: Option<String>,

    /// Same as `--permission-mode bypassPermissions`. Critical actions are still denied
    /// and high-risk actions still asked unless an allow rule names them.
    #[arg(long)]
    pub dangerously_skip_permissions: bool,

    /// Allow rules added for this run, e.g. `Bash(cargo test:*)`, `Edit(src/**)`.
    #[arg(long, alias = "allowedTools", value_delimiter = ',')]
    pub allowed_tools: Vec<String>,

    /// Deny rules for this run; a bare tool name also hides the tool from every agent.
    #[arg(long, alias = "disallowedTools", value_delimiter = ',')]
    pub disallowed_tools: Vec<String>,

    /// Extra directories the file tools may use.
    #[arg(long)]
    pub add_dir: Vec<PathBuf>,

    /// Extra config file, applied after the user and project ones.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Extra MCP servers file (Claude Code `.mcp.json` format).
    #[arg(long)]
    pub mcp_config: Option<PathBuf>,

    /// Replace the orchestrator's base system prompt.
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Text appended to every agent's system prompt.
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// Max model calls of the orchestrator for one prompt.
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// Run without any optimizer stage (requests sent as they are).
    #[arg(long)]
    pub no_optimizer: bool,

    /// Optimizer mode: `active` uses the optimized requests, `observe` only measures.
    #[arg(long, value_enum)]
    pub optimizer_mode: Option<ModeArg>,

    /// Project directory (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// With --print: show tool calls and agents on stderr.
    #[arg(long)]
    pub verbose: bool,

    /// Debug logging (to ~/.crusty/logs, or stderr with --print --verbose).
    #[arg(long)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the built-in optimizing proxy (Anthropic-compatible) for any client.
    Proxy {
        /// Listen address (default: proxy.listen).
        #[arg(long)]
        listen: Option<String>,
        /// Upstream for direct requests (default: proxy.forward_url).
        #[arg(long)]
        forward: Option<String>,
        /// Print one line per routed request (`--live false` to silence).
        #[arg(long, default_value_t = true, action = ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
        live: bool,
    },
    /// Code graph (native Graphify): build, map, find, explain, report.
    Graph {
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Configuration helpers.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check providers, keys, external tools and integrations.
    Doctor,
    /// Trust this project's own configuration (`.crusty/config.toml` providers, proxies,
    /// hooks, MCP servers and permission settings, and `.mcp.json`).
    Trust {
        /// Withdraw the trust instead.
        #[arg(long)]
        remove: bool,
    },
    /// List saved sessions of this project.
    Sessions,
    /// Connect to the configured MCP servers and list their tools.
    Mcp,
    /// List the models a provider offers.
    Models {
        /// Provider id (default: the orchestrator's).
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum GraphAction {
    /// Build or refresh the graph.
    Build,
    /// Print the repository map given to the orchestrator.
    Map,
    /// Find symbols and files.
    Find { query: String },
    /// Explain a file or symbol.
    Explain { target: String },
    /// Print GRAPH_REPORT.md.
    Report,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the effective configuration.
    Show,
    /// Print the config file locations, in load order.
    Path,
    /// Write a commented `.crusty/config.toml` in the project.
    Init,
    /// Validate the configuration.
    Validate,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let code = runtime.block_on(run(cli))?;
    // Background tasks (health probes, MCP children) must not keep the process alive.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    let _log = wiring::init_logging(&cli);
    match &cli.command {
        Some(cmd) => subcommands::run(&cli, cmd).await,
        None if cli.print => print_mode::run(&cli).await,
        None => wiring::run_interactive(&cli).await.map(|()| 0),
    }
}
