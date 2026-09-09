//! The attached terminal, driven with no socket at all.
//!
//! What arrives from a daemon is plain data, and so is what goes back, so a
//! whole session can be played into the model and read out of it without a
//! process on the other end. That was not possible while every keypress wrote
//! to a socket in the middle of deciding what to do.

use aphid_alate::gateway::wire::{Answer, Envelope, Frame, Request, Risk};
use aphid_alate::sessions::Info;
use aphid_alate::tui::{App, Effect, Msg};
use aphid_code::tui::runtime::Program;
use aphid_code::tui::scrollback::Entry;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A terminal that has been told which session it is looking at.
fn attached() -> App {
    let mut app = App::new("smoke");
    app.update(wire(
        Some("s1"),
        Frame::Hello {
            instance: "smoke".to_owned(),
            model: "deepseek-v4-flash".to_owned(),
            context_window: 1_000_000,
            thinking: None,
        },
    ));
    app
}

fn wire(session: Option<&str>, frame: Frame) -> Msg {
    Msg::Wire(Box::new(Envelope {
        session: session.map(ToOwned::to_owned),
        frame,
    }))
}

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .into_effects()
}

fn type_line(app: &mut App, line: &str) -> Vec<Effect> {
    let mut effects = Vec::new();
    for c in line.chars() {
        effects.extend(press(app, KeyCode::Char(c)));
    }
    effects.extend(press(app, KeyCode::Enter));
    effects
}

fn sent(request: Request) -> Effect {
    Effect::Send(Box::new(request))
}

