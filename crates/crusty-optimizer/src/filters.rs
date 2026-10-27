//! `ContentFilter` backed by the native RTK filters and HTML conversion.

use crusty_core::config::OptimizerConfig;
use crusty_core::filter::{ContentFilter, Filtered, HtmlDoc};

use crate::html::convert_html;
use crate::rtk::filter_command;

/// Native implementation of `ContentFilter`.
#[derive(Debug, Clone)]
pub struct NativeFilters {
    optimizer: OptimizerConfig,
}

impl NativeFilters {
    pub fn new(cfg: &OptimizerConfig) -> Self {
        NativeFilters { optimizer: cfg.clone() }
    }
}

impl ContentFilter for NativeFilters {
    fn command_output(&self, command: &str, output: &str, exit_code: Option<i32>) -> Option<Filtered> {
        if !(self.optimizer.enabled && self.optimizer.rtk.enabled) {
            return None;
        }
        filter_command(command, output, exit_code, &self.optimizer.rtk)
    }

    /// Conversion is needed to read pages, so it ignores `optimizer.enabled`; the gain policy
    /// in `choose_filtered` still measures it.
    fn html_to_markdown(&self, html: &str, url: &str) -> Option<HtmlDoc> {
        if !self.optimizer.html_to_markdown.enabled {
            return None;
        }
        convert_html(html, url, self.optimizer.html_to_markdown.max_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_filter_respects_switches() {
        let output = "x\n".repeat(3) + "\x1b[31mred\x1b[0m\n";
        let mut cfg = OptimizerConfig { enabled: true, ..Default::default() };
        cfg.rtk.enabled = true;
        assert!(NativeFilters::new(&cfg).command_output("make", &output, Some(0)).is_some());
        cfg.rtk.enabled = false;
        assert!(NativeFilters::new(&cfg).command_output("make", &output, Some(0)).is_none());
        cfg.rtk.enabled = true;
        cfg.enabled = false;
        assert!(NativeFilters::new(&cfg).command_output("make", &output, Some(0)).is_none());
    }

    #[test]
    fn html_conversion_ignores_optimizer_switch() {
        let mut cfg = OptimizerConfig { enabled: false, ..Default::default() };
        cfg.html_to_markdown.enabled = true;
        let doc = NativeFilters::new(&cfg).html_to_markdown("<html><title>T</title><p>Hello there</p></html>", "u");
        assert!(doc.is_some_and(|d| d.markdown.contains("Hello there")));
        cfg.html_to_markdown.enabled = false;
        assert!(NativeFilters::new(&cfg).html_to_markdown("<p>Hi</p>", "u").is_none());
    }
}
