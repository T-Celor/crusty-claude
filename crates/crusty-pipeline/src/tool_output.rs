//! Tool-output stages: Triage keeps the error blocks of a failed build or test log, Rerank
//! keeps the most relevant lines of a search. Both keep the removed text retrievable.

use crusty_advisor::rank::{bm25_scores, top_k};
use crusty_advisor::triage::{ErrorBlock, block_score, condense, extract};
use crusty_core::config::OptimizerMode;
use crusty_core::event::{AppEvent, Stage, StageStatus};
use crusty_core::tokens::estimate_text;
use crusty_core::tool::ToolOutput;
use crusty_core::{ContentBlock, OptimizeContext};

use crate::Pipeline;

/// Shell tools whose failed output is triaged.
const TRIAGE_TOOLS: [&str; 2] = ["Bash", "BashOutput"];
/// Search tools whose results are reranked.
const RERANK_TOOLS: [&str; 3] = ["Grep", "Glob", "Graph"];

impl Pipeline {
    /// Shortens `output` of `tool` when a stage applies. Outputs with images are never touched.
    /// Observe mode records what would have changed and leaves the output as it is.
    pub(crate) async fn shorten_tool_output(&self, tool: &str, output: &mut ToolOutput, goal: &str, ctx: &OptimizeContext) {
        if output.content.iter().any(|b| matches!(b, ContentBlock::Image { .. })) {
            return;
        }
        let failed = output.is_error || output.exit_code.is_some_and(|c| c != 0);
        if TRIAGE_TOOLS.contains(&tool) && failed && self.on(Stage::Triage) {
            self.triage_output(output, ctx).await;
        } else if RERANK_TOOLS.contains(&tool) && !output.is_error && self.on(Stage::Rerank) {
            // Error text is not a result list: reranking it could drop the lines that explain the failure.
            self.rerank_output(output, goal, ctx).await;
        }
    }

    /// Replaces `output` by `shortened` when it is shorter, storing the original; in observe
    /// mode only the metric is emitted. `label` is the stage and its transformer name.
    fn apply_shortened(
        &self,
        output: &mut ToolOutput,
        original: &str,
        shortened: String,
        label: (Stage, &str),
        detail: String,
        ctx: &OptimizeContext,
    ) {
        let (stage, transformer) = label;
        let before = estimate_text(original);
        let after = estimate_text(&shortened);
        let mut m = self.metric(stage, "tool_output", transformer, ctx);
        m.tokens_before_est = before;
        m.tokens_after_est = after;
        m.input_bytes = original.len() as u64;
        m.output_bytes = shortened.len() as u64;
        m.detail = detail;
        if after >= before {
            m.status = StageStatus::Skipped;
        } else if self.mode() == OptimizerMode::Observe {
            m.status = StageStatus::Observed;
            m.detail = format!("observe: would save {} tokens · {}", before - after, m.detail);
        } else {
            let handle = self.store.put(original.to_owned());
            m.status = StageStatus::Applied;
            m.original_ref = Some(handle.clone());
            m.detail = format!("{} · Retrieve(\"{handle}\") restores the full output", m.detail);
            output.content = vec![ContentBlock::text(format!("{shortened}\n[full output: Retrieve(\"{handle}\")]"))];
        }
        self.events.emit(AppEvent::Optimizer(m));
    }

    /// Triage: keeps the best `max_blocks` error blocks of a long failed log, in order.
    async fn triage_output(&self, output: &mut ToolOutput, ctx: &OptimizeContext) {
        let cfg = &self.laya.triage;
        let text = output.text_content();
        if text.lines().count() < cfg.min_lines {
            return;
        }
        let blocks: Vec<ErrorBlock> = extract(&text, cfg);
        if blocks.is_empty() {
            return;
        }
        let texts: Vec<String> = blocks.iter().map(|b| b.text.clone()).collect();
        let scores = self.advisor.error_scores(&texts).await;
        let values: Vec<f64> = match scores.for_items(blocks.len()) {
            Some(v) => v.to_vec(),
            None => texts.iter().map(|t| block_score(t)).collect(),
        };
        let mut keep = top_k(&values, cfg.max_blocks);
        keep.sort_unstable();
        let kept: Vec<ErrorBlock> = keep.iter().map(|&i| blocks[i].clone()).collect();
        let shortened = condense(&text, &kept);
        let detail = format!("kept {} of {} error blocks", kept.len(), blocks.len());
        self.apply_shortened(output, &text, shortened, (Stage::Triage, "triage:error-blocks"), detail, ctx);
    }

    /// Rerank: keeps the `keep` most relevant non-empty lines of a search, in order.
    async fn rerank_output(&self, output: &mut ToolOutput, goal: &str, ctx: &OptimizeContext) {
        let cfg = &self.laya.rerank;
        if goal.trim().is_empty() {
            return;
        }
        let text = output.text_content();
        let lines: Vec<String> = text.lines().filter(|l| !l.trim().is_empty()).map(str::to_owned).collect();
        if lines.len() < cfg.min_items || lines.len() <= cfg.keep {
            return;
        }
        let scores = self.advisor.relevance(goal, &lines).await;
        let values: Vec<f64> = match scores.for_items(lines.len()) {
            Some(v) => v.to_vec(),
            None => bm25_scores(goal, &lines),
        };
        let mut keep = top_k(&values, cfg.keep);
        keep.sort_unstable();
        let mut shortened: Vec<&str> = keep.iter().map(|&i| lines[i].as_str()).collect();
        let note = format!("[rerank: kept {} of {} results by relevance to the task]", keep.len(), lines.len());
        shortened.push(note.as_str());
        let detail = format!("kept {} of {} results", keep.len(), lines.len());
        self.apply_shortened(output, &text, shortened.join("\n"), (Stage::Rerank, "rerank:relevance"), detail, ctx);
    }
}