/// What the pane shows, as text.
fn shown(app: &App, session: &str) -> Vec<String> {
    app.pane(session)
        .map(|pane| {
            pane.entries()
                .iter()
                .map(|entry: &Entry| match entry {
                    Entry::User(text) | Entry::Assistant(text) | Entry::Notice(text) => {
                        text.clone()
                    }
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_typed_line_becomes_a_prompt_and_nothing_else() {
    let mut app = attached();

    assert_eq!(
        type_line(&mut app, "hello"),
        [sent(Request::Prompt {
            text: "hello".to_owned()
        })]
    );
    // Not echoed here: the daemon sends it back to everybody watching, and
    // echoing would show it twice in this terminal.
    assert!(
        !shown(&app, "s1").iter().any(|line| line == "hello"),
        "{:?}",
        shown(&app, "s1")
    );
}

#[test]
fn a_whole_reply_plays_into_the_pane() {
    let mut app = attached();

    for frame in [
        Frame::TurnStarted,
        Frame::Prompt {
            text: "hello".to_owned(),
        },
        Frame::Text {
            text: "hi back".to_owned(),
        },
        Frame::RunEnded {
            stop: aphid_core::StopReason::Stop,
            turns: 1,
            error: None,
        },
    ] {
        assert!(
            app.update(wire(Some("s1"), frame)).is_empty(),
            "watching a reply asks the daemon for nothing"
        );
    }

    let lines = shown(&app, "s1");
    assert!(lines.iter().any(|line| line == "hello"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "hi back"), "{lines:?}");
    assert!(!app.running());
}

#[test]
fn a_frame_for_another_session_does_not_touch_the_one_on_screen() {
    let mut app = attached();
    app.update(wire(
        Some("s2"),
        Frame::Text {
            text: "somewhere else".to_owned(),
        },
    ));

    assert!(
        !shown(&app, "s1")
            .iter()
            .any(|line| line == "somewhere else")
    );
    assert!(
        shown(&app, "s2")
            .iter()
            .any(|line| line == "somewhere else"),
        "but it is kept, for when that session is looked at"
    );
}

#[test]
fn a_permission_question_is_answered_by_its_id() {
    let mut app = attached();
    app.update(wire(
        None,
        Frame::Confirm {
            id: 7,
            tool: "bash".to_owned(),
            summary: "rm -rf build".to_owned(),
            risk: Risk::Destructive,
        },
    ));

    assert_eq!(
        press(&mut app, KeyCode::Char('n')),
        [sent(Request::Answer {
            id: 7,
            decision: Answer::Deny,
        })],
        "the daemon's own id goes back with the answer"
    );
}

#[test]
fn switching_session_asks_the_daemon_to_replay_it() {
    let mut app = attached();

    assert_eq!(
        type_line(&mut app, "/session s2"),
        [sent(Request::Watch {
            id: "s2".to_owned()
        })]
    );

    // And the replay clears whatever was drawn for it before.
    app.update(wire(
        Some("s2"),
        Frame::Text {
            text: "stale".to_owned(),
        },
    ));
    app.update(wire(
        None,
        Frame::HistoryStart {
            id: "s2".to_owned(),
        },
    ));
    assert!(shown(&app, "s2").is_empty(), "{:?}", shown(&app, "s2"));
}

#[test]
fn the_log_can_be_hidden_and_shown() {
    let mut app = attached();
    let notice = |text: &str| {
        wire(
            Some("s1"),
            Frame::Notice {
                text: text.to_owned(),
            },
        )
    };

    app.update(notice("first"));
    assert!(shown(&app, "s1").iter().any(|line| line == "first"));

    type_line(&mut app, "/log");
    app.update(notice("second"));
    assert!(
        !shown(&app, "s1").iter().any(|line| line == "second"),
        "hidden means hidden"
    );

    type_line(&mut app, "/log");
    app.update(notice("third"));
    assert!(shown(&app, "s1").iter().any(|line| line == "third"));
}

#[test]
fn a_command_the_terminal_does_not_know_goes_nowhere() {
    let mut app = attached();

    // A plugin in the daemon may own it, but this side cannot tell, so the
    // terminal says so rather than guessing.
    assert_eq!(type_line(&mut app, "/nope"), []);
    assert!(
        shown(&app, "s1")
            .iter()
            .any(|line| line.contains("no command /nope")),
        "{:?}",
        shown(&app, "s1")
    );
}

#[test]
fn detaching_stops_the_loop() {
    let mut app = attached();

    assert_eq!(type_line(&mut app, "/quit"), [Effect::Quit]);
    assert!(app.done());
}

#[test]
fn the_daemon_stopping_is_said_rather_than_silent() {
    let mut app = attached();
    app.update(wire(Some("s1"), Frame::TurnStarted));
    assert!(app.running());

    app.update(Msg::Stopped);

    assert!(!app.running(), "nothing is streaming any more");
    assert!(
        shown(&app, "s1")
            .iter()
            .any(|line| line.contains("the alate stopped")),
        "{:?}",
        shown(&app, "s1")
    );
}

/// A session list the daemon could have answered with.
fn listing() -> Frame {
    Frame::Sessions {
        live: vec![
            Info {
                id: "20260811T091500-0000".to_owned(),
                kind: "resident".to_owned(),
                started: "2026-08-11 09:15".to_owned(),
                running: true,
            },
            Info {
                id: "s1".to_owned(),
                kind: "attached".to_owned(),
                started: "2026-08-11 14:22".to_owned(),
                running: false,
            },
        ],
        stored: vec![Info {
            id: "20260810T090000-0000".to_owned(),
            kind: "cron: news".to_owned(),
            started: "2026-08-10 09:00".to_owned(),
            running: false,
        }],
    }
}

/// The ids the list on screen is offering, in order.
fn offered(app: &App) -> Vec<String> {
    app.picker()
        .map(|picker| {
            picker
                .shown()
                .iter()
                .map(|row| row.key.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[test]
fn sessions_opens_an_empty_list_and_asks_for_its_contents() {
    let mut app = attached();

    assert_eq!(type_line(&mut app, "/sessions"), [sent(Request::Sessions)]);
    assert!(app.picker().is_some(), "the list is on screen");
    assert!(offered(&app).is_empty(), "and it has nothing in it yet");
    assert!(
        shown(&app, "s1")
            .iter()
            .all(|line| !line.contains("resident")),
        "nothing is written into the transcript: the list is the answer"
    );
}

#[test]
fn the_answer_fills_the_list_that_asked_for_it() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    assert_eq!(
        offered(&app),
        ["20260811T091500-0000", "s1", "20260810T090000-0000"],
        "the running ones first, in the order the daemon gave"
    );
}

#[test]
fn typing_filters_the_list_and_backspace_puts_it_back() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    for c in "cron".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    assert_eq!(
        offered(&app),
        ["20260810T090000-0000"],
        "the filter reads the kind as well as the id"
    );

    for _ in 0..4 {
        press(&mut app, KeyCode::Backspace);
    }
    assert_eq!(offered(&app).len(), 3);
}

#[test]
fn choosing_from_the_list_watches_that_session() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    press(&mut app, KeyCode::Down);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        [sent(Request::Watch {
            id: "s1".to_owned()
        })],
        "the same request a typed /session makes"
    );
    assert!(app.picker().is_none(), "and the list is gone");
}

#[test]
fn escape_closes_the_list_and_asks_for_nothing() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    assert_eq!(press(&mut app, KeyCode::Esc), []);
    assert!(app.picker().is_none());
}

#[test]
fn a_session_opening_refreshes_the_list_on_screen() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    let opened = wire(
        None,
        Frame::SessionOpened {
            info: Info {
                id: "s9".to_owned(),
                kind: "cron: news".to_owned(),
                started: "2026-08-11 15:00".to_owned(),
                running: true,
            },
        },
    );
    assert_eq!(
        app.update(opened).into_effects(),
        [sent(Request::Sessions)],
        "a list being read must not go stale under the reader"
    );
}

#[test]
fn a_permission_question_takes_the_screen_from_the_list() {
    let mut app = attached();
    type_line(&mut app, "/sessions");
    app.update(wire(None, listing()));

    app.update(wire(
        None,
        Frame::Confirm {
            id: 7,
            tool: "bash".to_owned(),
            summary: "rm -rf build".to_owned(),
            risk: Risk::Destructive,
        },
    ));

    assert!(app.picker().is_none(), "the question is what is on screen");
    assert_eq!(
        press(&mut app, KeyCode::Char('n')),
        [sent(Request::Answer {
            id: 7,
            decision: Answer::Deny,
        })],
        "and the keys reach it, not the filter"
    );
}

#[test]
fn a_listing_that_outlives_its_list_is_still_written_down() {
    let mut app = attached();
    app.update(wire(None, listing()));

    assert!(
        shown(&app, "s1")
            .iter()
            .any(|line| line.contains("cron: news")),
        "an answer with nowhere to go is not lost in silence"
    );
}

/// Every frame a terminal can be sent reaches the model. A new one added to
/// the wire without a home here would be dropped in silence.
#[test]
fn no_frame_a_terminal_is_sent_is_dropped_in_silence() {
    let every = [
        Frame::TurnStarted,
        Frame::Text {
            text: "x".to_owned(),
        },
        Frame::Thinking {
            text: "x".to_owned(),
        },
        Frame::Prompt {
            text: "x".to_owned(),
        },
        Frame::Notice {
            text: "x".to_owned(),
        },
        Frame::Sessions {
            live: vec![],
            stored: vec![],
        },
        Frame::SessionOpened {
            info: Info {
                id: "s9".to_owned(),
                kind: "job".to_owned(),
                started: "now".to_owned(),
                running: false,
            },
        },
        Frame::SessionClosed {
            id: "s9".to_owned(),
        },
        Frame::Heartbeat {
            at: "now".to_owned(),
            note: "awake".to_owned(),
        },
    ];

    for frame in every {
        let mut app = attached();
        let before = shown(&app, "s1").len() + shown(&app, "s9").len();
        app.update(wire(Some("s1"), frame.clone()));
        let after = shown(&app, "s1").len() + shown(&app, "s9").len();
        assert!(
            after >= before,
            "{frame:?} left no trace anywhere the reader could find it"
        );
    }
}
