//! The session tree: every session of the workspace, and the branches of each.
//!
//! One row per session, and under an open session one row per prompt. A
//! conversation that never branched is a flat list; where it branches, each
//! branch is drawn indented under the turn it grew from. The answer to each
//! prompt is shown dimmed after it, and `●` marks where the session continues.
//!
//! The widget only decides. What a key asks for comes back as a [`TreeAction`]
//! for whoever holds it — the coding terminal, or an alate's — to carry out.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::session::{TreeView, Turn};
use crate::tui::modal::centred;
use crate::tui::scrollback::one_line;

/// One session, as the tree draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeSession {
    /// Where it is: a file for the coding agent, an id for an alate.
    pub path: PathBuf,
    pub view: TreeView,
    pub open: bool,
}

/// What a key asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    None,
    Close,
    /// Continue a session at its head.
    Open {
        path: PathBuf,
    },
    /// Continue the newest branch under a turn.
    Jump {
        path: PathBuf,
        node: String,
    },
    /// Start a branch: at a prompt to edit it, or after an answer.
    Fork {
        path: PathBuf,
        node: String,
    },
    /// Name a branch: the one that holds the turn `node`, when the cursor is
    /// on one.
    Rename {
        path: PathBuf,
        node: Option<String>,
    },
}

#[derive(Clone, Debug)]
enum Kind {
    Session(usize),
    Turn(usize, usize),
}

#[derive(Clone, Debug)]
struct Row {
    kind: Kind,
    /// The branch lines drawn before the text.
    prefix: String,
}

/// The tree, and the cursor in it.
#[derive(Clone, Debug)]
pub struct SessionTree {
    sessions: Vec<TreeSession>,
    rows: Vec<Row>,
    selected: usize,
    /// The filter while one is typed: `/` starts it.
    filter: Option<String>,
    /// A run is in flight: moving the session is refused, looking is not.
    pub running: bool,
}

impl SessionTree {
    /// A tree over `sessions`, with the one at `current` open and the cursor
    /// on where it continues.
    #[must_use]
    pub fn new(mut sessions: Vec<TreeSession>, current: Option<&std::path::Path>) -> Self {
        for session in &mut sessions {
            session.open |= current.is_some_and(|current| session.path == current);
        }
        let mut tree = Self {
            sessions,
            rows: Vec::new(),
            selected: 0,
            filter: None,
            running: false,
        };
        tree.layout();
        tree.selected = tree
            .rows
            .iter()
            .position(|row| match row.kind {
                Kind::Turn(session, turn) => {
                    current.is_some_and(|current| tree.sessions[session].path == current)
                        && tree.sessions[session].view.turns[turn].is_head
                }
                Kind::Session(_) => false,
            })
            .or_else(|| {
                tree.rows.iter().position(|row| match row.kind {
                    Kind::Session(session) => {
                        current.is_some_and(|current| tree.sessions[session].path == current)
                    }
                    Kind::Turn(..) => false,
                })
            })
            .unwrap_or(0);
        tree
    }

    /// Replace what is shown, keeping the cursor on the same row if it is
    /// still there. A view that updates while a run goes on calls this.
    pub fn update(&mut self, sessions: Vec<TreeSession>) {
        let before = self.selected_key();
        let open: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|session| session.open)
            .map(|session| session.path.clone())
            .collect();
        self.sessions = sessions;
        for session in &mut self.sessions {
            session.open |= open.contains(&session.path);
        }
        self.layout();
        if let Some(before) = before
            && let Some(index) = (0..self.rows.len()).find(|index| self.key(*index) == before)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    /// The sessions shown.
    #[must_use]
    pub fn sessions(&self) -> &[TreeSession] {
        &self.sessions
    }

    fn key(&self, index: usize) -> (PathBuf, Option<String>) {
        match self.rows[index].kind {
            Kind::Session(session) => (self.sessions[session].path.clone(), None),
            Kind::Turn(session, turn) => (
                self.sessions[session].path.clone(),
                Some(self.sessions[session].view.turns[turn].id.clone()),
            ),
        }
    }

    fn selected_key(&self) -> Option<(PathBuf, Option<String>)> {
        (self.selected < self.rows.len()).then(|| self.key(self.selected))
    }

