//! Taking the terminal over, and giving it back.

use std::io::Stdout;

use ratatui::Terminal;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::prelude::CrosstermBackend;

/// The screen an aphid terminal draws on.
pub type Tty = Terminal<CrosstermBackend<Stdout>>;

/// Take the terminal over.
///
/// Reports whether the keyboard-enhancement protocol was enabled — needed so
/// [`restore`] knows whether to pop it, and so Shift+Enter can be told apart
/// from plain Enter in the input box. On terminals without it, Shift+Enter is
/// indistinguishable from plain Enter, so it just submits: a graceful
/// degradation, not a bug.
///
/// # Errors
///
/// Fails when the terminal refuses raw mode or the alternate screen.
pub fn setup() -> std::io::Result<(Tty, bool)> {
    // Restore the terminal even when something panics, so a crash does not
    // leave the shell in raw mode.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(DisableMouseCapture);
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        previous(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    // Pasted text then arrives whole, instead of as the keys it looks like —
    // one Enter per line, each of which would submit. The legacy Windows
    // console has no such mode and says so; a session there is no worse off
    // than before, so the refusal is not worth failing the start-up over.
    let _ = stdout.execute(EnableBracketedPaste);
    // Mouse reporting is also best-effort: a terminal that cannot report the
    // wheel still works, it just keeps keyboard-only scrolling.
    let _ = stdout.execute(EnableMouseCapture);

    let kitty = supports_keyboard_enhancement().unwrap_or(false);
    if kitty {
        stdout.execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))?;
    }

    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok((terminal, kitty))
}

/// Give the terminal back.
///
/// # Errors
///
/// Fails when the terminal refuses to leave raw mode or the alternate screen.
pub fn restore(terminal: &mut Tty, kitty: bool) -> std::io::Result<()> {
    if kitty {
        terminal
            .backend_mut()
            .execute(PopKeyboardEnhancementFlags)?;
    }
    let _ = terminal.backend_mut().execute(DisableMouseCapture);
    let _ = terminal.backend_mut().execute(DisableBracketedPaste);
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    Ok(())
}
