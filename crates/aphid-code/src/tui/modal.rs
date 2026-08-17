//! The things that take over the screen: the model picker, the permission
//! prompt and the process list.

use std::sync::mpsc::Sender;
use std::time::Duration;

use aphid_agent::exec::{Process, Status};
use aphid_core::Model;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::plugins::permissions::{Decision, Risk};
use crate::tui::scrollback::{bytes, one_line};
use crate::tui::status::tokens;

/// What is covering the transcript, if anything.
pub enum Modal {
    Models {
        models: Vec<Model>,
        selected: usize,
    },
    Confirm(Confirm),
    /// The process list holds a snapshot, refreshed on the poll tick. Reading
    /// the registry takes a lock, and a lock has no business being taken while
    /// a frame is drawn.
    Processes {
        rows: Vec<Process>,
        selected: usize,
    },
}

/// A gated tool call waiting on an answer.
pub struct Confirm {
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
    pub reply: Sender<Decision>,
}

impl Confirm {
    /// Answer, releasing the agent's task.
    pub fn answer(self, decision: Decision) {
        // A closed channel means the run was already cancelled.
        let _ = self.reply.send(decision);
    }
}

impl Modal {
    pub fn move_selection(&mut self, delta: isize) {
        let (len, selected) = match self {
            Modal::Models { models, selected } => (models.len(), selected),
            // Only the running ones can be selected: a finished process is a
            // report, with nothing left to do to it.
            Modal::Processes { rows, selected } => (running(rows).len(), selected),
            Modal::Confirm(_) => return,
        };
        if len == 0 {
            return;
        }
        let next = (*selected as isize + delta).rem_euclid(len as isize);
        *selected = next as usize;
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&Model> {
        match self {
            Modal::Models { models, selected } => models.get(*selected),
            Modal::Confirm(_) | Modal::Processes { .. } => None,
        }
    }

    /// The running process under the cursor, if this is the process list.
    ///
    /// The list changes underneath the cursor, so the index is clamped here
    /// rather than trusted.
    #[must_use]
    pub fn selected_process(&self) -> Option<Process> {
        let Modal::Processes { rows, selected } = self else {
            return None;
        };
        let running = running(rows);
        running.get(*selected).or_else(|| running.last()).cloned()
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        match self {
            Modal::Models { models, selected } => render_models(frame, area, models, *selected),
            Modal::Confirm(confirm) => render_confirm(frame, area, confirm),
            Modal::Processes { rows, selected } => {
                render_processes(frame, area, rows, *selected);
            }
        }
    }
}

/// The running ones, in the order they started.
fn running(rows: &[Process]) -> Vec<Process> {
    rows.iter().filter(|p| p.running()).cloned().collect()
}

/// A centred box `width` columns wide and `height` rows tall, clamped to `area`.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    cell
}

