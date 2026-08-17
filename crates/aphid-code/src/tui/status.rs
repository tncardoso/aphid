//! The status line: what the session has cost so far.
//!
//! Every number here comes from the provider, never from an estimate. Context
//! use is the last turn's `input + cache_read` — exactly what the model was sent
//! — so it sits still while tools run and jumps when the next response lands.
//! That is the honest shape: aphid does not know the token count of text no
//! model has seen yet.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use aphid_core::{Model, Usage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Fraction of the context window past which the display warns.
pub const WARN_AT: f64 = 0.75;
/// Fraction past which it is urgent.
pub const ALARM_AT: f64 = 0.90;

/// What the status line shows.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Usage of the most recent assistant turn, or `None` before the first one.
    pub last: Option<Usage>,
    /// Everything this session has spent.
    pub total: Usage,
    pub model: String,
    pub context_window: u32,
    pub thinking: Option<String>,
    pub running: bool,
    /// A message queued while the agent was busy.
    pub queued: bool,
    /// Bytes streamed from the provider, for the live download speed.
    pub download: DownloadSpeed,
}

impl Status {
    #[must_use]
    pub fn from_model(model: &Model) -> Self {
        Self {
            model: model.id.to_string(),
            context_window: model.context_window,
            ..Self::default()
        }
    }

    /// Tokens the last request carried, fresh and cached together.
    #[must_use]
    pub fn context_used(&self) -> u32 {
        self.last.map_or(0, |usage| usage.input + usage.cache_read)
    }

    #[must_use]
    pub fn context_fraction(&self) -> f64 {
        if self.context_window == 0 {
            return 0.0;
        }
        f64::from(self.context_used()) / f64::from(self.context_window)
    }

    fn context_style(&self) -> Style {
        let fraction = self.context_fraction();
        if fraction >= ALARM_AT {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else if fraction >= WARN_AT {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    /// Render the line.
    ///
    /// Reads no clock: the download meter holds its last reading, so drawing
    /// the same status twice paints the same thing.
    #[must_use]
    pub fn line(&self) -> Line<'static> {
        let dim = Style::default().fg(Color::DarkGray);
        let mut spans = vec![Span::styled(" ", dim)];

        let model = match &self.thinking {
            Some(thinking) => format!("{}({thinking})", self.model),
            None => self.model.clone(),
        };
        spans.push(Span::styled(model, dim));

        spans.push(Span::styled(
            format!(
                " · {}/{}",
                tokens(self.context_used()),
                tokens(self.context_window)
            ),
            self.context_style(),
        ));
        if self.context_fraction() >= WARN_AT {
            spans.push(Span::styled(
                format!(" ⚠ context {:.0}%", self.context_fraction() * 100.0),
                self.context_style(),
            ));
        }

        spans.push(Span::styled(
            format!(
                " · {}/{} tok",
                tokens(self.total.input + self.total.cache_read),
                tokens(self.total.output),
            ),
            dim,
        ));

        spans.push(Span::styled(
            format!(" · ${:.4}", self.total.cost.total),
            dim,
        ));

        if let Some(kb_s) = self.download.rate_kb_s() {
            spans.push(Span::styled(format!(" · ↓ {kb_s:.1} KB/s"), dim));
        }

        if self.running {
            spans.push(Span::styled(
                "  working…",
                Style::default().fg(Color::Yellow),
            ));
        }
        if self.queued {
            spans.push(Span::styled("  (queued)", Style::default().fg(Color::Cyan)));
        }

        Line::from(spans)
    }
}

/// A rolling meter for the bytes a provider's stream delivers.
///
/// Samples `(when, bytes)` are kept in a deque, dropped once they are older
/// than [`DownloadSpeed::WINDOW`], and summed over the span they cover. The
/// rate is KB/s, decimal: 1 KB = 1000 bytes.
///
/// The reading is computed when the samples change, not when the line is
/// drawn. Drawing then needs no clock and no `&mut`, which is what lets the
/// status line be a function of the state and nothing else.
#[derive(Clone, Debug, Default)]
pub struct DownloadSpeed {
    /// When each chunk landed and how many bytes it carried.
    samples: VecDeque<(Instant, u64)>,
    /// What the samples add up to, carried rather than re-summed.
    ///
    /// A reading is taken at every chunk now, not once a frame, and a fast
    /// provider puts a thousand chunks in the window: summing them at each one
    /// would be a million additions for a reply.
    total: u64,
    /// The reading the samples last gave.
    rate: Option<f64>,
}

impl DownloadSpeed {
    /// The window over which a reading averages.
    const WINDOW: Duration = Duration::from_secs(2);
    /// Below this span a first chunk would read as an absurd speed.
    const MIN_SPAN: Duration = Duration::from_millis(100);

    /// Record that `bytes` landed at `now`.
    pub fn note(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
        self.total += bytes;
        self.recompute(now);
    }

    /// Age the samples out at `now`, without adding any.
    ///
    /// A stream that goes quiet stops calling [`Self::note`], and the reading
    /// has to fall away by itself rather than hold at the last burst.
    pub fn prune(&mut self, now: Instant) {
        self.recompute(now);
    }

    /// Forget every sample. Called when the turn ends, so a stale reading
    /// cannot sit on an idle line.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.total = 0;
        self.rate = None;
    }

    /// KB/s over the recent span, or `None` when nothing arrived within the
    /// window — that is, no stream is live.
    #[must_use]
    pub fn rate_kb_s(&self) -> Option<f64> {
        self.rate
    }

    /// Drop what has aged out and read what is left.
    fn recompute(&mut self, now: Instant) {
        self.rate = self.window_sum(now).map(|(oldest, sum)| {
            let span = now.saturating_duration_since(oldest).max(Self::MIN_SPAN);
            sum as f64 / span.as_secs_f64() / 1000.0
        });
    }

    /// Drop the samples older than the window, and report the oldest that is
    /// left with what the window now holds.
    ///
    /// The work is what fell out, not what stayed: the total is carried.
    fn window_sum(&mut self, now: Instant) -> Option<(Instant, u64)> {
        while let Some((at, bytes)) = self.samples.front().copied() {
            if now.saturating_duration_since(at) <= Self::WINDOW {
                break;
            }
            self.samples.pop_front();
            self.total -= bytes;
        }
        let (oldest, _) = *self.samples.front()?;
        Some((oldest, self.total))
    }

    /// How many chunks the window still holds. Test-only: the meter is a
    /// window and not a log, so this stays bounded however long a reply runs.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn samples(&self) -> usize {
        self.samples.len()
    }

