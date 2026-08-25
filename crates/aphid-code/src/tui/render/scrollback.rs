//! The transcript pane's line cache.
//!
//! Wrapping three hundred entries at every frame would be most of what the UI
//! does, so each entry's block is rendered once and kept. An entry carries a
//! revision that rises whenever it changes; the cache mirrors the revisions it
//! drew and re-renders only the ones that moved.
//!
//! A revision and not a staleness flag on purpose. A flag has to be cleared by
//! whoever drew it, which means drawing writes to the model — and this whole
//! module exists so that it does not.

use ratatui::text::Line;

use crate::tui::scrollback::{Scroll, Scrollback, Viewport, render_entry};
use crate::tui::select::{self, Spot};

/// What the last draw of the pane produced.
#[derive(Default)]
pub struct ScrollbackCache {
    /// Flattened rendered lines, in transcript order.
    lines: Vec<Line<'static>>,
    /// Where each entry's rendered block starts in [`Self::lines`].
    starts: Vec<usize>,
    /// The revision each cached block was rendered from.
    revs: Vec<u64>,
    /// The width and thinking state the blocks were wrapped for.
    width: usize,
    show_thinking: bool,
    /// How many evictions the cached blocks account for. When the pane has
    /// dropped more than this, the same prefix has to go from here.
    evicted: usize,
    /// Rises whenever every cached block was thrown away.
    ///
    /// A selection holds line numbers, and line numbers only mean something
    /// for as long as the lines they count do. This is how the model is told
    /// that they no longer do.
    generation: u64,
    /// The first line the last pass moved, if it moved any.
    ///
    /// A block that grows pushes everything below it down. Text arriving at
    /// the end of the transcript therefore leaves a selection above it alone,
    /// while a tool's output growing above one does not — and the model needs
    /// to tell those two apart.
    shifted_from: Option<usize>,
    /// How many blocks the last pass rendered. One changed entry must cost
    /// one block, so a test asserts the work and not only the bookkeeping.
    #[cfg(test)]
    pub(crate) rebuilt: usize,
}

impl ScrollbackCache {
    /// Bring the cache up to date and work out what the viewport shows.
    ///
    /// This is the only place the scroll position is resolved: the pane says
    /// which entry to hold and how far it was asked to move, and the wrapping
    /// says what that comes to in lines.
    pub fn layout(&mut self, pane: &Scrollback, width: usize, height: usize) -> Viewport {
        let width = width.max(8);
        #[cfg(test)]
        {
            self.rebuilt = 0;
        }
        self.shifted_from = None;

        self.drain_evicted(pane);
        if self.width != width || self.show_thinking != pane.show_thinking {
            self.rebuild_all(pane, width);
        } else {
            self.rebuild_stale(pane, width);
        }

        let total = self.lines.len();
        let bottom = total.saturating_sub(height);
        let held = match pane.scroll() {
            Scroll::Bottom => bottom,
            Scroll::Anchored { entry, offset } => {
                self.starts.get(entry).copied().unwrap_or(0) + offset
            }
        };
        // Saturating on both ends: a pane parked at the bottom stays parked
        // when the reader scrolls down again, and cannot go above the start.
        let asked = held.saturating_add_signed(pane.pending());
        let top = asked.min(bottom);

        Viewport {
            top,
            total,
            height,
            // Anchoring at the bottom would freeze the pane there while a
            // reply streamed in below it.
            scroll: if top >= bottom {
                Scroll::Bottom
            } else {
                self.anchor_at(top)
            },
        }
    }

    /// How many times every block was thrown away.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The first line the last layout moved, if it moved any.
    #[must_use]
    pub fn shifted_from(&self) -> Option<usize> {
        self.shifted_from
    }

    /// The text a selection covers.
    #[must_use]
    pub fn selected_text(&self, span: (Spot, Spot)) -> String {
        select::extract(&self.lines, span)
    }

