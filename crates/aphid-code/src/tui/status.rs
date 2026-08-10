//! The status line: what the session has cost so far.
//!
//! Every number here comes from the provider, never from an estimate. Context
//! use is the last turn's `input + cache_read` — exactly what the model was sent
//! — so it sits still while tools run and jumps when the next response lands.
//! That is the honest shape: aphid does not know the token count of text no
//! model has seen yet.

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
}
