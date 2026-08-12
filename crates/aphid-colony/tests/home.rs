//! Where one colony lives, and what may name it.

mod common;

use aphid_colony::home::{self, Home};
use common::Temp;

#[test]
fn opening_a_hub_makes_its_directory() {
    let temp = Temp::new("home");
    let home = Home::open_in(&temp.root, "default").expect("a hub opens");

    assert_eq!(home.name(), "default");
    assert_eq!(home.root(), temp.root.join("default"));
    assert!(home.root().is_dir());

    // The files are named but not made: a hub that has never run should not
    // look as though it has.
    assert!(!home.config_file().exists());
    assert!(!home.database().exists());
    assert!(!home.relay_key().exists());
}

#[test]
fn a_hub_is_not_an_agent_workspace() {
    let temp = Temp::new("home");
    let home = Home::open_in(&temp.root, "default").expect("a hub opens");

    // A colony runs no agent, so it has none of the things an alate's home has.
    for absent in [".aphid", "AGENTS.md", "memory", "cron.json"] {
        assert!(
            !home.root().join(absent).exists(),
            "a colony has no {absent}"
        );
    }
}

#[test]
fn a_name_that_could_escape_the_root_is_refused() {
    let temp = Temp::new("home");
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "one/two",
        "one two",
        &"x".repeat(65),
    ] {
        let error = Home::open_in(&temp.root, bad).expect_err("{bad:?} is not a name");
        assert!(
            error.to_string().contains("cannot name a colony"),
            "{error}"
        );
    }
    for good in ["default", "work", "a-b_c.1"] {
        assert!(
            Home::open_in(&temp.root, good).is_ok(),
            "{good:?} is a name"
        );
    }
}

#[test]
fn a_missing_root_holds_no_hubs() {
    let temp = Temp::new("home");
    let nowhere = temp.path("never-made");
    assert_eq!(
        Home::list_in(&nowhere).expect("no root is no hubs"),
        Vec::<String>::new()
    );
}

#[test]
fn listing_gives_the_names_in_order_and_skips_what_is_not_a_hub() {
    let temp = Temp::new("home");
    for name in ["work", "default"] {
        Home::open_in(&temp.root, name).expect("a hub opens");
    }
    // Neither of these is a hub: one is a file, the other a name `open` refuses.
    std::fs::write(temp.path("a-file"), "").expect("write");
    std::fs::create_dir_all(temp.path(".hidden")).expect("dir");

    assert_eq!(
        Home::list_in(&temp.root).expect("lists"),
        vec!["default".to_owned(), "work".to_owned()]
    );
}

#[test]
fn every_file_is_inside_the_hub() {
    let temp = Temp::new("home");
    let home = Home::open_in(&temp.root, "default").expect("a hub opens");
    for path in [
        home.config_file(),
        home.relay_key(),
        home.human_key(),
        home.database(),
    ] {
        assert!(path.starts_with(home.root()), "{} escapes", path.display());
    }
}

#[test]
fn the_rules_for_a_name_are_the_alates_rules() {
    assert!(home::check_name("default").is_ok());
    assert!(home::check_name("..").is_err());
    assert_eq!(home::DIR_NAME, "colony");
    assert_eq!(home::DEFAULT_NAME, "default");
}
