//! Overlay UIs rendered in an alternate screen.
//!
//! This module implements the pager-style overlays used by the TUI, including the transcript
//! overlay (`Ctrl+T`) that renders a full history view separate from the main viewport.
//!
//! The transcript overlay renders committed transcript cells plus an optional render-only live tail
//! derived from the current in-flight active cell. Because rebuilding wrapped `Line`s on every draw
//! can be expensive, that live tail is cached and only recomputed when its cache key changes, which
//! is derived from the terminal width (wrapping), an active-cell revision (in-place mutations), the
//! stream-continuation flag (spacing), and an animation tick (time-based spinner/shimmer output).
//!
//! The transcript overlay live tail is kept in sync by `App` during draws: `App` supplies an
//! `ActiveCellTranscriptKey` and a function to compute the active cell transcript lines, and
//! `TranscriptOverlay::sync_live_tail` uses the key to decide when the cached tail must be
//! recomputed. `ChatWidget` is responsible for producing a key that changes when the active cell
//! mutates in place or when its transcript output is time-dependent.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Result;
use std::sync::Arc;

use crate::chatwidget::ActiveCellTranscriptKey;
use crate::exec_cell::ExecCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::InlineExpansionRegion;
use crate::history_cell::UserHistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::key_hint::KeyBindingListExt;
use crate::keymap::PagerKeymap;
use crate::render::Insets;
use crate::render::renderable::InsetRenderable;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::mark_buffer_hyperlinks;
use crate::terminal_hyperlinks::visible_lines;
use crate::tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::MouseButton;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;

pub(crate) enum Overlay {
    Transcript(TranscriptOverlay),
    Static(StaticOverlay),
}

impl Overlay {
    pub(crate) fn new_transcript(cells: Vec<Arc<dyn HistoryCell>>, keymap: PagerKeymap) -> Self {
        Self::Transcript(TranscriptOverlay::new(cells, keymap))
    }

    pub(crate) fn new_mouse_scrollback(
        cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Transcript(TranscriptOverlay::new_with_presentation(
            cells,
            keymap,
            TranscriptPresentation::Display,
        ))
    }

    pub(crate) fn new_static_with_lines(
        lines: Vec<Line<'static>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Static(StaticOverlay::with_title(lines, title, keymap))
    }

    pub(crate) fn new_static_with_renderables(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self::Static(StaticOverlay::with_renderables(renderables, title, keymap))
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match self {
            Overlay::Transcript(o) => o.handle_event(tui, event),
            Overlay::Static(o) => o.handle_event(tui, event),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_done(),
            Overlay::Static(o) => o.is_done(),
        }
    }

    pub(crate) fn is_scrolled_to_bottom(&self) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_scrolled_to_bottom(),
            Overlay::Static(_) => false,
        }
    }
}

fn first_or_empty(bindings: &[KeyBinding]) -> Vec<KeyBinding> {
    bindings.first().copied().into_iter().collect()
}

// Render a single line of key hints from (key(s), description) pairs.
fn render_key_hints(area: Rect, buf: &mut Buffer, pairs: &[(Vec<KeyBinding>, &str)]) {
    let mut spans: Vec<Span<'static>> = vec![" ".into()];
    let mut first = true;
    for (keys, desc) in pairs {
        if !first {
            spans.push("   ".into());
        }
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                spans.push("/".into());
            }
            spans.push(Span::from(key));
        }
        spans.push(" ".into());
        spans.push(Span::from(desc.to_string()));
        first = false;
    }
    Paragraph::new(vec![Line::from(spans).dim()]).render_ref(area, buf);
}

/// Generic widget for rendering a pager view.
struct PagerView {
    renderables: Vec<Box<dyn Renderable>>,
    scroll_offset: usize,
    title: String,
    keymap: PagerKeymap,
    last_content_height: Option<usize>,
    last_rendered_height: Option<usize>,
    /// If set, on next render ensure this chunk is visible.
    pending_scroll_chunk: Option<usize>,
    /// Cached wrapped layout. Scrolling changes the viewport, not cell geometry.
    layout_width: Option<u16>,
    layout_starts: Vec<usize>,
    layout_heights: Vec<usize>,
    last_content_area: Option<Rect>,
}

impl PagerView {
    fn new(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        scroll_offset: usize,
        keymap: PagerKeymap,
    ) -> Self {
        Self {
            renderables,
            scroll_offset,
            title,
            keymap,
            last_content_height: None,
            last_rendered_height: None,
            pending_scroll_chunk: None,
            layout_width: None,
            layout_starts: Vec::new(),
            layout_heights: Vec::new(),
            last_content_area: None,
        }
    }

    fn ensure_layout(&mut self, width: u16) {
        if self.layout_width == Some(width) && self.layout_heights.len() == self.renderables.len() {
            return;
        }
        self.layout_width = Some(width);
        self.layout_starts.clear();
        self.layout_heights.clear();
        let mut start = 0usize;
        for renderable in &self.renderables {
            let height = usize::from(renderable.desired_height(width));
            self.layout_starts.push(start);
            self.layout_heights.push(height);
            start = start.saturating_add(height);
        }
    }

    fn content_height(&self) -> usize {
        self.layout_starts
            .last()
            .zip(self.layout_heights.last())
            .map_or(0, |(start, height)| start.saturating_add(*height))
    }

    fn replace_renderables(&mut self, renderables: Vec<Box<dyn Renderable>>) {
        self.renderables = renderables;
        self.layout_width = None;
        self.layout_starts.clear();
        self.layout_heights.clear();
    }

    fn push_renderable(&mut self, renderable: Box<dyn Renderable>) {
        if let Some(width) = self.layout_width {
            let start = self.content_height();
            self.layout_starts.push(start);
            self.layout_heights
                .push(usize::from(renderable.desired_height(width)));
        }
        self.renderables.push(renderable);
    }

