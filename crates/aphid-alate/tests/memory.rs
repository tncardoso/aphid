//! The memory: what it writes, what it finds again, and what it refuses.

mod common;

use aphid_alate::memory::{Memory, normalise, prompt_section};
use common::Temp;

fn open(temp: &Temp) -> Memory {
    Memory::open(&temp.path("memory")).expect("open")
}

#[test]
fn a_fact_comes_back() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory
        .store(
            "/projects/aphid",
            "The plugin API stays as small as it can be.",
        )
        .expect("store");

    let hits = memory.recall("plugin API", None, 5).expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/projects/aphid");
    assert!(hits[0].fact.contains("as small as it can be"));
    assert!(hits[0].score > 0.0);
}

#[test]
fn it_survives_being_reopened() {
    let temp = Temp::new("memory");
    open(&temp)
        .store(
            "/people/thiago",
            "Thiago writes documentation in ASD-STE100.",
        )
        .expect("store");

    // A new process would do exactly this, which is the whole promise of the
    // memory: what one session learned, the next one still knows.
    let hits = open(&temp)
        .recall("simplified english", None, 5)
        .expect("recall");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].fact.contains("ASD-STE100"));
}

#[test]
fn the_file_on_disk_is_the_shape_the_docs_claim() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory.store("/projects/aphid", "One.").expect("store");
    memory.store("/projects/aphid", "Two.").expect("store");

    let text = std::fs::read_to_string(temp.path("memory/projects/aphid.md")).expect("read");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    assert!(text.starts_with("# /projects/aphid\n"), "{text}");
    assert!(text.contains(&format!("- {today} — One.\n")), "{text}");
    // A second fact is added, never a replacement.
    assert!(text.contains(&format!("- {today} — Two.\n")), "{text}");
}

#[test]
fn a_fact_is_folded_onto_one_line() {
    // A bullet is a line, so a fact arriving with newlines would otherwise
    // become several facts, most of them nonsense.
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory
        .store("/notes", "First line\n\nsecond line\tand a tab")
        .expect("store");

    let hits = memory.recall("", None, 10).expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].fact, "First line second line and a tab");
}

#[test]
fn an_empty_query_gives_the_newest_first() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory
        .store("/a", "Older fact about boats.")
        .expect("store");
    memory
        .store("/b", "Newer fact about trains.")
        .expect("store");

    let hits = memory.recall("", None, 10).expect("recall");
    assert_eq!(hits.len(), 2);
}

#[test]
fn a_path_narrows_the_search() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory
        .store("/work/acme", "The build needs Rust.")
        .expect("store");
    memory
        .store("/home/garden", "The build needs water.")
        .expect("store");

    let hits = memory.recall("build", Some("/work"), 10).expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/work/acme");
}

#[test]
fn a_rare_word_beats_a_common_one() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    for index in 0..10 {
        memory
            .store("/noise", &format!("The build is the thing number {index}."))
            .expect("store");
    }
    memory
        .store("/signal", "The build uses embornal for storage.")
        .expect("store");

    let hits = memory.recall("build embornal", None, 3).expect("recall");
    assert_eq!(hits[0].path, "/signal", "{hits:?}");
}

#[test]
fn a_limit_of_zero_asks_for_nothing() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory.store("/a", "Something.").expect("store");
    assert!(
        memory
            .recall("something", None, 0)
            .expect("recall")
            .is_empty()
    );
}

#[test]
fn the_paths_are_the_map() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    memory.store("/projects/aphid", "One.").expect("store");
    memory.store("/people/thiago", "Two.").expect("store");
    memory.store("/projects/aphid", "Three.").expect("store");

    assert_eq!(
        memory.paths().expect("paths"),
        vec!["/people/thiago".to_owned(), "/projects/aphid".to_owned()]
    );
}

#[test]
fn a_path_cannot_leave_the_memory() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    for path in ["../escape", "/../escape", "/a/../../b", "/.hidden", "/"] {
        assert!(
            memory.store(path, "no").is_err(),
            "{path:?} should be refused"
        );
    }
    assert!(!temp.path("escape.md").exists());
}

#[test]
fn a_path_has_a_depth() {
    assert!(normalise("/a/b/c/d/e/f/g/h").is_ok());
    assert!(normalise("/a/b/c/d/e/f/g/h/i").is_err());
}

#[test]
fn a_path_is_written_one_way() {
    assert_eq!(normalise("projects/aphid").expect("ok"), "/projects/aphid");
    assert_eq!(
        normalise("/projects/aphid/").expect("ok"),
        "/projects/aphid"
    );
    assert_eq!(
        normalise(" /projects//aphid ").expect("ok"),
        "/projects/aphid"
    );
}

#[test]
fn an_empty_fact_is_refused() {
    let temp = Temp::new("memory");
    let mut memory = open(&temp);
    assert!(memory.store("/a", "   \n ").is_err());
}

#[test]
fn the_prompt_section_lists_the_paths_and_not_the_facts() {
    let section = prompt_section(&["/projects/aphid".to_owned()]).expect("some");
    assert!(section.contains("<memory_paths>"));
    assert!(section.contains("/projects/aphid"));
    assert!(section.contains("recall"));

    // Nothing to say when there is nothing filed, rather than an empty block
    // the model has to reason about.
    assert!(prompt_section(&[]).is_none());
}

#[test]
fn a_memory_edited_by_hand_still_reads() {
    // The files are in the workspace on purpose, so somebody will edit them.
    let temp = Temp::new("memory");
    temp.write(
        "memory/notes.md",
        "# /notes\n\nSome prose that is not a bullet.\n\n- A fact with no date at all.\n",
    );

    let hits = open(&temp).recall("fact", None, 5).expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].fact, "A fact with no date at all.");
}
