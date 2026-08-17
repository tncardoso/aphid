//! The layer that renders interactive plugin surfaces.
//!
//! The plugin host owns the widget tree; this module owns the TUI-side cache,
//! focus and hit-testing. A surface is open when its `render` returns a widget
//! tree, and closed when it returns unit. Rendering is cached by the plugin's
//! state version, then re-run after events and on the tick.

use std::collections::HashMap;
use std::sync::Arc;

use aphid_plugin::{Placement, PluginHost, Side, SurfaceRender, Widget};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

/// A surface is `plugin_name/surface_name`.
pub type SurfaceKey = (String, String);

/// The width a side column asks for.
const SURFACE_WIDTH: u16 = 40;

/// What the panels look like, as the model holds them.
#[derive(Default)]
pub struct SurfaceLayer {
    left: Vec<Pane>,
    right: Vec<Pane>,
    focus: Option<SurfaceKey>,
    /// The clickable regions the last draw reported back.
    hits: Vec<Hit>,
}

/// One open panel: a finished widget tree and what to say about it.
///
/// Plain data. It is made by running a plugin's `render`, which happens on the
/// executor's side of the line, and arrives here as a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub key: SurfaceKey,
    pub title: String,
    pub interactive: bool,
    pub widget: Widget,
}

/// The panels as a whole, left column and right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panes {
    pub left: Vec<Pane>,
    pub right: Vec<Pane>,
}

/// Renders the panels by asking the plugins. The executor's half.
///
/// Keeps what it last rendered against the plugin's state version, so a panel
/// whose plugin has not moved is not re-rendered.
#[derive(Default)]
pub struct SurfaceSource {
    cache: HashMap<SurfaceKey, Cached>,
}

struct Cached {
    version: u64,
    view: Option<Pane>,
}

/// One clickable region a panel drew.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    surface: SurfaceKey,
    target: Option<String>,
    interactive: bool,
    area: Rect,
}

impl SurfaceSource {
    /// Ask the host for the open surfaces and their widget trees.
    ///
    /// Runs plugin `render` functions on the calling thread. They are expected
    /// to be cheap, the same bargain slash commands make.
    #[must_use]
    pub fn refresh(&mut self, host: &Arc<PluginHost>) -> Panes {
        let mut left = Vec::new();
        let mut right = Vec::new();

        for surface in host.surfaces() {
            let Placement::Side(side) = surface.placement;
            let key = (surface.plugin.clone(), surface.name.clone());
            let version = host.state_version(&surface.plugin).unwrap_or(0);

            let stale = match self.cache.get(&key) {
                None => true,
                Some(cached) => cached.version != version,
            };
            if stale {
                let view = render_view(host, &surface);
                self.cache.insert(key.clone(), Cached { version, view });
            }

            let Some(view) = self.cache.get(&key).and_then(|cached| cached.view.clone()) else {
                continue;
            };
            match side {
                Side::Left => left.push(view),
                Side::Right => right.push(view),
            }
        }

        Panes { left, right }
    }
}

impl SurfaceLayer {
    /// Take on the panels a refresh produced.
    pub fn show(&mut self, panes: Panes) {
        self.left = panes.left;
        self.right = panes.right;

        // A panel that closed, or stopped listening, cannot keep the focus.
        if self
            .focus
            .as_ref()
            .is_some_and(|focus| !self.is_open_interactive(focus))
        {
            self.focus = None;
        }
    }

    /// Whether any surface is open.
    #[must_use]
    pub fn any_open(&self) -> bool {
        !self.left.is_empty() || !self.right.is_empty()
    }

    /// Whether any open surface can take focus.
    #[must_use]
    pub fn has_focusable(&self) -> bool {
        self.left
            .iter()
            .chain(self.right.iter())
            .any(|view| view.interactive)
    }

    /// The focused surface, if any.
    #[must_use]
    pub fn focus(&self) -> Option<SurfaceKey> {
        self.focus.clone()
    }

