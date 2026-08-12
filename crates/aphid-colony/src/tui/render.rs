//! The three panes.
//!
//! ```text
//! ┌ chats ───────┬ #general ─────────────────────────────┐
//! │ #general   2 │ 09:14  thiago  morning                │
//! │ #build       │ 09:15  scout   @thiago the build is   │
//! │ @scout     1 │                red on main            │
//! ├──────────────┴───────────────────────────────────────┤
//! │ > say something                                      │
//! ├──────────────────────────────────────────────────────┤
//! │ ws://127.0.0.1:7777 · 3 here · #general              │
//! └──────────────────────────────────────────────────────┘
//! ```

use aphid_code::tui::input::Input;
use aphid_code::tui::logo::COLOR as BANNER;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::{self, App};

/// How wide the nav is, when there is room for it. Enough for
/// `@a-long-name  9`.
const NAV: u16 = 24;

/// The banner green, so a colony looks like the rest of aphid.
const EDGE: Color = Color::Rgb(BANNER.0, BANNER.1, BANNER.2);

/// How many rows the input box may grow to, borders included.
const INPUT_ROWS: u16 = 6;

pub fn draw(frame: &mut Frame<'_>, app: &App, input: &Input) {
    let rows = u16::try_from(input.line_count()).unwrap_or(1).clamp(1, 4);
    let [body, editing, status] = Layout::vertical([
        Constraint::Min(1),
        // Two more for the border's own rows, the arithmetic `aphid-alate`'s
        // terminal already does.
        Constraint::Length((rows + 2).min(INPUT_ROWS)),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [nav, log] = Layout::horizontal([
        // Never more than a third of a narrow terminal: on eighty columns a
        // fixed twenty-four leaves the chat unreadable.
        Constraint::Length(NAV.min(body.width / 3).max(8)),
        Constraint::Min(20),
    ])
    .areas(body);

    chats(frame, app, nav);
    messages(frame, app, log);
    frame.render_widget(input.textarea(), editing);
    line(frame, app, status);
}

/// The left pane: what there is to talk in.
fn chats(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(EDGE))
        .title(" chats ");
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let height = inside.height as usize;
    let rows = app.chats.rows();
    // Follow the selection when the list is longer than the pane.
    let start = app.chats.at().saturating_sub(height.saturating_sub(1));

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(at, chat)| {
            let chosen = at == app.chats.at();
            let label = chat.label(&app.names);
            let count = if chat.unread > 0 {
                format!("{}", chat.unread)
            } else {
                String::new()
            };

            let width = inside.width as usize;
            let room = width.saturating_sub(count.chars().count() + 1);
            let label: String = label.chars().take(room).collect();
            let pad =
                " ".repeat(width.saturating_sub(label.chars().count() + count.chars().count()));

            let style = match (chosen, chat.joined) {
                (true, _) => Style::default().fg(EDGE).add_modifier(Modifier::REVERSED),
                (false, true) => Style::default(),
                (false, false) => Style::default().fg(Color::DarkGray),
            };

            Line::from(vec![
                Span::styled(label, style),
                Span::styled(pad, style),
                Span::styled(
                    count,
                    if chosen {
                        style
                    } else {
                        style.fg(EDGE).add_modifier(Modifier::BOLD)
                    },
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inside);
}

/// The right pane: what was said in the chat on screen.
fn messages(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(EDGE))
        .title(format!(" {} ", app::heading(app)));
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let rows = match app.current() {
        Some(log) if !log.is_empty() => {
            log.rows(inside.width, inside.height, &app.names, app.show_time)
        }
        // An empty colony says what to do about it rather than nothing at all.
        _ => vec![
            Line::from(""),
            Line::styled("  nothing here yet", Style::default().fg(Color::DarkGray)),
            Line::styled(
                "  /join a channel, /dm somebody, or /help",
                Style::default().fg(Color::DarkGray),
            ),
        ],
    };

    frame.render_widget(Paragraph::new(rows), inside);
}

/// The status line: where this is, and how busy.
fn line(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let here = app.names.len();
    let mut text = format!(" {} · {here} known", app.url);
    if let Some(chat) = app.chats.current() {
        text.push_str(&format!(" · {}", chat.label(&app.names)));
    }
    frame.render_widget(
        Paragraph::new(Line::styled(text, Style::default().fg(Color::DarkGray))),
        area,
    );
}
