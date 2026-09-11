//! A list you narrow by typing.
//!
//! The picker is the third thing that covers the transcript, next to the
//! [`Modal`](crate::tui::modal::Modal)s. What sets it apart is the query: a
//! modal is a fixed list you walk with the arrows, and a picker is an open list
//! you cut down to size first. That is what a list of sessions needs, because
//! there are more of them than fit on a screen and their ids are too long to
//! read one off it.
//!
//! It knows nothing about what it is picking. A caller turns its own things
//! into [`Row`]s, and gets a [`Row::key`] back when one is chosen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::modal::centred;
use crate::tui::scrollback::one_line;

/// How many rows are on screen at one time. A longer list scrolls under the
/// cursor rather than making a box taller than the terminal.
const WINDOW: usize = 12;

/// The widest the box is allowed to be, before the terminal's own width cuts it.
const WIDTH: u16 = 76;

/// One thing that can be picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What the picker gives back when this row is chosen.
    pub key: String,
    /// The text drawn first, and the first thing the query is matched against.
    pub label: String,
    /// What follows it, dimmed. Matched as well, so typing `cron` or a date
    /// finds a row whose label is an opaque id.
    pub detail: String,
    /// Marked as the one you are already on.
    pub current: bool,
}

impl Row {
    /// A row with no detail and no mark.
    #[must_use]
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            detail: String::new(),
            current: false,
        }
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    #[must_use]
    pub fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }
}

/// Who decides which rows survive the query.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Filter {
    /// The picker itself, with [`score`]. A caller hands over every row it has
    /// and the typing does the rest.
    Own,
    /// Somebody else, before the rows arrived. The picker keeps them in the
    /// order it was given and scores nothing.
    ///
    /// What a search engine needs: it has already ranked the answer, and a
    /// second opinion here would only fight it.
    External,
}

/// A list, a query that narrows it, and the row under the cursor.
pub struct Picker {
    /// What the box is called, drawn in its border.
    title: String,
    rows: Vec<Row>,
    query: String,
    filter: Filter,
    /// Indices into `rows`, best match first. The whole list, in its own order,
    /// while the query is empty.
    matches: Vec<usize>,
    /// An index into `matches`, not into `rows`.
    selected: usize,
}

impl Picker {
    #[must_use]
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self::build(title, rows, Filter::Own)
    }

    /// A picker over rows somebody else has already narrowed and ordered.
    ///
    /// It opens empty: the rows are asked for elsewhere and arrive by
    /// [`set_rows`](Self::set_rows), once for the empty query and again after
    /// every keystroke.
    #[must_use]
    pub fn external(title: impl Into<String>) -> Self {
        Self::build(title, Vec::new(), Filter::External)
    }

    fn build(title: impl Into<String>, rows: Vec<Row>, filter: Filter) -> Self {
        let mut picker = Self {
            title: title.into(),
            rows,
            query: String::new(),
            filter,
            matches: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    /// Replace the list, keeping the query.
    ///
    /// The list arrives after the picker opens and changes again while it is
    /// open, so the cursor is clamped here rather than trusted.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.refilter();
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The rows that survive the query, best match first.
    #[must_use]
    pub fn shown(&self) -> Vec<&Row> {
        self.matches
            .iter()
            .filter_map(|i| self.rows.get(*i))
            .collect()
    }

    /// The row under the cursor, if the query left one.
    #[must_use]
    pub fn chosen(&self) -> Option<&Row> {
        self.rows.get(*self.matches.get(self.selected)?)
    }

    /// Add one character to the query.
    ///
    /// The cursor goes back to the top: after a keystroke the best match is a
    /// different row, and leaving the cursor where it was would pick a row the
    /// typist never looked at.
    pub fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Add pasted text to the query.
    pub fn paste(&mut self, text: &str) {
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        self.refilter();
    }

    /// Take the last character back.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    /// Move the cursor, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    /// Score every row against the query and put the survivors in order.
    fn refilter(&mut self) {
        if self.filter == Filter::External || self.query.is_empty() {
            self.matches = (0..self.rows.len()).collect();
            self.selected = 0;
            return;
        }

        let mut scored: Vec<(i32, usize)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| Some((best(row, &self.query)?.0, index)))
            .collect();
        // Stable, so rows that score the same keep the order they came in.
        scored.sort_by_key(|(score, index)| (-*score, *index));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }

    /// The window of rows that fits on screen, and where the cursor is in it.
    fn window(&self) -> (usize, usize) {
        let len = self.matches.len();
        if len <= WINDOW {
            return (0, len);
        }
        // Keep the cursor in view, and keep the window full at either end.
        let top = self.selected.saturating_sub(WINDOW / 2).min(len - WINDOW);
        (top, top + WINDOW)
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let width = area.width.saturating_sub(8).min(WIDTH);
        let room = (width as usize).saturating_sub(4);
        let (top, bottom) = self.window();

        let mut lines = vec![Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(self.query.clone()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ])];

        if self.matches.is_empty() {
            let said = if self.rows.is_empty() {
                "  …"
            } else {
                "  nothing matches"
            };
            lines.push(Line::styled(said, Style::default().fg(Color::DarkGray)));
        }
        for (offset, index) in self.matches[top..bottom].iter().enumerate() {
            let Some(row) = self.rows.get(*index) else {
                continue;
            };
            lines.push(self.draw_row(row, top + offset == self.selected, room));
        }

        let cell = centred(area, width, lines.len() as u16 + 2);
        frame.render_widget(Clear, cell);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " {} — type to filter, ↑↓ to move, Enter to open, Esc to close ",
                        self.title
                    ))
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            cell,
        );
    }

    /// One row: the mark, the label with the matched characters lit, then the
    /// detail in grey.
    fn draw_row(&self, row: &Row, chosen: bool, room: usize) -> Line<'static> {
        let style = if chosen {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mark = match (chosen, row.current) {
            (true, _) => "▸",
            (false, true) => "*",
            (false, false) => " ",
        };

        // Only the field the query actually matched is lit, so a hit in the
        // detail does not paint the label as well. A row that came from an
        // external filter may light nothing: that filter can match what this
        // one cannot, a mistyped letter above all, and a row that nothing here
        // matches is simply drawn plain.
        let (label_hits, detail_hits) = match best(row, &self.query) {
            Some((_, Field::Label, hits)) => (hits, Vec::new()),
            Some((_, Field::Detail, hits)) => (Vec::new(), hits),
            None => (Vec::new(), Vec::new()),
        };

        let label = one_line(&row.label, room.saturating_sub(2));
        let mut spans = vec![Span::styled(format!("{mark} "), style)];
        spans.extend(lit(&label, &label_hits, style, chosen));
        if !row.detail.is_empty() {
            let left = room.saturating_sub(2 + label.chars().count() + 2);
            let detail = one_line(&row.detail, left);
            let grey = Style::default().fg(Color::DarkGray);
            let base = if chosen { style } else { grey };
            spans.push(Span::styled("  ".to_owned(), base));
            spans.extend(lit(&detail, &detail_hits, base, chosen));
        }
        Line::from(spans)
    }
}

