//! Output caps shared by the tools.
//!
//! A `cargo build` can produce megabytes. Sending that to the model wastes the
//! context window on text it does not need, so output is capped and the full
//! version spilled to a temp file whose path goes back in the result — the model
//! can `read` it if it really wants the rest.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Keep at most this many lines.
pub const MAX_LINES: usize = 1000;
/// Keep at most this many bytes.
pub const MAX_BYTES: usize = 64 * 1024;

/// Output after capping.
#[derive(Clone, Debug, Default)]
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    /// Lines in the untruncated output.
    pub total_lines: usize,
    /// Lines that did not make it into `text`.
    pub dropped_lines: usize,
    /// Where the full output was written, when it was.
    pub full_output_path: Option<PathBuf>,
}

impl Truncated {
    /// A note to append so the model knows something was withheld, and where the
    /// rest is.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        if !self.truncated {
            return None;
        }
        let mut notice = format!(
            "\n[{} of {} lines shown]",
            self.total_lines - self.dropped_lines,
            self.total_lines
        );
        if let Some(path) = &self.full_output_path {
            notice.push_str(&format!(" full output: {}", path.display()));
        }
        Some(notice)
    }

    /// The text with the truncation notice appended, ready to return.
    #[must_use]
    pub fn into_text(self) -> String {
        match self.notice() {
            Some(notice) => {
                let mut text = self.text;
                text.push_str(&notice);
                text
            }
            None => self.text,
        }
    }
}

/// Keep the **last** lines. What you want for a command's output: the error and
/// the summary are at the end.
#[must_use]
pub fn tail(full: &str) -> Truncated {
    cap(full, false)
}

/// Keep the **first** lines. What you want when reading a file: you asked for a
/// range and you want it from the top.
#[must_use]
pub fn head(full: &str) -> Truncated {
    cap(full, true)
}

fn cap(full: &str, from_start: bool) -> Truncated {
    let lines: Vec<&str> = full.lines().collect();
    let total_lines = lines.len();

    if total_lines <= MAX_LINES && full.len() <= MAX_BYTES {
        return Truncated {
            text: full.to_owned(),
            truncated: false,
            total_lines,
            dropped_lines: 0,
            full_output_path: None,
        };
    }

    let kept: Vec<&str> = if from_start {
        take_within_budget(lines.iter().copied())
    } else {
        let mut kept = take_within_budget(lines.iter().copied().rev());
        kept.reverse();
        kept
    };

    let dropped_lines = total_lines - kept.len();
    Truncated {
        text: kept.join("\n"),
        truncated: true,
        total_lines,
        dropped_lines,
        full_output_path: spill(full),
    }
}

/// Take lines in iteration order until either cap is reached.
fn take_within_budget<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for line in lines {
        if kept.len() >= MAX_LINES || bytes + line.len() + 1 > MAX_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept
}

/// Write the full output somewhere the model can read it back.
///
/// Best effort: if the spill fails there is nothing useful to do about it, and
/// the truncated output is still worth returning.
fn spill(full: &str) -> Option<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("aphid-output-{}-{n}.txt", std::process::id()));
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(full.as_bytes()).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_passes_through_untouched() {
        let result = tail("one\ntwo\nthree");
        assert!(!result.truncated);
        assert_eq!(result.text, "one\ntwo\nthree");
        assert_eq!(result.total_lines, 3);
        assert!(result.full_output_path.is_none());
        assert!(result.notice().is_none());
    }

    #[test]
    fn tail_keeps_the_end_and_spills_the_rest() {
        let full: String = (0..MAX_LINES + 500)
            .map(|n| format!("line {n}\n"))
            .collect();
        let result = tail(&full);

        assert!(result.truncated);
        assert_eq!(result.total_lines, MAX_LINES + 500);
        assert_eq!(result.dropped_lines, 500);
        assert!(result.text.starts_with("line 500\n"));
        assert!(result.text.ends_with(&format!("line {}", MAX_LINES + 499)));

        let path = result.full_output_path.clone().expect("spilled");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), full);
        assert!(
            result
                .notice()
                .unwrap()
                .contains(&path.display().to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn head_keeps_the_beginning() {
        let full: String = (0..MAX_LINES + 10).map(|n| format!("line {n}\n")).collect();
        let result = head(&full);

        assert!(result.truncated);
        assert!(result.text.starts_with("line 0\n"));
        assert_eq!(result.dropped_lines, 10);
        if let Some(path) = result.full_output_path {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn the_byte_cap_bites_before_the_line_cap() {
        // Ten lines, but each one is a quarter of the byte budget.
        let line = "x".repeat(MAX_BYTES / 4);
        let full: String = (0..10).map(|_| format!("{line}\n")).collect();
        let result = tail(&full);

        assert!(result.truncated);
        assert!(result.total_lines == 10);
        assert!(result.text.len() <= MAX_BYTES);
        if let Some(path) = result.full_output_path {
            let _ = std::fs::remove_file(path);
        }
    }
}
