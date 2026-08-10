//! The two things that take over the screen: the model picker and the
//! permission prompt.

use std::sync::mpsc::Sender;

use aphid_core::Model;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::plugins::permissions::{Decision, Risk};
use crate::tui::status::tokens;

/// What is covering the transcript, if anything.
pub enum Modal {
    Models { models: Vec<Model>, selected: usize },
    Confirm(Confirm),
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
        if let Modal::Models { models, selected } = self
            && !models.is_empty()
        {
            let len = models.len() as isize;
            let next = (*selected as isize + delta).rem_euclid(len);
            *selected = next as usize;
        }
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&Model> {
        match self {
            Modal::Models { models, selected } => models.get(*selected),
            Modal::Confirm(_) => None,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        match self {
            Modal::Models { models, selected } => render_models(frame, area, models, *selected),
            Modal::Confirm(confirm) => render_confirm(frame, area, confirm),
        }
    }
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
    let body = crate::tui::view::wrap(&confirm.summary, width.saturating_sub(4) as usize);
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