    fn pop_renderable(&mut self) -> Option<Box<dyn Renderable>> {
        let renderable = self.renderables.pop()?;
        if self.layout_heights.len() > self.renderables.len() {
            self.layout_heights.pop();
            self.layout_starts.pop();
        }
        Some(renderable)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        self.render_header(area, buf);
        let content_area = self.content_area(area);
        self.last_content_area = Some(content_area);
        self.update_last_content_height(content_area.height);
        self.ensure_layout(content_area.width);
        let content_height = self.content_height();
        self.last_rendered_height = Some(content_height);
        // If there is a pending request to scroll a specific chunk into view,
        // satisfy it now that wrapping is up to date for this width.
        if let Some(idx) = self.pending_scroll_chunk.take() {
            self.ensure_chunk_visible(idx, content_area);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(content_height.saturating_sub(content_area.height as usize));

        self.render_content(content_area, buf);

        self.render_bottom_bar(area, content_area, buf, content_height);
    }

    /// Render only transcript content for ordinary mouse scrollback. The full pager title and
    /// percentage bar belong to the explicit Ctrl+T view; showing them above the anchored composer
    /// makes normal history scrolling feel like an unrelated modal.
    fn render_bare(&mut self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        self.last_content_area = Some(area);
        self.update_last_content_height(area.height);
        self.ensure_layout(area.width);
        let content_height = self.content_height();
        self.last_rendered_height = Some(content_height);
        self.scroll_offset = self
            .scroll_offset
            .min(content_height.saturating_sub(usize::from(area.height)));
        self.render_content(area, buf);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        Span::from("/ ".repeat(area.width as usize / 2))
            .dim()
            .render_ref(area, buf);
        let header = format!("/ {}", self.title);
        header.dim().render_ref(area, buf);
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        let mut drawn_bottom = area.y;
        let viewport_top = self.scroll_offset;
        let viewport_bottom = viewport_top.saturating_add(usize::from(area.height));
        let first = self
            .layout_starts
            .partition_point(|start| *start < viewport_top)
            .saturating_sub(1);
        for index in first..self.renderables.len() {
            let start = self.layout_starts[index];
            let height = self.layout_heights[index];
            let bottom = start.saturating_add(height);
            if bottom <= viewport_top {
                continue;
            }
            if start >= viewport_bottom {
                break;
            }
            let renderable = &self.renderables[index];
            if start < viewport_top {
                let offset = u16::try_from(viewport_top - start).unwrap_or(u16::MAX);
                let drawn = render_offset_content(area, buf, &**renderable, offset);
                drawn_bottom = drawn_bottom.max(area.y + drawn);
            } else {
                let top = u16::try_from(start - viewport_top).unwrap_or(u16::MAX);
                let draw_height = u16::try_from(height)
                    .unwrap_or(u16::MAX)
                    .min(area.height.saturating_sub(top));
                let draw_area = Rect::new(area.x, area.y + top, area.width, draw_height);
                renderable.render(draw_area, buf);
                drawn_bottom = drawn_bottom.max(draw_area.y.saturating_add(draw_area.height));
            }
        }

        for y in drawn_bottom..area.bottom() {
            if area.width == 0 {
                break;
            }
            buf[(area.x, y)] = Cell::from('~');
            for x in area.x + 1..area.right() {
                buf[(x, y)] = Cell::from(' ');
            }
        }
    }

    fn render_bottom_bar(
        &self,
        full_area: Rect,
        content_area: Rect,
        buf: &mut Buffer,
        total_len: usize,
    ) {
        let sep_y = content_area.bottom();
        let sep_rect = Rect::new(full_area.x, sep_y, full_area.width, 1);

        Span::from("─".repeat(sep_rect.width as usize))
            .dim()
            .render_ref(sep_rect, buf);
        let percent = if total_len == 0 {
            100
        } else {
            let max_scroll = total_len.saturating_sub(content_area.height as usize);
            if max_scroll == 0 {
                100
            } else {
                (((self.scroll_offset.min(max_scroll)) as f32 / max_scroll as f32) * 100.0).round()
                    as u8
            }
        };
        let pct_text = format!(" {percent}% ");
        let pct_w = pct_text.chars().count() as u16;
        let pct_x = sep_rect.x + sep_rect.width - pct_w - 1;
        Span::from(pct_text)
            .dim()
            .render_ref(Rect::new(pct_x, sep_rect.y, pct_w, 1), buf);
    }

    fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) -> Result<()> {
        match key_event {
            e if self.keymap.scroll_up.is_pressed(e) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            e if self.keymap.scroll_down.is_pressed(e) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            e if self.keymap.page_up.is_pressed(e) => {
                let page_height = self.page_height(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_sub(page_height);
            }
            e if self.keymap.page_down.is_pressed(e) => {
                let page_height = self.page_height(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_add(page_height);
            }
            e if self.keymap.half_page_down.is_pressed(e) => {
                let area = self.content_area(tui.terminal.viewport_area);
                let half_page = (area.height as usize).saturating_add(1) / 2;
                self.scroll_offset = self.scroll_offset.saturating_add(half_page);
            }
            e if self.keymap.half_page_up.is_pressed(e) => {
                let area = self.content_area(tui.terminal.viewport_area);
                let half_page = (area.height as usize).saturating_add(1) / 2;
                self.scroll_offset = self.scroll_offset.saturating_sub(half_page);
            }
            e if self.keymap.jump_top.is_pressed(e) => {
                self.scroll_offset = 0;
            }
            e if self.keymap.jump_bottom.is_pressed(e) => {
                self.scroll_offset = usize::MAX;
            }
            _ => {
                return Ok(());
            }
        }
        tui.frame_requester()
            .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
        Ok(())
    }

    fn scroll_lines_up(&mut self, tui: &mut tui::Tui, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        tui.frame_requester()
            .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
    }

    fn scroll_lines_down(&mut self, tui: &mut tui::Tui, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        tui.frame_requester()
            .schedule_frame_in(crate::tui::TARGET_FRAME_INTERVAL);
    }

    /// Returns the height of one page in content rows.
    ///
    /// Prefers the last rendered content height (excluding header/footer chrome);
    /// if no render has occurred yet, falls back to the content area height
    /// computed from the given viewport.
    fn page_height(&self, viewport_area: Rect) -> usize {
        self.last_content_height
            .unwrap_or_else(|| self.content_area(viewport_area).height as usize)
    }

    fn update_last_content_height(&mut self, height: u16) {
        self.last_content_height = Some(height as usize);
    }

    fn content_area(&self, area: Rect) -> Rect {
        let mut area = area;
        area.y = area.y.saturating_add(1);
        area.height = area.height.saturating_sub(2);
        area
    }

    fn renderable_position_at(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        let area = self.last_content_area?;
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return None;
        }
        let logical_row = self
            .scroll_offset
            .saturating_add(usize::from(row.saturating_sub(area.y)));
        let index = self
            .layout_starts
            .partition_point(|start| *start <= logical_row)
            .checked_sub(1)?;
        (logical_row < self.layout_starts[index].saturating_add(self.layout_heights[index]))
            .then_some((index, logical_row.saturating_sub(self.layout_starts[index])))
    }
}

impl PagerView {
    fn is_scrolled_to_bottom(&self) -> bool {
        if self.scroll_offset == usize::MAX {
            return true;
        }
        let Some(height) = self.last_content_height else {
            return false;
        };
        if self.renderables.is_empty() {
            return true;
        }
        let Some(total_height) = self.last_rendered_height else {
            return false;
        };
        if total_height <= height {
            return true;
        }
        let max_scroll = total_height.saturating_sub(height);
        self.scroll_offset >= max_scroll
    }

    /// Request that the given text chunk index be scrolled into view on next render.
    fn scroll_chunk_into_view(&mut self, chunk_index: usize) {
        self.pending_scroll_chunk = Some(chunk_index);
    }

    fn ensure_chunk_visible(&mut self, idx: usize, area: Rect) {
        if area.height == 0 || idx >= self.renderables.len() {
            return;
        }
        self.ensure_layout(area.width);
        let first = self.layout_starts[idx];
        let last = first.saturating_add(self.layout_heights[idx]);
        let current_top = self.scroll_offset;
        let current_bottom = current_top.saturating_add(area.height.saturating_sub(1) as usize);
        if first < current_top {
            self.scroll_offset = first;
        } else if last > current_bottom {
            self.scroll_offset = last.saturating_sub(area.height.saturating_sub(1) as usize);
        }
    }
}

/// A renderable that caches its desired height.
struct CachedRenderable {
    renderable: Box<dyn Renderable>,
    height: std::cell::Cell<Option<u16>>,
    last_width: std::cell::Cell<Option<u16>>,
}

impl CachedRenderable {
    fn new(renderable: impl Into<Box<dyn Renderable>>) -> Self {
        Self {
            renderable: renderable.into(),
            height: std::cell::Cell::new(None),
            last_width: std::cell::Cell::new(None),
        }
    }
}

impl Renderable for CachedRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.renderable.render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        if self.last_width.get() != Some(width) {
            let height = self.renderable.desired_height(width);
            self.height.set(Some(height));
            self.last_width.set(Some(width));
        }
        self.height.get().unwrap_or(0)
    }
}

struct CellRenderable {
    cell: Arc<dyn HistoryCell>,
    style: Style,
    presentation: TranscriptPresentation,
    expanded: bool,
    lines_cache: RefCell<Option<(u16, Vec<HyperlinkLine>)>>,
}

