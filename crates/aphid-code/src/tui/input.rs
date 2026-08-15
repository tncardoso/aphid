//! The input box: a multi-line editor (capped to 4 visible rows) with history.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Padding};
use ratatui_textarea::TextArea;

use super::logo::COLOR as BANNER;

/// Border color of the input box: the banner green of the wordmark.
const BORDER: Color = Color::Rgb(BANNER.0, BANNER.1, BANNER.2);

/// Border color when the line is a `!` command.
const BANG_BORDER: Color = Color::Red;

/// What a keypress meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing the caller needs to handle.
    None,
    Submit(String),
    /// Enter on a line that starts with `!`: run the rest as a shell command.
    Bang(String),
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
        let border = if self.bang() { BANG_BORDER } else { BORDER };
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .padding(Padding::horizontal(1))
                .title(title),
        );
    }

    /// The first non-blank character of the buffer is `!`: the line is a
    /// shell command, not a prompt.
    fn bang(&self) -> bool {
        self.textarea
            .lines()
            .iter()
            .map(|line| line.trim_start())
            .find(|line| !line.is_empty())
            .is_some_and(|line| line.starts_with('!'))
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

    /// Drop pasted text into the buffer at the cursor.
    ///
    /// A paste is not typing: its newlines are part of the text, not a series
    /// of submits, so it lands whole and waits for Enter like anything else.
    /// Terminals disagree about how they end a line, and the textarea only
    /// knows `\n` and `\r\n`, so a lone `\r` is normalised here.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.textarea.insert_str(text);
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
                // A line starting with `!` is a shell command, not a prompt.
                if let Some(command) = trimmed.strip_prefix('!') {
                    let command = command.trim();
                    if command.is_empty() {
                        // A bare `!` runs nothing and stays for the user to
                        // finish.
                        return Action::None;
                    }
                    // Remembered like a prompt, so `Up` recalls the line as
                    // typed and `Enter` runs it again; cleared like a submit,
                    // because the line is going to the transcript, not back
                    // into the box.
                    self.remember(&trimmed);
                    self.clear();
                    return Action::Bang(command.to_owned());
                }
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
        if self.browsing == self.history.len() {
            // Down on the live line has nothing to recall: leave the draft be,
            // rather than "restoring" the empty parked line over it.
            if delta > 0 {
                return;
            }
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
    fn a_bang_line_runs_a_command_instead_of_submitting() {
        let mut input = Input::default();
        typed(&mut input, "!ls");
        assert_eq!(input.text(), "!ls");
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::Bang("ls".into()));
        assert_eq!(input.text(), "", "the box is cleared like a submit");
    }

    #[test]
    fn a_bang_line_is_remembered_in_history() {
        let mut input = Input::default();
        typed(&mut input, "!ls");
        input.handle(key(KeyCode::Enter));

        // Up recalls the line as typed, `!` included, so Enter runs it again.
        input.handle(key(KeyCode::Up));
        assert_eq!(input.text(), "!ls");
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::Bang("ls".into()));
    }

    #[test]
    fn a_bare_bang_runs_nothing() {
        let mut input = Input::default();
        typed(&mut input, "!");
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::None);
        assert_eq!(input.text(), "!", "the line stays for the user to finish");
    }

    #[test]
    fn leading_whitespace_still_makes_a_bang_line() {
        let mut input = Input::default();
        typed(&mut input, "  !ls");
        assert_eq!(input.handle(key(KeyCode::Enter)), Action::Bang("ls".into()));
    }

    #[test]
    fn the_border_turns_red_for_a_bang_line() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        // The block's border style is private to ratatui, so the border color
        // is read from what the textarea actually draws.
        fn border_color(input: &Input) -> Color {
            let area = Rect::new(0, 0, 20, 3);
            let mut buf = Buffer::empty(area);
            input.textarea().render(area, &mut buf);
            buf[(0, 0)].style().fg.expect("a border fg")
        }

        let mut input = Input::default();
        input.set_prompt(false);
        assert_eq!(
            border_color(&input),
            BORDER,
            "an ordinary line keeps the green border"
        );

        typed(&mut input, "!ls");
        input.set_prompt(false);
        assert_eq!(
            border_color(&input),
            BANG_BORDER,
            "a bang line turns the border red"
        );

        input.handle(key(KeyCode::Enter));
        input.set_prompt(false);
        assert_eq!(
            border_color(&input),
            BORDER,
            "clearing the line turns the border green again"
        );
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
    fn down_on_the_live_line_keeps_the_draft() {
        let mut input = Input::default();
        typed(&mut input, "first");
        input.handle(key(KeyCode::Enter));

        typed(&mut input, "draft");
        input.handle(key(KeyCode::Down));
        assert_eq!(input.text(), "draft", "there is nothing below the draft");
    }

    #[test]
    fn the_draft_survives_more_than_one_round_trip() {
        let mut input = Input::default();
        typed(&mut input, "first");
        input.handle(key(KeyCode::Enter));

        typed(&mut input, "draft");
        for _ in 0..2 {
            input.handle(key(KeyCode::Up));
            assert_eq!(input.text(), "first");
            input.handle(key(KeyCode::Down));
            assert_eq!(input.text(), "draft", "the parked line comes back again");
            // One Down too many, the way anybody checks they are at the bottom.
            input.handle(key(KeyCode::Down));
            assert_eq!(input.text(), "draft");
        }
    }

    #[test]
    fn a_pasted_block_waits_for_enter() {
        let mut input = Input::default();
        input.paste("one\ntwo\nthree");
        assert_eq!(input.line_count(), 3);
        assert_eq!(input.text(), "one\ntwo\nthree");
        assert_eq!(
            input.handle(key(KeyCode::Enter)),
            Action::Submit("one\ntwo\nthree".into()),
            "the whole block goes as one prompt"
        );
    }

    #[test]
    fn a_paste_lands_at_the_cursor_with_line_endings_normalised() {
        let mut input = Input::default();
        typed(&mut input, "see: ");
        input.paste("one\r\ntwo\rthree");
        assert_eq!(input.text(), "see: one\ntwo\nthree");
    }

    #[test]
    fn control_c_quits_and_esc_cancels() {
        let mut input = Input::default();
        assert_eq!(input.handle(control('c')), Action::Quit);
        assert_eq!(input.handle(key(KeyCode::Esc)), Action::Cancel);
    }
}
