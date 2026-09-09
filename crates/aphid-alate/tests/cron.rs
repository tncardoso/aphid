//! The crontab: what it accepts, what it refuses, and when it fires.

mod common;

use aphid_alate::cron::{Crontab, MAX_ENTRIES, parse};
use aphid_alate::gateway::Origin;
use chrono::{Local, TimeZone};
use common::Temp;

/// A daily job, set for twelve hours from now.
///
/// A test that measures from `Local::now()` needs the next occurrence to be
/// nowhere near now, and a fixed `0 9 * * *` is only far away depending on what
/// time the suite happens to run: between eight and nine in the morning, a job
/// that has just caught up comes due again an hour later, and the test fails on
/// the clock rather than on the code.
fn daily_in_twelve_hours() -> String {
    use chrono::Timelike;
    format!("0 {} * * *", (Local::now().hour() + 12) % 24)
}

fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> chrono::DateTime<Local> {
    Local
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("an unambiguous local time")
}

#[test]
fn a_missing_file_is_an_empty_crontab() {
    let temp = Temp::new("cron");
    let (crontab, problems) = Crontab::open(&temp.path("cron.json"));
    assert!(crontab.entries().is_empty());
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_job_round_trips() {
    let temp = Temp::new("cron");
    let path = temp.path("cron.json");

    let (mut crontab, _) = Crontab::open(&path);
    crontab
        .set("morning", "0 9 * * *", "Read yesterday's notes.", None)
        .expect("set");

    let (again, problems) = Crontab::open(&path);
    assert!(problems.is_empty(), "{problems:?}");
    let entry = again.find("morning").expect("kept");
    assert_eq!(entry.schedule, "0 9 * * *");
    assert_eq!(entry.prompt, "Read yesterday's notes.");
}

#[test]
fn a_job_is_replaced_by_name() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));

    crontab
        .set("job", "0 9 * * *", "First.", None)
        .expect("set");
    crontab
        .set("job", "0 17 * * *", "Second.", None)
        .expect("set");

    assert_eq!(crontab.entries().len(), 1);
    let entry = crontab.find("job").expect("kept");
    assert_eq!(entry.schedule, "0 17 * * *");
    assert_eq!(entry.prompt, "Second.");
}

#[test]
fn a_job_can_be_removed() {
    let temp = Temp::new("cron");
    let path = temp.path("cron.json");

    let (mut crontab, _) = Crontab::open(&path);
    crontab
        .set("job", "0 9 * * *", "Something.", None)
        .expect("set");
    assert!(crontab.remove("job"));
    assert!(!crontab.remove("job"), "removing twice does nothing");

    let (again, _) = Crontab::open(&path);
    assert!(again.entries().is_empty());
}

#[test]
fn five_fields_are_the_dialect() {
    assert!(parse("0 9 * * *").is_ok());
    assert!(parse("*/15 * * * MON-FRI").is_ok());
    assert!(parse("0 0 1 1 *").is_ok());

    // Seconds are refused on purpose: croner would take `0 0 9 * * *` as a
    // six-field pattern with seconds, and a job an hour early every day is a
    // worse answer than a message.
    let error = parse("0 0 9 * * *").expect_err("six fields are refused");
    assert!(error.contains("five fields"), "{error}");

    assert!(parse("").is_err());
    assert!(parse("not a schedule").is_err());
    assert!(parse("99 * * * *").is_err());
}

#[test]
fn a_job_needs_a_schedule_that_parses() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));

    assert!(crontab.set("job", "whenever", "Something.", None).is_err());
    assert!(crontab.set("job", "0 9 * * *", "   ", None).is_err());
    assert!(
        crontab
            .set("../escape", "0 9 * * *", "Something.", None)
            .is_err()
    );
    assert!(crontab.entries().is_empty());
}

#[test]
fn a_schedule_that_stopped_parsing_is_dropped_and_said_out_loud() {
    // A crontab edited by hand must not stop the alate, and must not be
    // silently ignored either.
    let temp = Temp::new("cron");
    let path = temp.write(
        "cron.json",
        r#"{"version":1,"entries":[
            {"name":"good","schedule":"0 9 * * *","prompt":"Fine."},
            {"name":"bad","schedule":"every tuesday-ish","prompt":"Not fine."}
        ]}"#,
    );

    let (crontab, problems) = Crontab::open(&path);
    assert_eq!(crontab.entries().len(), 1);
    assert!(crontab.find("good").is_some());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("bad"), "{}", problems[0]);
}

#[test]
fn a_file_that_makes_no_sense_is_reported_not_swallowed() {
    let temp = Temp::new("cron");
    let path = temp.write("cron.json", "{ this is not json");
    let (crontab, problems) = Crontab::open(&path);
    assert!(crontab.entries().is_empty());
    assert_eq!(problems.len(), 1, "{problems:?}");
}

#[test]
fn a_newer_version_is_refused_by_name() {
    let temp = Temp::new("cron");
    let path = temp.write("cron.json", r#"{"version":99,"entries":[]}"#);
    let (_crontab, problems) = Crontab::open(&path);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("99"), "{}", problems[0]);
}