impl Renderable for CellRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let hyperlink_lines = self.hyperlink_lines(area.width);
        let p = Paragraph::new(Text::from(visible_lines(hyperlink_lines.clone())))
            .style(self.style)
            .wrap(Wrap { trim: false });
        p.render(area, buf);
        mark_buffer_hyperlinks(buf, area, &hyperlink_lines, /*scroll_rows*/ 0);
    }

    fn desired_height(&self, width: u16) -> u16 {
        if self.expanded {
            return Paragraph::new(Text::from(visible_lines(self.hyperlink_lines(width))))
                .wrap(Wrap { trim: false })
                .line_count(width)
                .try_into()
                .unwrap_or(0);
        }
        match self.presentation {
            TranscriptPresentation::Detailed => self.cell.desired_transcript_height(width),
            TranscriptPresentation::Display => self.cell.desired_height(width),
        }
    }
}

impl CellRenderable {
    fn hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if let Some((_, lines)) = self
            .lines_cache
            .borrow()
            .as_ref()
            .filter(|(cached_width, _)| *cached_width == width)
        {
            return lines.clone();
        }
        let lines = self.compute_hyperlink_lines(width);
        self.lines_cache.replace(Some((width, lines.clone())));
        lines
    }

    fn compute_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if self.expanded {
            let expanded = match self.presentation {
                TranscriptPresentation::Detailed => {
                    self.cell.expanded_transcript_hyperlink_lines(width)
                }
                TranscriptPresentation::Display => {
                    self.cell.expanded_display_hyperlink_lines(width)
                }
            };
            if let Some(lines) = expanded {
                return lines;
            }
        }
        match self.presentation {
            TranscriptPresentation::Detailed => self.cell.transcript_hyperlink_lines(width),
            TranscriptPresentation::Display => self.cell.display_hyperlink_lines(width),
        }
    }
}

struct HyperlinkLinesRenderable {
    lines: Vec<HyperlinkLine>,
}

impl Renderable for HyperlinkLinesRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(Text::from(visible_lines(self.lines.clone())))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        mark_buffer_hyperlinks(buf, area, &self.lines, /*scroll_rows*/ 0);
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::from(visible_lines(self.lines.clone())))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(/*default*/ 0)
    }
}

pub(crate) struct TranscriptOverlay {
    /// Pager UI state and the renderables currently displayed.
    ///
    /// The invariant is that `view.renderables` is `render_cells(cells)` plus an optional trailing
    /// live-tail renderable appended after the committed cells.
    view: PagerView,
    /// Committed transcript cells (does not include the live tail).
    cells: Vec<Arc<dyn HistoryCell>>,
    highlight_cell: Option<usize>,
    hovered_cell: Option<usize>,
    hovered_region: Option<InlineExpansionRegion>,
    expanded_cells: HashSet<usize>,
    presentation: TranscriptPresentation,
    /// Cache key for the render-only live tail appended after committed cells.
    live_tail_key: Option<LiveTailKey>,
    is_done: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptPresentation {
    Detailed,
    Display,
}

/// Cache key for the active-cell "live tail" appended to the transcript overlay.
///
/// Changing any field implies a different rendered tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveTailKey {
    /// Current terminal width, which affects wrapping.
    width: u16,
    /// Revision that changes on in-place active cell transcript updates.
    revision: u64,
    /// Whether the tail should be treated as a continuation for spacing.
    is_stream_continuation: bool,
    /// Optional animation tick to refresh spinners/progress indicators.
    animation_tick: Option<u64>,
}

impl TranscriptOverlay {
    /// Creates a transcript overlay for a fixed set of committed cells.
    ///
    /// This overlay does not own the "active cell"; callers may optionally append a live tail via
    /// `sync_live_tail` during draws to reflect in-flight activity.
    pub(crate) fn new(transcript_cells: Vec<Arc<dyn HistoryCell>>, keymap: PagerKeymap) -> Self {
        Self::new_with_presentation(transcript_cells, keymap, TranscriptPresentation::Detailed)
    }

    fn new_with_presentation(
        transcript_cells: Vec<Arc<dyn HistoryCell>>,
        keymap: PagerKeymap,
        presentation: TranscriptPresentation,
    ) -> Self {
        Self {
            view: PagerView::new(
                Self::render_cells(
                    &transcript_cells,
                    /*highlight_cell*/ None,
                    &HashSet::new(),
                    presentation,
                ),
                "T R A N S C R I P T".to_string(),
                usize::MAX,
                keymap,
            ),
            cells: transcript_cells,
            highlight_cell: None,
            hovered_cell: None,
            hovered_region: None,
            expanded_cells: HashSet::new(),
            presentation,
            live_tail_key: None,
            is_done: false,
        }
    }

    fn render_cells(
        cells: &[Arc<dyn HistoryCell>],
        highlight_cell: Option<usize>,
        expanded_cells: &HashSet<usize>,
        presentation: TranscriptPresentation,
    ) -> Vec<Box<dyn Renderable>> {
        cells
            .iter()
            .enumerate()
            .flat_map(|(i, c)| {
                let mut v: Vec<Box<dyn Renderable>> = Vec::new();
                let interactive = match presentation {
                    TranscriptPresentation::Detailed => c.has_inline_transcript_expansion(),
                    TranscriptPresentation::Display => c.has_inline_expansion(),
                };
                let expanded = expanded_cells.contains(&i) && interactive;
                let mut cell_renderable = if c.as_any().is::<UserHistoryCell>() {
                    Box::new(CachedRenderable::new(CellRenderable {
                        cell: c.clone(),
                        presentation,
                        expanded,
                        lines_cache: RefCell::new(None),
                        style: if highlight_cell == Some(i) {
                            user_message_style().reversed()
                        } else {
                            user_message_style()
                        },
                    })) as Box<dyn Renderable>
                } else {
                    Box::new(CachedRenderable::new(CellRenderable {
                        cell: c.clone(),
                        presentation,
                        expanded,
                        lines_cache: RefCell::new(None),
                        style: Style::default(),
                    })) as Box<dyn Renderable>
                };
                if !c.is_stream_continuation() && i > 0 {
                    cell_renderable = Box::new(InsetRenderable::new(
                        cell_renderable,
                        Insets::tlbr(
                            /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                        ),
                    ));
                }
                v.push(cell_renderable);
                v
            })
            .collect()
    }

