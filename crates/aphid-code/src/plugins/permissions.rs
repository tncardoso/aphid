//! A permission gate for the tools that change things.
//!
//! Off unless asked for: the agent runs as you, on your machine, and prompting
//! for every `ls` teaches you to hit yes without reading. What this catches is
//! the small set of commands you would want to have been asked about.
//!
//! The decision itself is somebody else's job. [`Permissions`] holds a
//! [`Confirmer`] — the TUI shows a modal, headless mode refuses — so the policy
//! and the interface stay separate.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use aphid_agent::Blocked;
use aphid_agent::ToolRequest;
use aphid_agent::rt::{Bus, Component, Composition, Context, Disposer, Scope};
use tokio::runtime::RuntimeFlavor;

/// How much damage a command could do.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Risk {
    /// Reads and reports. Runs without asking.
    Read,
    /// Changes files or state.
    Mutate,
    /// Hard or impossible to undo.
    Destructive,
}

/// What the user said.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    /// Allow this, and anything identical for the rest of the session.
    AllowAlways,
    Deny,
}

/// Asks the user.
///
/// Called from the agent's task, synchronously, and expected to block until
/// there is an answer. An implementation that cannot ask should return
/// [`Decision::Deny`] rather than guess.
pub trait Confirmer: Send + Sync + 'static {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision;
}

/// Refuses everything it is asked about. What headless mode uses: there is no
/// terminal to ask at, and silently allowing would defeat the flag.
pub struct DenyAll;

impl Confirmer for DenyAll {
    fn confirm(&self, _tool: &str, _summary: &str, _risk: Risk) -> Decision {
        Decision::Deny
    }
}

/// Allows everything. Useful in tests and for a `--yolo` style override.
pub struct AllowAll;

impl Confirmer for AllowAll {
    fn confirm(&self, _tool: &str, _summary: &str, _risk: Risk) -> Decision {
        Decision::Allow
    }
}

/// Gates `bash`, `write` and `edit` behind a [`Confirmer`].
pub struct Permissions {
    confirmer: Arc<dyn Confirmer>,
    /// Commands the user said "always" to, verbatim.
    remembered: Mutex<HashSet<String>>,
}

impl Permissions {
    #[must_use]
    pub fn new(confirmer: Arc<dyn Confirmer>) -> Self {
        Self {
            confirmer,
            remembered: Mutex::new(HashSet::new()),
        }
    }

    /// Put the question, without stranding the runtime while it waits.
    ///
    /// [`Confirmer::confirm`] blocks until somebody answers, and it is called
    /// from inside the agent's task. A worker thread that blocks keeps whatever
    /// else was queued on it, and what was queued on it is the very work that
    /// carries the question out — the frames a terminal or a chat has to
    /// receive before anybody can answer. So the question would wait on its own
    /// answer until the timeout.
    ///
    /// [`block_in_place`] hands that work to another thread before blocking,
    /// which is the whole of the fix. It is only allowed on a multi-threaded
    /// runtime, so a current-thread one — a test, a headless run — blocks in
    /// place as before; there is no other worker for it to strand.
    ///
    /// [`block_in_place`]: tokio::task::block_in_place
    /// Whether this call needs refusing, asking the user if it has to.
    ///
    /// `None` means it may run: either nothing about it is worth a question, or
    /// the user has already said yes to exactly this.
    #[must_use]
    pub fn verdict(&self, tool: &str, arguments: &str) -> Option<Blocked> {
        let (risk, summary) = assess(tool, arguments)?;
        if risk == Risk::Read {
            return None;
        }

        let key = format!("{tool}\u{0}{summary}");
        if self
            .remembered
            .lock()
            .is_ok_and(|remembered| remembered.contains(&key))
        {
            return None;
        }

        match self.ask(tool, &summary, risk) {
            Decision::Allow => None,
            Decision::AllowAlways => {
                if let Ok(mut remembered) = self.remembered.lock() {
                    remembered.insert(key);
                }
                None
            }
            // Refusing is not an error to recover from by retrying, so the run
            // is asked to stop once the batch finishes.
            Decision::Deny => Some(
                Blocked::new(format!(
                    "The user declined to run this. Ask before trying again: {summary}"
                ))
                .and_stop(),
            ),
        }
    }

