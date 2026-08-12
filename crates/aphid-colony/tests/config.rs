//! `colony.json`, and the keys beside it.

mod common;

use aphid_colony::config::{self, Config};
use aphid_colony::identity;
use common::Temp;

#[test]
fn a_missing_file_is_the_defaults() {
    let temp = Temp::new("config");
    let config = Config::load(&temp.path("colony.json")).expect("a missing file is the defaults");
    assert_eq!(config, Config::default());
    assert_eq!(config.listen, config::DEFAULT_LISTEN);
    assert_eq!(config.channels, vec!["general".to_owned()]);
}

#[test]
fn an_empty_file_is_the_defaults_too() {
    // That is what a truncated write leaves behind, and it should not stop a
    // colony from starting.
    let temp = Temp::new("config");
    for text in ["", "\n", "   \n\n"] {
        let path = temp.write("colony.json", text);
        assert_eq!(
            Config::load(&path).expect("an empty file"),
            Config::default()
        );
    }
}

#[test]
fn a_newer_version_is_refused_by_name() {
    let temp = Temp::new("config");
    let path = temp.write("colony.json", r#"{"version": 99}"#);
    let error = Config::load(&path).expect_err("a newer file is refused");
    assert!(error.to_string().contains("version 99"), "{error}");
    assert!(error.to_string().contains("newer aphid"), "{error}");
}

#[test]
fn a_file_that_is_not_json_says_where() {
    let temp = Temp::new("config");
    let path = temp.write("colony.json", "{not json");
    let error = Config::load(&path).expect_err("this is not json");
    assert!(error.to_string().contains("colony.json"), "{error}");
}

#[test]
fn a_configuration_round_trips() {
    let temp = Temp::new("config");
    let path = temp.path("colony.json");
    let config = Config {
        version: 1,
        listen: "127.0.0.1:9999".to_owned(),
        name: Some("thiago".to_owned()),
        channels: vec!["general".to_owned(), "build".to_owned()],
        history: 100,
    };

    config.save(&path).expect("saves");
    assert_eq!(Config::load(&path).expect("loads"), config);
}

#[test]
fn a_partial_file_keeps_the_defaults_for_everything_else() {
    let temp = Temp::new("config");
    let path = temp.write("colony.json", r#"{"name": "thiago"}"#);
    let config = Config::load(&path).expect("loads");
    assert_eq!(config.name.as_deref(), Some("thiago"));
    assert_eq!(config.listen, config::DEFAULT_LISTEN);
    assert_eq!(config.history, config::DEFAULT_HISTORY);
}

#[test]
fn a_listen_that_is_not_an_address_is_a_sentence() {
    let config = Config {
        listen: "port seven".to_owned(),
        ..Config::default()
    };
    let error = config.address().expect_err("this is not an address");
    assert!(error.contains("127.0.0.1:7777"), "{error}");

    assert_eq!(
        Config::default().address().expect("the default listens"),
        "127.0.0.1:7777".parse().expect("an address")
    );
}

#[test]
fn the_url_follows_the_listen_address() {
    let config = Config {
        listen: "127.0.0.1:9999".to_owned(),
        ..Config::default()
    };
    assert_eq!(config.url().expect("a url"), "ws://127.0.0.1:9999");
    assert_eq!(
        Config::default().url().expect("a url"),
        "ws://127.0.0.1:7777"
    );
}

#[test]
fn a_bind_address_is_not_a_connect_address() {
    // `0.0.0.0` and `::` say "every interface", which is an instruction to
    // bind and not somewhere a terminal can dial. It gets loopback instead.
    let every = Config {
        listen: "0.0.0.0:7777".to_owned(),
        ..Config::default()
    };
    assert_eq!(every.url().expect("a url"), "ws://127.0.0.1:7777");

    let every_six = Config {
        listen: "[::]:7777".to_owned(),
        ..Config::default()
    };
    assert_eq!(
        every_six.url().expect("a url"),
        "ws://[::1]:7777",
        "an IPv6 host keeps its brackets"
    );
}

#[test]
fn a_url_from_a_listen_that_is_not_an_address_is_the_same_sentence() {
    let config = Config {
        listen: "port seven".to_owned(),
        ..Config::default()
    };
    let error = config.url().expect_err("this is not an address");
    assert!(error.contains("127.0.0.1:7777"), "{error}");
}

#[test]
fn a_key_is_made_once_and_read_back() {
    let temp = Temp::new("config");
    let path = temp.path("relay.key");

    let made = identity::open(&path).expect("a missing key is made");
    assert!(path.exists());
    let again = identity::open(&path).expect("and read back");
    assert_eq!(made.public_key(), again.public_key());
}

#[cfg(unix)]
#[test]
fn a_key_is_readable_by_nobody_else() {
    use std::os::unix::fs::PermissionsExt;

    let temp = Temp::new("config");
    let path = temp.path("relay.key");
    identity::open(&path).expect("a key is made");

    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "a secret key is the owner's alone");
}

#[test]
fn a_key_file_that_is_not_a_key_is_refused_by_name() {
    let temp = Temp::new("config");
    let path = temp.write("relay.key", "this is not a key\n");
    let error = identity::open(&path).expect_err("this is not a key");
    assert!(error.to_string().contains("delete it"), "{error}");
}
