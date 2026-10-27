//! Terminal UI: conversation, agents, live optimizer panel, approvals.
//!
//! `run` owns the terminal for its lifetime: raw mode, alternate screen and bracketed
//! paste are restored on return and on panic. Mouse capture stays off so text selection
//! keeps working.

pub mod app;
pub mod input;
pub mod markdown;
pub mod panel;
pub mod theme;
pub mod ui;

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crusty_core::config::Config;
use crusty_core::event::{AppEvent, UiCommand};
use crusty_core::metrics::Metrics;
use crusty_core::permission::PermissionMode;
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::app::App;

/// What the UI needs to start: configuration, session identity and shared metrics.
#[derive(Clone)]
pub struct TuiOptions {
    /// Loaded configuration (panel flags, optimizer and proxy settings).
    pub config: Arc<Config>,
    /// Id of the session, shown in the header.
    pub session_id: String,
    /// Working directory of the session.
    pub cwd: String,
    /// Orchestrator as `provider:model`.
    pub orchestrator: String,
    /// Permission mode at start.
    pub permission_mode: PermissionMode,
    /// Metrics the side panel reads on each tick.
    pub metrics: Arc<Metrics>,
    /// Slash commands as (name without '/', description), for completion.
    pub commands: Vec<(String, String)>,
    /// Prompt submitted as soon as the UI starts.
    pub initial_prompt: Option<String>,
}

/// Restores the terminal when dropped, so every exit path leaves it usable.
struct TerminalGuard;

impl TerminalGuard {
    /// Switches the terminal to the UI modes.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            restore_terminal();
            return Err(e);
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Puts the terminal back in its normal state. Errors are ignored: this runs on exit paths.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, Show);
}

/// Runs the UI until the user quits or `AppEvent::Shutdown` arrives.
///
/// Engine events come in on `events`; user commands go out on `commands`. A quit sends
/// `UiCommand::Quit` first. An `initial_prompt` is submitted before the first draw.
pub async fn run(
    opts: TuiOptions,
    mut events: mpsc::UnboundedReceiver<AppEvent>,
    commands: mpsc::UnboundedSender<UiCommand>,
) -> io::Result<()> {
    let tick = Duration::from_millis(opts.config.tui.tick_ms.max(16));
    let mut app = App::new(&opts);
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(tick);
    let mut dirty = true;

    if let Some(prompt) = opts.initial_prompt.as_deref()
        && !send_all(&commands, app.submit_text(prompt))
    {
        return Ok(());
    }

    loop {
        if dirty {
            if app.take_redraw_request() {
                terminal.clear()?;
            }
            terminal.draw(|f| ui::draw(f, &app))?;
            dirty = false;
        }
        if app.should_quit() {
            break;
        }
        tokio::select! {
            maybe = keys.next() => match maybe {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    if !send_all(&commands, app.handle_key(key)) {
                        break;
                    }
                    dirty = true;
                }
                Some(Ok(Event::Paste(text))) => {
                    app.handle_paste(&text);
                    dirty = true;
                }
                Some(Ok(Event::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => break,
            },
            ev = events.recv() => match ev {
                Some(ev) => {
                    app.handle_event(ev);
                    dirty = true;
                }
                None => break,
            },
            _ = ticker.tick() => {
                if app.on_tick() || app.is_animating() {
                    dirty = true;
                }
            }
        }
    }
    Ok(())
}

/// Sends every command; false when the session has gone away.
fn send_all(tx: &mpsc::UnboundedSender<UiCommand>, cmds: Vec<UiCommand>) -> bool {
    cmds.into_iter().all(|c| tx.send(c).is_ok())
}

/// Restores the terminal before a panic message is printed.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}