#[test]
fn nothing_is_due_before_its_time() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    crontab
        .set("morning", &daily_in_twelve_hours(), "Wake.", None)
        .expect("set");

    // The crontab was opened now, and an entry that never ran measures from
    // there, so nothing fires the moment a daemon starts.
    assert!(crontab.due(Local::now()).is_empty());
}

#[test]
fn a_job_fires_once_when_its_time_has_passed() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    crontab
        .set("morning", "0 9 * * *", "Wake.", None)
        .expect("set");

    let tomorrow = Local::now() + chrono::Duration::days(1);
    let fired = crontab.due(tomorrow);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].name, "morning");
    assert_eq!(fired[0].prompt, "Wake.");

    // And not again for the same occurrence.
    assert!(crontab.due(tomorrow).is_empty());
}

#[test]
fn a_week_of_downtime_is_one_run_and_not_seven() {
    // The behaviour people ask about. `last` moves to now rather than to the
    // occurrence that was missed, so a daily job catches up exactly once.
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    crontab
        .set("daily", &daily_in_twelve_hours(), "Wake.", None)
        .expect("set");

    let next_week = Local::now() + chrono::Duration::days(7);
    assert_eq!(crontab.due(next_week).len(), 1);
    assert!(crontab.due(next_week).is_empty());
    assert!(
        crontab
            .due(next_week + chrono::Duration::hours(1))
            .is_empty()
    );
}

#[test]
fn a_rewritten_job_starts_its_clock_again() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    crontab
        .set("job", "0 9 * * *", "First.", None)
        .expect("set");

    let tomorrow = Local::now() + chrono::Duration::days(1);
    assert_eq!(crontab.due(tomorrow).len(), 1);

    // The old `last` belonged to a schedule that no longer exists.
    crontab
        .set("job", "0 17 * * *", "Second.", None)
        .expect("set");
    assert!(crontab.find("job").expect("kept").last.is_none());
}

#[test]
fn the_next_run_is_the_one_the_expression_names() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    let entry = crontab
        .set("nine", "0 9 * * *", "Wake.", None)
        .expect("set");

    // From eight in the morning, nine the same day.
    let next = crontab
        .next_for(&entry, at(2026, 8, 11, 8, 0))
        .expect("an occurrence");
    assert_eq!(next, at(2026, 8, 11, 9, 0));

    // From ten, nine the next day.
    let next = crontab
        .next_for(&entry, at(2026, 8, 11, 10, 0))
        .expect("an occurrence");
    assert_eq!(next, at(2026, 8, 12, 9, 0));
}

#[test]
fn a_schedule_is_read_in_local_time() {
    // What `0 9 * * *` means is nine where the machine is. Asserted through the
    // local offset rather than assumed, so a machine in another zone still
    // proves the same thing.
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    let entry = crontab
        .set("nine", "0 9 * * *", "Wake.", None)
        .expect("set");

    let next = crontab
        .next_for(&entry, at(2026, 8, 11, 0, 1))
        .expect("an occurrence");
    assert_eq!(next.format("%H:%M").to_string(), "09:00");
}

#[test]
fn the_prompt_section_says_what_is_scheduled() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    assert!(crontab.prompt_section().is_none(), "nothing to say yet");

    crontab
        .set("morning", "0 9 * * *", "Read the notes.", None)
        .expect("set");
    let section = crontab.prompt_section().expect("some");

    assert!(section.contains("<scheduled_jobs>"), "{section}");
    assert!(section.contains("morning"), "{section}");
    assert!(section.contains("0 9 * * *"), "{section}");
    assert!(section.contains("Read the notes."), "{section}");
    // And what the expression means in words, so the model can check itself.
    assert!(section.contains('9'), "{section}");
}

#[test]
fn there_is_a_limit_on_how_many_jobs_there_can_be() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    for index in 0..MAX_ENTRIES {
        crontab
            .set(&format!("job-{index}"), "0 9 * * *", "Something.", None)
            .expect("set");
    }
    assert!(
        crontab
            .set("one-more", "0 9 * * *", "Something.", None)
            .is_err()
    );
    // Replacing one that exists is still allowed at the limit.
    assert!(crontab.set("job-0", "0 10 * * *", "Changed.", None).is_ok());
}

#[test]
fn an_overdue_job_fires_at_once() {
    // What the daemon relies on: a job whose last run is in the past is due the
    // moment the clock is next read, and does not wait for a fresh boundary.
    let temp = Temp::new("cron");
    let yesterday = (Local::now() - chrono::Duration::days(1)).to_rfc3339();
    let path = temp.write(
        "cron.json",
        &format!(
            r#"{{"version":1,"entries":[
                {{"name":"sweep","schedule":"* * * * *",
                  "prompt":"Check on things.","last":"{yesterday}"}}
            ]}}"#
        ),
    );

    let (mut crontab, problems) = Crontab::open(&path);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(crontab.entries().len(), 1);

    let fired = crontab.due(Local::now());
    assert_eq!(fired.len(), 1, "an overdue job should fire at once");
}