    /// Flatten what is open into rows.
    fn layout(&mut self) {
        let mut rows = Vec::new();
        let filter = self
            .filter
            .as_deref()
            .filter(|filter| !filter.is_empty())
            .map(str::to_lowercase);

        for (index, session) in self.sessions.iter().enumerate() {
            if let Some(filter) = &filter {
                let title_hit = session.view.title.to_lowercase().contains(filter.as_str());
                let hits: Vec<usize> = (0..session.view.turns.len())
                    .filter(|turn| {
                        let turn = &session.view.turns[*turn];
                        turn.prompt.to_lowercase().contains(filter.as_str())
                            || turn.reply.to_lowercase().contains(filter.as_str())
                            || turn
                                .label
                                .as_deref()
                                .is_some_and(|label| label.to_lowercase().contains(filter.as_str()))
                    })
                    .collect();
                if !title_hit && hits.is_empty() {
                    continue;
                }
                rows.push(Row {
                    kind: Kind::Session(index),
                    prefix: String::new(),
                });
                for turn in hits {
                    rows.push(Row {
                        kind: Kind::Turn(index, turn),
                        prefix: "  ".to_owned(),
                    });
                }
                continue;
            }

            rows.push(Row {
                kind: Kind::Session(index),
                prefix: String::new(),
            });
            if session.open {
                let view = &session.view;
                let roots: Vec<usize> = children(view, None);
                emit_all(view, index, &roots, "  ", &mut rows);
            }
        }
        self.rows = rows;
    }

    /// Handle one key.
    pub fn handle(&mut self, key: KeyEvent) -> TreeAction {
        if let Some(filter) = self.filter.as_ref() {
            let empty = filter.is_empty();
            match key.code {
                KeyCode::Esc => {
                    self.filter = None;
                    self.layout();
                }
                // Enter on what the filter kept acts on it; on nothing typed,
                // it just leaves the filter.
                KeyCode::Enter if empty => {
                    self.filter = None;
                    self.layout();
                }
                KeyCode::Enter | KeyCode::Up | KeyCode::Down => return self.act(key.code),
                KeyCode::Backspace => {
                    if empty {
                        self.filter = None;
                    } else if let Some(filter) = self.filter.as_mut() {
                        filter.pop();
                    }
                    self.layout();
                    self.selected = 0;
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(filter) = self.filter.as_mut() {
                        filter.push(ch);
                    }
                    self.layout();
                    self.selected = 0;
                }
                _ => {}
            }
            return TreeAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return TreeAction::Close;
        }
        self.act(key.code)
    }