/// Split `text` so the characters at `hits` carry the match style.
///
/// The row under the cursor is already painted on cyan, so a colour there would
/// have to be legible on cyan as well as on the terminal's own background. An
/// underline is legible on both, so it is what marks the match on that row; a
/// row that is not under the cursor gets the colour too, which reads from
/// further away.
///
/// Truncation may have cut a hit off the end, so any position past the text is
/// dropped rather than trusted.
fn lit(text: &str, hits: &[usize], base: Style, chosen: bool) -> Vec<Span<'static>> {
    if hits.is_empty() {
        return vec![Span::styled(text.to_owned(), base)];
    }
    let matched = if chosen {
        base.add_modifier(Modifier::UNDERLINED)
    } else {
        base.fg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut lit_run = false;
    for (index, c) in text.chars().enumerate() {
        let is_hit = hits.contains(&index);
        if is_hit != lit_run && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if lit_run { matched } else { base },
            ));
        }
        lit_run = is_hit;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if lit_run { matched } else { base }));
    }
    spans
}

/// Which half of a row a match landed in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Field {
    Label,
    Detail,
}

/// The better of the row's two fields, or `None` if neither matches.
///
/// A label hit wins a tie: it is the identity of the row, and the detail is
/// only there to make the identity findable.
fn best(row: &Row, query: &str) -> Option<(i32, Field, Vec<usize>)> {
    let label = score(&row.label, query).map(|(s, hits)| (s, Field::Label, hits));
    let detail = score(&row.detail, query).map(|(s, hits)| (s, Field::Detail, hits));
    match (label, detail) {
        (Some(label), Some(detail)) => Some(if detail.0 > label.0 { detail } else { label }),
        (some, None) | (None, some) => some,
    }
}

/// Reward for a character that follows one that also matched.
const RUN: i32 = 8;
/// Reward for a character that opens a word.
const BOUNDARY: i32 = 6;
/// Cost of each character skipped between two matches.
const SKIP: i32 = 1;

/// Where `query` matches in `text`, and how well, or `None`.
///
/// A subsequence match, ignoring case: every character of the query must appear
/// in `text`, in order, but not next to each other. The score rewards runs and
/// characters that open a word, so `cron` scores higher on `cron: news` than
/// the same four letters scattered through an id.
///
/// The match is greedy — the first place each character fits is where it goes —
/// which can miss the prettiest alignment of a repetitive string. It costs one
/// pass instead of a table, and for a list of ids and dates it agrees with what
/// a reader would have picked.
#[must_use]
fn score(text: &str, query: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let haystack: Vec<char> = text.chars().collect();
    let mut wanted = query.chars().flat_map(char::to_lowercase).peekable();

    let mut hits = Vec::new();
    let mut points = 0;
    let mut last: Option<usize> = None;

    for (index, c) in haystack.iter().enumerate() {
        let Some(want) = wanted.peek().copied() else {
            break;
        };
        if !c.to_lowercase().eq(std::iter::once(want)) {
            continue;
        }
        wanted.next();

        let after_run = last == Some(index.wrapping_sub(1));
        let opens_word = index == 0
            || haystack
                .get(index - 1)
                .is_some_and(|before| !before.is_alphanumeric());
        points += 10;
        if after_run {
            points += RUN;
        }
        if opens_word {
            points += BOUNDARY;
        }
        if let Some(previous) = last {
            points -= SKIP * (index - previous - 1) as i32;
        } else {
            // A match near the front is a better match: `s1` should find `s1`
            // before it finds a session whose id merely ends in one.
            points -= SKIP * index as i32;
        }
        hits.push(index);
        last = Some(index);
    }

    (wanted.peek().is_none()).then_some((points, hits))
}

