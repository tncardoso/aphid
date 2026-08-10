//! The input line: a single-line editor with history.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a keypress meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing the caller needs to handle.
    None,
    Submit(String),
    /// Esc — cancel a run, or clear the line when idle.
    Cancel,
    Quit,
    ScrollUp,
    ScrollDown,
    /// Ctrl-P: next model without opening the picker.
    CycleModel,
    /// Ctrl-T: show or hide reasoning.
    ToggleThinking,
}

/// A one-line editor.
///
/// Cursor positions are byte offsets into `text`, always on a character
/// boundary — every move goes through `prev`/`next`, which step by character.
#[derive(Default)]
pub struct Input {
    text: String,
    cursor: usize,
    history: Vec<String>,
    /// Where we are in history; `history.len()` means "on the live line".
    browsing: usize,
    /// The live line, parked while browsing history.
    parked: Option<String>,
}

impl Input {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Cursor position measured in characters, which is what a renderer wants.
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn handle(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('c') if control => return Action::Quit,
            KeyCode::Char('d') if control && self.text.is_empty() => return Action::Quit,
            KeyCode::Char('p') if control => return Action::CycleModel,
            KeyCode::Char('t') if control => return Action::ToggleThinking,

            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::Char('e') if control => self.cursor = self.text.len(),
            KeyCode::Char('u') if control => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if control => {
                self.text.truncate(self.cursor);
            }
            KeyCode::Char('w') if control => self.delete_word(),

            KeyCode::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if let Some(at) = self.prev() {
                    self.text.remove(at);
                    self.cursor = at;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(at) = self.prev() {
                    self.cursor = at;
                }
            }
            KeyCode::Right => {
                if let Some(at) = self.next() {
                    self.cursor = at;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),

            KeyCode::Up => self.recall(-1),
            KeyCode::Down => self.recall(1),
            KeyCode::PageUp => return Action::ScrollUp,
            KeyCode::PageDown => return Action::ScrollDown,

            KeyCode::Enter => {
                let text = self.text.trim().to_owned();
                if text.is_empty() {
                    return Action::None;
                }
                self.remember(&text);
                self.clear();
                return Action::Submit(text);
            }
            KeyCode::Esc => return Action::Cancel,
            _ => {}
        }

        Action::None
    }

    fn prev(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(at, _)| at)
    }

    fn next(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    fn delete_word(&mut self) {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end();
        let start = trimmed.rfind(char::is_whitespace).map_or(0, |at| {
            at + head[at..].chars().next().map_or(1, char::len_utf8)
        });
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn remember(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_owned());
        }
        self.browsing = self.history.len();
        self.parked = None;
    }

    /// Step through history. Leaving the live line parks it so Down returns it.
    fn recall(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        if self.browsing == self.history.len() && delta < 0 {
            self.parked = Some(self.text.clone());
        }

        let target = self.browsing as isize + delta;
        if target < 0 {
            return;
        }
        let target = target as usize;

        if target >= self.history.len() {
            self.browsing = self.history.len();
            let parked = self.parked.take().unwrap_or_default();
            self.set(parked);
            return;
        }

        self.browsing = target;
        let entry = self.history[target].clone();
        self.set(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(input: &mut Input, text: &str) {
        for c in text.chars() {
            input.handle(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_submitting() {
        let mut input = Input::default();
        typed(&mut input, "hello");
        assert_eq!(input.text(), "hello");
        assert_eq!(
            input.handle(key(KeyCode::Enter)),
            Action::Submit("hello".into())
        );
        assert_eq!(input.text(), "");
    }

    #[test]
    fn an_empty_line_does_not_submit() {
        let mut input = Input::default();
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::None);
        typed(&mut input, "   ");
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn editing_stays_on_character_boundaries() {
        let mut input = Input::default();
        typed(&mut input, "héllo — ok");
        // Walk all the way left one character at a time, then back.
        for _ in 0..20 {
            input.handle(key(KeyCode::Left));
        }
        assert_eq!(input.cursor(), 0);
        for _ in 0..20 {
            input.handle(key(KeyCode::Right));
        }
        assert_eq!(input.cursor(), input.text().len());

        input.handle(key(KeyCode::Left));
        input.handle(key(KeyCode::Backspace));
        assert_eq!(input.text(), "héllo — k");
    }

    #[test]
    fn control_keys_edit_the_line() {
        let mut input = Input::default();
        typed(&mut input, "one two three");

        input.handle(control('w'));
        assert_eq!(input.text(), "one two ");

        input.handle(control('u'));
        assert_eq!(input.text(), "");

        typed(&mut input, "abc");
        input.handle(control('a'));
        assert_eq!(input.cursor(), 0);
        input.handle(control('k'));
        assert_eq!(input.text(), "");
    }

    #[test]
    fn history_walks_back_and_returns_the_live_line() {
        let mut input = Input::default();
        typed(&mut input, "first");
        input.handle(key(KeyCode::Enter));
        typed(&mut input, "second");
        input.handle(key(KeyCode::Enter));

        typed(&mut input, "draft");
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "second");
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "first");
        input.handle(key(KeyCode::Down));
        assert_eq!(input.text(), "second");
        input.handle(key(KeyCode::Down));
        assert_eq!(input.text(), "draft", "the parked line comes back");
    }

    #[test]
    fn control_c_quits_and_esc_cancels() {
        let mut input = Input::default();
        assert_eq!(input.handle(control('c')), Action::Quit);
        assert_eq!(input.handle(key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(input.handle(control('d')), Action::Quit);
    }
}
