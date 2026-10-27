//! Drawing: header, tabs, conversation, exec and log tabs, input box, status line,
//! completion popup and the approval modal. Pure functions of `App`.

use std::time::Duration;

use crusty_core::permission::RiskLevel;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::app::{App, COMPLETION_ROWS, Entry, RewindPicker, Tab, ToolStatus};
use crate::logo;
use crate::markdown;
use crate::panel;
use crate::theme;

/// Side panel is hidden below this terminal width.
const PANEL_MIN_WIDTH: u16 = 90;
/// Welcome header with the logo needs at least this many columns.
const WELCOME_MIN_WIDTH: u16 = 60;
/// Input box grows to this many text lines (plus its border).
const INPUT_MAX_LINES: u16 = 8;
/// Lines of tool output previewed under a tool line.
const PREVIEW_LINES: usize = 4;

/// Draws the whole screen.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let text_lines = app.input.text().split('\n').count() as u16;
    let input_height = (text_lines.min(INPUT_MAX_LINES) + 2).max(3);
    let head = header_lines(app, area.width);
    let head_height = head.len() as u16;
    let [header, body, input_area, status] =
        Layout::vertical([Constraint::Length(head_height), Constraint::Min(3), Constraint::Length(input_height), Constraint::Length(1)])
            .areas(area);

    frame.render_widget(Paragraph::new(head), header);

    let main = if app.show_panel && area.width >= PANEL_MIN_WIDTH {
        let [main, panel_area] =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(app.config().tui.side_panel_width)]).areas(body);
        panel::draw(frame, panel_area, app);
        main
    } else {
        body
    };

    let [tabs_area, content] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(main);
    frame.render_widget(Paragraph::new(tabs_line(app.tab)), tabs_area);
    match app.tab {
        Tab::Chat => draw_chat(frame, content, app),
        Tab::Exec => draw_exec(frame, content, app),
        Tab::Log => draw_log(frame, content, app),
    }

    draw_completion(frame, content, input_area, app);
    draw_input(frame, input_area, app);
    frame.render_widget(Paragraph::new(status_line(app)), status);

    if let Some(req) = app.approvals.front() {
        draw_approval(frame, area, app, req);
    } else if let Some(picker) = &app.rewind {
        draw_rewind(frame, area, picker);
    }
}

/// Number of terminal rows `lines` take when wrapped to `width` columns.
pub(crate) fn rows(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines.iter().map(|l| l.width().max(1).div_ceil(width)).sum()
}

/// Header rows. While the transcript is empty and the terminal is wide enough, a welcome
/// banner with the logo; otherwise one line.
fn header_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let ctx = app.context_pct.map_or_else(|| "–".to_string(), |p| format!("{p}%"));
    let version = env!("CARGO_PKG_VERSION");
    let mode = Span::styled(app.mode.to_string(), ratatui::style::Style::new().fg(theme::mode_color(app.mode)));
    if app.entries.is_empty() && width >= WELCOME_MIN_WIDTH {
        let mut lines = logo::logo_lines();
        let texts = [
            Line::styled(format!("Crusty-Claude v{version} — orchestrator {}", app.orchestrator), theme::bold()),
            Line::from(vec![Span::raw(format!("{} │ mode: ", app.short_session())), mode, Span::raw(format!(" │ ctx {ctx}"))]),
        ];
        for (i, text) in texts.into_iter().enumerate() {
            if let Some(line) = lines.get_mut(i + 1) {
                line.spans.push(Span::raw("  "));
                line.spans.extend(text.spans);
            }
        }
        return lines;
    }
    vec![Line::from(vec![
        Span::styled(format!("Crusty-Claude v{version}"), theme::bold()),
        Span::raw(format!(" │ {} │ mode: ", app.short_session())),
        mode,
        Span::raw(format!(" │ orchestrator: {} │ ctx {ctx}", app.orchestrator)),
    ])]
}

/// The checkpoint picker, centred over the screen.
fn draw_rewind(frame: &mut Frame<'_>, area: Rect, picker: &RewindPicker) {
    let width = (area.width * 7 / 10).max(40).min(area.width);
    let mut lines = vec![Line::styled("↑/↓ select · c code · v conversation · enter both · esc close", theme::dim())];
    for (i, cp) in picker.items.iter().enumerate() {
        let selected = i == picker.selected;
        let marker = if selected { "› " } else { "  " };
        let style = if selected { theme::selected() } else { ratatui::style::Style::new() };
        let when: String = cp.created.chars().take(19).collect::<String>().replace('T', " ");
        lines.push(Line::styled(format!("{marker}#{} {} · {} files · {when}", cp.id, cp.label, cp.files), style));
    }
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + area.height.saturating_sub(height) / 2, width, height);
    let block = Block::bordered().title(" Rewind — checkpoints ");
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), rect);
}