#[cfg(test)]
mod tests {
    use super::{Picker, Row, score};

    fn rows() -> Vec<Row> {
        vec![
            Row::new("a", "20260811T091500-0000").detail("resident  2026-08-11 09:15 running"),
            Row::new("b", "20260811T142200-0000")
                .detail("attached  2026-08-11 14:22")
                .current(true),
            Row::new("c", "20260811T143000-0000").detail("cron: news  2026-08-11 14:30"),
        ]
    }

    fn labels(picker: &Picker) -> Vec<String> {
        picker.shown().iter().map(|row| row.key.clone()).collect()
    }

    #[test]
    fn an_empty_query_keeps_every_row_in_the_order_it_came() {
        let picker = Picker::new("sessions", rows());
        assert_eq!(labels(&picker), ["a", "b", "c"]);
    }

    #[test]
    fn a_subsequence_matches_and_a_missing_character_does_not() {
        assert!(score("cron: news", "cnews").is_some());
        assert!(score("cron: news", "czews").is_none());
    }

    #[test]
    fn a_run_beats_the_same_characters_scattered() {
        let together = score("abcd efgh", "abcd").expect("matches");
        let apart = score("axbxcxdx", "abcd").expect("matches");
        assert!(
            together.0 > apart.0,
            "{} should beat {}",
            together.0,
            apart.0
        );
    }

    #[test]
    fn a_match_that_opens_a_word_beats_one_in_the_middle() {
        let opening = score("xxx news", "news").expect("matches");
        let buried = score("xxxnews", "news").expect("matches");
        assert!(
            opening.0 > buried.0,
            "{} should beat {}",
            opening.0,
            buried.0
        );
    }

    #[test]
    fn the_query_ignores_case() {
        let (_, hits) = score("Cron: News", "cron").expect("matches");
        assert_eq!(hits, [0, 1, 2, 3]);
    }

    #[test]
    fn typing_narrows_the_list_and_backspace_widens_it_again() {
        let mut picker = Picker::new("sessions", rows());
        for c in "cron".chars() {
            picker.type_char(c);
        }
        assert_eq!(labels(&picker), ["c"]);
        assert_eq!(picker.chosen().map(|row| row.key.as_str()), Some("c"));

        for _ in 0..4 {
            picker.backspace();
        }
        assert_eq!(labels(&picker), ["a", "b", "c"]);
    }

    #[test]
    fn a_query_that_matches_nothing_has_nothing_to_choose() {
        let mut picker = Picker::new("sessions", rows());
        picker.paste("zzz");
        assert!(picker.shown().is_empty());
        assert!(picker.chosen().is_none());
    }

    #[test]
    fn the_cursor_wraps_at_both_ends() {
        let mut picker = Picker::new("sessions", rows());
        picker.move_selection(-1);
        assert_eq!(picker.chosen().map(|row| row.key.as_str()), Some("c"));
        picker.move_selection(1);
        assert_eq!(picker.chosen().map(|row| row.key.as_str()), Some("a"));
    }

    #[test]
    fn a_list_replaced_under_the_cursor_leaves_it_inside_the_new_one() {
        let mut picker = Picker::new("sessions", rows());
        picker.move_selection(-1);
        picker.set_rows(vec![Row::new("z", "only one")]);
        assert_eq!(picker.chosen().map(|row| row.key.as_str()), Some("z"));
    }

    #[test]
    fn an_external_picker_keeps_the_order_it_was_given() {
        let mut picker = Picker::external("files");
        picker.set_rows(vec![
            Row::new("z", "zzz.rs"),
            Row::new("a", "aaa.rs"),
            Row::new("m", "mmm.rs"),
        ]);
        for c in "aaa".chars() {
            picker.type_char(c);
        }
        assert_eq!(
            labels(&picker),
            ["z", "a", "m"],
            "the query narrows nothing here: somebody else already did"
        );
        assert_eq!(picker.chosen().map(|row| row.key.as_str()), Some("z"));
    }

    #[test]
    fn an_external_picker_still_reports_what_was_typed() {
        let mut picker = Picker::external("files");
        for c in "tui/".chars() {
            picker.type_char(c);
        }
        picker.backspace();
        assert_eq!(picker.query(), "tui");
    }

    #[test]
    fn replacing_the_list_keeps_the_query() {
        let mut picker = Picker::new("sessions", rows());
        for c in "cron".chars() {
            picker.type_char(c);
        }
        picker.set_rows(rows());
        assert_eq!(picker.query(), "cron");
        assert_eq!(labels(&picker), ["c"]);
    }
}