    fn act(&mut self, code: KeyCode) -> TreeAction {
        let len = self.rows.len();
        match code {
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                self.selected = (self.selected + len - 1) % len;
            }
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                self.selected = (self.selected + 1) % len;
            }
            KeyCode::Right | KeyCode::Char('l') => self.set_open(true),
            KeyCode::Left | KeyCode::Char('h') => self.set_open(false),
            KeyCode::Char('/') => {
                self.filter = Some(String::new());
            }
            KeyCode::Esc | KeyCode::Char('q') => return TreeAction::Close,
            KeyCode::Enter => {
                return match self.rows.get(self.selected).map(|row| row.kind.clone()) {
                    Some(Kind::Session(session)) => TreeAction::Open {
                        path: self.sessions[session].path.clone(),
                    },
                    Some(Kind::Turn(session, turn)) => TreeAction::Jump {
                        path: self.sessions[session].path.clone(),
                        node: self.sessions[session].view.turns[turn].id.clone(),
                    },
                    None => TreeAction::None,
                };
            }
            KeyCode::Char('e') => {
                if let Some((path, turn)) = self.selected_turn() {
                    return TreeAction::Fork {
                        path,
                        node: turn.id.clone(),
                    };
                }
            }
            KeyCode::Char('f') => {
                if let Some((path, turn)) = self.selected_turn()
                    && let Some(end) = &turn.end
                {
                    return TreeAction::Fork {
                        path,
                        node: end.clone(),
                    };
                }
            }
            KeyCode::Char('r') => {
                if let Some(row) = self.rows.get(self.selected) {
                    let (Kind::Session(session) | Kind::Turn(session, _)) = row.kind;
                    let node = match row.kind {
                        Kind::Turn(session, turn) => {
                            Some(self.sessions[session].view.turns[turn].id.clone())
                        }
                        Kind::Session(_) => None,
                    };
                    return TreeAction::Rename {
                        path: self.sessions[session].path.clone(),
                        node,
                    };
                }
            }
            _ => {}
        }
        TreeAction::None
    }

    fn selected_turn(&self) -> Option<(PathBuf, &Turn)> {
        match self.rows.get(self.selected)?.kind {
            Kind::Turn(session, turn) => Some((
                self.sessions[session].path.clone(),
                &self.sessions[session].view.turns[turn],
            )),
            Kind::Session(_) => None,
        }
    }

    fn set_open(&mut self, open: bool) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let (Kind::Session(session) | Kind::Turn(session, _)) = row.kind;
        if self.sessions[session].open == open {
            return;
        }
        self.sessions[session].open = open;
        self.layout();
        if !open {
            // The cursor goes back to the session row it was under.
            self.selected = self
                .rows
                .iter()
                .position(|row| matches!(row.kind, Kind::Session(index) if index == session))
                .unwrap_or(0);
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let width = area.width.saturating_sub(4).min(110);
        let height = area.height.saturating_sub(4).max(3);
        let room = (width as usize).saturating_sub(4);
        let visible = height.saturating_sub(2) as usize;

        let top = self
            .selected
            .saturating_sub(visible / 2)
            .min(self.rows.len().saturating_sub(visible));
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(filter) = &self.filter {
            lines.push(Line::from(vec![
                Span::styled("/ ", Style::default().fg(Color::Cyan)),
                Span::raw(filter.clone()),
                Span::styled("█", Style::default().fg(Color::Cyan)),
            ]));
        }
        if self.rows.is_empty() {
            lines.push(Line::styled(
                "  no sessions",
                Style::default().fg(Color::DarkGray),
            ));
        }
        let budget = visible.saturating_sub(lines.len());
        for index in top..(top + budget).min(self.rows.len()) {
            lines.push(self.draw(index, room));
        }

        let hint = if self.running {
            " sessions — a run is going, so you can look but not move "
        } else {
            " sessions — Enter jump · e edit prompt · f fork after · r rename · / filter · Esc close "
        };
        let cell = centred(area, width, (lines.len() as u16 + 2).min(height));
        frame.render_widget(Clear, cell);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(hint)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            cell,
        );
    }

    fn draw(&self, index: usize, room: usize) -> Line<'static> {
        let row = &self.rows[index];
        let chosen = index == self.selected;
        let highlight = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let grey = Style::default().fg(Color::DarkGray);

        match row.kind {
            Kind::Session(session) => {
                let session = &self.sessions[session];
                let arrow = if session.open { "▾" } else { "▸" };
                let started = session
                    .view
                    .started
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string();
                let branches = session
                    .view
                    .turns
                    .iter()
                    .filter(|turn| {
                        !session
                            .view
                            .turns
                            .iter()
                            .any(|other| other.parent.as_deref() == Some(turn.id.as_str()))
                    })
                    .count();
                let detail = format!("{started} · {branches} branches");
                let title = one_line(
                    &session.view.title,
                    room.saturating_sub(detail.chars().count() + 5),
                );
                let style = if chosen {
                    highlight
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(format!("{arrow} {title}"), style),
                    Span::styled(format!("  {detail}"), if chosen { highlight } else { grey }),
                ])
            }
            Kind::Turn(session, turn) => {
                let turn = &self.sessions[session].view.turns[turn];
                let mark = if turn.running {
                    "◌ "
                } else if turn.is_head {
                    "● "
                } else {
                    "  "
                };
                let name = TreeView::name(turn);
                let base = if chosen {
                    highlight
                } else if turn.on_head {
                    Style::default()
                } else {
                    Style::default().fg(Color::Gray)
                };
                let used = row.prefix.chars().count() + 2;
                let prompt = one_line(name, (room.saturating_sub(used) * 3 / 5).max(8));
                let mut spans = vec![
                    Span::styled(row.prefix.clone(), grey),
                    Span::styled(mark.to_owned(), Style::default().fg(Color::Cyan)),
                    Span::styled(prompt.clone(), base),
                ];
                let left = room.saturating_sub(used + prompt.chars().count() + 3);
                let mut reply = turn.reply.clone();
                if turn.tool_calls > 0 {
                    reply = format!("{} tools · {reply}", turn.tool_calls);
                }
                if left > 4 && !reply.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", one_line(&reply, left)),
                        if chosen { highlight } else { grey },
                    ));
                }
                Line::from(spans)
            }
        }
    }
}

fn children(view: &TreeView, parent: Option<&str>) -> Vec<usize> {
    (0..view.turns.len())
        .filter(|index| view.turns[*index].parent.as_deref() == parent)
        .collect()
}

/// Rows for `turns`, siblings under one parent: one is a continuation drawn
/// at the same depth, several are branches drawn one level in.
fn emit_all(view: &TreeView, session: usize, turns: &[usize], rest: &str, rows: &mut Vec<Row>) {
    if let [only] = turns {
        emit(view, session, *only, rest, rest, rows);
        return;
    }
    for (position, turn) in turns.iter().enumerate() {
        let last = position + 1 == turns.len();
        let first = format!("{rest}{}", if last { "└─ " } else { "├─ " });
        let next = format!("{rest}{}", if last { "   " } else { "│  " });
        emit(view, session, *turn, &first, &next, rows);
    }
}