    /// Give focus to the first open interactive surface.
    pub fn focus_first(&mut self) {
        self.focus = self.interactive_keys().into_iter().next();
    }

    /// Move focus to the next open interactive surface, wrapping around.
    pub fn cycle_focus(&mut self) {
        let keys = self.interactive_keys();
        if keys.is_empty() {
            self.focus = None;
            return;
        }

        let next = match self.focus.as_ref() {
            Some(current) => keys
                .iter()
                .position(|key| key == current)
                .map_or(0, |index| (index + 1) % keys.len()),
            None => 0,
        };
        self.focus = Some(keys[next].clone());
    }

    /// Return focus to the input box.
    pub fn release_focus(&mut self) {
        self.focus = None;
    }

    /// The open interactive surface under a terminal cell, if any.
    #[must_use]
    pub fn hit(&self, column: u16, row: u16) -> Option<(SurfaceKey, Option<String>)> {
        self.hits.iter().find_map(|hit| {
            let inside = hit.interactive
                && column >= hit.area.x
                && column < hit.area.x.saturating_add(hit.area.width)
                && row >= hit.area.y
                && row < hit.area.y.saturating_add(hit.area.height);
            inside.then(|| (hit.surface.clone(), hit.target.clone()))
        })
    }

    /// Focus the open interactive surface under a terminal cell, if any.
    /// Returns the surface and the widget id under the cursor.
    #[must_use]
    pub fn click(&mut self, column: u16, row: u16) -> Option<(SurfaceKey, Option<String>)> {
        let hit = self.hit(column, row)?;
        self.focus = Some(hit.0.clone());
        Some(hit)
    }

    /// Take on what the last draw laid out.
    pub fn laid_out(&mut self, hits: Vec<Hit>) {
        self.hits = hits;
    }

    /// Draw the side columns inside the transcript area, and return the area
    /// left for the transcript itself.
    ///
    /// Collects the clickable regions into `hits` rather than keeping them:
    /// what a click lands on is the model's business, and it hears about it
    /// as a message like everything else.
    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, hits: &mut Vec<Hit>) -> Rect {
        let left_width = self.side_width(area.width, !self.left.is_empty());
        let right_width = self.side_width(area.width, !self.right.is_empty());

        if left_width == 0 && right_width == 0 {
            return area;
        }

        let mut constraints = Vec::new();
        if left_width > 0 {
            constraints.push(Constraint::Length(left_width));
        }
        constraints.push(Constraint::Min(1));
        if right_width > 0 {
            constraints.push(Constraint::Length(right_width));
        }

        let cells = Layout::horizontal(constraints).split(area);
        let mut index = 0;

        if left_width > 0 {
            self.render_side(frame, cells[index], Side::Left, hits);
            index += 1;
        }
        let main = cells[index];
        index += 1;
        if right_width > 0 {
            self.render_side(frame, cells[index], Side::Right, hits);
        }

        main
    }

    fn side_width(&self, terminal: u16, open: bool) -> u16 {
        if !open {
            return 0;
        }
        SURFACE_WIDTH.min(terminal / 3).max(1)
    }

    fn render_side(&self, frame: &mut Frame<'_>, column: Rect, side: Side, hits: &mut Vec<Hit>) {
        let views: &[Pane] = match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        };
        if views.is_empty() {
            return;
        }

        let constraints = vec![Constraint::Ratio(1, views.len() as u32); views.len()];
        let cells = Layout::vertical(constraints).split(column);

        for (view, cell) in views.iter().zip(cells.iter()) {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", view.title))
                .border_style(Style::default().fg(Color::DarkGray));
            let inner = block.inner(*cell);
            frame.render_widget(block, *cell);

            render_widget(
                frame,
                &view.widget,
                inner,
                &view.key,
                view.interactive,
                hits,
            );
        }
    }

    fn interactive_keys(&self) -> Vec<SurfaceKey> {
        self.left
            .iter()
            .chain(self.right.iter())
            .filter(|view| view.interactive)
            .map(|view| view.key.clone())
            .collect()
    }

    fn is_open_interactive(&self, key: &SurfaceKey) -> bool {
        self.left
            .iter()
            .chain(self.right.iter())
            .any(|view| view.interactive && view.key == *key)
    }
}