    /// Insert a committed history cell while keeping any cached live tail.
    ///
    /// The live tail is temporarily removed, the committed cells are rebuilt,
    /// then the tail is reattached. If the tail previously had no leading
    /// spacing because it was the only renderable, we add the missing inset
    /// when the first committed cell arrives.
    ///
    /// This expects `cell` to be a committed transcript cell (not the in-flight active cell). If
    /// the overlay was scrolled to bottom before insertion, it remains pinned to bottom after the
    /// insertion to preserve the "follow along" behavior.
    pub(crate) fn insert_cell(&mut self, cell: Arc<dyn HistoryCell>) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        let had_prior_cells = !self.cells.is_empty();
        let tail_renderable = self.take_live_tail_renderable();
        self.cells.push(cell);
        self.view.replace_renderables(Self::render_cells(
            &self.cells,
            self.highlight_cell,
            &self.expanded_cells,
            self.presentation,
        ));
        if let Some(tail) = tail_renderable {
            let tail = if !had_prior_cells
                && self
                    .live_tail_key
                    .is_some_and(|key| !key.is_stream_continuation)
            {
                // The tail was rendered as the only entry, so it lacks a top
                // inset; add one now that it follows a committed cell.
                Box::new(InsetRenderable::new(
                    tail,
                    Insets::tlbr(
                        /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                    ),
                )) as Box<dyn Renderable>
            } else {
                tail
            };
            self.view.push_renderable(tail);
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Replace committed transcript cells while keeping any cached in-progress output that is
    /// currently shown at the end of the overlay.
    ///
    /// This is used when existing history is trimmed (for example after rollback) so the
    /// transcript overlay immediately reflects the same committed cells as the main transcript.
    pub(crate) fn replace_cells(&mut self, cells: Vec<Arc<dyn HistoryCell>>) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        self.cells = cells;
        self.expanded_cells.retain(|idx| *idx < self.cells.len());
        if self
            .highlight_cell
            .is_some_and(|idx| idx >= self.cells.len())
        {
            self.highlight_cell = None;
        }
        self.rebuild_renderables();
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Replace a range of committed cells with a single consolidated cell.
    ///
    /// Mirrors the splice performed on `App::transcript_cells` during
    /// `ConsolidateAgentMessage` so the Ctrl+T overlay stays in sync with the
    /// main transcript. The range is clamped defensively: cells may have been
    /// inserted after the overlay opened, leaving it with fewer entries than
    /// the main transcript.
    pub(crate) fn consolidate_cells(
        &mut self,
        range: std::ops::Range<usize>,
        consolidated: Arc<dyn HistoryCell>,
    ) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        // Clamp the range to the overlay's cell count to avoid panic if the overlay has fewer
        // cells than the main transcript (e.g. cells were inserted after the overlay has opened).
        let clamped_end = range.end.min(self.cells.len());
        let clamped_start = range.start.min(clamped_end);
        if clamped_start < clamped_end {
            let removed = clamped_end - clamped_start;
            if let Some(highlight_cell) = self.highlight_cell.as_mut()
                && *highlight_cell >= clamped_start
            {
                if *highlight_cell < clamped_end {
                    *highlight_cell = clamped_start;
                } else {
                    *highlight_cell = highlight_cell.saturating_sub(removed.saturating_sub(1));
                }
            }
            self.cells
                .splice(clamped_start..clamped_end, std::iter::once(consolidated));
            self.expanded_cells.clear();
            if self
                .highlight_cell
                .is_some_and(|highlight_cell| highlight_cell >= self.cells.len())
            {
                self.highlight_cell = None;
            }
            self.rebuild_renderables();
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    /// Sync the active-cell live tail with the current width and cell state.
    ///
    /// Recomputes the tail only when the cache key changes, preserving scroll
    /// position and dropping the tail if there is nothing to render.
    ///
    /// The overlay owns committed transcript cells while the live tail is derived from the current
    /// active cell, which can mutate in place while streaming. `App` calls this during
    /// `TuiEvent::Draw` for `Overlay::Transcript`, passing a key that changes when the active cell
    /// mutates or animates so the cached tail stays fresh.
    ///
    /// Passing a key that does not change on in-place active-cell mutations will freeze the tail in
    /// `Ctrl+T` while the main viewport continues to update.
    pub(crate) fn sync_live_tail(
        &mut self,
        width: u16,
        active_key: Option<ActiveCellTranscriptKey>,
        compute_lines: impl FnOnce(u16) -> Option<Vec<HyperlinkLine>>,
    ) {
        let next_key = active_key.map(|key| LiveTailKey {
            width,
            revision: key.revision,
            is_stream_continuation: key.is_stream_continuation,
            animation_tick: key.animation_tick,
        });

        if self.live_tail_key == next_key {
            return;
        }
        let follow_bottom = self.view.is_scrolled_to_bottom();

        self.take_live_tail_renderable();
        self.live_tail_key = next_key;

        if let Some(key) = next_key {
            let lines = compute_lines(width).unwrap_or_default();
            if !lines.is_empty() {
                self.view.push_renderable(Self::live_tail_renderable(
                    lines,
                    !self.cells.is_empty(),
                    key.is_stream_continuation,
                ));
            }
        }
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    pub(crate) fn set_highlight_cell(&mut self, cell: Option<usize>) {
        self.highlight_cell = cell;
        self.rebuild_renderables();
        if let Some(idx) = self.highlight_cell {
            self.view.scroll_chunk_into_view(idx);
        }
    }

    /// Returns whether the underlying pager view is currently pinned to the bottom.
    ///
    /// The `App` draw loop uses this to decide whether to schedule animation frames for the live
    /// tail; if the user has scrolled up, we avoid driving animation work that they cannot see.
    pub(crate) fn is_scrolled_to_bottom(&self) -> bool {
        self.view.is_scrolled_to_bottom()
    }

    fn rebuild_renderables(&mut self) {
        let tail_renderable = self.take_live_tail_renderable();
        self.view.replace_renderables(Self::render_cells(
            &self.cells,
            self.highlight_cell,
            &self.expanded_cells,
            self.presentation,
        ));
        if let Some(tail) = tail_renderable {
            self.view.push_renderable(tail);
        }
        if let Some(area) = self.view.last_content_area {
            self.view.ensure_layout(area.width);
        }
    }

    /// Removes and returns the cached live-tail renderable, if present.
    ///
    /// The live tail is represented as a single optional renderable appended after the committed
    /// cell renderables, so this relies on the live tail always being the final entry in
    /// `view.renderables` when present.
    fn take_live_tail_renderable(&mut self) -> Option<Box<dyn Renderable>> {
        (self.view.renderables.len() > self.cells.len()).then(|| self.view.pop_renderable())?
    }

    fn live_tail_renderable(
        lines: Vec<HyperlinkLine>,
        has_prior_cells: bool,
        is_stream_continuation: bool,
    ) -> Box<dyn Renderable> {
        let mut renderable: Box<dyn Renderable> =
            Box::new(CachedRenderable::new(HyperlinkLinesRenderable { lines }));
        if has_prior_cells && !is_stream_continuation {
            renderable = Box::new(InsetRenderable::new(
                renderable,
                Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                ),
            ));
        }
        renderable
    }

    fn render_hints(&self, area: Rect, buf: &mut Buffer) {
        let line1 = Rect::new(area.x, area.y, area.width, 1);
        let line2 = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
        render_key_hints(
            line1,
            buf,
            &[
                (
                    first_or_empty(&self.view.keymap.scroll_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.scroll_down))
                        .collect(),
                    "to scroll",
                ),
                (
                    first_or_empty(&self.view.keymap.page_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.page_down))
                        .collect(),
                    "to page",
                ),
                (
                    first_or_empty(&self.view.keymap.jump_top)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.jump_bottom))
                        .collect(),
                    "to jump",
                ),
            ],
        );

        let mut pairs: Vec<(Vec<KeyBinding>, &str)> =
            vec![(first_or_empty(&self.view.keymap.close), "to quit")];
        if self.highlight_cell.is_some() {
            pairs.push((
                vec![
                    key_hint::plain(KeyCode::Esc),
                    key_hint::plain(KeyCode::Left),
                ],
                "to edit prev",
            ));
            pairs.push((vec![key_hint::plain(KeyCode::Right)], "to edit next"));
            pairs.push((vec![key_hint::plain(KeyCode::Enter)], "to edit message"));
        } else {
            pairs.push((vec![key_hint::plain(KeyCode::Esc)], "to edit prev"));
        }
        render_key_hints(line2, buf, &pairs);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 3);
        self.view.render(top, buf);
        self.render_hovered_region(buf);
        self.render_hints(bottom, buf);
    }

    pub(crate) fn render_scrollback(&mut self, area: Rect, buf: &mut Buffer) {
        self.view.render_bare(area, buf);
        self.render_hovered_region(buf);
    }

    fn render_hovered_region(&self, buf: &mut Buffer) {
        let (Some(index), Some(region)) = (self.hovered_cell, self.hovered_region.as_ref()) else {
            return;
        };
        let Some(cell) = self.cells.get(index) else {
            return;
        };
        let Some(area) = self.view.last_content_area else {
            return;
        };
        let Some(start) = self.view.layout_starts.get(index).copied() else {
            return;
        };
        let content_start = start + usize::from(index > 0 && !cell.is_stream_continuation());
        let region_row = content_start.saturating_add(region.row);
        if region_row < self.view.scroll_offset {
            return;
        }
        let viewport_row = region_row - self.view.scroll_offset;
        if viewport_row >= usize::from(area.height) {
            return;
        }
        let text_start = area
            .x
            .saturating_add(u16::try_from(region.columns.start).unwrap_or(u16::MAX));
        let text_end = area
            .x
            .saturating_add(u16::try_from(region.columns.end).unwrap_or(u16::MAX))
            .min(area.right());
        let y = area
            .y
            .saturating_add(u16::try_from(viewport_row).unwrap_or(u16::MAX));
        let mut hover_style = Style::default().bold();
        if cell.as_any().is::<ExecCell>() && region.row > 0 {
            hover_style = hover_style.fg(Color::Gray).remove_modifier(Modifier::DIM);
        }
        for x in text_start..text_end {
            buf[(x, y)].set_style(hover_style);
        }
    }

    fn interactive_region_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<(usize, InlineExpansionRegion)> {
        let (index, row_in_renderable) = self.view.renderable_position_at(column, row)?;
        let cell = self.cells.get(index)?;
        let interactive = match self.presentation {
            TranscriptPresentation::Detailed => cell.has_inline_transcript_expansion(),
            TranscriptPresentation::Display => cell.has_inline_expansion(),
        };
        if !interactive {
            return None;
        }
        let area = self.view.last_content_area?;
        let content_row = row_in_renderable
            .checked_sub(usize::from(index > 0 && !cell.is_stream_continuation()))?;
        let expanded = self.expanded_cells.contains(&index);
        cell.inline_expansion_regions(area.width, expanded)
            .into_iter()
            .find(|region| {
                let start = area
                    .x
                    .saturating_add(u16::try_from(region.columns.start).unwrap_or(u16::MAX));
                let end = area
                    .x
                    .saturating_add(u16::try_from(region.columns.end).unwrap_or(u16::MAX))
                    .min(area.right());
                content_row == region.row && column >= start && column < end
            })
            .map(|region| (index, region))
    }
}