fn emit(
    view: &TreeView,
    session: usize,
    turn: usize,
    first: &str,
    rest: &str,
    rows: &mut Vec<Row>,
) {
    // A chain is walked in a loop rather than by recursion, so a long
    // conversation does not use a stack frame per prompt.
    let mut turn = turn;
    let mut prefix = first.to_owned();
    loop {
        rows.push(Row {
            kind: Kind::Turn(session, turn),
            prefix: prefix.clone(),
        });
        let next = children(view, Some(&view.turns[turn].id));
        match next.as_slice() {
            [only] => {
                turn = *only;
                prefix = rest.to_owned();
            }
            [] => return,
            many => {
                emit_all(view, session, many, rest, rows);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, parent: Option<&str>, prompt: &str) -> Turn {
        Turn {
            id: id.to_owned(),
            parent: parent.map(ToOwned::to_owned),
            prompt: prompt.to_owned(),
            reply: format!("re {prompt}"),
            tool_calls: 0,
            end: Some(format!("{id}-end")),
            label: None,
            ts: chrono::Utc::now(),
            on_head: false,
            is_head: false,
            running: false,
        }
    }

    fn sample() -> SessionTree {
        let mut b2 = turn("b2", Some("a"), "second, again");
        b2.is_head = true;
        b2.on_head = true;
        let mut a = turn("a", None, "first");
        a.on_head = true;
        let view = TreeView {
            session: "s".to_owned(),
            title: "first".to_owned(),
            started: chrono::Utc::now(),
            head: Some("b2-end".to_owned()),
            turns: vec![
                a,
                turn("b1", Some("a"), "second"),
                turn("c1", Some("b1"), "third"),
                b2,
            ],
        };
        let other = TreeView {
            session: "t".to_owned(),
            title: "other".to_owned(),
            started: chrono::Utc::now(),
            head: None,
            turns: vec![turn("x", None, "elsewhere")],
        };
        SessionTree::new(
            vec![
                TreeSession {
                    path: "s".into(),
                    view,
                    open: false,
                },
                TreeSession {
                    path: "t".into(),
                    view: other,
                    open: false,
                },
            ],
            Some(std::path::Path::new("s")),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_current_session_opens_with_the_cursor_on_its_head() {
        let tree = sample();
        let prefixes: Vec<_> = tree.rows.iter().map(|row| row.prefix.as_str()).collect();
        assert_eq!(prefixes, ["", "  ", "  ├─ ", "  │  ", "  └─ ", ""]);
        assert_eq!(tree.selected, 4, "on b2");
    }

    #[test]
    fn keys_ask_for_jumps_forks_and_renames() {
        let mut tree = sample();
        assert_eq!(
            tree.handle(key(KeyCode::Enter)),
            TreeAction::Jump {
                path: "s".into(),
                node: "b2".to_owned()
            }
        );
        assert_eq!(
            tree.handle(key(KeyCode::Char('e'))),
            TreeAction::Fork {
                path: "s".into(),
                node: "b2".to_owned()
            }
        );
        assert_eq!(
            tree.handle(key(KeyCode::Char('f'))),
            TreeAction::Fork {
                path: "s".into(),
                node: "b2-end".to_owned()
            }
        );
        assert_eq!(
            tree.handle(key(KeyCode::Char('r'))),
            TreeAction::Rename {
                path: "s".into(),
                node: Some("b2".to_owned())
            }
        );
        tree.handle(key(KeyCode::Down));
        assert_eq!(
            tree.handle(key(KeyCode::Enter)),
            TreeAction::Open { path: "t".into() }
        );
    }

    #[test]
    fn a_session_opens_and_closes() {
        let mut tree = sample();
        tree.handle(key(KeyCode::Down));
        tree.handle(key(KeyCode::Right));
        assert_eq!(tree.rows.len(), 7);
        tree.handle(key(KeyCode::Left));
        assert_eq!(tree.rows.len(), 6);
        assert!(matches!(tree.rows[tree.selected].kind, Kind::Session(1)));
    }

    #[test]
    fn a_filter_keeps_the_matching_prompts() {
        let mut tree = sample();
        tree.handle(key(KeyCode::Char('/')));
        for ch in "third".chars() {
            tree.handle(key(KeyCode::Char(ch)));
        }
        assert_eq!(tree.rows.len(), 2, "the session and its one hit");
        tree.handle(key(KeyCode::Down));
        assert_eq!(
            tree.handle(key(KeyCode::Enter)),
            TreeAction::Jump {
                path: "s".into(),
                node: "c1".to_owned()
            }
        );
    }
}