fn tabs_line(selected: Tab) -> Line<'static> {
    let tabs = [(Tab::Chat, "F1 Chat"), (Tab::Exec, "F2 Exec"), (Tab::Log, "F3 Log")];
    let spans: Vec<Span<'static>> = tabs
        .into_iter()
        .flat_map(|(tab, label)| {
            let style = if tab == selected { theme::selected() } else { theme::dim() };
            [Span::styled(format!(" {label} "), style), Span::raw(" ")]
        })
        .collect();
    Line::from(spans)
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = chat_lines(app);
    let total = rows(&lines, area.width);
    let height = usize::from(area.height);
    let max_top = total.saturating_sub(height);
    let top = max_top.saturating_sub(app.scroll_back);
    let top = u16::try_from(top).unwrap_or(u16::MAX);
    let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((top, 0));
    frame.render_widget(widget, area);
}

/// Conversation lines for the Chat tab.
fn chat_lines(app: &App) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in &app.entries {
        match entry {
            Entry::User { text } => {
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "> " } else { "  " };
                    out.push(Line::styled(format!("{prefix}{line}"), theme::bold()));
                }
            }
            Entry::Assistant { agent_id, label, text } => {
                if app.is_executor(agent_id) {
                    out.push(Line::styled(format!("  ⎿ {label}"), theme::dim()));
                    out.extend(indented(markdown::render(text), "    "));
                } else {
                    out.extend(first_prefixed(markdown::render(text), "● ", "  "));
                }
            }
            Entry::Thinking { text, .. } => {
                if app.show_thinking {
                    for line in text.lines() {
                        out.push(Line::styled(format!("  {line}"), theme::thinking()));
                    }
                }
            }
            Entry::Tool { summary, status, preview, exit_code, .. } => {
                out.push(tool_line(summary, status, *exit_code));
                for line in preview.lines().take(PREVIEW_LINES) {
                    out.push(Line::styled(format!("    {line}"), theme::dim()));
                }
            }
            Entry::Info(text) => out.push(Line::styled(text.clone(), theme::dim())),
            Entry::Warning(text) => out.push(Line::styled(format!("⚠ {text}"), theme::warn())),
            Entry::Error(text) => out.push(Line::styled(format!("✗ {text}"), theme::err())),
            Entry::Command { command, output } => {
                out.push(Line::styled(format!("❯ {command}"), theme::code()));
                if !output.is_empty() {
                    out.extend(indented(output.lines().map(|l| Line::raw(l.to_string())).collect(), "  "));
                }
            }
            Entry::Delegation { domain, model, task, .. } => {
                let short = model.strip_prefix("claude-").unwrap_or(model);
                out.push(Line::styled(format!("↳ delegated to {domain} ({short}): {task}"), theme::code()));
            }
        }
    }
    out
}

/// One tool line: `⏺ summary` plus its status.
fn tool_line(summary: &str, status: &ToolStatus, exit_code: Option<i32>) -> Line<'static> {
    let (text, style) = match status {
        ToolStatus::Running => ("…".to_string(), theme::dim()),
        ToolStatus::Ok(d) => (format!("✓ {}", fmt_duration(*d)), ratatui::style::Style::new().fg(theme::OK)),
        ToolStatus::Failed(d) => match exit_code {
            Some(code) => (format!("✗ exit {code}"), ratatui::style::Style::new().fg(theme::ERR)),
            None => (format!("✗ {}", fmt_duration(*d)), ratatui::style::Style::new().fg(theme::ERR)),
        },
    };
    Line::from(vec![Span::styled("⏺ ", theme::code()), Span::raw(summary.to_string()), Span::raw("  "), Span::styled(text, style)])
}

