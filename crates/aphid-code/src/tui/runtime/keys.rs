//! Terminal input, on the one thread that is allowed to block for it.

use ratatui::crossterm::event::{self, Event, MouseEventKind};

use super::Hub;

/// Read the terminal for as long as the loop is listening.
///
/// An OS thread and not a task, because [`event::read`] blocks: a task doing
/// this would hold a runtime worker for the whole session. It ends by itself
/// when the hub closes, so there is nothing to join.
///
/// `into_msg` says what each event means to this app, and returns `None` for
/// the ones it does not want. Mouse events that are neither a button nor the
/// wheel are dropped before that, because a terminal reporting every cursor
/// move would otherwise wake the loop thousands of times for nothing.
pub fn spawn_input_thread<M, F>(hub: Hub<M>, into_msg: F)
where
    M: Send + 'static,
    F: Fn(Event) -> Option<M> + Send + 'static,
{
    std::thread::spawn(move || {
        loop {
            let Ok(event) = event::read() else { return };
            if let Event::Mouse(mouse) = &event
                && !matches!(
                    mouse.kind,
                    MouseEventKind::Down(_)
                        | MouseEventKind::Up(_)
                        | MouseEventKind::Drag(_)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                )
            {
                continue;
            }
            if let Some(msg) = into_msg(event)
                && !hub.send(msg)
            {
                return;
            }
        }
    });
}
