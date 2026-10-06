//! The session tree on a canvas: one card per turn, laid out from the top down.
//!
//! The first prompt is at the top and the conversation runs down. Where it
//! branches, the branches open side by side under the turn they grew from.
//! Drag the background to pan, scroll to zoom, click a card to read it, and
//! right-click a card for what can be done there.
//!
//! The canvas draws a [`TreeView`] and nothing else, so the coding agent and
//! an alate, which gets its tree over a socket, draw it the same way. What a
//! person asks for comes back as a [`TreeEvent`] for the view that holds it.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    Bounds, Context, EventEmitter, FontWeight, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, PathBuilder, Pixels, Point, Render, ScrollWheelEvent, SharedString, Window,
    canvas, div, point, prelude::*, px, rgb, rgba,
};
use gpui_component::Disableable;
use gpui_component::button::{Button, ButtonVariants};

use super::theme::{ACCENT, BACKGROUND, BORDER, MUTED, PANEL, PANEL_RAISED, TEXT};
use crate::session::{TreeView, Turn};

/// A card's size and the room between cards, before zoom.
const CARD_WIDTH: f32 = 250.;
const CARD_HEIGHT: f32 = 84.;
const GAP_X: f32 = 36.;
const GAP_Y: f32 = 40.;
const MARGIN: f32 = 40.;
/// How far the zoom goes either way.
const ZOOM_MIN: f32 = 0.3;
const ZOOM_MAX: f32 = 2.0;

/// What a person asked for on the canvas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeEvent {
    /// A card was chosen, or nothing was: show what it holds.
    Selected(Option<String>),
    /// Continue the newest branch under this turn.
    Jump(String),
    /// Start a branch at this message: a prompt to edit it, or an answer to
    /// continue after it.
    Fork(String),
    /// Name the branch that holds this turn.
    Rename(String),
}

/// The full text of the selected turn, when the host can read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    pub prompt: String,
    pub reply: String,
}

/// Where each turn sits, in canvas units before zoom.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// The top-left corner of each turn's card, by index into the view.
    pub cards: Vec<(f32, f32)>,
    /// Parent and child, by index.
    pub edges: Vec<(usize, usize)>,
    pub width: f32,
    pub height: f32,
}

/// Lay a tree out from the top down.
///
/// Each leaf gets a column of its own, in the order the turns were written; a
/// turn with children sits over the middle of them. So a conversation with no
/// branches is one column, and every branch adds one.
#[must_use]
pub fn layout(view: &TreeView) -> Layout {
    let count = view.turns.len();
    let index: HashMap<&str, usize> = view
        .turns
        .iter()
        .enumerate()
        .map(|(position, turn)| (turn.id.as_str(), position))
        .collect();
    let parent: Vec<Option<usize>> = view
        .turns
        .iter()
        .map(|turn| turn.parent.as_deref().and_then(|id| index.get(id).copied()))
        .collect();
    let mut children = vec![Vec::new(); count];
    let mut roots = Vec::new();
    for (child, parent) in parent.iter().enumerate() {
        match parent {
            Some(parent) => children[*parent].push(child),
            None => roots.push(child),
        }
    }

    // Pre-order, with a stack rather than recursion: a long conversation is a
    // deep tree.
    let mut order = Vec::with_capacity(count);
    let mut depth = vec![0usize; count];
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(turn) = stack.pop() {
        order.push(turn);
        for child in children[turn].iter().rev() {
            depth[*child] = depth[turn] + 1;
            stack.push(*child);
        }
    }

    let mut column = vec![0f32; count];
    let mut next = 0f32;
    for turn in &order {
        if children[*turn].is_empty() {
            column[*turn] = next;
            next += 1.;
        }
    }
    for turn in order.iter().rev() {
        if let (Some(first), Some(last)) = (children[*turn].first(), children[*turn].last()) {
            column[*turn] = (column[*first] + column[*last]) / 2.;
        }
    }

    let cards: Vec<(f32, f32)> = (0..count)
        .map(|turn| {
            (
                MARGIN + column[turn] * (CARD_WIDTH + GAP_X),
                MARGIN + depth[turn] as f32 * (CARD_HEIGHT + GAP_Y),
            )
        })
        .collect();
    let edges = parent
        .iter()
        .enumerate()
        .filter_map(|(child, parent)| parent.map(|parent| (parent, child)))
        .collect();
    let deepest = depth.iter().copied().max().unwrap_or(0) as f32;
    Layout {
        cards,
        edges,
        width: 2. * MARGIN + next.max(1.) * (CARD_WIDTH + GAP_X) - GAP_X,
        height: 2. * MARGIN + (deepest + 1.) * (CARD_HEIGHT + GAP_Y) - GAP_Y,
    }
}