impl TranscriptOverlay {
    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if self.view.keymap.close.is_pressed(e)
                    || self.view.keymap.close_transcript.is_pressed(e) =>
                {
                    self.is_done = true;
                    Ok(())
                }
                other => self.view.handle_key_event(tui, other),
            },
            TuiEvent::Mouse(mouse_event) => {
                match mouse_event.kind {
                    MouseEventKind::ScrollUp => self.view.scroll_lines_up(tui, 3),
                    MouseEventKind::ScrollDown => self.view.scroll_lines_down(tui, 3),
                    MouseEventKind::Moved => {
                        let hovered =
                            self.interactive_region_at(mouse_event.column, mouse_event.row);
                        let (hovered_cell, hovered_region) = hovered
                            .map(|(index, region)| (Some(index), Some(region)))
                            .unwrap_or((None, None));
                        if self.hovered_cell != hovered_cell
                            || self.hovered_region != hovered_region
                        {
                            self.hovered_cell = hovered_cell;
                            self.hovered_region = hovered_region;
                            tui.frame_requester().schedule_frame();
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some((index, _)) =
                            self.interactive_region_at(mouse_event.column, mouse_event.row)
                        {
                            if !self.expanded_cells.insert(index) {
                                self.expanded_cells.remove(&index);
                            }
                            self.hovered_cell = None;
                            self.hovered_region = None;
                            self.rebuild_renderables();
                            tui.frame_requester().schedule_frame();
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            TuiEvent::Draw | TuiEvent::Resize => {
                tui.draw(u16::MAX, |frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }

    #[cfg(test)]
    pub(crate) fn committed_cell_count(&self) -> usize {
        self.cells.len()
    }
}

pub(crate) struct StaticOverlay {
    view: PagerView,
    is_done: bool,
}

impl StaticOverlay {
    pub(crate) fn with_title(
        lines: Vec<Line<'static>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        Self::with_renderables(
            vec![Box::new(CachedRenderable::new(paragraph))],
            title,
            keymap,
        )
    }

    pub(crate) fn with_renderables(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
        keymap: PagerKeymap,
    ) -> Self {
        Self {
            view: PagerView::new(renderables, title, /*scroll_offset*/ 0, keymap),
            is_done: false,
        }
    }

    fn render_hints(&self, area: Rect, buf: &mut Buffer) {
        let line1 = Rect::new(area.x, area.y, area.width, 1);
        let line2 = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
        render_key_hints(
            line1,
            buf,
            &[
                (
                    first_or_empty(&self.view.keymap.scroll_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.scroll_down))
                        .collect(),
                    "to scroll",
                ),
                (
                    first_or_empty(&self.view.keymap.page_up)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.page_down))
                        .collect(),
                    "to page",
                ),
                (
                    first_or_empty(&self.view.keymap.jump_top)
                        .into_iter()
                        .chain(first_or_empty(&self.view.keymap.jump_bottom))
                        .collect(),
                    "to jump",
                ),
            ],
        );
        let pairs: Vec<(Vec<KeyBinding>, &str)> =
            vec![(first_or_empty(&self.view.keymap.close), "to quit")];
        render_key_hints(line2, buf, &pairs);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 3);
        self.view.render(top, buf);
        self.render_hints(bottom, buf);
    }
}

impl StaticOverlay {
    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if self.view.keymap.close.is_pressed(e) => {
                    self.is_done = true;
                    Ok(())
                }
                other => self.view.handle_key_event(tui, other),
            },
            TuiEvent::Draw | TuiEvent::Resize => {
                tui.draw(u16::MAX, |frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }
}

