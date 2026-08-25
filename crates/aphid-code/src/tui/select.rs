//! Selecting text in the transcript, and the two things you do with a
//! selection: paint it, and read it.
//!
//! Both operate on the wrapped lines the draw cache holds, because that is the
//! only place the transcript exists as text laid out on a screen. Neither
//! touches the model, and neither reads a terminal, so both are ordinary
//! functions with ordinary tests.
//!
//! A column is a character and not a display cell. That is the convention the
//! wrapping already uses — [`wrap`](crate::tui::scrollback::wrap) and its
//! neighbours count `chars()` — and a selection that counted differently from
//! the wrapping would select something other than what is under the mouse.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// A place in the transcript.
///
/// `line` indexes the whole wrapped transcript, not the visible part of it, so
/// scrolling does not move a selection: it holds text, not screen rows.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub line: usize,
    /// The character offset in that line. It may point past the end; only the
    /// lines themselves know how long they are, and they clamp it.
    pub column: usize,
}

/// What is selected in the transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started.
    pub anchor: Spot,
    /// Where it is now. It may be before the anchor: a drag upwards is a
    /// selection like any other.
    pub head: Spot,
    /// The button is still down.
    pub dragging: bool,
    /// The button came up on something worth copying, and the next draw is
    /// asked for its text. The draw is where the text is; the model only knows
    /// that it wants it.
    pub pending_copy: bool,
}

impl Selection {
    /// Start a selection at one spot.
    #[must_use]
    pub fn at(spot: Spot) -> Self {
        Self {
            anchor: spot,
            head: spot,
            dragging: true,
            pending_copy: false,
        }
    }

    /// The two spots in reading order.
    #[must_use]
    pub fn span(&self) -> (Spot, Spot) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether the selection covers no text at all — a click that never became
    /// a drag.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The last line the selection touches, for deciding whether content
    /// moving above it has left it pointing at the wrong text.
    #[must_use]
    pub fn last_line(&self) -> usize {
        self.anchor.line.max(self.head.line)
    }
}

