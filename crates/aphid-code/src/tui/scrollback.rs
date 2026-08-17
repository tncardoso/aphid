//! The transcript the UI shows, and how it is drawn.
//!
//! This model is built from [`UiEvent`](super::event::UiEvent)s, not from the
//! agent's arena. Collapsing, scrolling and diff rendering are presentation
//! state, and the transcript on disk should not carry any of it.

use std::collections::HashMap;

use aphid_core::Json;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Tool output longer than this is folded behind a summary line.
pub const COLLAPSE_AFTER: usize = 15;

/// How many transcript entries the view keeps in memory.
///
/// The agent transcript is not touched; this only bounds what the TUI can
/// render and scroll back through.
const MAX_ENTRIES: usize = 300;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ToolState {
    /// The arguments are still arriving; the call has not been announced yet.
    Streaming,
    Running,
    Done,
    Failed,
}

/// One thing in the transcript.
#[derive(Debug)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool {
        name: String,
        arguments: String,
        output: String,
        state: ToolState,
        /// The `edit` tool's payload, which becomes a diff.
        details: Option<Json>,
        /// Argument bytes seen so far, which is all there is to show while the
        /// call streams.
        streamed: usize,
    },
    /// A divider or a message from the harness itself.
    Notice(String),
    /// A `!` command the user ran, with its output.
    Shell {
        command: String,
        output: String,
    },
    /// The wordmark, shown once when a new session opens.
    Logo,
}

/// Where the transcript is parked.
///
/// The bottom is the common case and costs nothing to hold. Once the reader
/// scrolls up, the position is held against an *entry* rather than a line
/// number, so a block that grows or shrinks does not move what is under the
/// cursor. That is what the renderer used to work out by hand, from line
/// counts only it could see — and it had to write the answer back.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Scroll {
    /// Pinned to the newest line.
    #[default]
    Bottom,
    /// The first visible line is `offset` lines into `entry`'s block.
    Anchored { entry: usize, offset: usize },
}

/// What a viewport shows, worked out from the scroll position and the lines
/// there are.
///
/// Derived at each draw. The pane keeps the last one so a page key knows how
/// far a page is: a position in lines is only meaningful against a wrapping,
/// and the wrapping is only known once the pane has a width.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    /// The first visible line.
    pub top: usize,
    /// Lines in the whole transcript.
    pub total: usize,
    pub height: usize,
}

/// The transcript pane's state.
#[derive(Default)]
pub struct Scrollback {
    entries: Vec<Entry>,
    /// Call id to entry index, so progress and results find their call.
    by_call: HashMap<String, usize>,
    /// Streaming block index to entry index, so a delta finds its placeholder.
    /// Block indices restart with each turn, so this is cleared at every one.
    streams: HashMap<u32, usize>,
    scroll: Scroll,
    /// What the last draw laid out. Read by the page keys, written by
    /// [`Self::layout`] and by a scroll that moves it on.
    viewport: Viewport,
    pub show_thinking: bool,
    /// The entry index where the current turn starts. Entries before this are
    /// settled history and may be evicted by [`MAX_ENTRIES`]. `None` means no
    /// user turn has started yet, so every entry is evictable.
    turn_start: Option<usize>,
    /// A number for each entry, raised whenever the entry changes.
    ///
    /// A number and not a staleness flag, because a flag has to be cleared by
    /// whoever drew it: the cache mirrors the numbers it drew, and drawing
    /// writes to the cache alone.
    revs: Vec<u64>,
    next_rev: u64,
    /// Flattened rendered lines, in transcript order.
    lines: Vec<Line<'static>>,
    /// Where each entry's rendered block starts in [`Self::lines`].
    starts: Vec<usize>,
    /// The revision each cached block was rendered from.
    cached_revs: Vec<u64>,
    /// The width and thinking state [`Self::lines`] was built for.
    cache_width: usize,
    cache_show_thinking: bool,
    /// How many entry blocks the last cache pass rendered.
    ///
    /// The point of the per-entry cache is that one changed entry costs one
    /// block, so a test asserts the work and not only the staleness flag.
    #[cfg(test)]
    rebuilt: usize,
}