    /// Total bytes still in the window. Test-only: the app tests assert that
    /// streaming events feed the meter without peeking at its internals.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> u64 {
        self.total
    }
}

/// Compact token counts: `812`, `12.4k`, `1.0M`.
#[must_use]
pub fn tokens(count: u32) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=999_999 => {
            let thousands = f64::from(count) / 1000.0;
            if thousands < 10.0 {
                format!("{thousands:.1}k")
            } else {
                format!("{thousands:.0}k")
            }
        }
        _ => format!("{:.1}M", f64::from(count) / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(input: u32, cache_read: u32, window: u32) -> Status {
        Status {
            last: Some(Usage {
                input,
                cache_read,
                ..Usage::default()
            }),
            context_window: window,
            model: "deepseek-v4-flash".into(),
            ..Status::default()
        }
    }

    #[test]
    fn token_counts_are_compact() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(812), "812");
        assert_eq!(tokens(1_200), "1.2k");
        assert_eq!(tokens(12_400), "12k");
        assert_eq!(tokens(1_000_000), "1.0M");
    }

    #[test]
    fn context_use_counts_cached_tokens_too() {
        let status = status(8_000, 4_400, 1_000_000);
        assert_eq!(status.context_used(), 12_400);
        assert!(status.line().to_string().contains("12k/1.0M"));
    }

    #[test]
    fn with_no_turn_yet_the_context_reads_zero() {
        let status = Status {
            context_window: 1_000_000,
            ..Status::default()
        };
        assert_eq!(status.context_used(), 0);
        assert!(status.line().to_string().contains("0/1.0M"));
    }

    #[test]
    fn the_warning_appears_only_when_the_window_fills() {
        let calm = status(500_000, 0, 1_000_000);
        assert!(!calm.line().to_string().contains('⚠'));

        let warned = status(780_000, 0, 1_000_000);
        assert!(warned.line().to_string().contains("⚠ context 78%"));

        let alarmed = status(950_000, 0, 1_000_000);
        assert!(alarmed.line().to_string().contains("95%"));
    }

    #[test]
    fn totals_and_state_are_shown() {
        let mut status = status(100, 0, 1_000);
        status.total = Usage {
            input: 8_200,
            output: 1_100,
            cost: aphid_core::Cost {
                total: 0.0041,
                ..aphid_core::Cost::default()
            },
            ..Usage::default()
        };
        status.running = true;
        status.queued = true;

        let rendered = status.line().to_string();
        assert!(rendered.contains("8.2k/1.1k tok"));
        assert!(rendered.contains("$0.0041"));
        assert!(rendered.contains("deepseek-v4-flash"));
        assert!(rendered.contains("working…"));
        assert!(rendered.contains("(queued)"));
    }

    #[test]
    fn thinking_level_is_parenthesised_after_the_model() {
        let mut thinking = status(100, 0, 1_000);
        thinking.thinking = Some("medium".into());
        assert!(
            thinking
                .line()
                .to_string()
                .contains("deepseek-v4-flash(medium)")
        );

        let mut off = status(100, 0, 1_000);
        off.thinking = None;
        let rendered = off.line().to_string();
        assert!(rendered.contains("deepseek-v4-flash"));
        assert!(!rendered.contains('('));
    }

    #[test]
    fn a_zero_window_does_not_divide_by_zero() {
        let status = status(100, 0, 0);
        assert_eq!(status.context_fraction(), 0.0);
    }

    #[test]
    fn no_samples_means_no_speed() {
        let meter = DownloadSpeed::default();
        assert_eq!(meter.rate_kb_s(), None);

        let status = status(100, 0, 1_000);
        let rendered = status.line().to_string();
        assert!(!rendered.contains("KB/s"));
    }

    #[test]
    fn the_rate_is_bytes_over_span() {
        let t0 = Instant::now();
        let mut meter = DownloadSpeed::default();
        meter.note(t0, 1_000);
        meter.note(t0 + Duration::from_millis(250), 1_000);

        // 2000 bytes over 0.5 s, in decimal KB/s.
        meter.prune(t0 + Duration::from_millis(500));
        let rate = meter.rate_kb_s().unwrap();
        assert!((rate - 4.0).abs() < 1e-9);
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let t0 = Instant::now();
        let mut meter = DownloadSpeed::default();
        meter.note(t0, 1_000);
        meter.note(t0 + Duration::from_millis(250), 1_000);

        // Everything has aged out of the 2 s window.
        meter.prune(t0 + DownloadSpeed::WINDOW + Duration::from_millis(251));
        assert_eq!(meter.rate_kb_s(), None);
        assert_eq!(meter.bytes(), 0, "expired samples are popped");
    }

    #[test]
    fn a_first_burst_is_bounded_by_the_min_span() {
        let t0 = Instant::now();
        let mut meter = DownloadSpeed::default();
        meter.note(t0, 1_000);

        // 1 ms after the first chunk the span is clamped to MIN_SPAN, so a
        // single burst cannot read as an absurd speed.
        meter.prune(t0 + Duration::from_millis(1));
        let rate = meter.rate_kb_s().unwrap();
        assert!((rate - 10.0).abs() < 1e-9, "clamped to {rate} KB/s");
    }

    #[test]
    fn the_speed_is_shown_while_a_stream_is_live() {
        let t0 = Instant::now();
        let mut status = status(100, 0, 1_000);
        status.download.note(t0, 2_000);

        let rendered = status.line().to_string();
        assert!(rendered.contains("KB/s"), "{rendered:?}");
    }

    /// The total is carried rather than re-summed, so it has to stay in step
    /// with the samples through every note and every expiry.
    #[test]
    fn the_carried_total_matches_what_the_window_holds() {
        let t0 = Instant::now();
        let mut meter = DownloadSpeed::default();

        for step in 0..200u64 {
            meter.note(t0 + Duration::from_millis(step * 25), 100);
        }
        // Half the notes are older than the two-second window by now.
        assert_eq!(meter.bytes(), meter.samples() as u64 * 100);
        assert!(meter.samples() < 200, "the old ones fell out");

        meter.prune(t0 + Duration::from_secs(60));
        assert_eq!(
            meter.bytes(),
            0,
            "everything aged out, and so did the total"
        );
        assert_eq!(meter.rate_kb_s(), None);
    }

    #[test]
    fn a_stream_that_goes_quiet_stops_reading() {
        let t0 = Instant::now();
        let mut status = status(100, 0, 1_000);
        status.download.note(t0, 2_000);

        // Nothing more arrives. The meter is aged rather than fed, and the
        // reading has to go away by itself.
        status
            .download
            .prune(t0 + DownloadSpeed::WINDOW + Duration::from_millis(1));

        assert!(!status.line().to_string().contains("KB/s"));
    }
}
