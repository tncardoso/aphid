//! What the UI actually paints, asserted against a fixed-size buffer.
//!
//! `TestBackend` renders into memory, so these run in CI with no terminal.

use aphid_agent::exec;
use aphid_code::plugins::permissions::Risk;
use aphid_code::tui::modal::{Confirm, Modal};
use aphid_code::tui::scrollback::Scrollback;
use aphid_code::tui::status::Status;
use aphid_core::{Cost, Usage, providers::deepseek};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

/// The rendered buffer as one string per row, trailing blanks trimmed.
fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn draw(width: u16, height: u16, f: impl FnOnce(&mut ratatui::Frame<'_>)) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| f(frame)).expect("draw");
    rows(&terminal)
}

#[test]
fn the_status_line_shows_exact_provider_numbers() {
    let mut status = Status::from_model(&deepseek::flash());
    status.last = Some(Usage {
        input: 8_000,
        cache_read: 4_400,
        ..Usage::default()
    });
    status.total = Usage {
        input: 8_200,
        output: 1_100,
        cost: Cost {
            total: 0.0041,
            ..Cost::default()
        },
        ..Usage::default()
    };

    let rendered = draw(80, 1, |frame| {
        frame.render_widget(Paragraph::new(status.line()), frame.area());
    });

    assert_eq!(
        rendered[0],
        " deepseek-v4-flash · 12k/1.0M · 8.2k/1.1k tok · $0.0041"
    );
}

#[test]
fn a_filling_context_window_is_called_out() {
    let mut status = Status::from_model(&deepseek::flash());
    status.last = Some(Usage {
        input: 800_000,
        ..Usage::default()
    });

    let rendered = draw(80, 1, |frame| {
        frame.render_widget(Paragraph::new(status.line()), frame.area());
    });

    assert!(rendered[0].contains("⚠ context 80%"), "{}", rendered[0]);
}

#[test]
fn a_tool_call_renders_as_a_header_and_collapsed_output() {
    let mut view = Scrollback::default();
    view.push_tool_call("c1", "bash", r#"{"command":"cargo test"}"#);
    let output: String = (0..40).map(|n| format!("line {n}\n")).collect();
    view.finish_tool("c1", &output, false, None);

    let rendered = draw(50, 20, |frame| {
        frame.render_widget(Paragraph::new(view.lines(50)), frame.area());
    });

    assert!(rendered[0].starts_with("→ bash"), "{}", rendered[0]);
    assert!(rendered[0].contains("cargo test"));
    assert!(rendered.iter().any(|row| row.contains("line 0")));
    assert!(
        rendered.iter().any(|row| row.contains("… 25 more lines")),
        "{rendered:#?}"
    );
    assert!(!rendered.iter().any(|row| row.contains("line 39")));
}

#[test]
fn a_streaming_call_counts_up_then_becomes_the_call() {
    let mut view = Scrollback::default();
    view.begin_tool_stream(0, "bash");
    view.push_tool_stream(0, 412);

    let rendered = draw(50, 4, |frame| {
        frame.render_widget(Paragraph::new(view.lines(50)), frame.area());
    });
    assert!(rendered[0].starts_with("◌ bash"), "{}", rendered[0]);
    assert!(
        rendered[0].contains("receiving arguments… 412 B"),
        "{}",
        rendered[0]
    );

    view.push_tool_call("c1", "bash", r#"{"command":"cargo test --all"}"#);

    let rendered = draw(50, 4, |frame| {
        frame.render_widget(Paragraph::new(view.lines(50)), frame.area());
    });
    assert!(rendered[0].starts_with("⋯ bash"), "{}", rendered[0]);
    assert!(rendered[0].contains("cargo test --all"), "{}", rendered[0]);
}

#[test]
fn an_edit_renders_as_a_diff() {
    let mut view = Scrollback::default();
    view.push_tool_call("c1", "edit", r#"{"path":"src/buffer.rs"}"#);
    view.finish_tool(
        "c1",
        "Applied 1 edit to src/buffer.rs",
        false,
        Some(serde_json::json!({
            "path": "src/buffer.rs",
            "edits": [{ "line": 42, "old": "let mut n = 0;", "new": "let mut n = self.len();" }]
        })),
    );

    let rendered = draw(60, 10, |frame| {
        frame.render_widget(Paragraph::new(view.lines(60)), frame.area());
    });

    assert!(rendered[0].starts_with("→ edit"));
    assert_eq!(rendered[1].trim(), "@@ line 42");
    assert_eq!(rendered[2].trim(), "- let mut n = 0;");
    assert_eq!(rendered[3].trim(), "+ let mut n = self.len();");
}

#[test]
fn a_failed_tool_is_marked_and_keeps_its_message() {
    let mut view = Scrollback::default();
    view.push_tool_call("c1", "edit", r#"{"path":"a.rs"}"#);
    view.finish_tool("c1", "edit 1: old_text does not appear in a.rs", true, None);

    let rendered = draw(60, 6, |frame| {
        frame.render_widget(Paragraph::new(view.lines(60)), frame.area());
    });

    assert!(rendered[0].starts_with("✗ edit"), "{}", rendered[0]);
    assert!(rendered[1].contains("does not appear"));
}

#[test]
fn a_running_tool_shows_its_latest_output() {
    let mut view = Scrollback::default();
    view.push_tool_call("c1", "bash", r#"{"command":"cargo build"}"#);
    for n in 0..40 {
        view.push_tool_progress("c1", &format!("Compiling crate-{n}"));
    }

    let rendered = draw(60, 20, |frame| {
        frame.render_widget(Paragraph::new(view.lines(60)), frame.area());
    });

    assert!(rendered[0].starts_with("⋯ bash"), "{}", rendered[0]);
    // The tail is what a build log needs while it runs.
    assert!(rendered.iter().any(|row| row.contains("crate-39")));
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("… 25 earlier lines"))
    );
}

#[test]
fn the_conversation_reads_in_order() {
    let mut view = Scrollback::default();
    view.push_user("fix the failing test");
    view.push_tool_call("c1", "bash", r#"{"command":"cargo test"}"#);
    view.finish_tool("c1", "test result: FAILED. 1 failed", true, None);
    view.push_text("The assertion on line 42 is wrong.");

    let rendered = draw(50, 12, |frame| {
        frame.render_widget(Paragraph::new(view.lines(50)), frame.area());
    });

    let joined = rendered.join("\n");
    let user = joined.find("fix the failing test").expect("user line");
    let call = joined.find("cargo test").expect("tool call");
    let reply = joined.find("assertion on line 42").expect("assistant");
    assert!(user < call && call < reply, "{joined}");
    assert!(rendered[0].starts_with("> fix the failing test"));
}

#[test]
fn long_text_wraps_to_the_pane_width() {
    let mut view = Scrollback::default();
    view.push_text("the quick brown fox jumps over the lazy dog and keeps going");

    let rendered = draw(20, 8, |frame| {
        frame.render_widget(Paragraph::new(view.lines(20)), frame.area());
    });

    for row in &rendered {
        assert!(row.chars().count() <= 20, "row too wide: {row:?}");
    }
    assert!(rendered.iter().any(|row| row.contains("the quick brown")));
    assert!(rendered.iter().any(|row| row.contains("keeps going")));
}

#[test]
fn the_model_picker_lists_what_it_offers() {
    let modal = Modal::Models {
        models: deepseek::models(),
        selected: 1,
    };

    let rendered = draw(80, 12, |frame| {
        modal.render(frame, Rect::new(0, 0, 80, 12));
    });

    let joined = rendered.join("\n");
    assert!(joined.contains("deepseek-v4-flash"));
    assert!(joined.contains("deepseek-v4-pro"));
    assert!(joined.contains("1.0M ctx"));
    // The selected row is marked.
    assert!(
        rendered.iter().any(|row| row.contains("▸ deepseek-v4-pro")),
        "{joined}"
    );
}

#[test]
fn the_permission_prompt_says_what_it_is_asking_about() {
    let (reply, _answer) = std::sync::mpsc::channel();
    let modal = Modal::Confirm(Confirm {
        tool: "bash".into(),
        summary: "rm -rf build".into(),
        risk: Risk::Destructive,
        reply,
    });

    let rendered = draw(80, 14, |frame| {
        modal.render(frame, Rect::new(0, 0, 80, 14));
    });

    let joined = rendered.join("\n");
    assert!(joined.contains("bash — destructive"), "{joined}");
    assert!(joined.contains("rm -rf build"), "{joined}");
    assert!(joined.contains("[y] once"), "{joined}");
    assert!(joined.contains("[a] always"), "{joined}");
    assert!(joined.contains("[n] no"), "{joined}");
}

#[tokio::test]
async fn the_process_list_separates_what_is_running_from_what_just_ran() {
    use std::sync::Arc;

    let processes = Arc::new(exec::Registry::new());

    // One that is over, so the recent section has something to show.
    exec::run(
        &processes,
        exec::Spec::new("bash", "exit 7"),
        None,
        Arc::new(|_, _| {}),
    )
    .await;

    // One that is still going while the list is drawn.
    let running = tokio::spawn({
        let processes = Arc::clone(&processes);
        async move {
            exec::run(
                &processes,
                exec::Spec::new("webchat", "sleep 30"),
                None,
                Arc::new(|_, _| {}),
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let modal = Modal::Processes {
        rows: processes.snapshot(),
        selected: 0,
    };
    let rendered = draw(90, 14, |frame| {
        modal.render(frame, Rect::new(0, 0, 90, 14));
    });
    let joined = rendered.join("\n");

    assert!(joined.contains("processes"), "{joined}");
    // The running one is selected, named after the plugin that started it, and
    // counting.
    assert!(joined.contains("▸"), "{joined}");
    assert!(joined.contains("webchat"), "{joined}");
    assert!(joined.contains("sleep 30"), "{joined}");
    assert!(joined.contains("0:00"), "{joined}");
    // The finished one keeps its exit code and how much it wrote.
    assert!(joined.contains("recent"), "{joined}");
    assert!(joined.contains("exit 7"), "{joined}");
    assert!(joined.contains("✗ 7"), "{joined}");
    assert!(joined.contains("0 B"), "{joined}");

    let id = processes
        .snapshot()
        .into_iter()
        .find(exec::Process::running)
        .expect("the sleep")
        .id;
    processes.kill(id);
    running.await.expect("the sleep");
}

#[test]
fn an_empty_process_list_says_so() {
    let modal = Modal::Processes {
        rows: Vec::new(),
        selected: 0,
    };

    let rendered = draw(90, 8, |frame| {
        modal.render(frame, Rect::new(0, 0, 90, 8));
    });

    assert!(rendered.join("\n").contains("nothing running"));
}

// ---------------------------------------------------------------------------
// The transcript cache, pinned.
//
// The pane keeps a per-entry line cache and re-renders only the blocks that
// changed. These say what the pane must paint, so a change to how the cache
// is kept has to keep painting it.
// ---------------------------------------------------------------------------

/// A transcript with one of each thing the pane draws.
fn transcript() -> Scrollback {
    let mut view = Scrollback::default();
    view.push_user("explain the wrapping rule");
    view.push_text("Lines are wrapped at the pane width, and broken at a space when there is one.");
    view.push_tool_call("c1", "bash", r#"{"command":"ls"}"#);
    view.finish_tool("c1", "Cargo.toml\nsrc\n", false, None);
    view.push_notice("── switched to deepseek-v4-flash ──");
    view
}

/// What the pane paints into a `width` by `height` viewport.
fn painted(view: &mut Scrollback, width: u16, height: u16) -> Vec<String> {
    let lines = view.visible_lines(width as usize, height as usize);
    draw(width, height, |frame| {
        frame.render_widget(Paragraph::new(lines), frame.area());
    })
}

#[test]
fn a_narrow_pane_wraps_every_kind_of_entry() {
    assert_eq!(
        painted(&mut transcript(), 30, 14),
        [
            "> explain the wrapping rule",
            "",
            "Lines are wrapped at the pane",
            "width, and broken at a space",
            "when there is one.",
            "",
            "→ bash {\"command\":\"ls\"}",
            "  Cargo.toml",
            "  src",
            "",
            "── switched to",
            "deepseek-v4-flash ──",
            "",
            "",
        ]
    );
}

#[test]
fn a_wide_pane_wraps_less_and_says_the_same_thing() {
    assert_eq!(
        painted(&mut transcript(), 60, 14),
        [
            "> explain the wrapping rule",
            "",
            "Lines are wrapped at the pane width, and broken at a space",
            "when there is one.",
            "",
            "→ bash {\"command\":\"ls\"}",
            "  Cargo.toml",
            "  src",
            "",
            "── switched to deepseek-v4-flash ──",
            "",
            "",
            "",
            "",
        ]
    );
}

#[test]
fn a_streamed_answer_leaves_what_is_above_it_alone() {
    let mut view = Scrollback::default();
    view.push_user("explain the wrapping rule");
    view.push_text("Lines are wrapped at the pane width");
    let before = painted(&mut view, 40, 20);

    view.push_text(", and broken at a space when there is one.");
    let after = painted(&mut view, 40, 20);

    assert_eq!(after[..2], before[..2], "the question does not move");
    assert!(after.join("\n").contains("broken at a space"), "{after:#?}");
}

#[test]
fn eviction_paints_what_the_survivors_alone_would_paint() {
    const PUSHED: usize = 400;

    let mut capped = Scrollback::default();
    for number in 0..PUSHED {
        capped.push_notice(format!("notice {number}"));
    }
    let kept = capped.entries().len();
    assert!(
        kept < PUSHED,
        "the cap has to have bitten for this to mean anything"
    );

    let mut survivors = Scrollback::default();
    for number in PUSHED - kept..PUSHED {
        survivors.push_notice(format!("notice {number}"));
    }

    assert_eq!(
        painted(&mut capped, 40, 12),
        painted(&mut survivors, 40, 12),
        "an evicted prefix leaves no trace in the pane"
    );
}

#[test]
fn scrolling_up_holds_the_viewport_while_content_arrives_below() {
    let mut view = Scrollback::default();
    for number in 0..40 {
        view.push_notice(format!("notice {number}"));
    }
    let _ = view.visible_lines(40, 10);

    view.scroll_up(12);
    let parked = painted(&mut view, 40, 10);

    for number in 40..46 {
        view.push_notice(format!("notice {number}"));
    }

    assert_eq!(
        painted(&mut view, 40, 10),
        parked,
        "reading back through the transcript is not disturbed by what arrives"
    );
}