impl Scrollback {
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.revs.clear();
        self.by_call.clear();
        self.streams.clear();
        self.scroll = Scroll::Bottom;
        self.turn_start = None;
        self.invalidate_cache();
    }

    /// Where the pane is parked.
    #[must_use]
    pub fn scroll(&self) -> Scroll {
        self.scroll
    }

    /// Whether the reader has scrolled back through the transcript.
    #[must_use]
    pub fn scrolled(&self) -> bool {
        self.scroll != Scroll::Bottom
    }

    /// Park the pane back on the newest line.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = Scroll::Bottom;
    }

    /// Move the viewport `lines` lines towards the start of the transcript.
    pub fn scroll_up(&mut self, lines: usize) {
        self.anchor_at(self.viewport.top.saturating_sub(lines));
    }

    /// Move the viewport `lines` lines towards the newest entry, parking at
    /// the bottom once it gets there.
    pub fn scroll_down(&mut self, lines: usize) {
        let bottom = self.viewport.total.saturating_sub(self.viewport.height);
        let top = self.viewport.top + lines;
        if top >= bottom {
            self.scroll = Scroll::Bottom;
            self.viewport.top = bottom;
        } else {
            self.anchor_at(top);
        }
    }

    /// Hold `top` against the entry whose block contains it.
    ///
    /// The remembered top moves with it, so two page keys in a row move two
    /// pages even when no frame was drawn between them.
    fn anchor_at(&mut self, top: usize) {
        // The last block starting at or before `top` owns it. Starts rise, so
        // the partition point is one past that block.
        let entry = self.starts.partition_point(|start| *start <= top);
        let entry = entry.saturating_sub(1);
        let offset = top - self.starts.get(entry).copied().unwrap_or(0).min(top);
        self.scroll = Scroll::Anchored { entry, offset };
        self.viewport.top = top;
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        // A user message starts a new turn. The turn, not the message, is the
        // unit the agent works on, so everything from here to the next user
        // message is protected from the history cap.
        self.turn_start = Some(self.entries.len());
        self.push_entry(Entry::User(text.into()));
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push_entry(Entry::Notice(text.into()));
    }

    pub fn push_logo(&mut self) {
        self.push_entry(Entry::Logo);
    }

    /// A `!` command the user ran, and its output.
    pub fn push_shell(&mut self, command: String, output: String) {
        self.push_entry(Entry::Shell { command, output });
    }

    /// Append streamed prose, continuing the current assistant entry.
    pub fn push_text(&mut self, chunk: &str) {
        match self.entries.last_mut() {
            Some(Entry::Assistant(text)) => {
                text.push_str(chunk);
                self.mark_dirty(self.entries.len() - 1);
            }
            _ => self.push_entry(Entry::Assistant(chunk.to_owned())),
        }
    }

    pub fn push_thinking(&mut self, chunk: &str) {
        match self.entries.last_mut() {
            Some(Entry::Thinking(text)) => {
                text.push_str(chunk);
                self.mark_dirty(self.entries.len() - 1);
            }
            _ => self.push_entry(Entry::Thinking(chunk.to_owned())),
        }
    }

    /// Open a placeholder for a tool call whose arguments are still streaming.
    ///
    /// The placeholder is the card the call will end up in, not a separate
    /// entry to be deleted later: removing an entry would shift every index in
    /// `by_call`.
    pub fn begin_tool_stream(&mut self, block: u32, name: &str) {
        if self.streams.contains_key(&block) {
            return;
        }
        self.streams.insert(block, self.entries.len());
        self.push_entry(Entry::Tool {
            name: name.to_owned(),
            arguments: String::new(),
            output: String::new(),
            state: ToolState::Streaming,
            details: None,
            streamed: 0,
        });
    }

    /// Count argument bytes into the placeholder for `block`.
    pub fn push_tool_stream(&mut self, block: u32, bytes: usize) {
        let Some(&index) = self.streams.get(&block) else {
            return;
        };
        if let Some(Entry::Tool { streamed, .. }) = self.entries.get_mut(index) {
            *streamed += bytes;
        }
        self.mark_dirty(index);
    }

    /// Forget which blocks were streaming, without touching their entries.
    pub fn clear_tool_streams(&mut self) {
        self.streams.clear();
    }

    /// Resolve placeholders whose call never arrived.
    ///
    /// A turn that failed or was cancelled mid-stream never announces its
    /// calls, and a card left at `Streaming` would count up forever.
    pub fn settle_tool_streams(&mut self) {
        let indices: Vec<usize> = self.streams.values().copied().collect();
        for index in indices {
            if let Some(Entry::Tool { output, state, .. }) = self.entries.get_mut(index)
                && *state == ToolState::Streaming
            {
                *state = ToolState::Failed;
                *output = "the turn ended before the call was complete".to_owned();
                self.mark_dirty(index);
            }
        }
        self.streams.clear();
    }

    pub fn push_tool_call(&mut self, id: &str, name: &str, arguments: &str) {
        // Calls are announced in assistant source order, which is the order
        // their blocks streamed in, so the first unclaimed placeholder is this
        // call's own.
        if let Some(index) = self.first_streaming()
            && let Some(Entry::Tool {
                name: slot,
                arguments: raw,
                state,
                ..
            }) = self.entries.get_mut(index)
        {
            slot.clear();
            slot.push_str(name);
            raw.clear();
            raw.push_str(arguments);
            *state = ToolState::Running;
            self.by_call.insert(id.to_owned(), index);
            self.mark_dirty(index);
            return;
        }

        self.by_call.insert(id.to_owned(), self.entries.len());
        self.push_entry(Entry::Tool {
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            output: String::new(),
            state: ToolState::Running,
            details: None,
            streamed: 0,
        });
    }

    fn first_streaming(&self) -> Option<usize> {
        self.entries.iter().position(
            |entry| matches!(entry, Entry::Tool { state, .. } if *state == ToolState::Streaming),
        )
    }

    pub fn push_tool_progress(&mut self, id: &str, chunk: &str) {
        let Some(&index) = self.by_call.get(id) else {
            return;
        };
        if let Some(Entry::Tool { output, .. }) = self.entries.get_mut(index) {
            output.push_str(chunk);
            output.push('\n');
        }
        self.mark_dirty(index);
    }

    pub fn finish_tool(&mut self, id: &str, text: &str, is_error: bool, payload: Option<Json>) {
        let Some(&index) = self.by_call.get(id) else {
            return;
        };
        if let Some(Entry::Tool {
            output,
            state,
            details,
            ..
        }) = self.entries.get_mut(index)
        {
            // The final result is authoritative; progress chunks were a preview
            // of it, so they are replaced rather than appended to.
            *output = text.to_owned();
            *state = if is_error {
                ToolState::Failed
            } else {
                ToolState::Done
            };
            *details = payload;
        }
        self.mark_dirty(index);
    }

    fn push_entry(&mut self, entry: Entry) {
        self.next_rev += 1;
        self.entries.push(entry);
        self.revs.push(self.next_rev);
        self.trim_to_cap();
    }

    /// Say that entry `index` is not what it was.
    fn mark_dirty(&mut self, index: usize) {
        self.next_rev += 1;
        if let Some(rev) = self.revs.get_mut(index) {
            *rev = self.next_rev;
        }
    }

    fn invalidate_cache(&mut self) {
        self.lines.clear();
        self.starts.clear();
        self.cached_revs.clear();
        self.cache_width = 0;
        self.cache_show_thinking = self.show_thinking;
    }

    fn trim_to_cap(&mut self) {
        let removable = match self.turn_start {
            Some(start) => self.entries.len().saturating_sub(MAX_ENTRIES).min(start),
            None => self.entries.len().saturating_sub(MAX_ENTRIES),
        };
        if removable == 0 {
            return;
        }

        // The cache covers a prefix of the entries: everything drawn so far,
        // with anything pushed since this draw still to come. It can be
        // drained in step only if it is current and reaches past what goes.
        let spliceable = self.cache_width != 0
            && self.cache_show_thinking == self.show_thinking
            && self.starts.len() == self.cached_revs.len()
            && removable <= self.cached_revs.len();

        if !spliceable {
            self.entries.drain(0..removable);
            self.revs.drain(0..removable);
            self.evict(removable);
            self.invalidate_cache();
            return;
        }

        // Where the first surviving block starts, or the whole buffer when
        // every block the cache holds is going.
        let removed_lines = self
            .starts
            .get(removable)
            .copied()
            .unwrap_or(self.lines.len());
        self.entries.drain(0..removable);
        self.revs.drain(0..removable);
        self.starts.drain(0..removable);
        self.cached_revs.drain(0..removable);
        self.lines.drain(0..removed_lines);
        for start in &mut self.starts {
            *start -= removed_lines;
        }
        self.evict(removable);
    }

    /// Move every entry index down by `removed`, now that the front is gone.
    fn evict(&mut self, removed: usize) {
        self.adjust_maps(removed);
        if let Some(start) = &mut self.turn_start {
            *start -= removed;
        }
        // An anchor whose own entry was evicted has nothing left to hold: the
        // content it named is gone, so the pane goes back to the newest line
        // rather than to some arbitrary neighbour.
        if let Scroll::Anchored { entry, offset } = self.scroll {
            self.scroll = match entry.checked_sub(removed) {
                Some(entry) => Scroll::Anchored { entry, offset },
                None => Scroll::Bottom,
            };
        }
    }

    fn adjust_maps(&mut self, removed: usize) {
        self.by_call = self
            .by_call
            .iter()
            .filter_map(|(id, index)| {
                let index = index.checked_sub(removed)?;
                Some((id.clone(), index))
            })
            .collect();
        self.streams = self
            .streams
            .iter()
            .filter_map(|(block, index)| {
                let index = index.checked_sub(removed)?;
                Some((*block, index))
            })
            .collect();
    }

    /// Render one entry's block, counting the work the cache did not save.
    fn render_block(&mut self, index: usize, width: usize) -> Vec<Line<'static>> {
        #[cfg(test)]
        {
            self.rebuilt += 1;
        }
        render_entry(&self.entries[index], width, self.show_thinking)
    }

    /// Rebuild every rendered block for `width`.
    fn rebuild_all(&mut self, width: usize) {
        self.lines.clear();
        self.starts.clear();
        self.cached_revs.clear();

        for index in 0..self.entries.len() {
            let rendered = self.render_block(index, width);
            self.starts.push(self.lines.len());
            self.lines.extend(rendered);
            self.cached_revs.push(self.revs[index]);
        }

        self.cache_width = width;
        self.cache_show_thinking = self.show_thinking;
    }

    /// Re-render the entries whose revision moved on, splicing their new
    /// blocks into the flattened line buffer and leaving the rest alone.
    fn rebuild_stale(&mut self, width: usize) {
        for index in 0..self.entries.len() {
            let rev = self.revs[index];
            match self.cached_revs.get(index).copied() {
                Some(cached) if cached == rev => continue,
                // Drawn before, and changed since: splice the new block in
                // where the old one was and shift what follows.
                Some(_) => {
                    let rendered = self.render_block(index, width);
                    let start = self.starts[index];
                    let old_len = self
                        .starts
                        .get(index + 1)
                        .map_or(self.lines.len(), |next| *next)
                        - start;
                    let delta = rendered.len() as isize - old_len as isize;
                    self.lines.splice(start..start + old_len, rendered);
                    if delta != 0 {
                        for start in &mut self.starts[index + 1..] {
                            *start = (*start as isize + delta) as usize;
                        }
                    }
                    self.cached_revs[index] = rev;
                }
                // Never drawn: it can only be at the end, so append.
                None => {
                    let rendered = self.render_block(index, width);
                    self.starts.push(self.lines.len());
                    self.lines.extend(rendered);
                    self.cached_revs.push(rev);
                }
            }
        }
    }

    /// Render every entry to wrapped lines at `width`, keeping the result in
    /// the view's cache.
    #[must_use]
    pub fn lines(&mut self, width: usize) -> &[Line<'static>] {
        self.lines_for(width)
    }

    /// What the viewport shows, for a pane this size.
    ///
    /// The scroll position says which content to hold and the wrapping says
    /// where that content sits, so neither alone is an answer. Nothing about
    /// the position is written here: what is kept is the answer itself, for
    /// the page keys to measure a page against.
    pub fn layout(&mut self, width: usize, height: usize) -> Viewport {
        self.lines_for(width);
        let total = self.lines.len();
        let bottom = total.saturating_sub(height);
        let top = match self.scroll {
            Scroll::Bottom => bottom,
            Scroll::Anchored { entry, offset } => {
                let start = self.starts.get(entry).copied().unwrap_or(0);
                (start + offset).min(bottom)
            }
        };
        self.viewport = Viewport { top, total, height };
        self.viewport
    }

    /// The visible transcript lines for a viewport, cloned from the cache.
    ///
    /// Ratatui's `Paragraph` owns its text, so the visible slice is the only
    /// copy. The cache means the full transcript is not re-wrapped or copied
    /// on every frame.
    pub fn visible_lines(&mut self, width: usize, height: usize) -> Vec<Line<'static>> {
        let view = self.layout(width, height);
        self.lines
            .iter()
            .skip(view.top)
            .take(height)
            .cloned()
            .collect()
    }

    /// Ensure the cache is current for `width`, then return it.
    fn lines_for(&mut self, width: usize) -> &[Line<'static>] {
        let width = width.max(8);
        #[cfg(test)]
        {
            self.rebuilt = 0;
        }

        if self.cache_width != width || self.cache_show_thinking != self.show_thinking {
            self.rebuild_all(width);
        } else {
            self.rebuild_stale(width);
        }
        self.trim_to_cap();

        &self.lines
    }
}

