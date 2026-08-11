//! The instance home: its layout, and the rules that keep a name inside it.

mod common;

use aphid_alate::home::{Home, check_name};
use common::Temp;

#[test]
fn opening_makes_the_layout() {
    let temp = Temp::new("home");
    let home = Home::open_in(&temp.root, "work").expect("open");

    assert_eq!(home.name(), "work");
    assert_eq!(home.root(), temp.path("work"));
    assert!(home.memory_dir().is_dir());
    // Skills and plugins live where every workspace keeps them, which is what
    // lets the existing discovery find them with no new code.
    assert!(home.aphid_dir().join("skills").is_dir());
    assert!(home.aphid_dir().join("plugins").is_dir());

    // Not made until the alate has actually run, so an instance that never ran
    // does not look as though it did.
    assert!(!home.aphid_dir().join("sessions").exists());
}

#[test]
fn opening_twice_keeps_what_is_there() {
    let temp = Temp::new("home");
    let first = Home::open_in(&temp.root, "work").expect("open");
    std::fs::write(first.config_file(), "{}").expect("write");

    let again = Home::open_in(&temp.root, "work").expect("reopen");
    assert_eq!(first, again);
    assert_eq!(
        std::fs::read_to_string(again.config_file()).expect("read"),
        "{}"
    );
}

#[test]
fn a_name_cannot_leave_the_root() {
    let temp = Temp::new("home");
    for name in ["..", ".", "../escape", "a/b", "a\\b", "", ".hidden"] {
        assert!(
            Home::open_in(&temp.root, name).is_err(),
            "{name:?} should not name an alate"
        );
    }
    for name in ["work", "work-2", "work_2", "Work.2"] {
        assert!(check_name(name).is_ok(), "{name:?} should be allowed");
    }
}

#[test]
fn a_name_has_a_length() {
    assert!(check_name(&"a".repeat(64)).is_ok());
    assert!(check_name(&"a".repeat(65)).is_err());
}

#[test]
fn listing_finds_the_instances_in_order() {
    let temp = Temp::new("home");
    Home::open_in(&temp.root, "work").expect("open");
    Home::open_in(&temp.root, "home").expect("open");
    // A directory somebody made by hand under a name no alate could have is not
    // an instance: listing it would offer a name that `open` then refuses.
    std::fs::create_dir_all(temp.path(".git")).expect("dirs");
    std::fs::write(temp.path("stray"), "not a directory").expect("write");

    assert_eq!(
        Home::list_in(&temp.root).expect("list"),
        vec!["home".to_owned(), "work".to_owned()]
    );
}

#[test]
fn listing_a_root_that_is_not_there_is_no_instances() {
    let temp = Temp::new("home");
    assert!(
        Home::list_in(&temp.path("never-made"))
            .expect("list")
            .is_empty()
    );
}
