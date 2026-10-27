//! Colors and styles, all in one place. Named ANSI colors keep both light and dark
//! terminals readable, unlike fixed RGB values.

use crusty_core::permission::{PermissionMode, RiskLevel};
use ratatui::style::{Color, Modifier, Style};

/// Accents: prompt marks, tool lines, delegation, focused tab.
pub const ACCENT: Color = Color::Cyan;
/// Success: finished tools, applied optimizer stages, done agents.
pub const OK: Color = Color::Green;
/// Warnings, pending approvals, plan-mode and accept-edits badges.
pub const WARN: Color = Color::Yellow;
/// Errors, failed tools, bypass mode, down proxies.
pub const ERR: Color = Color::Red;
/// Secondary text: previews, info lines, disabled stages.
pub const MUTED: Color = Color::DarkGray;

/// Secondary text such as previews and hints.
pub fn dim() -> Style {
    Style::new().fg(MUTED)
}

/// Model thinking, shown dim and italic.
pub fn thinking() -> Style {
    Style::new().fg(MUTED).add_modifier(Modifier::ITALIC)
}

/// Warning text.
pub fn warn() -> Style {
    Style::new().fg(WARN)
}

/// Error text.
pub fn err() -> Style {
    Style::new().fg(ERR)
}

/// Bold text, used for the user's prompts and headings.
pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Inline and fenced code.
pub fn code() -> Style {
    Style::new().fg(ACCENT)
}

/// The focused tab or selected row.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// Color of the permission mode badge.
pub fn mode_color(mode: PermissionMode) -> Color {
    match mode {
        PermissionMode::Default => Color::White,
        PermissionMode::AcceptEdits => WARN,
        PermissionMode::Plan => Color::Blue,
        PermissionMode::Auto => Color::Cyan,
        PermissionMode::BypassPermissions => ERR,
    }
}

/// Color of an approval's risk level.
pub fn risk_color(risk: RiskLevel) -> Color {
    match risk {
        RiskLevel::Low => OK,
        RiskLevel::Moderate => WARN,
        RiskLevel::High => Color::Magenta,
        RiskLevel::Critical => ERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_colors_escalate() {
        assert_eq!(risk_color(RiskLevel::Moderate), WARN);
        assert_eq!(risk_color(RiskLevel::Critical), ERR);
    }
}
