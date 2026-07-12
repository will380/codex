//! Patch summaries and image-tool transcript helpers.

use super::*;
use codex_utils_path_uri::LegacyAppPathString;

#[derive(Debug)]
pub(crate) struct PatchHistoryCell {
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
}

impl HistoryCell for PatchHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = create_diff_summary(&self.changes, &self.cwd, width as usize);
        if is_large_patch(&self.changes) {
            lines.push(Line::from(vec![
                "  └ ".dim(),
                "Click to inspect full diff".light_blue().underlined(),
            ]));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(create_diff_summary(
            &self.changes,
            &self.cwd,
            RAW_DIFF_SUMMARY_WIDTH,
        ))
    }

    fn display_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        annotate_changed_file_paths(self.display_lines(width), &self.changes, &self.cwd)
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn transcript_interaction(&self) -> Option<HistoryCellInteraction> {
        let cwd = AbsolutePathBuf::try_from(self.cwd.clone()).ok()?;
        Some(HistoryCellInteraction::OpenPatchDiff {
            changes: self.changes.clone(),
            cwd,
        })
    }

    fn has_transcript_interaction(&self) -> bool {
        true
    }
}
/// Create a new `PendingPatch` cell that lists the file‑level summary of
/// a proposed patch. The summary lines should already be formatted (e.g.
/// "A path/to/file.rs").
pub(crate) fn new_patch_event(
    changes: HashMap<PathBuf, FileChange>,
    cwd: &Path,
) -> PatchHistoryCell {
    PatchHistoryCell {
        changes,
        cwd: cwd.to_path_buf(),
    }
}

pub(crate) fn new_patch_apply_failure(stderr: String) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Failure title
    lines.push(Line::from("✘ Failed to apply patch".magenta().bold()));

    if !stderr.trim().is_empty() {
        let output = output_lines(
            Some(&CommandOutput {
                exit_code: 1,
                formatted_output: String::new(),
                aggregated_output: stderr,
            }),
            OutputLinesParams {
                line_limit: TOOL_CALL_MAX_LINES,
                only_err: true,
                include_angle_pipe: true,
                include_prefix: true,
            },
        );
        lines.extend(output.lines);
    }

    PlainHistoryCell { lines }
}

pub(crate) fn new_view_image_tool_call(
    path: LegacyAppPathString,
    cwd: &Path,
) -> HyperlinkHistoryCell {
    let rendered_path = path.render_for_ui();
    let resolved_path = if let Some(path) = path.to_inferred_abs_path() {
        Some(path.as_path().to_path_buf())
    } else if path.infer_absolute_path_convention().is_none() {
        Some(cwd.join(PathBuf::from(&rendered_path)))
    } else {
        // Preserve an absolute path from another OS without creating a misleading local file URL.
        None
    };
    let display_path = resolved_path
        .as_ref()
        .map(|path| display_path_for(path, cwd))
        .unwrap_or(rendered_path);
    let destination = resolved_path.as_deref().and_then(file_url);

    let mut path_line = HyperlinkLine::new(Line::default());
    path_line.push_span("  └ ".dim(), None);
    path_line.push_span(
        display_path.light_blue().underlined(),
        destination.as_deref(),
    );

    HyperlinkHistoryCell::new(vec![
        HyperlinkLine::from(Line::from(vec!["• ".dim(), "Viewed Image".bold()])),
        path_line,
    ])
}

pub(crate) fn new_image_generation_call(
    call_id: String,
    status: &str,
    revised_prompt: Option<String>,
    saved_path: Option<AbsolutePathBuf>,
) -> HyperlinkHistoryCell {
    let detail = revised_prompt.unwrap_or(call_id);
    let heading: Line<'static> = if status == "failed" {
        vec!["✗ ".red().bold(), "Image generation failed".bold()].into()
    } else {
        vec!["• ".dim(), "Generated Image:".bold()].into()
    };
    let mut lines = vec![
        HyperlinkLine::from(heading),
        HyperlinkLine::from(Line::from(vec!["  └ ".dim(), detail.dim()])),
    ];
    if let Some(saved_path) = saved_path {
        let destination = file_url(saved_path.as_path());
        let mut saved_line = HyperlinkLine::new(Line::default());
        saved_line.push_span("  └ ".dim(), None);
        saved_line.push_span("Saved to: ".dim(), None);
        saved_line.push_span(
            saved_path.display().to_string().light_blue().underlined(),
            destination.as_deref(),
        );
        lines.push(saved_line);
    }

    HyperlinkHistoryCell::new(lines)
}

fn file_url(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

fn is_large_patch(changes: &HashMap<PathBuf, FileChange>) -> bool {
    const COLLAPSED_DIFF_LINE_THRESHOLD: usize = 20;
    let changed_lines = changes
        .values()
        .map(|change| match change {
            FileChange::Add { content } | FileChange::Delete { content } => content.lines().count(),
            FileChange::Update { unified_diff, .. } => unified_diff
                .lines()
                .filter(|line| {
                    (line.starts_with('+') && !line.starts_with("+++"))
                        || (line.starts_with('-') && !line.starts_with("---"))
                })
                .count(),
        })
        .sum::<usize>();
    changes.len() > 1 || changed_lines >= COLLAPSED_DIFF_LINE_THRESHOLD
}

fn annotate_changed_file_paths(
    lines: Vec<Line<'static>>,
    changes: &HashMap<PathBuf, FileChange>,
    cwd: &Path,
) -> Vec<HyperlinkLine> {
    lines
        .into_iter()
        .map(|line| {
            let visible_text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let mut line = HyperlinkLine::new(line);
            for path in changes.keys() {
                let display_path = display_path_for(path.as_path(), cwd);
                let Some(byte_start) = visible_text.find(&display_path) else {
                    continue;
                };
                let absolute_path = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(path)
                };
                let Some(destination) = file_url(&absolute_path) else {
                    continue;
                };
                let start = visible_text[..byte_start].width();
                line.hyperlinks.push(TerminalHyperlink {
                    columns: start..start + display_path.width(),
                    destination,
                });
            }
            line.hyperlinks.sort_by_key(|link| link.columns.start);
            line
        })
        .collect()
}