    fn ask(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let multi = tokio::runtime::Handle::try_current()
            .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread);
        if multi {
            tokio::task::block_in_place(|| self.confirmer.confirm(tool, summary, risk))
        } else {
            self.confirmer.confirm(tool, summary, risk)
        }
    }
}

/// Subscribes the gate to a composition.
pub struct PermissionGate {
    permissions: Arc<Permissions>,
    bus: Arc<Bus>,
    /// The conversation this gate answers for, or `None` for a standalone
    /// agent. Scoped so one session's tool calls are not answered by another
    /// session's gate — the shared `Permissions` still remembers "allow always"
    /// wherever it was decided.
    scope: Scope,
}

impl PermissionGate {
    #[must_use]
    pub fn new(scope: Scope, permissions: Arc<Permissions>, composition: &Composition) -> Self {
        Self {
            permissions,
            bus: Arc::clone(&composition.bus),
            scope,
        }
    }
}

impl Component for PermissionGate {
    fn name(&self) -> &str {
        "permissions"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();
        let permissions = Arc::clone(&self.permissions);

        self.bus
            .on_scoped::<ToolRequest>(self.scope.clone(), owner, move |request| {
                // Somebody already refused, so there is nothing left to ask about
                // — and asking anyway would put a question in front of the user
                // for a call that is not going to run either way.
                if request.is_blocked() {
                    return;
                }
                if let Some(blocked) = permissions.verdict(&request.name, &request.arguments) {
                    request.refuse(blocked);
                }
            });

        let bus = Arc::clone(&self.bus);
        ctx.effect(move || Disposer::sync(move || bus.unsubscribe::<ToolRequest>(owner)));
        Ok(())
    }
}

/// What a call would do, and a one-line description of it.
///
/// `None` means the tool is not gated at all.
#[must_use]
pub fn assess(tool: &str, arguments: &str) -> Option<(Risk, String)> {
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    match tool {
        "bash" => {
            let command = parsed.get("command")?.as_str()?;
            Some((classify(command), command.to_owned()))
        }
        "write" => {
            let path = parsed.get("path")?.as_str()?;
            Some((Risk::Mutate, format!("write {path}")))
        }
        "edit" => {
            let path = parsed.get("path")?.as_str()?;
            Some((Risk::Mutate, format!("edit {path}")))
        }
        _ => None,
    }
}

/// Commands that only look.
const READ_ONLY: &[&str] = &[
    "awk", "basename", "cat", "cd", "date", "df", "diff", "dirname", "du", "echo", "env", "fd",
    "file", "find", "grep", "head", "hostname", "jq", "less", "ls", "man", "nl", "od", "printenv",
    "ps", "pwd", "readlink", "realpath", "rg", "sort", "stat", "tail", "tree", "type", "uname",
    "uniq", "uptime", "wc", "which", "whoami", "xxd",
];

/// Subcommands that make an otherwise-ambiguous tool read-only.
const READ_ONLY_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "git",
        &[
            "status",
            "log",
            "diff",
            "show",
            "branch",
            "blame",
            "describe",
            "remote",
            "stash",
            "config",
            "rev-parse",
            "ls-files",
        ],
    ),
    (
        "cargo",
        &[
            "check", "test", "build", "clippy", "fmt", "tree", "metadata", "search", "bench", "doc",
        ],
    ),
];

/// Fragments that mark a command as hard to undo.
const DESTRUCTIVE: &[&str] = &[
    "rm -r",
    "rm -f",
    "rm -rf",
    "rm -fr",
    "sudo ",
    "doas ",
    "mkfs",
    "dd if=",
    "shutdown",
    "reboot",
    "chmod -R 777",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "git clean -fd",
    ":(){",
    "> /dev/sd",
];

/// Judge one shell command.
///
/// Segments are judged separately and the worst one wins, so
/// `ls && rm -rf build` is destructive rather than a read.
#[must_use]
pub fn classify(command: &str) -> Risk {
    // `str::split` takes one pattern, so the two-character operators are folded
    // onto a sentinel first and everything is split in one pass.
    const SEPARATOR: char = '\u{1}';
    let normalised = command.replace("&&", "\u{1}").replace("||", "\u{1}");
    normalised
        .split([SEPARATOR, ';', '|', '&', '\n'])
        .map(classify_segment)
        .max()
        .unwrap_or(Risk::Mutate)
}