/// Adds `prefix` to the first line and `rest` to the others.
fn first_prefixed(lines: Vec<Line<'static>>, first: &str, rest: &str) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 { first } else { rest };
            let mut spans = vec![Span::raw(prefix.to_string())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Adds `prefix` in front of every line.
fn indented(lines: Vec<Line<'static>>, prefix: &str) -> Vec<Line<'static>> {
    first_prefixed(lines, prefix, prefix)
}

/// `1.2s` for a second or more, `850ms` otherwise.
pub fn fmt_duration(d: Duration) -> String {
    if d.as_secs() >= 1 { format!("{:.1}s", d.as_secs_f64()) } else { format!("{}ms", d.as_millis()) }
}

fn draw_exec(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let header = Row::new(["time", "agent", "tool", "status", "duration", "exit"]).style(theme::bold());
    let rows_iter = app.exec_log.iter().rev().map(|run| {
        let (status, duration) = match &run.status {
            ToolStatus::Running => ("running", "-".to_string()),
            ToolStatus::Ok(d) => ("ok", fmt_duration(*d)),
            ToolStatus::Failed(d) => ("failed", fmt_duration(*d)),
        };
        let exit = run.exit_code.map_or_else(|| "-".to_string(), |c| c.to_string());
        Row::new([
            Cell::from(clock(run.at_secs)),
            Cell::from(run.agent_id.clone()),
            Cell::from(run.summary.clone()),
            Cell::from(status),
            Cell::from(duration),
            Cell::from(exit),
        ])
    });
    let widths = [
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Min(10),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(6),
    ];
    frame.render_widget(Table::new(rows_iter, widths).header(header), area);
}

/// `HH:MM:SS` in UTC from unix seconds.
fn clock(secs: u64) -> String {
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

fn draw_log(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines: Vec<Line<'static>> = app
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Info(t) => Some(Line::styled(t.clone(), theme::dim())),
            Entry::Warning(t) => Some(Line::styled(format!("warning: {t}"), theme::warn())),
            Entry::Error(t) => Some(Line::styled(format!("error: {t}"), theme::err())),
            Entry::Command { command, output } => Some(Line::styled(format!("{command}\n{output}"), theme::code())),
            _ => None,
        })
        .collect();
    let total = rows(&lines, area.width);
    let top = total.saturating_sub(usize::from(area.height));
    let top = u16::try_from(top).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((top, 0)), area);
}

/// The completion popup above the input box, when typing a slash command.
fn draw_completion(frame: &mut Frame<'_>, content: Rect, input: Rect, app: &App) {
    let items = app.completion_items();
    if items.is_empty() || app.approvals.front().is_some() || app.rewind.is_some() {
        return;
    }
    let count = items.len().min(COMPLETION_ROWS) as u16;
    let y = input.y.saturating_sub(count).max(content.y);
    let rect = Rect::new(input.x, y, input.width.min(60), count.min(input.y.saturating_sub(y)));
    let lines: Vec<Line<'static>> = items
        .iter()
        .take(COMPLETION_ROWS)
        .enumerate()
        .map(|(i, item)| {
            let label_style = if i == app.completion_sel { theme::selected() } else { theme::code() };
            Line::from(vec![Span::styled(item.label.clone(), label_style), Span::styled(format!("  {}", item.detail), theme::dim())])
        })
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines), rect);
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::bordered().title(" prompt ");
    let inner = block.inner(area);
    let text = app.input.text();
    let mut lines: Vec<Line<'static>> = if text.is_empty() {
        vec![Line::styled("Type a message or /command", theme::dim())]
    } else {
        text.split('\n').map(|l| Line::raw(l.to_string())).collect()
    };
    if let Some(rest) = app.history_suggestion().and_then(|full| full.strip_prefix(text))
        && let Some(last) = lines.last_mut()
    {
        last.spans.push(Span::styled(rest.to_string(), theme::dim()));
    }
    let (row, col) = app.input.cursor_row_col();
    let scroll = row.saturating_sub(usize::from(inner.height.saturating_sub(1)));
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
    if app.approvals.is_empty() && inner.width > 0 && inner.height > 0 {
        let x = inner.x + (col as u16).min(inner.width - 1);
        let y = inner.y + (row as u16).saturating_sub(scroll).min(inner.height - 1);
        frame.set_cursor_position((x, y));
    }
}

fn status_line(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled("⏎ send · ⇧⏎ newline · ⇧⇥ mode · esc interrupt · ^T panel · /help", theme::dim())];
    if app.busy() {
        let frame = app.spinner % 4;
        let glyph = ["◐", "◓", "◑", "◒"][frame];
        spans.push(Span::styled(
            format!(" · {glyph} working… · {} agents running", app.running_agents()),
            ratatui::style::Style::new().fg(theme::ACCENT),
        ));
    }
    Line::from(spans)
}