fn render_models(frame: &mut Frame<'_>, area: Rect, models: &[Model], selected: usize) {
    let width = 64u16;
    let height = models.len() as u16 + 2;
    let cell = centred(area, width, height);

    let rows: Vec<Line<'_>> = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let chosen = index == selected;
            let style = if chosen {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Rates are already per million tokens, which is how providers
            // quote them.
            let rates = &model.cost.rates;
            let detail = format!(
                "{} ctx · ${:.2}/${:.2} per M",
                tokens(model.context_window),
                rates.input,
                rates.output,
            );
            Line::from(vec![
                Span::styled(
                    format!("{} {:<24}", if chosen { "▸" } else { " " }, model.id),
                    style,
                ),
                Span::styled(detail, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    frame.render_widget(Clear, cell);
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" model — ↑↓ to move, Enter to switch, Esc to cancel ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        cell,
    );
}

/// The columns before the command, which are the same in both sections so the
/// two read as one table.
const COLUMNS: usize = 40;

fn render_processes(frame: &mut Frame<'_>, area: Rect, rows: &[Process], selected: usize) {
    let (live, done): (Vec<Process>, Vec<Process>) =
        rows.iter().cloned().partition(Process::running);

    let width = area.width.saturating_sub(8).min(96);
    let room = (width as usize).saturating_sub(COLUMNS + 2);

    let mut rows = Vec::new();
    if live.is_empty() {
        rows.push(Line::styled(
            "  nothing running",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        // A list that changes under the cursor: clamp rather than trust.
        let chosen = selected.min(live.len() - 1);
        for (index, process) in live.iter().enumerate() {
            rows.push(row(process, index == chosen, room));
        }
    }
    if !done.is_empty() {
        rows.push(Line::default());
        rows.push(Line::styled(
            "  recent",
            Style::default().fg(Color::DarkGray),
        ));
        // Newest first: what just happened is what a reader came for.
        for process in done.iter().rev() {
            rows.push(row(process, false, room));
        }
    }

    let cell = centred(area, width, rows.len() as u16 + 2);
    frame.render_widget(Clear, cell);
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" processes — ↑↓ to move, k to stop, Esc to close ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        cell,
    );
}

/// One process: what it is, then how it is going, then the command itself.
fn row(process: &Process, chosen: bool, room: usize) -> Line<'static> {
    let style = if chosen {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let pid = process
        .pid
        .map_or_else(|| "—".to_owned(), |pid| pid.to_string());
    let size = if process.running() {
        String::new()
    } else {
        bytes(process.bytes as usize)
    };
    let (token, colour) = state(&process.status);

    Line::from(vec![
        Span::styled(
            format!(
                "{} {:<3} {:>7} {:<9}",
                if chosen { "▸" } else { " " },
                process.id,
                pid,
                one_line(&process.origin, 9),
            ),
            style,
        ),
        Span::styled(format!(" {token:<9}"), Style::default().fg(colour)),
        Span::styled(
            format!("{:>5} {:>8}  ", elapsed(process.elapsed()), size),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(one_line(&process.command, room)),
    ])
}

/// How a process is going, in a word and a colour.
fn state(status: &Status) -> (String, Color) {
    match status {
        Status::Running => (String::new(), Color::Reset),
        Status::Killing => ("stopping…".to_owned(), Color::Yellow),
        Status::Exited(0) => ("✓".to_owned(), Color::Green),
        Status::Exited(code) => (format!("✗ {code}"), Color::Red),
        Status::Signalled => ("signal".to_owned(), Color::Red),
        Status::TimedOut => ("timeout".to_owned(), Color::Yellow),
        Status::Cancelled => ("cancelled".to_owned(), Color::DarkGray),
        Status::Killed => ("stopped".to_owned(), Color::Red),
        Status::Failed(_) => ("failed".to_owned(), Color::Red),
    }
}

/// `m:ss`, or `h:mm:ss` once it has been running that long.
fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn render_confirm(frame: &mut Frame<'_>, area: Rect, confirm: &Confirm) {
    let colour = match confirm.risk {
        Risk::Destructive => Color::Red,
        _ => Color::Yellow,
    };
    let label = match confirm.risk {
        Risk::Destructive => "destructive",
        Risk::Mutate => "changes files",
        Risk::Read => "reads",
    };

    let width = area.width.saturating_sub(8).min(80);
    let body = crate::tui::scrollback::wrap(&confirm.summary, width.saturating_sub(4) as usize);
    // title, blank, body, blank, key hints, plus the two border rows.
    let height = body.len() as u16 + 6;
    let cell = centred(area, width, height);

    let mut lines = vec![Line::styled(
        format!("{} — {label}", confirm.tool),
        Style::default().fg(colour).add_modifier(Modifier::BOLD),
    )];
    lines.push(Line::default());
    for part in body {
        lines.push(Line::raw(part));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        "[y] once   [a] always   [n] no",
        Style::default().fg(Color::DarkGray),
    ));

    frame.render_widget(Clear, cell);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" permission ")
                .border_style(Style::default().fg(colour)),
        ),
        cell,
    );
}