/// Reverse the cells the selection covers.
///
/// `top` is the transcript line the first of `lines` is, so the absolute spots
/// can be found among the visible ones.
///
/// Reversing and not a background colour: the transcript paints in colours of
/// its own — cyan for what you typed, grey for a notice, true colour for the
/// logo — and reversing keeps every one of them legible without aphid having
/// to pick a highlight that suits each.
pub fn highlight(lines: &mut [Line<'static>], top: usize, span: (Spot, Spot)) {
    let (start, end) = span;
    for (offset, line) in lines.iter_mut().enumerate() {
        let number = top + offset;
        if number < start.line || number > end.line {
            continue;
        }
        let from = if number == start.line {
            start.column
        } else {
            0
        };
        let to = if number == end.line {
            end.column
        } else {
            usize::MAX
        };
        reverse(line, from, to);
    }
}

/// The text the selection covers, with the lines joined by newlines.
///
/// The first line is taken from the starting column, the last one up to the
/// ending column, and everything between them whole — the way a selection
/// flows in any terminal, rather than the rectangle a column selection would
/// cut.
///
/// What comes out is exactly the characters that are on the screen, prefixes
/// and all. A drag that starts at column zero takes the `> ` of a prompt with
/// it; one that starts after it does not. Guessing which characters are
/// content and which are frame would make the copy unpredictable, and what you
/// see is the one rule that never surprises.
#[must_use]
pub fn extract(lines: &[Line<'static>], span: (Spot, Spot)) -> String {
    let (start, end) = span;
    let mut out = String::new();
    for number in start.line..=end.line {
        let Some(line) = lines.get(number) else { break };
        let from = if number == start.line {
            start.column
        } else {
            0
        };
        let to = if number == end.line {
            end.column
        } else {
            usize::MAX
        };
        if number > start.line {
            out.push('\n');
        }
        out.push_str(&slice(line, from, to));
    }
    out.trim_end().to_owned()
}

/// The characters of one line between two columns.
fn slice(line: &Line<'static>, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut seen = 0usize;
    for span in &line.spans {
        let length = span.content.chars().count();
        if seen + length > from && seen < to {
            let skip = from.saturating_sub(seen);
            let take = to.min(seen + length) - seen - skip;
            out.extend(span.content.chars().skip(skip).take(take));
        }
        seen += length;
    }
    out
}

/// Add [`Modifier::REVERSED`] to the characters of `line` between two columns,
/// splitting the spans that straddle either edge.
///
/// The spans carry the transcript's own styling, and a line built with
/// [`Line::styled`] carries it on the line instead of on its span. Patching the
/// span keeps both: ratatui lays the line's style down first, so a slice that
/// only adds the reverse modifier keeps whatever colour it had.
fn reverse(line: &mut Line<'static>, from: usize, to: usize) {
    if from >= to {
        return;
    }

    let selected = Style::default().add_modifier(Modifier::REVERSED);
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut seen = 0usize;

    for span in line.spans.drain(..) {
        let length = span.content.chars().count();
        // Wholly outside, or empty: nothing to do but keep it.
        if length == 0 || seen + length <= from || seen >= to {
            seen += length;
            out.push(span);
            continue;
        }

        let head = from.saturating_sub(seen);
        let tail = to.min(seen + length) - seen;
        let text = span.content.chars().collect::<Vec<_>>();

        for (range, style) in [
            (0..head, span.style),
            (head..tail, span.style.patch(selected)),
            (tail..length, span.style),
        ] {
            if range.is_empty() {
                continue;
            }
            out.push(Span::styled(text[range].iter().collect::<String>(), style));
        }
        seen += length;
    }

    line.spans = out;
}

#[cfg(test)]
mod tests {
    use super::{Selection, Spot, extract, highlight};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    fn spot(line: usize, column: usize) -> Spot {
        Spot { line, column }
    }

    fn plain(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|text| Line::from((*text).to_owned()))
            .collect()
    }

    #[test]
    fn a_selection_inside_one_line_takes_the_columns_it_covers() {
        let lines = plain(&["hello world"]);
        assert_eq!(extract(&lines, (spot(0, 6), spot(0, 11))), "world");
    }

    #[test]
    fn a_selection_across_lines_joins_them_with_newlines() {
        let lines = plain(&["first line", "middle", "last line"]);
        assert_eq!(
            extract(&lines, (spot(0, 6), spot(2, 4))),
            "line\nmiddle\nlast"
        );
    }

    #[test]
    fn a_selection_past_the_end_of_a_line_stops_at_it() {
        let lines = plain(&["short", "also short"]);
        assert_eq!(
            extract(&lines, (spot(0, 0), spot(1, 999))),
            "short\nalso short"
        );
    }

    #[test]
    fn a_selection_reads_across_the_spans_of_one_line() {
        let lines = vec![Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::styled("typed text", Style::default().fg(Color::Cyan)),
        ])];
        assert_eq!(extract(&lines, (spot(0, 0), spot(0, 7))), "> typed");
    }

    #[test]
    fn an_empty_selection_reads_nothing() {
        let lines = plain(&["hello"]);
        assert_eq!(extract(&lines, (spot(0, 2), spot(0, 2))), "");
    }

    #[test]
    fn highlighting_splits_a_span_and_keeps_its_colour() {
        let coloured = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from(vec![Span::styled("hello world", coloured)])];

        highlight(&mut lines, 0, (spot(0, 6), spot(0, 11)));

        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 2, "the span is cut at the selection's edge");
        assert_eq!(spans[0].content, "hello ");
        assert_eq!(spans[0].style, coloured, "what is outside is untouched");
        assert_eq!(spans[1].content, "world");
        assert_eq!(spans[1].style.fg, Some(Color::Cyan), "the colour survives");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(spans[1].style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn highlighting_only_touches_the_lines_the_selection_covers() {
        let mut lines = plain(&["above", "inside", "below"]);
        // The visible lines start at transcript line 10.
        highlight(&mut lines, 10, (spot(11, 0), spot(11, 6)));

        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn a_selection_dragged_upwards_reads_the_same_way() {
        let mut backwards = Selection::at(spot(2, 4));
        backwards.head = spot(0, 6);
        let mut forwards = Selection::at(spot(0, 6));
        forwards.head = spot(2, 4);

        assert_eq!(backwards.span(), forwards.span());
    }
}