fn render_view(host: &Arc<PluginHost>, surface: &aphid_plugin::RegisteredSurface) -> Option<Pane> {
    match host.render_surface(&surface.plugin, &surface.name) {
        Some(SurfaceRender::Widget(widget)) => Some(Pane {
            key: (surface.plugin.clone(), surface.name.clone()),
            title: surface.name.clone(),
            interactive: surface.interactive,
            widget,
        }),
        Some(SurfaceRender::Closed) | None => None,
        Some(SurfaceRender::Failed(error)) => Some(Pane {
            key: (surface.plugin.clone(), surface.name.clone()),
            title: surface.name.clone(),
            interactive: surface.interactive,
            widget: Widget::Text {
                id: None,
                text: format!("plugin error: {error}"),
            },
        }),
    }
}

fn render_widget(
    frame: &mut Frame<'_>,
    widget: &Widget,
    area: Rect,
    surface: &SurfaceKey,
    interactive: bool,
    hits: &mut Vec<Hit>,
) {
    match widget {
        Widget::Rows { children } | Widget::Cols { children } => {
            if children.is_empty() {
                return;
            }
            let constraints = vec![Constraint::Ratio(1, children.len() as u32); children.len()];
            let cells = if matches!(widget, Widget::Rows { .. }) {
                Layout::vertical(constraints).split(area)
            } else {
                Layout::horizontal(constraints).split(area)
            };
            for (child, cell) in children.iter().zip(cells.iter()) {
                render_widget(frame, child, *cell, surface, interactive, hits);
            }
        }
        Widget::Text { id, text } => {
            frame.render_widget(
                Paragraph::new(text.as_str()).wrap(Wrap { trim: false }),
                area,
            );
            push_hit(id, area, surface, interactive, hits);
        }
        Widget::List {
            id,
            items,
            selected,
        } => {
            let list = List::new(
                items
                    .iter()
                    .map(|item| ListItem::new(Line::from(item.as_str())))
                    .collect::<Vec<_>>(),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));
            let mut state = ListState::default();
            state.select((*selected < items.len()).then_some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
            push_hit(id, area, surface, interactive, hits);
        }
        Widget::Input {
            id,
            text,
            placeholder,
        } => {
            let shown = if text.is_empty() { placeholder } else { text };
            frame.render_widget(
                Paragraph::new(shown.as_str()).block(Block::default().borders(Borders::ALL)),
                area,
            );
            push_hit(id, area, surface, interactive, hits);
        }
        Widget::Button { id, label } => {
            frame.render_widget(
                Paragraph::new(label.as_str())
                    .block(Block::default().borders(Borders::ALL))
                    .style(Style::default().bg(Color::DarkGray)),
                area,
            );
            push_hit(id, area, surface, interactive, hits);
        }
        Widget::Spacer => {}
    }
}

fn push_hit(
    id: &Option<String>,
    area: Rect,
    surface: &SurfaceKey,
    interactive: bool,
    hits: &mut Vec<Hit>,
) {
    if let Some(target) = id {
        hits.push(Hit {
            surface: surface.clone(),
            target: Some(target.clone()),
            interactive,
            area,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_width_uses_a_third_capped_at_forty() {
        let layer = SurfaceLayer::default();
        assert_eq!(layer.side_width(120, true), 40);
        assert_eq!(layer.side_width(60, true), 20);
        assert_eq!(layer.side_width(9, true), 3);
        assert_eq!(layer.side_width(120, false), 0);
    }
}