/// Centered modal listing the front approval request.
fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App, req: &crusty_core::permission::ApprovalRequest) {
    let width = (area.width * 7 / 10).max(40).min(area.width);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![Span::raw("tool:   "), Span::styled(req.summary.clone(), theme::bold())]),
        Line::raw(format!("reason: {}", req.reason)),
        Line::raw(format!("agent:  {}", req.agent_id)),
    ];
    lines.extend(req.preview.lines().take(12).map(|l| Line::styled(format!("  {l}"), theme::dim())));
    if let Some(fb) = &app.feedback {
        lines.push(Line::styled(format!("feedback: {fb}_"), theme::warn()));
        lines.push(Line::styled("[enter] send feedback  [esc] cancel", theme::dim()));
    } else {
        let mut keys = String::from("[y] allow once");
        if let Some(rule) = &req.suggested_rule {
            keys.push_str(&format!("  [a] always allow {rule}"));
        }
        keys.push_str("  [n] deny  [f] deny with feedback");
        lines.push(Line::styled(keys, theme::bold()));
    }
    let queued = app.approvals.len().saturating_sub(1);
    let more = if queued > 0 { format!(" (+{queued} more)") } else { String::new() };
    let title = format!(" Permission required — {}{more} ", risk_label(req.risk));
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + area.height.saturating_sub(height) / 2, width, height);
    let style = ratatui::style::Style::new().fg(theme::risk_color(req.risk));
    let block = Block::bordered().title(Span::styled(title, style)).border_style(style);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), rect);
}

fn risk_label(risk: RiskLevel) -> String {
    risk.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TuiOptions;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crusty_core::config::Config;
    use crusty_core::event::AppEvent;
    use crusty_core::metrics::Metrics;
    use crusty_core::permission::{ApprovalRequest, PermissionMode};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn app() -> App {
        let opts = TuiOptions {
            config: Arc::new(Config::default()),
            session_id: "abcdefghijk".into(),
            cwd: ".".into(),
            orchestrator: "anthropic:claude-opus-5-5".into(),
            permission_mode: PermissionMode::Plan,
            metrics: Arc::new(Metrics::default()),
            commands: vec![("help".into(), "show help".into())],
            initial_prompt: None,
        };
        App::new(&opts)
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        term.draw(|f| draw(f, app)).expect("draw");
        let buf = term.backend().buffer();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn approval() -> ApprovalRequest {
        ApprovalRequest {
            id: "r1".into(),
            agent_id: "orchestrator".into(),
            tool: "Bash".into(),
            summary: "Bash(rm -rf target)".into(),
            risk: RiskLevel::Critical,
            reason: "deletes build output".into(),
            suggested_rule: Some("Bash(rm -rf target)".into()),
            preview: "rm -rf target".into(),
        }
    }

    #[test]
    fn wide_screen_shows_panel_and_header() {
        let mut app = app();
        app.on_tick();
        let text = screen(&app, 120, 60);
        for needle in ["Optimizer", "RTK", "Headroom", "pxpipe", "Ponytail", "Agents", "mode: plan"] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn approval_modal_title_shows_when_pending() {
        let mut app = app();
        app.handle_event(AppEvent::ApprovalRequested(approval()));
        let text = screen(&app, 120, 40);
        assert!(text.contains("Permission required"));
        assert!(text.contains("always allow"));
    }

    #[test]
    fn narrow_terminal_renders_without_panel() {
        let app = app();
        let text = screen(&app, 60, 20);
        assert!(!text.contains("Optimizer"));
    }

    #[test]
    fn chat_shows_user_prompt_and_completion_popup() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        let text = screen(&app, 100, 30);
        assert!(text.contains("/help"));
        // Enter first accepts the highlighted popup row, the next Enter submits it.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "/help ");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let text = screen(&app, 100, 30);
        assert!(text.contains("❯ /help"));
    }

    #[test]
    fn welcome_header_shows_logo_and_version() {
        let app = app();
        let text = screen(&app, 100, 30);
        assert!(text.contains("Crusty-Claude v"));
        assert!(text.contains("orchestrator anthropic:claude-opus-5-5"));
    }

    #[test]
    fn panel_renders_new_blocks() {
        let mut app = app();
        app.on_tick();
        let text = screen(&app, 140, 60);
        for needle in ["Advisor", "Sandbox", "DLP", "Checkpoints", "not reported", "nothing masked"] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn auto_mode_badge_and_cycle() {
        let mut app = app();
        app.mode = PermissionMode::Auto;
        assert!(screen(&app, 100, 30).contains("mode: auto"));
        assert_eq!(PermissionMode::AcceptEdits.next(), PermissionMode::Auto);
    }

    #[test]
    fn completion_popup_shows_file_rows() {
        let mut app = app();
        app.set_file_index(vec!["src/lib.rs".into()]);
        for c in "@lib".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(screen(&app, 100, 30).contains("src/lib.rs"));
    }

    #[test]
    fn duration_and_clock_format() {
        assert_eq!(fmt_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(fmt_duration(Duration::from_millis(1200)), "1.2s");
        assert_eq!(clock(3_661), "01:01:01");
    }
}