/// A daemon that has been up for thirteen hours.
///
/// Long enough that the occurrence twelve hours back — the other side of
/// [`daily_in_twelve_hours`] — falls inside the window, whatever minute of the
/// hour the suite runs at.
fn started_thirteen_hours_ago() -> chrono::DateTime<Local> {
    Local::now() - chrono::Duration::hours(13)
}

#[test]
fn a_job_written_after_the_daemon_started_waits_for_its_schedule() {
    // The occurrence twelve hours ago belongs to a job that did not exist
    // then. A job is measured from when it was written, not from when the
    // daemon came up, so nothing is due yet.
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open_at(&temp.path("cron.json"), started_thirteen_hours_ago());

    let entry = crontab
        .set("morning", &daily_in_twelve_hours(), "Wake.", None)
        .expect("set");
    let promised = crontab
        .next_for(&entry, Local::now())
        .expect("an occurrence");

    assert!(
        crontab.due(Local::now()).is_empty(),
        "a job just written must wait for {promised}, not fire at once"
    );
}

#[test]
fn a_job_written_after_the_daemon_started_still_fires_when_it_is_due() {
    // The other half: waiting is not the same as never running.
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open_at(&temp.path("cron.json"), started_thirteen_hours_ago());

    let entry = crontab
        .set("morning", &daily_in_twelve_hours(), "Wake.", None)
        .expect("set");
    let promised = crontab
        .next_for(&entry, Local::now())
        .expect("an occurrence");

    let fired = crontab.due(promised);
    assert_eq!(fired.len(), 1, "the job should run at {promised}");
    assert_eq!(fired[0].name, "morning");
}

#[test]
fn a_rewritten_job_waits_for_its_new_schedule() {
    // Rewriting clears `last`, and the entry must then measure from the
    // rewrite rather than falling back to the start of the daemon.
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open_at(&temp.path("cron.json"), started_thirteen_hours_ago());

    let entry = crontab
        .set("job", &daily_in_twelve_hours(), "First.", None)
        .expect("set");
    let promised = crontab
        .next_for(&entry, Local::now())
        .expect("an occurrence");
    assert_eq!(crontab.due(promised).len(), 1);

    crontab
        .set("job", &daily_in_twelve_hours(), "Second.", None)
        .expect("set");
    assert!(
        crontab.due(Local::now()).is_empty(),
        "a rewritten job must wait as a new one does"
    );
}

#[test]
fn an_entry_from_an_older_file_measures_from_the_open() {
    // A crontab written by an older aphid, or by hand, says nothing about when
    // its jobs were written. Those fall back to the open, which is what a
    // daemon that restarts gives them: one catch-up run, and no more.
    let temp = Temp::new("cron");
    let path = temp.write(
        "cron.json",
        &format!(
            r#"{{"version":1,"entries":[
                {{"name":"morning","schedule":"{}","prompt":"Wake."}}
            ]}}"#,
            daily_in_twelve_hours()
        ),
    );

    let (mut crontab, problems) = Crontab::open_at(&path, started_thirteen_hours_ago());
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(crontab.due(Local::now()).len(), 1);
}

#[test]
fn a_job_remembers_the_conversation_that_wrote_it() {
    let temp = Temp::new("cron");
    let path = temp.path("cron.json");
    let origin = Origin::new("20260909T090000-0000", "telegram: 42");

    let (mut crontab, _) = Crontab::open(&path);
    crontab
        .set("morning", "0 9 * * *", "Say good morning.", Some(&origin))
        .expect("set");

    let (again, problems) = Crontab::open(&path);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(again.find("morning").expect("the job").origin, Some(origin));
}

#[test]
fn a_job_written_by_hand_answers_nowhere() {
    let temp = Temp::new("cron");
    let path = temp.path("cron.json");
    temp.write(
        "cron.json",
        r#"{"version":1,"entries":[
            {"name":"typed","schedule":"0 9 * * *","prompt":"Wake."}
        ]}"#,
    );

    let (crontab, problems) = Crontab::open(&path);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(crontab.find("typed").expect("the job").origin, None);
}

#[test]
fn a_rewritten_job_answers_where_it_was_rewritten() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    let first = Origin::new("s1", "telegram: 42");
    let second = Origin::new("s2", "colony: #general");

    crontab
        .set("job", "0 9 * * *", "First.", Some(&first))
        .expect("set");
    crontab
        .set("job", "0 9 * * *", "Second.", Some(&second))
        .expect("set");

    assert_eq!(crontab.find("job").expect("the job").origin, Some(second));
}

#[test]
fn the_prompt_section_says_where_a_job_answers() {
    let temp = Temp::new("cron");
    let (mut crontab, _) = Crontab::open(&temp.path("cron.json"));
    crontab
        .set(
            "morning",
            "0 9 * * *",
            "Read the notes.",
            Some(&Origin::new("s1", "telegram: 42")),
        )
        .expect("set");

    let section = crontab.prompt_section().expect("a section");
    assert!(
        section.contains("answers in telegram: 42"),
        "the section says where it answers: {section}"
    );
}
