//! One chat, drawn.
//!
//! Not [`aphid_code::tui::view::View`], although it was tempting. That type's
//! entries are `User`, `Assistant`, `Thinking` and `Tool`, and putting a group
//! chat through it would mean deciding that everything anybody else said is
//! "user" — which throws the author away, and the author is the one thing a
//! group chat cannot lose.

use std::collections::HashMap;

use aphid_nostr::nostr::event::{Event, EventId};
use aphid_nostr::nostr::key::PublicKey;
use aphid_nostr::nostr::types::Timestamp;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as Row, Span};

/// How many ids to keep for the `previous` tag on what is typed next.
const RECENT: usize = 8;

/// The width of the time column, `HH:MM` and a space.
const TIME: usize = 6;

/// The width of the author column.
const AUTHOR: usize = 12;

/// Names for keys, as their kind 0 events said.
pub type Names = HashMap<PublicKey, String>;

/// One thing on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub id: Option<EventId>,
    pub at: Timestamp,
    /// Who said it. `None` is the terminal talking to the person at it — the
    /// answer to a `/who`, or a note that the colony stopped.
    pub author: Option<PublicKey>,
    pub text: String,
}

/// Everything said in one group.
#[derive(Debug, Default)]
pub struct Log {
    lines: Vec<Line>,
    /// How many rows up from the bottom the view is. Zero is following.
    scroll: usize,
}

impl Log {
    /// Put a message in, in time order.
    ///
    /// Events do not arrive in order — a subscription answers with what is
    /// stored, newest first, and then with what is live — so this inserts
    /// rather than appends. A message already here is not added twice.
    pub fn push(&mut self, event: &Event) {
        if self.lines.iter().any(|line| line.id == Some(event.id)) {
            return;
        }
        let line = Line {
            id: Some(event.id),
            at: event.created_at,
            author: Some(event.pubkey),
            text: event.content.clone(),
        };
        let at = self
            .lines
            .partition_point(|held| (held.at, held.id) < (line.at, line.id));
        self.lines.insert(at, line);
    }

    /// Say something to the person at the terminal.
    pub fn note(&mut self, text: impl Into<String>) {
        self.lines.push(Line {
            id: None,
            at: Timestamp::now(),
            author: None,
            text: text.into(),
        });
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll = 0;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The oldest message here, which is where a backfill asks from.
    #[must_use]
    pub fn oldest(&self) -> Option<Timestamp> {
        self.lines.first().map(|line| line.at)
    }

    /// The newest ids, for the `previous` tag on what is typed next.
    #[must_use]
    pub fn recent(&self) -> Vec<EventId> {
        self.lines
            .iter()
            .rev()
            .filter_map(|line| line.id)
            .take(RECENT)
            .collect()
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows).min(self.lines.len());
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Render, wrapped to `width`, and cut to the last `height` rows.
    #[must_use]
    pub fn rows(&self, width: u16, height: u16, names: &Names, show_time: bool) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        let mut last_author = None;

        for line in &self.lines {
            // A run of messages from one person gets one header. A chat where
            // every line repeats the name is a chat nobody can read.
            let repeated = line.author.is_some() && line.author == last_author;
            rows.extend(self.draw(line, width, names, show_time, repeated));
            last_author = line.author;
        }

        let height = height as usize;
        let end = rows.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        rows[start..end].to_vec()
    }

    fn draw<'a>(
        &self,
        line: &'a Line,
        width: u16,
        names: &Names,
        show_time: bool,
        repeated: bool,
    ) -> Vec<Row<'a>> {
        let gutter = if show_time { TIME + AUTHOR } else { AUTHOR };
        let text_width = (width as usize).saturating_sub(gutter).max(8);

        let mut header: Vec<Span<'a>> = Vec::new();
        if show_time {
            let stamp = if repeated {
                " ".repeat(TIME)
            } else {
                format!("{:<TIME$}", clock(line.at))
            };
            header.push(Span::styled(stamp, Style::default().fg(Color::DarkGray)));
        }
        header.push(match line.author {
            None => Span::styled(
                format!("{:<AUTHOR$}", "·"),
                Style::default().fg(Color::DarkGray),
            ),
            Some(_) if repeated => Span::raw(" ".repeat(AUTHOR)),
            Some(author) => Span::styled(
                format!("{:<AUTHOR$}", cut(&name_of(author, names), AUTHOR - 1)),
                Style::default()
                    .fg(colour(author))
                    .add_modifier(Modifier::BOLD),
            ),
        });

        let style = if line.author.is_none() {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default()
        };

        let mut rows = Vec::new();
        for (index, piece) in wrap(&line.text, text_width).into_iter().enumerate() {
            let mut spans = if index == 0 {
                header.clone()
            } else {
                vec![Span::raw(" ".repeat(gutter))]
            };
            spans.push(Span::styled(piece, style));
            rows.push(Row::from(spans));
        }
        rows
    }
}

/// What to call a key.
#[must_use]
pub fn name_of(who: PublicKey, names: &Names) -> String {
    names
        .get(&who)
        .cloned()
        .unwrap_or_else(|| who.to_hex()[..8].to_owned())
}

/// A colour for a key, so one person keeps one colour.
///
/// Six, and none of them the banner green or a grey: those are the terminal's
/// own, and a person coloured like the wordmark reads as part of the frame.
#[must_use]
pub fn colour(who: PublicKey) -> Color {
    const WHEEL: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Blue,
        Color::Red,
        Color::LightCyan,
    ];
    let byte = who.as_bytes()[0] as usize;
    WHEEL[byte % WHEEL.len()]
}

/// `HH:MM`, in this machine's time.
///
/// Local and not UTC, because the times in a chat are read against the clock on
/// the wall behind it. A message stamped in a time zone nobody is in is worse
/// than one with no time on it at all.
fn clock(at: Timestamp) -> String {
    let secs = i64::try_from(at.as_secs()).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(
        || "--:--".to_owned(),
        |utc| {
            chrono::DateTime::<chrono::Local>::from(utc)
                .format("%H:%M")
                .to_string()
        },
    )
}

/// Cut to `width` characters, never through the middle of one.
fn cut(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Break `text` into pieces no wider than `width`, at a space where there is
/// one. A word longer than the whole width is cut rather than allowed to run
/// off the side.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            let word: String = if word.chars().count() > width {
                cut(word, width)
            } else {
                word.to_owned()
            };
            let would = row.chars().count() + usize::from(!row.is_empty()) + word.chars().count();
            if would > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&word);
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_text_breaks_at_a_space() {
        assert_eq!(
            wrap("the build is red on main", 12),
            vec!["the build is", "red on main"]
        );
    }

    #[test]
    fn a_word_wider_than_the_screen_is_cut() {
        assert_eq!(wrap("aaaaaaaaaaaaaaa", 5), vec!["aaaaa"]);
    }

    #[test]
    fn an_empty_message_is_still_one_row() {
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    #[test]
    fn a_newline_is_kept() {
        assert_eq!(wrap("one\ntwo", 10), vec!["one", "two"]);
    }
}