fn render_entry(entry: &Entry, width: usize, show_thinking: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    match entry {
        Entry::User(text) => {
            for (index, part) in wrap(text, width - 2).into_iter().enumerate() {
                let prefix = if index == 0 { "> " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan)),
                    Span::styled(
                        part,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            lines.push(Line::default());
        }
        Entry::Assistant(text) => {
            lines.extend(markdown(text, width));
            lines.push(Line::default());
        }
        Entry::Thinking(text) => {
            if show_thinking {
                for part in wrap(text, width - 2) {
                    lines.push(Line::styled(
                        format!("┊ {part}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                lines.push(Line::default());
            }
        }
        Entry::Tool {
            name,
            arguments,
            output,
            state,
            details,
            streamed,
        } => {
            lines.push(tool_header(name, arguments, *state, *streamed, width));
            if let Some(diff) = diff_lines(details) {
                lines.extend(diff);
            } else {
                lines.extend(output_lines(output, *state, width));
            }
            lines.push(Line::default());
        }
        Entry::Notice(text) => {
            for raw in text.lines() {
                for part in wrap(raw, width) {
                    lines.push(Line::styled(part, Style::default().fg(Color::DarkGray)));
                }
            }
            lines.push(Line::default());
        }
        Entry::Shell { command, output } => {
            lines.push(Line::from(vec![
                Span::styled("$ ", Style::default().fg(Color::Gray)),
                Span::styled(
                    command.clone(),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            for raw in output.lines() {
                for part in wrap(raw, width.saturating_sub(2)) {
                    lines.push(Line::styled(
                        format!("  {part}"),
                        Style::default().fg(Color::Gray),
                    ));
                }
            }
            lines.push(Line::default());
        }
        Entry::Logo => {
            let (r, g, b) = super::logo::COLOR;
            let style = Style::default().fg(Color::Rgb(r, g, b));
            lines.push(Line::default());
            for raw in super::logo::LINES {
                lines.push(Line::styled(raw, style));
            }
            lines.push(Line::default());
        }
    }

    lines
}

fn tool_header(
    name: &str,
    arguments: &str,
    state: ToolState,
    streamed: usize,
    width: usize,
) -> Line<'static> {
    let (marker, colour) = match state {
        ToolState::Streaming => ("◌", Color::Yellow),
        ToolState::Running => ("⋯", Color::Yellow),
        ToolState::Done => ("→", Color::Green),
        ToolState::Failed => ("✗", Color::Red),
    };
    // One line, however long the arguments are: the point is to see at a glance
    // what the agent is doing.
    let budget = width.saturating_sub(name.len() + 4);
    // A half-written call has no arguments worth reading yet, so the count is
    // the thing to show: it proves the stream is moving.
    let summary = if state == ToolState::Streaming {
        one_line(&format!("receiving arguments… {}", bytes(streamed)), budget)
    } else {
        one_line(arguments, budget)
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(colour)),
        Span::styled(
            name.to_owned(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(Color::DarkGray)),
    ])
}

fn output_lines(output: &str, state: ToolState, width: usize) -> Vec<Line<'static>> {
    let style = Style::default().fg(if state == ToolState::Failed {
        Color::Red
    } else {
        Color::Gray
    });

    let all: Vec<&str> = output.lines().collect();
    let mut lines = Vec::new();

    // While a tool runs, the tail is what matters; once it is done, the head
    // usually carries the answer.
    let (shown, hidden, from_end) = if all.len() <= COLLAPSE_AFTER {
        (all.as_slice(), 0, false)
    } else if state == ToolState::Running {
        let from = all.len() - COLLAPSE_AFTER;
        (&all[from..], from, true)
    } else {
        (&all[..COLLAPSE_AFTER], all.len() - COLLAPSE_AFTER, false)
    };

    if from_end && hidden > 0 {
        lines.push(Line::styled(
            format!("  … {hidden} earlier lines"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    for line in shown {
        for part in wrap(line, width.saturating_sub(2)) {
            lines.push(Line::styled(format!("  {part}"), style));
        }
    }
    if !from_end && hidden > 0 {
        lines.push(Line::styled(
            format!("  … {hidden} more lines"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    lines
}

/// Turn the `edit` tool's details into a diff.
fn diff_lines(details: &Option<Json>) -> Option<Vec<Line<'static>>> {
    let edits = details.as_ref()?.get("edits")?.as_array()?;
    if edits.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    for edit in edits {
        let at = edit.get("line").and_then(Json::as_u64).unwrap_or(0);
        lines.push(Line::styled(
            format!("  @@ line {at}"),
            Style::default().fg(Color::DarkGray),
        ));
        for removed in edit.get("old").and_then(Json::as_str)?.lines() {
            lines.push(Line::styled(
                format!("  - {removed}"),
                Style::default().fg(Color::Red),
            ));
        }
        for added in edit.get("new").and_then(Json::as_str)?.lines() {
            lines.push(Line::styled(
                format!("  + {added}"),
                Style::default().fg(Color::Green),
            ));
        }
    }
    Some(lines)
}

/// Light markdown: fenced code blocks, headings, bullets and inline code.
///
/// Deliberately not a parser. Assistant replies are mostly prose, and the parts
/// worth distinguishing are the ones a reader scans for.
fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;

    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            lines.push(Line::styled(
                raw.to_owned(),
                Style::default().fg(Color::DarkGray),
            ));
            continue;
        }

        if in_code {
            for part in wrap(raw, width) {
                lines.push(Line::styled(part, Style::default().fg(Color::LightGreen)));
            }
            continue;
        }

        let style = if raw.starts_with('#') {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        for part in wrap(raw, width) {
            lines.push(Line::styled(part, style));
        }
    }

    lines
}

/// Compact byte counts: `412 B`, `1.2 kB`, `48 kB`.
pub(crate) fn bytes(count: usize) -> String {
    match count {
        0..=999 => format!("{count} B"),
        1_000..=999_999 => {
            let thousands = count as f64 / 1000.0;
            if thousands < 10.0 {
                format!("{thousands:.1} kB")
            } else {
                format!("{thousands:.0} kB")
            }
        }
        _ => format!("{:.1} MB", count as f64 / 1_000_000.0),
    }
}

/// Collapse to one line, ellipsised to fit.
#[must_use]
pub fn one_line(text: &str, width: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, width)
}

/// Cut to `width` characters, marking the cut.
#[must_use]
pub fn truncate_chars(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Hard-wrap to `width` columns, breaking at whitespace when there is any.
///
/// A newline in the text is a line the writer asked for, so each one starts a
/// new row instead of being reflowed away as ordinary whitespace — a message
/// typed or pasted over several lines reads the way it was written.
///
/// The pane wraps its own text rather than letting a `Paragraph` do it, because
/// scrolling needs an exact line count and ratatui will not report one without
/// an unstable feature.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        lines.extend(wrap_paragraph(paragraph, width));
    }
    lines
}

/// One newline-free run of text, hard-wrapped to `width`.
fn wrap_paragraph(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;

    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let word_len = trimmed.chars().count();

        // A word longer than the pane is broken across lines rather than
        // pushing the layout wider.
        if word_len > width {
            if count > 0 {
                lines.push(take_line(&mut current));
                count = 0;
            }
            for chunk in chunks(trimmed, width) {
                lines.push(chunk);
            }
            continue;
        }

        if count + word_len > width && count > 0 {
            lines.push(take_line(&mut current));
            count = 0;
        }
        current.push_str(trimmed);
        count += word_len;

        if word.ends_with(char::is_whitespace) && count < width {
            current.push(' ');
            count += 1;
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current.trim_end().to_owned());
    }
    lines
}

/// Take the line being built, without the separator space that ends it.
fn take_line(current: &mut String) -> String {
    let line = current.trim_end().to_owned();
    current.clear();
    line
}

fn chunks(text: &str, width: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_breaks_at_whitespace() {
        assert_eq!(
            wrap("the quick brown fox", 10),
            vec!["the quick".to_owned(), "brown fox".to_owned()]
        );
    }

    #[test]
    fn a_word_longer_than_the_pane_is_broken_up() {
        assert_eq!(
            wrap("supercalifragilistic", 8),
            vec![
                "supercal".to_owned(),
                "ifragili".to_owned(),
                "stic".to_owned()
            ]
        );
    }

    #[test]
    fn empty_text_still_produces_a_line() {
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    #[test]
    fn a_newline_starts_a_new_line() {
        assert_eq!(
            wrap("one\ntwo", 10),
            vec!["one".to_owned(), "two".to_owned()]
        );
    }

    #[test]
    fn a_blank_line_stays_blank() {
        assert_eq!(
            wrap("one\n\ntwo", 10),
            vec!["one".to_owned(), String::new(), "two".to_owned()]
        );
    }

    #[test]
    fn a_multiline_message_keeps_its_shape() {
        let mut view = Scrollback::default();
        view.push_user("one\ntwo".to_owned());
        let rendered: Vec<String> = view.lines(40).iter().map(ToString::to_string).collect();

        assert_eq!(rendered.first().map(String::as_str), Some("> one"));
        assert_eq!(rendered.get(1).map(String::as_str), Some("  two"));
    }

    #[test]
    fn one_line_flattens_and_ellipsises() {
        assert_eq!(one_line("a\n  b\tc", 40), "a b c");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn tool_output_collapses_once_it_is_done() {
        let output: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let lines = output_lines(&output, ToolState::Done, 40);
        let rendered: Vec<String> = lines.iter().map(ToString::to_string).collect();

        assert!(rendered.iter().any(|line| line.contains("line 0")));
        assert!(rendered.iter().any(|line| line.contains("… 25 more lines")));
        assert!(!rendered.iter().any(|line| line.contains("line 39")));
    }

    #[test]
    fn a_running_tool_shows_its_tail_instead() {
        let output: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let lines = output_lines(&output, ToolState::Running, 40);
        let rendered: Vec<String> = lines.iter().map(ToString::to_string).collect();

        assert!(rendered.iter().any(|line| line.contains("line 39")));
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("… 25 earlier lines"))
        );
    }

    #[test]
    fn edit_details_become_a_diff() {
        let details = Some(serde_json::json!({
            "edits": [{ "line": 42, "old": "let n = 0;", "new": "let n = 1;" }]
        }));
        let lines = diff_lines(&details).expect("a diff");
        let rendered: Vec<String> = lines.iter().map(ToString::to_string).collect();

        assert_eq!(rendered[0].trim(), "@@ line 42");
        assert_eq!(rendered[1].trim(), "- let n = 0;");
        assert_eq!(rendered[2].trim(), "+ let n = 1;");
    }

    #[test]
    fn a_tool_without_edit_details_renders_its_output() {
        assert!(diff_lines(&None).is_none());
        assert!(diff_lines(&Some(serde_json::json!({ "truncated": false }))).is_none());
    }

    #[test]
    fn thinking_is_hidden_until_it_is_asked_for() {
        let mut view = Scrollback::default();
        view.push_thinking("weighing it up");
        assert!(view.lines(40).is_empty());

        view.show_thinking = true;
        let rendered: Vec<String> = view.lines(40).iter().map(ToString::to_string).collect();
        assert!(rendered.iter().any(|line| line.contains("weighing it up")));
    }

    #[test]
    fn a_shell_command_renders_a_header_and_its_output() {
        let mut view = Scrollback::default();
        view.push_shell("ls".to_owned(), "a.rs\nb.rs".to_owned());

        let rendered: Vec<String> = view.lines(40).iter().map(ToString::to_string).collect();
        assert_eq!(rendered[0].trim(), "$ ls");
        assert_eq!(rendered[1].trim(), "a.rs");
        assert_eq!(rendered[2].trim(), "b.rs");
    }

    #[test]
    fn a_shell_command_wraps_long_output() {
        let mut view = Scrollback::default();
        view.push_shell("ls".to_owned(), "some very long output line".to_owned());

        let rendered: Vec<String> = view.lines(12).iter().map(ToString::to_string).collect();
        assert_eq!(rendered[0].trim(), "$ ls");
        assert!(
            rendered
                .iter()
                .skip(1)
                .any(|line| line.contains("some very")),
            "the output is wrapped, not dropped: {rendered:?}"
        );
    }

    #[test]
    fn streamed_text_accumulates_into_one_entry() {
        let mut view = Scrollback::default();
        view.push_text("Hello");
        view.push_text(", world");
        assert_eq!(view.entries().len(), 1);

        let rendered: Vec<String> = view.lines(40).iter().map(ToString::to_string).collect();
        assert!(rendered.iter().any(|line| line.contains("Hello, world")));
    }

    #[test]
    fn byte_counts_are_compact() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(412), "412 B");
        assert_eq!(bytes(1_200), "1.2 kB");
        assert_eq!(bytes(48_000), "48 kB");
        assert_eq!(bytes(2_500_000), "2.5 MB");
    }

    #[test]
    fn a_streaming_call_shows_its_name_and_a_growing_count() {
        let mut view = Scrollback::default();
        view.begin_tool_stream(0, "bash");
        view.push_tool_stream(0, 300);

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("◌ bash"), "{}", rendered[0]);
        assert!(rendered[0].contains("receiving arguments… 300 B"));

        view.push_tool_stream(0, 112);
        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].contains("412 B"), "{}", rendered[0]);
    }

    #[test]
    fn an_announced_call_claims_its_placeholder() {
        let mut view = Scrollback::default();
        view.begin_tool_stream(0, "bash");
        view.push_tool_stream(0, 24);
        view.push_tool_call("c1", "bash", r#"{"command":"cargo test"}"#);

        assert_eq!(view.entries().len(), 1, "the placeholder became the card");

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("⋯ bash"), "{}", rendered[0]);
        assert!(rendered[0].contains("cargo test"));
        assert!(!rendered[0].contains("receiving"));

        // The claim registered the id, so results still find their card.
        view.finish_tool("c1", "ok", false, None);
        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("→ bash"), "{}", rendered[0]);
    }

    #[test]
    fn two_streamed_calls_are_claimed_in_source_order() {
        let mut view = Scrollback::default();
        view.begin_tool_stream(0, "read");
        view.begin_tool_stream(1, "bash");
        view.push_tool_call("c1", "read", r#"{"path":"a.rs"}"#);
        view.push_tool_call("c2", "bash", r#"{"command":"ls"}"#);

        assert_eq!(view.entries().len(), 2);
        view.finish_tool("c2", "a.rs b.rs", false, None);

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("⋯ read"), "{}", rendered[0]);
        // The second card is the one that finished, not the first.
        assert!(
            rendered.iter().any(|line| line.starts_with("→ bash")),
            "{rendered:#?}"
        );
    }

    #[test]
    fn a_placeholder_whose_call_never_arrives_is_settled() {
        let mut view = Scrollback::default();
        view.begin_tool_stream(0, "edit");
        view.push_tool_stream(0, 90);
        view.settle_tool_streams();

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("✗ edit"), "{}", rendered[0]);
        assert!(rendered[1].contains("before the call was complete"));
    }

    #[test]
    fn settling_leaves_a_call_that_did_arrive_alone() {
        let mut view = Scrollback::default();
        view.begin_tool_stream(0, "bash");
        view.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
        view.settle_tool_streams();

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered[0].starts_with("⋯ bash"), "{}", rendered[0]);
    }

    #[test]
    fn a_tool_result_replaces_its_streamed_preview() {
        let mut view = Scrollback::default();
        view.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
        view.push_tool_progress("c1", "partial");
        view.finish_tool("c1", "final output", false, None);

        let rendered: Vec<String> = view.lines(60).iter().map(ToString::to_string).collect();
        assert!(rendered.iter().any(|line| line.contains("final output")));
        assert!(!rendered.iter().any(|line| line.contains("partial")));
        assert!(rendered.iter().any(|line| line.contains("bash")));
    }

    #[test]
    fn old_entries_are_evicted_at_the_cap() {
        let mut view = Scrollback::default();
        for number in 0..MAX_ENTRIES + 50 {
            view.push_notice(format!("notice {number}"));
        }

        assert_eq!(view.entries().len(), MAX_ENTRIES);
        assert!(
            matches!(view.entries().first(), Some(Entry::Notice(text)) if text == "notice 50"),
            "the oldest settled entries are gone"
        );
    }

    #[test]
    fn the_current_turn_is_never_evicted() {
        let mut view = Scrollback::default();
        for number in 0..10 {
            view.push_notice(format!("notice {number}"));
        }
        view.push_user("keep me");
        for number in 0..MAX_ENTRIES + 20 {
            view.push_tool_call(&format!("call-{number}"), "bash", "{}");
        }

        assert!(view.entries().len() > MAX_ENTRIES);
        assert!(
            matches!(view.entries().first(), Some(Entry::User(text)) if text == "keep me"),
            "the cap may not eat the turn the user is watching"
        );
    }

    #[test]
    fn only_changed_blocks_move_their_revision() {
        let mut view = Scrollback::default();
        view.push_user("hello");
        view.push_text("an answer");
        let _ = view.lines(40);

        assert_eq!(view.revs, view.cached_revs, "a drawn cache is level");

        view.push_text(" with more");
        assert_eq!(
            view.revs
                .iter()
                .zip(&view.cached_revs)
                .filter(|(rev, cached)| rev != cached)
                .count(),
            1,
            "one entry changed, so one revision moved"
        );

        let _ = view.lines(40);
        assert_eq!(view.revs, view.cached_revs);
    }

    #[test]
    fn only_the_changed_block_is_rebuilt() {
        let mut view = Scrollback::default();
        for number in 0..MAX_ENTRIES {
            view.push_notice(format!("notice {number}"));
        }

        let _ = view.lines(40);
        assert_eq!(
            view.rebuilt, MAX_ENTRIES,
            "the first pass renders everything"
        );

        view.push_notice("one more");
        let _ = view.lines(40);
        assert_eq!(view.rebuilt, 1, "an appended entry costs one block");

        view.push_text("an answer");
        view.push_text(" with more");
        let _ = view.lines(40);
        assert_eq!(view.rebuilt, 1, "a grown entry costs one block");

        let _ = view.lines(40);
        assert_eq!(view.rebuilt, 0, "an unchanged transcript costs nothing");
    }

    #[test]
    fn a_new_width_rebuilds_every_block() {
        let mut view = Scrollback::default();
        for number in 0..10 {
            view.push_notice(format!("notice {number}"));
        }

        let _ = view.lines(40);
        let _ = view.lines(20);
        assert_eq!(
            view.rebuilt, 10,
            "wrapping is width-bound, so nothing carries"
        );
    }

    #[test]
    fn new_lines_below_the_viewport_do_not_move_it() {
        let mut view = Scrollback::default();
        for number in 0..10 {
            view.push_notice(format!("line {number}"));
        }
        view.layout(20, 5);

        view.scroll_up(2);
        let parked = view.layout(20, 5).top;

        view.push_notice("fresh content");
        assert_eq!(
            view.layout(20, 5).top,
            parked,
            "what is being read stays where it is"
        );
    }

    #[test]
    fn a_block_above_the_viewport_growing_carries_it_along() {
        let mut view = Scrollback::default();
        view.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
        for number in 0..10 {
            view.push_notice(format!("line {number}"));
        }
        view.layout(20, 5);
        view.scroll_up(3);

        let before = view.layout(20, 5);
        // The first block gains lines, so everything below it moves down.
        for number in 0..4 {
            view.push_tool_progress("c1", &format!("output {number}"));
        }
        let after = view.layout(20, 5);

        assert!(after.total > before.total);
        assert_eq!(
            after.top - before.top,
            after.total - before.total,
            "the anchor rides down with the content above it"
        );
    }

    #[test]
    fn scrolling_back_to_the_bottom_parks_there() {
        let mut view = Scrollback::default();
        for number in 0..40 {
            view.push_notice(format!("line {number}"));
        }
        view.layout(20, 5);

        view.scroll_up(10);
        assert!(view.scrolled());

        view.scroll_down(10);
        assert!(!view.scrolled(), "it does not stick just short of the end");
        assert_eq!(view.scroll(), Scroll::Bottom);
    }

    #[test]
    fn two_page_keys_without_a_draw_between_move_two_pages() {
        let mut view = Scrollback::default();
        for number in 0..40 {
            view.push_notice(format!("line {number}"));
        }
        let bottom = view.layout(20, 5).top;

        view.scroll_up(4);
        view.scroll_up(4);

        assert_eq!(view.layout(20, 5).top, bottom - 8);
    }

    #[test]
    fn an_evicted_anchor_falls_back_to_the_bottom() {
        let mut view = Scrollback::default();
        for number in 0..MAX_ENTRIES {
            view.push_notice(format!("line {number}"));
        }
        view.layout(20, 5);
        view.scroll_up(1_000);
        assert!(view.scrolled());

        // Push until the cap eats the entry the anchor named.
        for number in 0..MAX_ENTRIES {
            view.push_notice(format!("later {number}"));
        }

        assert_eq!(
            view.scroll(),
            Scroll::Bottom,
            "the content it held is gone, so it holds nothing"
        );
    }

    #[test]
    fn assistant_code_blocks_wrap_long_lines() {
        let mut view = Scrollback::default();
        view.push_text("```\n012345678901234567890123456789\n```");

        let rendered: Vec<String> = view.lines(12).iter().map(ToString::to_string).collect();
        assert!(
            rendered.iter().all(|line| line.chars().count() <= 12),
            "code lines are wrapped to the pane: {rendered:?}"
        );
        assert!(rendered.iter().any(|line| line.contains("0123456789")));
    }
}