/// The canvas, its camera, and what is selected on it.
pub struct TreeCanvas {
    view: Option<TreeView>,
    layout: Layout,
    /// Where the canvas origin is drawn, relative to the element.
    offset: Point<f32>,
    zoom: f32,
    selected: Option<String>,
    preview: Option<Preview>,
    /// The card a context menu is open on, and where.
    menu: Option<(String, Point<f32>)>,
    /// The last pointer position of a drag of the background.
    dragging: Option<Point<Pixels>>,
    /// Center on the head at the next frame, once the size is known.
    center: bool,
    /// A run is in flight: looking is fine, moving the session is not.
    running: bool,
    /// The element's bounds at the last frame, for zooming at the pointer and
    /// centering.
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl EventEmitter<TreeEvent> for TreeCanvas {}

impl Default for TreeCanvas {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeCanvas {
    #[must_use]
    pub fn new() -> Self {
        Self {
            view: None,
            layout: Layout {
                cards: Vec::new(),
                edges: Vec::new(),
                width: 0.,
                height: 0.,
            },
            offset: point(0., 0.),
            zoom: 1.,
            selected: None,
            preview: None,
            menu: None,
            dragging: None,
            center: true,
            running: false,
            bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }

    /// Draw `view`. A different session than the one shown is centered on its
    /// head; the same one keeps the camera where it is.
    pub fn set_view(&mut self, view: Option<TreeView>, running: bool, cx: &mut Context<Self>) {
        let same =
            matches!((&self.view, &view), (Some(old), Some(new)) if old.session == new.session);
        if !same {
            self.center = true;
            self.selected = None;
            self.preview = None;
            self.menu = None;
        }
        self.layout = view.as_ref().map_or_else(
            || Layout {
                cards: Vec::new(),
                edges: Vec::new(),
                width: 0.,
                height: 0.,
            },
            layout,
        );
        self.view = view;
        self.running = running;
        cx.notify();
    }

    /// What the host read for the selected turn.
    pub fn set_preview(&mut self, preview: Option<Preview>, cx: &mut Context<Self>) {
        self.preview = preview;
        cx.notify();
    }

    /// Put the head in the middle of the canvas.
    pub fn center_on_head(&mut self, cx: &mut Context<Self>) {
        self.center = true;
        cx.notify();
    }

    fn turn(&self, id: &str) -> Option<&Turn> {
        self.view.as_ref()?.get(id)
    }

    fn apply_center(&mut self) {
        let bounds = self.bounds.get();
        let width: f32 = bounds.size.width.into();
        let height: f32 = bounds.size.height.into();
        if width <= 0. {
            return;
        }
        self.center = false;
        let Some(view) = &self.view else { return };
        let target = view
            .turns
            .iter()
            .position(|turn| turn.is_head)
            .or_else(|| view.turns.len().checked_sub(1));
        let Some((x, y)) = target.and_then(|turn| self.layout.cards.get(turn).copied()) else {
            self.offset = point(0., 0.);
            return;
        };
        self.offset = point(
            width / 2. - (x + CARD_WIDTH / 2.) * self.zoom,
            height / 2. - (y + CARD_HEIGHT / 2.) * self.zoom,
        );
    }

    fn on_background_down(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = Some(event.position);
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn on_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.dragging else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = None;
            return;
        }
        let delta = event.position - last;
        self.offset.x += f32::from(delta.x);
        self.offset.y += f32::from(delta.y);
        self.dragging = Some(event.position);
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta: f32 = event.delta.pixel_delta(px(20.)).y.into();
        if delta == 0. {
            return;
        }
        let zoom = (self.zoom * (1. + delta / 400.)).clamp(ZOOM_MIN, ZOOM_MAX);
        // Zoom about the pointer: the point under it stays under it.
        let origin = self.bounds.get().origin;
        let at = point(
            f32::from(event.position.x - origin.x),
            f32::from(event.position.y - origin.y),
        );
        let scale = zoom / self.zoom;
        self.offset = point(
            at.x - (at.x - self.offset.x) * scale,
            at.y - (at.y - self.offset.y) * scale,
        );
        self.zoom = zoom;
        cx.notify();
    }

    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_owned();
        self.menu = None;
        if self.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        self.selected = Some(id.clone());
        self.preview = None;
        cx.emit(TreeEvent::Selected(Some(id)));
        cx.notify();
    }

