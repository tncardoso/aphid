//! `alate.json`, and the durations written in it.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use aphid_alate::config::{
    Config, Heartbeat, MemoryConfig, Permissions, TOKEN_ENV, Telegram, Thinking, duration,
};
use common::Temp;

#[test]
fn a_missing_file_is_the_defaults() {
    let temp = Temp::new("config");
    let config = Config::load(&temp.path("nothing.json")).expect("load");
    assert_eq!(config, Config::default());
    assert_eq!(config.permissions, Permissions::Ask);
    assert_eq!(config.thinking, Some(Thinking::Medium));
    assert_eq!(config.memory.recall, 5);
}

#[test]
fn an_empty_file_is_the_defaults_too() {
    // What a truncated write leaves behind. Reporting a parse error nobody can
    // act on would be worse than starting with what a new alate starts with.
    let temp = Temp::new("config");
    let path = temp.write("alate.json", "   \n");
    assert_eq!(Config::load(&path).expect("load"), Config::default());
}

#[test]
fn a_partial_file_keeps_every_other_default() {
    let temp = Temp::new("config");
    let path = temp.write("alate.json", r#"{"model": "deepseek-chat"}"#);
    let config = Config::load(&path).expect("load");

    assert_eq!(config.model.as_deref(), Some("deepseek-chat"));
    assert_eq!(config.memory.recall, 5);
    assert_eq!(config.gateway.socket, None);
    assert_eq!(config.gateway.telegram, None);
    assert_eq!(config.heartbeat.every, "15m");
}

#[test]
fn a_bot_is_off_until_it_is_written_down() {
    let temp = Temp::new("config");
    let path = temp.write("alate.json", r#"{"gateway": {"telegram": {}}}"#);
    let config = Config::load(&path).expect("load");
    let bot = config.gateway.telegram.expect("a bot");

    assert_eq!(bot, Telegram::default());
    assert_eq!(bot.token_env, TOKEN_ENV);
    // Whoever reaches the bot can make the agent run commands, so an allow list
    // that was not written allows nobody.
    assert!(bot.chats.is_empty());
    assert!(!bot.tools);
    assert_eq!(bot.interval().expect("a poll"), Duration::from_secs(25));
}

#[test]
fn a_poll_of_no_length_is_refused() {
    // `off` is a heartbeat that never fires. A poll that never waits is a loop
    // asking Telegram as fast as it can answer, which is a different thing.
    let bot = Telegram {
        poll: "off".to_owned(),
        ..Telegram::default()
    };
    assert!(bot.interval().is_err());
}

#[test]
fn a_bot_keeps_what_was_written_for_it() {
    let temp = Temp::new("config");
    let path = temp.write(
        "alate.json",
        r#"{"gateway": {"telegram": {"chats": [42, -100], "tools": true, "poll": "5s"}}}"#,
    );
    let bot = Config::load(&path)
        .expect("load")
        .gateway
        .telegram
        .expect("a bot");

    // Negative ids are groups, and are as ordinary as any other.
    assert_eq!(bot.chats, vec![42, -100]);
    assert!(bot.tools);
    assert_eq!(bot.interval().expect("a poll"), Duration::from_secs(5));
}

#[test]
fn it_round_trips() {
    let temp = Temp::new("config");
    let path = temp.path("alate.json");

    let config = Config {
        model: Some("some-model".to_owned()),
        permissions: Permissions::Allow,
        heartbeat: Heartbeat {
            every: "2h".to_owned(),
            prompt: Some("Look around.".to_owned()),
        },
        memory: MemoryConfig { recall: 9 },
        environment: BTreeMap::from([("MODE".to_owned(), "production".to_owned())]),
        ..Config::default()
    };
    config.save(&path).expect("save");

    assert_eq!(Config::load(&path).expect("load"), config);
}

#[test]
fn a_newer_version_is_refused_by_name() {
    let temp = Temp::new("config");
    let path = temp.write("alate.json", r#"{"version": 99}"#);
    let error = Config::load(&path).expect_err("refused").to_string();
    assert!(error.contains("99"), "{error}");
    assert!(error.contains("alate.json"), "{error}");
}

#[test]
fn durations_read_the_way_they_look() {
    assert_eq!(duration("30s"), Ok(Some(Duration::from_secs(30))));
    assert_eq!(duration("15m"), Ok(Some(Duration::from_secs(900))));
    assert_eq!(duration("2h"), Ok(Some(Duration::from_secs(7200))));
    assert_eq!(duration("1d"), Ok(Some(Duration::from_secs(86400))));
    // A bare number is seconds, because the alternative is guessing.
    assert_eq!(duration("45"), Ok(Some(Duration::from_secs(45))));
    assert_eq!(duration(" 15m "), Ok(Some(Duration::from_secs(900))));
}

#[test]
fn a_heartbeat_can_be_turned_off() {
    for text in ["off", "never", "none", "0", "", "0m"] {
        assert_eq!(duration(text), Ok(None), "{text:?}");
    }
}

#[test]
fn a_duration_that_is_not_one_says_so() {
    assert!(duration("soon").is_err());
    assert!(duration("15 fortnights").is_err());
    assert!(duration("m15").is_err());
}

#[test]
fn the_heartbeat_reads_its_own_interval() {
    let config = Config::default();
    assert_eq!(
        config.heartbeat.interval(),
        Ok(Some(Duration::from_secs(900)))
    );
}

#[test]
fn a_colony_configuration_round_trips() {
    let temp = Temp::new("config-colony");
    let path = temp.path("alate.json");

    let mut config = Config::default();
    config.gateway.colony = Some(aphid_alate::config::Colony {
        relay: "ws://127.0.0.1:9999".to_owned(),
        key_env: "SCOUT_COLONY_KEY".to_owned(),
        channels: vec!["general".to_owned(), "build".to_owned()],
        name: Some("scout".to_owned()),
        mentions: true,
        retry: "10s".to_owned(),
    });

    config.save(&path).expect("saves");
    assert_eq!(Config::load(&path).expect("loads"), config);
}

#[test]
fn an_alate_json_without_a_colony_still_parses() {
    // The struct is compiled into every build, colony feature or not, so one
    // alate.json is the same file whichever build reads it.
    let temp = Temp::new("config-colony");
    let path = temp.write("alate.json", r#"{"version": 1, "gateway": {}}"#);
    let config = Config::load(&path).expect("loads");
    assert!(config.gateway.colony.is_none());
}

#[test]
fn a_colony_retry_that_is_not_a_length_of_time_is_a_sentence() {
    let colony = aphid_alate::config::Colony {
        retry: "off".to_owned(),
        ..aphid_alate::config::Colony::default()
    };
    let error = colony
        .interval()
        .expect_err("a wait of no length is a busy loop");
    assert!(error.contains("gateway.colony.retry"), "{error}");
}