fn render_offset_content(
    area: Rect,
    buf: &mut Buffer,
    renderable: &dyn Renderable,
    scroll_offset: u16,
) -> u16 {
    let height = renderable.desired_height(area.width);
    let mut tall_buf = Buffer::empty(Rect::new(
        0,
        0,
        area.width,
        height.min(area.height + scroll_offset),
    ));
    renderable.render(*tall_buf.area(), &mut tall_buf);
    let copy_height = area
        .height
        .min(tall_buf.area().height.saturating_sub(scroll_offset));
    for y in 0..copy_height {
        let src_y = y + scroll_offset;
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)] = tall_buf[(x, src_y)].clone();
        }
    }

    copy_height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::ReviewDecision;
    use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::diff_model::FileChange;
    use crate::exec_cell::CommandOutput;
    use crate::history_cell;
    use crate::history_cell::HistoryCell;
    use crate::history_cell::new_patch_event;
    use codex_protocol::parse_command::ParsedCommand;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::text::Text;

    #[derive(Debug)]
    struct TestCell {
        lines: Vec<Line<'static>>,
    }

    impl crate::history_cell::HistoryCell for TestCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }
    }

    #[derive(Debug)]
    struct DivergentTestCell;

    impl HistoryCell for DivergentTestCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            vec!["condensed rich view".green().into()]
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            vec!["expanded raw transcript".into()]
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.transcript_lines(u16::MAX)
        }
    }

    fn paragraph_block(label: &str, lines: usize) -> Box<dyn Renderable> {
        let text = Text::from(
            (0..lines)
                .map(|i| Line::from(format!("{label}{i}")))
                .collect::<Vec<_>>(),
        );
        Box::new(Paragraph::new(text)) as Box<dyn Renderable>
    }

    fn default_pager_keymap() -> crate::keymap::PagerKeymap {
        crate::keymap::RuntimeKeymap::defaults().pager
    }

    fn transcript_overlay(cells: Vec<Arc<dyn HistoryCell>>) -> TranscriptOverlay {
        TranscriptOverlay::new(cells, default_pager_keymap())
    }

    fn static_overlay(lines: Vec<Line<'static>>, title: &str) -> StaticOverlay {
        StaticOverlay::with_title(lines, title.to_string(), default_pager_keymap())
    }

    fn pager_view(
        renderables: Vec<Box<dyn Renderable>>,
        title: &str,
        scroll_offset: usize,
    ) -> PagerView {
        PagerView::new(
            renderables,
            title.to_string(),
            scroll_offset,
            default_pager_keymap(),
        )
    }

    struct CountingRenderable {
        desired_height_calls: Arc<AtomicUsize>,
    }

    impl Renderable for CountingRenderable {
        fn render(&self, _area: Rect, _buf: &mut Buffer) {}

        fn desired_height(&self, _width: u16) -> u16 {
            self.desired_height_calls.fetch_add(1, Ordering::Relaxed);
            1
        }
    }

    #[derive(Debug)]
    struct CountingHistoryCell {
        display_calls: Arc<AtomicUsize>,
    }

    impl HistoryCell for CountingHistoryCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.display_calls.fetch_add(1, Ordering::Relaxed);
            vec![Line::from("cached transcript row")]
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            vec![Line::from("cached transcript row")]
        }
    }

    #[test]
    fn scrolling_reuses_cached_transcript_layout() {
        let desired_height_calls = Arc::new(AtomicUsize::new(0));
        let renderables = (0..500)
            .map(|_| {
                Box::new(CountingRenderable {
                    desired_height_calls: desired_height_calls.clone(),
                }) as Box<dyn Renderable>
            })
            .collect();
        let mut pager = pager_view(renderables, "PERF", /*scroll_offset*/ 0);
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);

        pager.render(area, &mut buf);
        assert_eq!(desired_height_calls.load(Ordering::Relaxed), 500);
        pager.scroll_offset = 10;
        pager.render(area, &mut buf);

        assert_eq!(desired_height_calls.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn repeated_draws_reuse_committed_cell_lines() {
        let display_calls = Arc::new(AtomicUsize::new(0));
        let mut overlay = TranscriptOverlay::new_with_presentation(
            vec![Arc::new(CountingHistoryCell {
                display_calls: display_calls.clone(),
            })],
            default_pager_keymap(),
            TranscriptPresentation::Display,
        );
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);

        overlay.render_scrollback(area, &mut buf);
        let calls_after_first_draw = display_calls.load(Ordering::Relaxed);
        overlay.render_scrollback(area, &mut buf);

        assert!(calls_after_first_draw > 0);
        assert_eq!(
            display_calls.load(Ordering::Relaxed),
            calls_after_first_draw
        );
    }

    #[tokio::test]
    async fn patch_headline_hover_is_subtle_and_click_expands_inline() -> std::io::Result<()> {
        let cwd = std::env::current_dir()?;
        let changes = HashMap::from([(
            PathBuf::from("src/large.rs"),
            FileChange::Add {
                content: (0..25).map(|line| format!("line {line}\n")).collect(),
            },
        )]);
        let mut overlay = transcript_overlay(vec![Arc::new(new_patch_event(changes, &cwd))]);
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);
        let headline_style_before = buf[(10, 1)].style();
        assert!(
            !headline_style_before
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        let body_style_before = buf[(10, 2)].style();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let pointer = |kind| {
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 10,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Moved))?;
        assert_eq!(overlay.hovered_cell, Some(0));
        overlay.render(area, &mut buf);
        let headline_style = buf[(10, 1)].style();
        assert!(
            headline_style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            !headline_style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(buf[(10, 2)].style(), body_style_before);

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Down(MouseButton::Left)))?;
        assert!(overlay.expanded_cells.contains(&0));
        overlay.render(area, &mut buf);
        assert!(buffer_to_text(&buf, area).contains("line 24"));

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Down(MouseButton::Left)))?;
        assert!(!overlay.expanded_cells.contains(&0));
        Ok(())
    }

    #[tokio::test]
    async fn patch_preview_body_is_not_a_hover_target() -> std::io::Result<()> {
        let cwd = std::env::current_dir()?;
        let changes = HashMap::from([(
            PathBuf::from("src/large.rs"),
            FileChange::Add {
                content: (0..25).map(|line| format!("line {line}\n")).collect(),
            },
        )]);
        let mut overlay = transcript_overlay(vec![Arc::new(new_patch_event(changes, &cwd))]);
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        overlay.handle_event(
            &mut tui,
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column: 8,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        )?;

        assert_eq!(overlay.hovered_cell, None);
        Ok(())
    }

    #[tokio::test]
    async fn patch_omitted_count_hover_and_click_expand_inline() -> std::io::Result<()> {
        let cwd = std::env::current_dir()?;
        let changes = HashMap::from([(
            PathBuf::from("src/large.rs"),
            FileChange::Add {
                content: (0..25).map(|line| format!("line {line}\n")).collect(),
            },
        )]);
        let mut overlay = TranscriptOverlay::new_with_presentation(
            vec![Arc::new(new_patch_event(changes, &cwd))],
            default_pager_keymap(),
            TranscriptPresentation::Display,
        );
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        overlay.render_scrollback(area, &mut buf);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let pointer = |kind| {
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 8,
                row: 7,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Moved))?;
        assert_eq!(overlay.hovered_cell, Some(0));
        assert_eq!(
            overlay.hovered_region.as_ref().map(|region| region.row),
            Some(7)
        );
        overlay.render_scrollback(area, &mut buf);
        let style = buf[(8, 7)].style();
        assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
        assert!(
            !style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Down(MouseButton::Left)))?;
        assert!(overlay.expanded_cells.contains(&0));
        overlay.render_scrollback(area, &mut buf);
        assert!(buffer_to_text(&buf, area).contains("line 24"));
        Ok(())
    }

    #[tokio::test]
    async fn command_output_hover_and_click_reveal_full_output_inline() -> std::io::Result<()> {
        let mut exec_cell = crate::exec_cell::new_active_exec_command(
            "exec-output".into(),
            vec!["bash".into(), "-lc".into(), "generate-output".into()],
            vec![ParsedCommand::Unknown {
                cmd: "generate-output".into(),
            }],
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
            /*animations_enabled*/ false,
        );
        let output = (0..30)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        exec_cell.complete_call(
            "exec-output",
            CommandOutput {
                exit_code: 0,
                aggregated_output: output.clone(),
                formatted_output: output,
            },
            Duration::from_millis(50),
        );
        let mut overlay = TranscriptOverlay::new_with_presentation(
            vec![Arc::new(exec_cell)],
            default_pager_keymap(),
            TranscriptPresentation::Display,
        );
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        overlay.render_scrollback(area, &mut buf);
        assert!(!buffer_to_text(&buf, area).contains("output 15"));
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let pointer = |kind| {
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 6,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Moved))?;
        assert_eq!(overlay.hovered_cell, Some(0));
        overlay.render_scrollback(area, &mut buf);
        let style = buf[(6, 0)].style();
        assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
        assert!(
            !style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );

        overlay.handle_event(&mut tui, pointer(MouseEventKind::Down(MouseButton::Left)))?;
        overlay.render_scrollback(area, &mut buf);
        assert!(buffer_to_text(&buf, area).contains("output 15"));

        let output_pointer = |kind| {
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 6,
                row: 16,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        overlay.handle_event(&mut tui, output_pointer(MouseEventKind::Moved))?;
        overlay.render_scrollback(area, &mut buf);
        let output_style = buf[(6, 16)].style();
        assert!(
            output_style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            !output_style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert_eq!(output_style.fg, Some(ratatui::style::Color::Gray));

        overlay.handle_event(
            &mut tui,
            output_pointer(MouseEventKind::Down(MouseButton::Left)),
        )?;
        overlay.render_scrollback(area, &mut buf);
        assert!(!buffer_to_text(&buf, area).contains("output 15"));

        let omitted_row = buffer_to_text(&buf, area)
            .lines()
            .position(|line| line.contains("… +"))
            .expect("collapsed command output should show an omitted-line count")
            .try_into()
            .expect("test row should fit in u16");
        let omitted_pointer = |kind| {
            TuiEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 6,
                row: omitted_row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        overlay.handle_event(&mut tui, omitted_pointer(MouseEventKind::Moved))?;
        overlay.render_scrollback(area, &mut buf);
        let omitted_style = buf[(6, omitted_row)].style();
        assert!(
            omitted_style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            !omitted_style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );

        overlay.handle_event(
            &mut tui,
            omitted_pointer(MouseEventKind::Down(MouseButton::Left)),
        )?;
        overlay.render_scrollback(area, &mut buf);
        assert!(buffer_to_text(&buf, area).contains("output 15"));
        Ok(())
    }

    #[test]
    fn edit_prev_hint_is_visible() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("hello")],
        })]);

        // Render into a wide buffer so the footer hints aren't truncated.
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let s = buffer_to_text(&buf, area);
        assert!(
            s.contains("edit prev"),
            "expected 'edit prev' hint in overlay footer, got: {s:?}"
        );
    }

    #[test]
    fn mouse_scrollback_omits_transcript_pager_chrome() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("history line")],
        })]);
        let area = Rect::new(0, 0, 80, 8);
        let mut buf = Buffer::empty(area);

        overlay.render_scrollback(area, &mut buf);

        let rendered = buffer_to_text(&buf, area);
        assert!(rendered.contains("history line"));
        assert!(!rendered.contains("T R A N S C R I P T"));
        assert!(!rendered.contains("100%"));
    }

    #[test]
    fn mouse_scrollback_preserves_condensed_rich_cell_presentation() {
        let Overlay::Transcript(mut overlay) = Overlay::new_mouse_scrollback(
            vec![Arc::new(DivergentTestCell)],
            default_pager_keymap(),
        ) else {
            panic!("expected transcript overlay");
        };
        let area = Rect::new(0, 0, 80, 8);
        let mut buf = Buffer::empty(area);

        overlay.render_scrollback(area, &mut buf);

        let rendered = buffer_to_text(&buf, area);
        assert!(rendered.contains("condensed rich view"));
        assert!(!rendered.contains("expanded raw transcript"));
        assert_eq!(buf[(0, 0)].fg, Color::Green);
    }

    #[test]
    fn edit_next_hint_is_visible_when_highlighted() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("hello")],
        })]);
        overlay.set_highlight_cell(Some(0));

        // Render into a wide buffer so the footer hints aren't truncated.
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let s = buffer_to_text(&buf, area);
        assert!(
            s.contains("edit next"),
            "expected 'edit next' hint in overlay footer, got: {s:?}"
        );
    }

    #[test]
    fn transcript_overlay_snapshot_basic() {
        // Prepare a transcript overlay with a few lines
        let mut overlay = transcript_overlay(vec![
            Arc::new(TestCell {
                lines: vec![Line::from("alpha")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("beta")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("gamma")],
            }),
        ]);
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn transcript_overlay_preserves_semantic_web_links() {
        let destination = "https://example.com/a/very/long/path";
        let mut overlay = transcript_overlay(vec![Arc::new(history_cell::AgentMarkdownCell::new(
            destination.to_string(),
            std::path::Path::new("/tmp"),
        ))]);
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 24, /*height*/ 10,
        );
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);

        assert!(area.positions().any(|position| {
            buf[position]
                .symbol()
                .contains(&format!("\x1b]8;;{destination}\x07"))
        }));
    }

    #[test]
    fn transcript_overlay_renders_live_tail() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("alpha")],
        })]);
        overlay.sync_live_tail(
            /*width*/ 40,
            Some(ActiveCellTranscriptKey {
                revision: 1,
                is_stream_continuation: false,
                animation_tick: None,
            }),
            |_| Some(vec![HyperlinkLine::from("tail")]),
        );

        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn transcript_overlay_live_tail_preserves_semantic_web_links() {
        let destination = "https://example.com/a/streamed/path";
        let cell = history_cell::AgentMarkdownCell::new(
            destination.to_string(),
            std::path::Path::new("/tmp"),
        );
        let mut overlay = transcript_overlay(Vec::new());
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 24, /*height*/ 10,
        );
        let mut buf = Buffer::empty(area);

        overlay.sync_live_tail(
            area.width,
            Some(ActiveCellTranscriptKey {
                revision: 1,
                is_stream_continuation: false,
                animation_tick: None,
            }),
            |width| Some(cell.transcript_hyperlink_lines(width)),
        );
        overlay.render(area, &mut buf);

        assert!(area.positions().any(|position| {
            buf[position]
                .symbol()
                .contains(&format!("\x1b]8;;{destination}\x07"))
        }));
    }

    #[test]
    fn transcript_overlay_sync_live_tail_is_noop_for_identical_key() {
        let mut overlay = transcript_overlay(vec![Arc::new(TestCell {
            lines: vec![Line::from("alpha")],
        })]);

        let calls = std::cell::Cell::new(0usize);
        let key = ActiveCellTranscriptKey {
            revision: 1,
            is_stream_continuation: false,
            animation_tick: None,
        };

        overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
            calls.set(calls.get() + 1);
            Some(vec![HyperlinkLine::from("tail")])
        });
        overlay.sync_live_tail(/*width*/ 40, Some(key), |_| {
            calls.set(calls.get() + 1);
            Some(vec![HyperlinkLine::from("tail2")])
        });

        assert_eq!(calls.get(), 1);
    }

    fn buffer_to_text(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let symbol = buf[(x, y)].symbol();
                if symbol.is_empty() {
                    out.push(' ');
                } else {
                    out.push(symbol.chars().next().unwrap_or(' '));
                }
            }
            // Trim trailing spaces for stability.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn transcript_overlay_apply_patch_scroll_vt100_clears_previous_page() {
        let cwd = PathBuf::from("/repo");
        let mut cells: Vec<Arc<dyn HistoryCell>> = Vec::new();

        let mut approval_changes = HashMap::new();
        approval_changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\nworld\n".to_string(),
            },
        );
        let approval_cell: Arc<dyn HistoryCell> = Arc::new(new_patch_event(approval_changes, &cwd));
        cells.push(approval_cell);

        let mut apply_changes = HashMap::new();
        apply_changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\nworld\n".to_string(),
            },
        );
        let apply_begin_cell: Arc<dyn HistoryCell> = Arc::new(new_patch_event(apply_changes, &cwd));
        cells.push(apply_begin_cell);

        let apply_end_cell: Arc<dyn HistoryCell> = history_cell::new_approval_decision_cell(
            history_cell::ApprovalDecisionSubject::Command(vec!["ls".into()]),
            ReviewDecision::Approved,
            history_cell::ApprovalDecisionActor::User,
        )
        .into();
        cells.push(apply_end_cell);

        let mut exec_cell = crate::exec_cell::new_active_exec_command(
            "exec-1".into(),
            vec!["bash".into(), "-lc".into(), "ls".into()],
            vec![ParsedCommand::Unknown { cmd: "ls".into() }],
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
            /*animations_enabled*/ true,
        );
        exec_cell.complete_call(
            "exec-1",
            CommandOutput {
                exit_code: 0,
                aggregated_output: "src\nREADME.md\n".into(),
                formatted_output: "src\nREADME.md\n".into(),
            },
            Duration::from_millis(420),
        );
        let exec_cell: Arc<dyn HistoryCell> = Arc::new(exec_cell);
        cells.push(exec_cell);

        let mut overlay = transcript_overlay(cells);
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);
        overlay.view.scroll_offset = 0;
        overlay.render(area, &mut buf);

        let snapshot = buffer_to_text(&buf, area);
        assert_snapshot!("transcript_overlay_apply_patch_scroll_vt100", snapshot);
    }

    #[test]
    fn transcript_overlay_keeps_scroll_pinned_at_bottom() {
        let mut overlay = transcript_overlay(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        assert!(
            overlay.view.is_scrolled_to_bottom(),
            "expected initial render to leave view at bottom"
        );

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, usize::MAX);
    }

    #[test]
    fn transcript_overlay_preserves_manual_scroll_position() {
        let mut overlay = transcript_overlay(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        overlay.view.scroll_offset = 0;

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, 0);
    }

    #[test]
    fn transcript_overlay_consolidation_remaps_highlight_inside_range() {
        let mut overlay = transcript_overlay(
            (0..6)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        overlay.set_highlight_cell(Some(3));

        overlay.consolidate_cells(
            2..5,
            Arc::new(TestCell {
                lines: vec![Line::from("consolidated")],
            }),
        );

        assert_eq!(
            overlay.highlight_cell,
            Some(2),
            "highlight inside consolidated range should point to replacement cell",
        );
    }

    #[test]
    fn transcript_overlay_consolidation_remaps_highlight_after_range() {
        let mut overlay = transcript_overlay(
            (0..7)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        overlay.set_highlight_cell(Some(6));

        overlay.consolidate_cells(
            2..5,
            Arc::new(TestCell {
                lines: vec![Line::from("consolidated")],
            }),
        );

        assert_eq!(
            overlay.highlight_cell,
            Some(4),
            "highlight after consolidated range should shift left by removed cells",
        );
    }

    #[test]
    fn static_overlay_snapshot_basic() {
        // Prepare a static overlay with a few lines and a title
        let mut overlay = static_overlay(
            vec!["one".into(), "two".into(), "three".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    /// Render transcript overlay and return visible line numbers (`line-NN`) in order.
    fn transcript_line_numbers(overlay: &mut TranscriptOverlay, area: Rect) -> Vec<usize> {
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        let top_h = area.height.saturating_sub(3);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let content_area = overlay.view.content_area(top);

        let mut nums = Vec::new();
        for y in content_area.y..content_area.bottom() {
            let mut line = String::new();
            for x in content_area.x..content_area.right() {
                line.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            if let Some(n) = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("line-"))
                .and_then(|s| s.parse().ok())
            {
                nums.push(n);
            }
        }
        nums
    }

    #[test]
    fn transcript_overlay_paging_is_continuous_and_round_trips() {
        let mut overlay = transcript_overlay(
            (0..50)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line-{i:02}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let area = Rect::new(0, 0, 40, 15);

        // Prime layout so last_content_height is populated and paging uses the real content height.
        let mut buf = Buffer::empty(area);
        overlay.view.scroll_offset = 0;
        overlay.render(area, &mut buf);
        let page_height = overlay.view.page_height(area);

        // Scenario 1: starting from the top, PageDown should show the next page of content.
        overlay.view.scroll_offset = 0;
        let page1 = transcript_line_numbers(&mut overlay, area);
        let page1_len = page1.len();
        let expected_page1: Vec<usize> = (0..page1_len).collect();
        assert_eq!(
            page1, expected_page1,
            "first page should start at line-00 and show a full page of content"
        );

        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let page2 = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            page2.len(),
            page1_len,
            "second page should have the same number of visible lines as the first page"
        );
        let expected_page2_first = *page1.last().unwrap() + 1;
        assert_eq!(
            page2[0], expected_page2_first,
            "second page after PageDown should immediately follow the first page"
        );

        // Scenario 2: from an interior offset (start=3), PageDown then PageUp should round-trip.
        let interior_offset = 3usize;
        overlay.view.scroll_offset = interior_offset;
        let before = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let _ = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_sub(page_height);
        let after = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            before, after,
            "PageDown+PageUp from interior offset ({interior_offset}) should round-trip"
        );

        // Scenario 3: from the top of the second page, PageUp then PageDown should round-trip.
        overlay.view.scroll_offset = page_height;
        let before2 = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_sub(page_height);
        let _ = transcript_line_numbers(&mut overlay, area);
        overlay.view.scroll_offset = overlay.view.scroll_offset.saturating_add(page_height);
        let after2 = transcript_line_numbers(&mut overlay, area);
        assert_eq!(
            before2, after2,
            "PageUp+PageDown from the top of the second page should round-trip"
        );
    }

    #[test]
    fn static_overlay_wraps_long_lines() {
        let mut overlay = static_overlay(
            vec!["a very long line that should wrap when rendered within a narrow pager overlay width".into()],
            "S T A T I C",
        );
        let mut term = Terminal::new(TestBackend::new(24, 8)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn pager_view_content_height_counts_renderables() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 2),
                paragraph_block("b", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );

        pv.ensure_layout(/*width*/ 80);
        assert_eq!(pv.content_height(), 5);
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_down_when_needed() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 1),
                paragraph_block("b", /*lines*/ 3),
                paragraph_block("c", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 8);

        pv.scroll_offset = 0;
        let content_area = pv.content_area(area);
        pv.ensure_chunk_visible(/*idx*/ 2, content_area);

        let mut buf = Buffer::empty(area);
        pv.render(area, &mut buf);
        let rendered = buffer_to_text(&buf, area);

        assert!(
            rendered.contains("c0"),
            "expected chunk top in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c1"),
            "expected chunk middle in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c2"),
            "expected chunk bottom in view: {rendered:?}"
        );
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_up_when_needed() {
        let mut pv = pager_view(
            vec![
                paragraph_block("a", /*lines*/ 2),
                paragraph_block("b", /*lines*/ 3),
                paragraph_block("c", /*lines*/ 3),
            ],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 3);

        pv.scroll_offset = 6;
        pv.ensure_chunk_visible(/*idx*/ 0, area);

        assert_eq!(pv.scroll_offset, 0);
    }

    #[test]
    fn pager_view_is_scrolled_to_bottom_accounts_for_wrapped_height() {
        let mut pv = pager_view(
            vec![paragraph_block("a", /*lines*/ 10)],
            "T",
            /*scroll_offset*/ 0,
        );
        let area = Rect::new(0, 0, 20, 8);
        let mut buf = Buffer::empty(area);

        pv.render(area, &mut buf);

        assert!(
            !pv.is_scrolled_to_bottom(),
            "expected view to report not at bottom when offset < max"
        );

        pv.scroll_offset = usize::MAX;
        pv.render(area, &mut buf);

        assert!(
            pv.is_scrolled_to_bottom(),
            "expected view to report at bottom after scrolling to end"
        );
    }
}