    fn act(&mut self, event: TreeEvent, cx: &mut Context<Self>) {
        self.menu = None;
        if !self.running {
            cx.emit(event);
        }
        cx.notify();
    }

    fn render_card(&self, index: usize, turn: &Turn, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (x, y) = self.layout.cards[index];
        let zoom = self.zoom;
        let selected = self.selected.as_deref() == Some(turn.id.as_str());
        let border = if turn.running {
            rgb(0x9a8038)
        } else if turn.is_head || selected {
            rgb(ACCENT)
        } else {
            rgb(BORDER)
        };
        let id = turn.id.clone();
        let menu_id = turn.id.clone();
        let tools = if turn.tool_calls > 0 {
            format!("{} tools · ", turn.tool_calls)
        } else {
            String::new()
        };
        div()
            .id(SharedString::from(format!("turn-{}", turn.id)))
            .absolute()
            .left(px(self.offset.x + x * zoom))
            .top(px(self.offset.y + y * zoom))
            .w(px(CARD_WIDTH * zoom))
            .h(px(CARD_HEIGHT * zoom))
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(if selected {
                rgb(PANEL_RAISED)
            } else {
                rgb(PANEL)
            })
            .px(px(10. * zoom))
            .py(px(6. * zoom))
            .text_size(px(12. * zoom))
            .cursor_pointer()
            // A press on a card is not the start of a pan.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| this.select(&id, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let origin = this.bounds.get().origin;
                    this.menu = Some((
                        menu_id.clone(),
                        point(
                            f32::from(event.position.x - origin.x),
                            f32::from(event.position.y - origin.y),
                        ),
                    ));
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4. * zoom))
                    .text_color(if turn.on_head { rgb(TEXT) } else { rgb(MUTED) })
                    .font_weight(if turn.label.is_some() {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(if turn.running {
                        "◌"
                    } else if turn.is_head {
                        "●"
                    } else {
                        ""
                    })
                    .child(TreeView::name(turn).to_owned()),
            )
            .child(
                div()
                    .mt(px(4. * zoom))
                    .text_color(rgb(MUTED))
                    .child(format!("{tools}{}", turn.reply)),
            )
            .into_any_element()
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (id, at) = self.menu.as_ref()?;
        let turn = self.turn(id)?;
        let running = self.running;
        let jump = turn.id.clone();
        let edit = turn.id.clone();
        let rename = turn.id.clone();
        let after = turn.end.clone();
        let mut menu = div()
            .absolute()
            .left(px(at.x))
            .top(px(at.y))
            .w(px(220.))
            .flex()
            .flex_col()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_RAISED))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("tree-jump")
                    .ghost()
                    .w_full()
                    .label("Jump here")
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(TreeEvent::Jump(jump.clone()), cx);
                    })),
            );
        if let Some(after) = after {
            menu = menu.child(
                Button::new("tree-fork")
                    .ghost()
                    .w_full()
                    .label("Fork after this answer")
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(TreeEvent::Fork(after.clone()), cx);
                    })),
            );
        }
        menu = menu
            .child(
                Button::new("tree-edit")
                    .ghost()
                    .w_full()
                    .label("Edit and resend")
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(TreeEvent::Fork(edit.clone()), cx);
                    })),
            )
            .child(
                Button::new("tree-rename")
                    .ghost()
                    .w_full()
                    .label("Rename branch")
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(TreeEvent::Rename(rename.clone()), cx);
                    })),
            );
        Some(menu.into_any_element())
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let id = self.selected.as_ref()?;
        let turn = self.turn(id)?;
        let (prompt, reply) = match &self.preview {
            Some(preview) => (preview.prompt.clone(), preview.reply.clone()),
            None => (turn.prompt.clone(), turn.reply.clone()),
        };
        let jump = turn.id.clone();
        let started = turn
            .ts
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        Some(
            div()
                .id("tree-preview")
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(360.))
                .overflow_scroll()
                .bg(rgb(PANEL))
                .border_l_1()
                .border_color(rgb(BORDER))
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().text_xs().text_color(rgb(MUTED)).child(started))
                .children(
                    turn.label
                        .clone()
                        .map(|label| div().font_weight(FontWeight::BOLD).child(label)),
                )
                .child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(PANEL_RAISED))
                        .whitespace_normal()
                        .child(prompt),
                )
                .child(div().text_sm().whitespace_normal().child(reply))
                .child(
                    Button::new("tree-preview-jump")
                        .primary()
                        .label(if self.running {
                            "A run is going"
                        } else {
                            "Continue this branch"
                        })
                        .disabled(self.running)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.act(TreeEvent::Jump(jump.clone()), cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

impl Render for TreeCanvas {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.center {
            self.apply_center();
        }

        let zoom = self.zoom;
        let offset = self.offset;
        let segments: Vec<[(f32, f32); 4]> = self
            .layout
            .edges
            .iter()
            .map(|(parent, child)| {
                let (px_, py) = self.layout.cards[*parent];
                let (cx_, cy) = self.layout.cards[*child];
                let from = (px_ + CARD_WIDTH / 2., py + CARD_HEIGHT);
                let to = (cx_ + CARD_WIDTH / 2., cy);
                let middle = from.1 + GAP_Y / 2.;
                [from, (from.0, middle), (to.0, middle), to]
                    .map(|(x, y)| (offset.x + x * zoom, offset.y + y * zoom))
            })
            .collect();
        let bounds = Rc::clone(&self.bounds);
        let edges = canvas(
            move |area, _, _| bounds.set(area),
            move |area, (), window, _| {
                for segment in &segments {
                    let mut path = PathBuilder::stroke(px(1.5));
                    let at = |(x, y): (f32, f32)| area.origin + point(px(x), px(y));
                    path.move_to(at(segment[0]));
                    for corner in &segment[1..] {
                        path.line_to(at(*corner));
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(BORDER));
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let cards: Vec<gpui::AnyElement> = match &self.view {
            Some(view) => view
                .turns
                .iter()
                .enumerate()
                .map(|(index, turn)| self.render_card(index, turn, cx))
                .collect(),
            None => Vec::new(),
        };
        let empty = cards.is_empty();

        let mut root = div()
            .id("tree-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_background_down))
            .on_mouse_move(cx.listener(Self::on_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = None),
            )
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(edges)
            .children(cards);
        if empty {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(MUTED))
                    .child("No prompts yet. The tree grows as you talk."),
            );
        }
        root = root.child(
            div()
                .absolute()
                .bottom_3()
                .left_3()
                .flex()
                .gap_2()
                .p_1()
                .rounded_md()
                .bg(rgba(0x171c22cc))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    Button::new("tree-center")
                        .ghost()
                        .label("◎ Head")
                        .on_click(cx.listener(|this, _, _, cx| this.center_on_head(cx))),
                )
                .child(
                    Button::new("tree-zoom-out")
                        .ghost()
                        .label("−")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.zoom = (this.zoom / 1.2).max(ZOOM_MIN);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("tree-zoom-in")
                        .ghost()
                        .label("+")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.zoom = (this.zoom * 1.2).min(ZOOM_MAX);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(if self.running {
                            "a run is going: you can look, not move"
                        } else {
                            "right-click a turn to jump or fork"
                        }),
                ),
        );
        if let Some(preview) = self.render_preview(cx) {
            root = root.child(preview);
        }
        if let Some(menu) = self.render_menu(cx) {
            root = root.child(menu);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, parent: Option<&str>) -> Turn {
        Turn {
            id: id.to_owned(),
            parent: parent.map(ToOwned::to_owned),
            prompt: id.to_owned(),
            reply: String::new(),
            tool_calls: 0,
            end: None,
            label: None,
            ts: chrono::Utc::now(),
            on_head: false,
            is_head: false,
            running: false,
        }
    }

    fn view(turns: Vec<Turn>) -> TreeView {
        TreeView {
            session: "s".to_owned(),
            title: "t".to_owned(),
            started: chrono::Utc::now(),
            head: None,
            turns,
        }
    }

    #[test]
    fn a_line_is_one_column_going_down() {
        let laid = layout(&view(vec![
            turn("a", None),
            turn("b", Some("a")),
            turn("c", Some("b")),
        ]));
        let xs: Vec<f32> = laid.cards.iter().map(|card| card.0).collect();
        assert!(xs.iter().all(|x| *x == xs[0]));
        assert!(laid.cards[0].1 < laid.cards[1].1 && laid.cards[1].1 < laid.cards[2].1);
        assert_eq!(laid.edges, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn a_parent_sits_over_the_middle_of_its_branches() {
        let laid = layout(&view(vec![
            turn("a", None),
            turn("b", Some("a")),
            turn("c", Some("a")),
        ]));
        let (a, b, c) = (laid.cards[0].0, laid.cards[1].0, laid.cards[2].0);
        assert!(b < c);
        assert!((a - (b + c) / 2.).abs() < f32::EPSILON);
        assert_eq!(laid.cards[1].1, laid.cards[2].1, "siblings share a row");
    }
}
