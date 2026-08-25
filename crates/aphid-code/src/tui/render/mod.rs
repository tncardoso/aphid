//! What the terminal paints, and the scratchpad it paints from.
//!
//! [`draw`] takes the model by shared reference. Everything it needs that the
//! model does not hold — wrapped lines, laid-out rectangles — is derived here
//! into a [`CodeCache`] the runtime owns, and anything the model has to know
//! about comes back as a message. So drawing the same model twice paints the
//! same thing, which is a property a test can check.

mod scrollback;

pub use scrollback::ScrollbackCache;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::tui::app::{App, MAX_INPUT_ROWS};
use crate::tui::msg::Msg;
use crate::tui::scrollback::Viewport;
use crate::tui::select;
use crate::tui::surface::Hit;

/// Everything a draw works out that the model does not hold.
#[derive(Default)]
pub struct CodeCache {
    pub scrollback: ScrollbackCache,
    /// What the last draw laid out, for the model to be told about.
    laid_out: Option<Laid>,
}

/// What a draw settled that a later keypress will need.
///
/// The one road from the screen back to the model, and it is a message like
/// everything else. A page key cannot know how far a page is until something
/// has been wrapped to a width, and a click cannot know what it hit until the
/// panels have been placed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Laid {
    pub viewport: Viewport,
    /// The clickable regions the panels drew, in the order they were drawn.
    pub hits: Vec<Hit>,
    /// Where the transcript pane was put. A click cannot become a line and a
    /// column without it, because the panels decide how wide the pane is.
    pub main: Rect,
    /// How many times the cache threw every block away.
    pub generation: u64,
    /// The first line the last layout moved, if it moved any.
    pub shifted_from: Option<usize>,
    /// The text under the selection, and only when the model asked for it.
    /// The lines live here and nowhere else, so this is the one way they get
    /// back to the model.
    pub selected: Option<String>,
}

/// Paint the whole screen.
pub fn draw(app: &App, frame: &mut Frame<'_>, cache: &mut CodeCache) {
    let content_height = (app.input.line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    // +2 for the border's top and bottom rows.
    let input_height = content_height + 2;

    let [transcript, input_row, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let mut hits = Vec::new();
    let main = app.surfaces.draw(frame, transcript, &mut hits);

    let viewport =
        cache
            .scrollback
            .layout(&app.scrollback, main.width as usize, main.height as usize);

    let mut lines = cache.scrollback.visible(viewport);
    if let Some(selection) = &app.selection {
        select::highlight(&mut lines, viewport.top, selection.span());
    }
    frame.render_widget(Paragraph::new(lines), main);

    cache.laid_out = Some(Laid {
        viewport,
        hits,
        main,
        generation: cache.scrollback.generation(),
        shifted_from: cache.scrollback.shifted_from(),
        // Read only when the mouse came up on something: the text is a fresh
        // allocation, and every other frame must not pay for one.
        selected: app
            .selection
            .as_ref()
            .filter(|selection| selection.pending_copy)
            .map(|selection| cache.scrollback.selected_text(selection.span())),
    });

    frame.render_widget(app.input.textarea(), input_row);

    if app.input.line_count() > content_height as usize {
        let mut state =
            ScrollbarState::new(app.input.line_count()).position(app.input.scroll_top());
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::default().fg(Color::DarkGray)),
            // Trim the border's top/bottom rows so the thumb only ever
            // covers the content rows it actually represents.
            input_row.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }

    frame.render_widget(Paragraph::new(app.status.line()), status);

    // The textarea draws its own cursor cell during render; there is no
    // manual `set_cursor_position` to do here.
    if let Some(modal) = &app.modal {
        modal.render(frame, frame.area());
    }
}

/// What the last draw settled.
///
/// Reported after every frame; the update compares it with what it already
/// knows and does nothing when it has not moved, which is the common case.
#[must_use]
pub fn laid_out(cache: &CodeCache) -> Option<Msg> {
    cache.laid_out.clone().map(Msg::LaidOut)
}