    /// The lines a viewport shows, cloned out of the cache.
    ///
    /// Ratatui's `Paragraph` owns its text, so this slice is the only copy;
    /// the whole transcript is neither re-wrapped nor copied at each frame.
    #[must_use]
    pub fn visible(&self, view: Viewport) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .skip(view.top)
            .take(view.height)
            .cloned()
            .collect()
    }

    /// Hold `top` against the entry whose block contains it.
    fn anchor_at(&self, top: usize) -> Scroll {
        // The last block starting at or before `top` owns it. Starts rise, so
        // the partition point is one past that block.
        let entry = self.starts.partition_point(|start| *start <= top);
        let entry = entry.saturating_sub(1);
        let offset = top - self.starts.get(entry).copied().unwrap_or(0).min(top);
        Scroll::Anchored { entry, offset }
    }

    /// Drop the blocks whose entries the history cap has taken.
    fn drain_evicted(&mut self, pane: &Scrollback) {
        let gone = pane.evicted().saturating_sub(self.evicted);
        self.evicted = pane.evicted();
        if gone == 0 {
            return;
        }
        if gone >= self.revs.len() {
            self.invalidate();
            return;
        }

        let removed_lines = self.starts[gone];
        self.generation += 1;
        self.starts.drain(0..gone);
        self.revs.drain(0..gone);
        self.lines.drain(0..removed_lines);
        for start in &mut self.starts {
            *start -= removed_lines;
        }
    }

    fn invalidate(&mut self) {
        self.lines.clear();
        self.starts.clear();
        self.revs.clear();
        self.width = 0;
        self.generation += 1;
    }

    /// Rebuild every block. Only a new width or a thinking toggle needs this:
    /// both change every wrapping there is.
    fn rebuild_all(&mut self, pane: &Scrollback, width: usize) {
        self.lines.clear();
        self.starts.clear();
        self.revs.clear();
        self.generation += 1;

        for (entry, rev) in pane.blocks() {
            let rendered = render_entry(entry, width, pane.show_thinking);
            #[cfg(test)]
            {
                self.rebuilt += 1;
            }
            self.starts.push(self.lines.len());
            self.lines.extend(rendered);
            self.revs.push(rev);
        }

        self.width = width;
        self.show_thinking = pane.show_thinking;
    }

    /// Re-render the entries whose revision moved on, splicing their new
    /// blocks in where the old ones were and leaving the rest alone.
    fn rebuild_stale(&mut self, pane: &Scrollback, width: usize) {
        for (index, (entry, rev)) in pane.blocks().enumerate() {
            match self.revs.get(index).copied() {
                Some(cached) if cached == rev => continue,
                // Drawn before, and changed since.
                Some(_) => {
                    let rendered = render_entry(entry, width, pane.show_thinking);
                    #[cfg(test)]
                    {
                        self.rebuilt += 1;
                    }
                    let start = self.starts[index];
                    let old_len = self
                        .starts
                        .get(index + 1)
                        .map_or(self.lines.len(), |next| *next)
                        - start;
                    let delta = rendered.len() as isize - old_len as isize;
                    self.lines.splice(start..start + old_len, rendered);
                    // From here down, a line number no longer names the text it
                    // named before: this block was re-wrapped, and anything a
                    // length change pushed came after it.
                    self.shifted_from =
                        Some(self.shifted_from.map_or(start, |first| first.min(start)));
                    if delta != 0 {
                        for start in &mut self.starts[index + 1..] {
                            *start = (*start as isize + delta) as usize;
                        }
                    }
                    self.revs[index] = rev;
                }
                // Never drawn: it can only be at the end, so append.
                None => {
                    let rendered = render_entry(entry, width, pane.show_thinking);
                    #[cfg(test)]
                    {
                        self.rebuilt += 1;
                    }
                    self.starts.push(self.lines.len());
                    self.lines.extend(rendered);
                    self.revs.push(rev);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ScrollbackCache;
    use crate::tui::scrollback::{MAX_ENTRIES, Scroll, Scrollback, Viewport};

    /// One frame: lay the pane out and tell it what was laid out, exactly as
    /// the runtime does between draws.
    fn frame(pane: &mut Scrollback, cache: &mut ScrollbackCache, height: usize) -> Viewport {
        let view = cache.layout(pane, 20, height);
        pane.laid_out(view);
        view
    }

    fn notices(count: usize) -> Scrollback {
        let mut pane = Scrollback::default();
        for number in 0..count {
            pane.push_notice(format!("line {number}"));
        }
        pane
    }

    #[test]
    fn only_the_changed_block_is_rebuilt() {
        let mut pane = notices(MAX_ENTRIES);
        let mut cache = ScrollbackCache::default();

        frame(&mut pane, &mut cache, 10);
        assert_eq!(
            cache.rebuilt, MAX_ENTRIES,
            "the first pass renders everything"
        );

        pane.push_notice("one more");
        frame(&mut pane, &mut cache, 10);
        assert_eq!(cache.rebuilt, 1, "an appended entry costs one block");

        pane.push_text("an answer");
        pane.push_text(" with more");
        frame(&mut pane, &mut cache, 10);
        assert_eq!(cache.rebuilt, 1, "a grown entry costs one block");

        frame(&mut pane, &mut cache, 10);
        assert_eq!(cache.rebuilt, 0, "an unchanged transcript costs nothing");
    }

    #[test]
    fn a_new_width_rebuilds_every_block() {
        let pane = notices(10);
        let mut cache = ScrollbackCache::default();

        cache.layout(&pane, 40, 10);
        cache.layout(&pane, 20, 10);
        assert_eq!(
            cache.rebuilt, 10,
            "wrapping is width-bound, so nothing carries"
        );
    }

    #[test]
    fn new_lines_below_the_viewport_do_not_move_it() {
        let mut pane = notices(10);
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 5);

        pane.scroll_up(2);
        let parked = frame(&mut pane, &mut cache, 5).top;

        pane.push_notice("fresh content");
        assert_eq!(
            frame(&mut pane, &mut cache, 5).top,
            parked,
            "what is being read stays where it is"
        );
    }

    #[test]
    fn a_block_above_the_viewport_growing_carries_it_along() {
        let mut pane = Scrollback::default();
        pane.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
        for number in 0..10 {
            pane.push_notice(format!("line {number}"));
        }
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 5);
        pane.scroll_up(3);
        let before = frame(&mut pane, &mut cache, 5);

        // The first block gains lines, so everything below it moves down.
        for number in 0..4 {
            pane.push_tool_progress("c1", &format!("output {number}"));
        }
        let after = frame(&mut pane, &mut cache, 5);

        assert!(after.total > before.total);
        assert_eq!(
            after.top - before.top,
            after.total - before.total,
            "the anchor rides down with the content above it"
        );
    }

    #[test]
    fn scrolling_back_to_the_bottom_parks_there() {
        let mut pane = notices(40);
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 5);

        pane.scroll_up(10);
        frame(&mut pane, &mut cache, 5);
        assert!(pane.scrolled());

        pane.scroll_down(10);
        frame(&mut pane, &mut cache, 5);
        assert!(!pane.scrolled(), "it does not stick just short of the end");
        assert_eq!(pane.scroll(), Scroll::Bottom);
    }

    #[test]
    fn two_page_keys_without_a_draw_between_move_two_pages() {
        let mut pane = notices(40);
        let mut cache = ScrollbackCache::default();
        let bottom = frame(&mut pane, &mut cache, 5).top;

        // No frame in between: both asks add up rather than the second one
        // starting over from where the first began.
        pane.scroll_up(4);
        pane.scroll_up(4);

        assert_eq!(frame(&mut pane, &mut cache, 5).top, bottom - 8);
    }

    #[test]
    fn an_evicted_anchor_falls_back_to_the_bottom() {
        let mut pane = notices(MAX_ENTRIES);
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 5);
        pane.scroll_up(1_000);
        frame(&mut pane, &mut cache, 5);
        assert!(pane.scrolled());

        // Push until the cap eats the entry the anchor named.
        for number in 0..MAX_ENTRIES {
            pane.push_notice(format!("later {number}"));
        }

        assert_eq!(
            pane.scroll(),
            Scroll::Bottom,
            "the content it held is gone, so it holds nothing"
        );
    }

    #[test]
    fn clearing_the_pane_empties_the_cache() {
        let mut pane = notices(10);
        let mut cache = ScrollbackCache::default();
        assert!(frame(&mut pane, &mut cache, 5).total > 0);

        pane.clear();
        assert_eq!(
            frame(&mut pane, &mut cache, 5).total,
            0,
            "a cleared transcript leaves nothing on the screen"
        );
    }

    #[test]
    fn text_arriving_below_a_line_does_not_move_it() {
        let mut pane = notices(10);
        let mut cache = ScrollbackCache::default();
        let above = frame(&mut pane, &mut cache, 5).total;

        pane.push_text("an answer");
        frame(&mut pane, &mut cache, 5);
        pane.push_text(" with more");
        frame(&mut pane, &mut cache, 5);

        let shifted = cache.shifted_from().expect("the answer was re-wrapped");
        assert!(
            shifted >= above,
            "the notices above the growing answer stayed where they were: \
             moved from {shifted}, and there are {above} lines above"
        );
    }

    #[test]
    fn a_block_growing_above_reports_the_line_it_moved_from() {
        let mut pane = Scrollback::default();
        pane.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
        for number in 0..10 {
            pane.push_notice(format!("line {number}"));
        }
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 5);
        assert_eq!(cache.shifted_from(), None, "a settled pane moves nothing");

        pane.push_tool_progress("c1", "output");
        frame(&mut pane, &mut cache, 5);

        assert_eq!(
            cache.shifted_from(),
            Some(0),
            "the first block grew, so everything after it moved"
        );
    }

    #[test]
    fn a_new_width_moves_the_generation_on() {
        let pane = notices(10);
        let mut cache = ScrollbackCache::default();
        cache.layout(&pane, 40, 10);
        let before = cache.generation();
        cache.layout(&pane, 20, 10);

        assert_ne!(
            cache.generation(),
            before,
            "every line was re-wrapped, so no line number carries over"
        );
    }

    #[test]
    fn an_evicted_prefix_leaves_the_cache_holding_the_survivors() {
        let mut pane = notices(MAX_ENTRIES);
        let mut cache = ScrollbackCache::default();
        frame(&mut pane, &mut cache, 10);

        pane.push_notice("after the cap");
        let warm = frame(&mut pane, &mut cache, 10);

        let mut cold = ScrollbackCache::default();
        assert_eq!(
            cold.layout(&pane, 20, 10),
            warm,
            "a drained cache lays out what a fresh one would"
        );
        assert_eq!(cold.visible(warm), cache.visible(warm));
    }
}
