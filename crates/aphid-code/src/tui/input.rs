//! The input box: a multi-line editor (capped to 4 visible rows) with history.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Padding};
use ratatui_textarea::TextArea;

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

/// A multi-line editor, backed by `ratatui-textarea`.
///
/// Editing itself (arrows, Backspace/Delete, Ctrl-A/E/K/W, undo/redo, …) is
/// delegated to the textarea's own default key bindings; this type only
/// intercepts the keys the app gives special meaning to — quitting,
/// switching models, submitting, and history recall — before anything else
/// reaches the textarea.
pub struct Input {
    textarea: TextArea<'static>,
    /// Where the visible window currently starts, in screen rows. Tracked by
    /// hand with the same scroll-to-cursor rule the widget uses internally,
    /// since that state isn't exposed publicly — this is what drives the
    /// scrollbar.
    scroll_top: usize,
    history: Vec<String>,
    /// Where we are in history; `history.len()` means "on the live line".
    browsing: usize,
    /// The live line, parked while browsing history.
    parked: Option<String>,
}

impl Default for Input {
    fn default() -> Self {
        let mut textarea = TextArea::default();
        // This UI has no underlined text anywhere; the crate's default
        // current-line underline would stand out as the only one.
        textarea.set_cursor_line_style(Style::default());
        Self {
            textarea,
            scroll_top: 0,
            history: Vec::new(),
            browsing: 0,
            parked: None,
        }
    }
}

impl Input {
    #[must_use]
    pub fn textarea(&self) -> &TextArea<'static> {
        &self.textarea
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.textarea.lines().len()
    }

    #[must_use]
    pub fn scroll_top(&self) -> usize {
        self.scroll_top
    }

    /// Draw the border and the running/idle indicator as its title. Called
    /// each frame, since the title depends on `Status::running`.
    pub fn set_prompt(&mut self, running: bool) {
        let title = if running { " … " } else { " > " };
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .padding(Padding::horizontal(1))
                .title(title),
        );
    }

    /// Recompute the scroll window for a viewport of `height` screen rows.
    /// Call once per frame, after the textarea has been rendered into an
    /// area of that height, so the scrollbar matches what was actually drawn.
    pub fn sync_scroll(&mut self, height: usize) {
        let cursor_row = self.textarea.screen_cursor().row;
        self.scroll_top = next_scroll_top(self.scroll_top, cursor_row, height);
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    fn set_text(&mut self, text: &str) {
        self.textarea.clear();
        self.textarea.insert_str(text);
    }

    pub fn handle(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Char('c') if control => return Action::Quit,
            KeyCode::Char('d') if control && self.textarea.is_empty() => return Action::Quit,
            KeyCode::Char('p') if control => return Action::CycleModel,
            KeyCode::Char('t') if control => return Action::ToggleThinking,

            KeyCode::PageUp => return Action::ScrollUp,
            KeyCode::PageDown => return Action::ScrollDown,

            KeyCode::Up if self.textarea.cursor().0 == 0 => {
                self.recall(-1);
                return Action::None;
            }
            KeyCode::Down if self.textarea.cursor().0 + 1 == self.line_count() => {
                self.recall(1);
                return Action::None;
            }

            KeyCode::Enter if shift => {
                self.textarea.insert_newline();
                return Action::None;
            }
            KeyCode::Enter => {
                let text = self.text();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Action::None;
                }
                let trimmed = trimmed.to_owned();
                self.remember(&trimmed);
                self.clear();
                return Action::Submit(trimmed);
            }
            KeyCode::Esc => return Action::Cancel,
            _ => {}
        }

        self.textarea.input(key);
        Action::None
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
            self.parked = Some(self.text());
        }

        let target = self.browsing as isize + delta;
        if target < 0 {
            return;
        }
        let target = target as usize;

        if target >= self.history.len() {
            self.browsing = self.history.len();
            let parked = self.parked.take().unwrap_or_default();
            self.set_text(&parked);
            return;
        }

        self.browsing = target;
        let entry = self.history[target].clone();
        self.set_text(&entry);
    }
}

/// Mirrors `ratatui-textarea`'s own internal scroll-to-cursor rule (its
/// `viewport` field isn't public), so the scrollbar we draw matches the
/// window the widget actually rendered.
fn next_scroll_top(prev_top: usize, cursor: usize, height: usize) -> usize {
    if height == 0 {
        prev_top
    } else if cursor < prev_top {
        cursor
    } else if prev_top + height <= cursor {
        cursor + 1 - height
    } else {
        prev_top
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

    fn shift_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
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
    fn unicode_text_round_trips() {
        let mut input = Input::default();
        typed(&mut input, "héllo — ok");
        assert_eq!(input.text(), "héllo — ok");
        input.handle(key(KeyCode::Left));
        input.handle(key(KeyCode::Backspace));
        assert_eq!(input.text(), "héllo — k");
    }

    #[test]
    fn shift_enter_inserts_a_newline_instead_of_submitting() {
        let mut input = Input::default();
        typed(&mut input, "one");
        assert_eq!(input.handle(shift_enter()), Action::None);
        typed(&mut input, "two");
        assert_eq!(input.line_count(), 2);
        assert_eq!(
            input.handle(key(KeyCode::Enter)),
            Action::Submit("one\ntwo".into())
        );
    }

    #[test]
    fn ctrl_u_undoes_instead_of_clearing_to_line_start() {
        // Undo is per-edit, not "clear to line start" — typing "abc" then
        // Backspace-ing to "ab" is one edit; Ctrl-U undoes just that one.
        let mut input = Input::default();
        typed(&mut input, "ab");
        input.handle(key(KeyCode::Char('c')));
        input.handle(key(KeyCode::Backspace));
        assert_eq!(input.text(), "ab");
        input.handle(control('u'));
        assert_eq!(input.text(), "abc", "undo restores the deleted 'c'");
    }

    #[test]
    fn ctrl_d_quits_only_when_the_buffer_is_empty() {
        let mut input = Input::default();
        assert_eq!(input.handle(control('d')), Action::Quit);

        typed(&mut input, "a");
        assert_eq!(input.handle(control('d')), Action::None);
    }

    #[test]
    fn up_moves_within_a_multiline_draft_before_recalling_history() {
        let mut input = Input::default();
        typed(&mut input, "first");
        input.handle(key(KeyCode::Enter));

        typed(&mut input, "one");
        input.handle(shift_enter());
        typed(&mut input, "two");
        input.handle(shift_enter());
        typed(&mut input, "three");

        // Cursor starts on the last of three lines: Up should move within
        // the draft, not touch history, until it reaches the first line.
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "one\ntwo\nthree", "moving up doesn't edit");
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "one\ntwo\nthree", "still just moving up");

        // Now on the first line: one more Up recalls history instead.
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "first");
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
    }
}
