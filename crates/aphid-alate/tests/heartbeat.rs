//! The clock an alate keeps for itself.

mod common;

use aphid_alate::config::Heartbeat as Config;
use aphid_alate::heartbeat::{DEFAULT_PROMPT, Schedule};
use chrono::Utc;
use common::Temp;

fn every(text: &str) -> Config {
    Config {
        every: text.to_owned(),
        prompt: None,
    }
}

#[test]
fn a_wake_is_not_due_before_its_interval() {
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(&temp.path("state.json"), &every("15m"), None);

    assert!(schedule.due(Utc::now()).is_none());
    assert!(
        schedule
            .due(Utc::now() + chrono::Duration::minutes(14))
            .is_none()
    );
    assert!(
        schedule
            .due(Utc::now() + chrono::Duration::minutes(16))
            .is_some()
    );
}

#[test]
fn the_first_wake_is_measured_from_when_the_alate_started() {
    // The bug this is here for: with no wake recorded yet, reading the clock on
    // each check pushed the appointment one interval into the future every time
    // and it never arrived. The daemon asks with the real `now`, so nothing but
    // this caught it.
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(&temp.path("state.json"), &every("1s"), None);

    let first = schedule.next().expect("an interval means an appointment");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        schedule.next(),
        Some(first),
        "the appointment must not move"
    );

    std::thread::sleep(std::time::Duration::from_millis(1000));
    assert!(schedule.due(Utc::now()).is_some());
}

#[test]
fn a_wake_with_no_line_of_its_own_says_the_built_in_one() {
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(&temp.path("state.json"), &every("1s"), None);

    let note = schedule
        .due(Utc::now() + chrono::Duration::seconds(2))
        .expect("due");
    assert_eq!(note, DEFAULT_PROMPT);
}

#[test]
fn the_home_can_write_the_line() {
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(
        &temp.path("state.json"),
        &every("1s"),
        Some("Water the plants.".to_owned()),
    );

    let note = schedule
        .due(Utc::now() + chrono::Duration::seconds(2))
        .expect("due");
    assert_eq!(note, "Water the plants.");
}

#[test]
fn the_configuration_wins_over_the_file() {
    let temp = Temp::new("heartbeat");
    let config = Config {
        every: "1s".to_owned(),
        prompt: Some("From alate.json.".to_owned()),
    };
    let mut schedule = Schedule::open(
        &temp.path("state.json"),
        &config,
        Some("From HEARTBEAT.md.".to_owned()),
    );

    assert_eq!(
        schedule.due(Utc::now() + chrono::Duration::seconds(2)),
        Some("From alate.json.".to_owned())
    );
}

#[test]
fn taking_a_wake_starts_the_clock_again() {
    // A wake that came late must not immediately come again, which is what an
    // interval measured from the last wake and not from the last check buys.
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(&temp.path("state.json"), &every("15m"), None);

    let late = Utc::now() + chrono::Duration::hours(3);
    assert!(schedule.due(late).is_some());
    assert!(schedule.due(late).is_none());
    assert!(schedule.due(late + chrono::Duration::minutes(16)).is_some());
}

#[test]
fn a_pulse_can_be_turned_off_entirely() {
    // Then nothing wakes the alate on its own. Something that should happen at
    // a particular time is a cron entry, which runs in a session of its own;
    // the heartbeat is only the pulse.
    let temp = Temp::new("heartbeat");
    let mut schedule = Schedule::open(&temp.path("state.json"), &every("off"), None);

    assert!(schedule.next().is_none());
    assert!(schedule.due(Utc::now()).is_none());
    assert!(
        schedule
            .due(Utc::now() + chrono::Duration::days(365))
            .is_none()
    );
}

#[test]
fn the_last_wake_survives_a_restart() {
    // So a restart does not immediately wake an alate that woke a minute ago.
    let temp = Temp::new("heartbeat");
    let path = temp.path("state.json");

    let mut schedule = Schedule::open(&path, &every("1h"), None);
    let woke = Utc::now() + chrono::Duration::hours(2);
    assert!(schedule.due(woke).is_some());
    drop(schedule);

    let mut again = Schedule::open(&path, &every("1h"), None);
    assert_eq!(again.next(), Some(woke + chrono::Duration::hours(1)));
    assert!(again.due(woke + chrono::Duration::minutes(30)).is_none());
    assert!(again.due(woke + chrono::Duration::minutes(61)).is_some());
}

#[test]
fn a_state_file_that_makes_no_sense_is_a_fresh_clock() {
    // The file is the agent's note to itself; a corrupt one must not stop it
    // starting.
    let temp = Temp::new("heartbeat");
    let path = temp.write("state.json", "{ this is not json");

    let schedule = Schedule::open(&path, &every("15m"), None);
    assert!(schedule.next().is_some());
}