fn classify_segment(segment: &str) -> Risk {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return Risk::Read;
    }

    let lowered = trimmed.to_ascii_lowercase();
    if DESTRUCTIVE.iter().any(|pattern| lowered.contains(pattern)) {
        return Risk::Destructive;
    }

    // A redirection writes, whatever the command in front of it is.
    if trimmed.contains('>') {
        return Risk::Mutate;
    }

    let mut words = trimmed.split_whitespace();
    // Skip leading `VAR=value` assignments.
    let Some(program) = words.find(|word| !word.contains('=')) else {
        return Risk::Mutate;
    };
    let program = program.rsplit('/').next().unwrap_or(program);

    if READ_ONLY.contains(&program) {
        return Risk::Read;
    }

    for (name, subcommands) in READ_ONLY_SUBCOMMANDS {
        if program == *name {
            return match subcommand(words.clone()) {
                Some(sub) if subcommands.contains(&sub) => Risk::Read,
                _ => Risk::Mutate,
            };
        }
    }

    Risk::Mutate
}

/// Global flags that swallow the word after them, so `git -C dir status` finds
/// `status` rather than `dir`.
const FLAGS_WITH_VALUES: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config",
    "--manifest-path",
];

/// The first word that is a subcommand rather than a flag or a flag's value.
fn subcommand<'a>(words: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut skip_next = false;
    for word in words {
        if skip_next {
            skip_next = false;
            continue;
        }
        if word.starts_with('-') {
            // `--flag=value` carries its own value; a bare flag may take the
            // next word.
            if !word.contains('=') && FLAGS_WITH_VALUES.contains(&word) {
                skip_next = true;
            }
            continue;
        }
        return Some(word);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_reads_run_without_asking() {
        for command in [
            "ls -la",
            "cat src/lib.rs",
            "rg TODO crates/",
            "git status",
            "git -C crates log --oneline",
            "cargo test -p aphid-core",
            "/usr/bin/wc -l Cargo.toml",
            "RUST_LOG=debug cargo check",
        ] {
            assert_eq!(classify(command), Risk::Read, "{command}");
        }
    }

    #[test]
    fn changes_are_flagged_as_mutating() {
        for command in [
            "cargo add serde",
            "git commit -m wip",
            "git push",
            "echo hi > file.txt",
            "mv a b",
            "npm install",
        ] {
            assert_eq!(classify(command), Risk::Mutate, "{command}");
        }
    }

    #[test]
    fn hard_to_undo_commands_are_destructive() {
        for command in [
            "rm -rf build",
            "sudo systemctl restart nginx",
            "git push --force origin main",
            "git reset --hard HEAD~3",
            "dd if=/dev/zero of=/dev/sda",
        ] {
            assert_eq!(classify(command), Risk::Destructive, "{command}");
        }
    }

    #[test]
    fn the_worst_segment_decides() {
        assert_eq!(classify("ls && rm -rf build"), Risk::Destructive);
        assert_eq!(classify("cat a.txt | grep x"), Risk::Read);
        assert_eq!(classify("cargo build; git commit -am wip"), Risk::Mutate);
    }

    #[test]
    fn writes_and_edits_are_gated_by_path() {
        let (risk, summary) = assess("write", r#"{"path":"src/lib.rs","content":"x"}"#).unwrap();
        assert_eq!(risk, Risk::Mutate);
        assert_eq!(summary, "write src/lib.rs");

        let (risk, summary) = assess("edit", r#"{"path":"a.rs","edits":[]}"#).unwrap();
        assert_eq!(risk, Risk::Mutate);
        assert_eq!(summary, "edit a.rs");
    }

    #[test]
    fn a_flags_value_is_not_mistaken_for_a_subcommand() {
        assert_eq!(
            subcommand("-C crates log --oneline".split(' ')),
            Some("log")
        );
        assert_eq!(subcommand("--git-dir=/x status".split(' ')), Some("status"));
        assert_eq!(subcommand("commit -m log".split(' ')), Some("commit"));
        assert_eq!(subcommand("--all".split(' ')), None);
    }

    #[test]
    fn ungated_tools_are_left_alone() {
        assert!(assess("read", r#"{"path":"a.rs"}"#).is_none());
        // Unparseable arguments are the loop's problem, not the gate's.
        assert!(assess("bash", "not json").is_none());
    }
}
