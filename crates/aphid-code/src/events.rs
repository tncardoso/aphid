//! What the coding harness announces.
//!
//! The agent loop announces what a run does. These are the things it has no
//! word for — a permission, a file changed, a session opened — because they are
//! this crate's ideas, not the loop's.
//!
//! Being this crate's ideas is not a reason for them to be a second mechanism.
//! They are declared here, they go on the same bus, and a component subscribes
//! to them the same way. Anything that wanted its own dispatch table would be
//! one more thing to keep in step with the first.

use std::path::PathBuf;

use aphid_agent::rt::{Bailed, Emitted, Event, Failure, Waterfalled};

use crate::plugins::permissions::Risk;

/// The system prompt, before anything sees it.
///
/// A **waterfall**: each listener receives the prompt as it stands and returns
/// what the next one should see, so appending and replacing are the same
/// operation seen from two ends. Fires once, while the harness is being built,
/// which makes it the only announcement that happens before an agent exists.
#[derive(Debug)]
pub struct SystemPrompt;

impl Event for SystemPrompt {
    const NAME: &'static str = "code/system-prompt";
}
impl Waterfalled for SystemPrompt {
    type In = String;
    type Out = String;
}

/// Time passed.
///
/// The only announcement the agent does not cause, so it is what a component
/// watching something outside the session — a file, a queue, a clock — is woken
/// by. Not reentrant: a tick that is still being handled is not announced
/// again, so a slow listener costs its own time rather than a growing queue of
/// ticks behind it.
#[derive(Debug)]
pub struct Tick;

impl Event for Tick {
    const NAME: &'static str = "code/tick";
    const REENTRANT: bool = false;
}
impl Emitted for Tick {}

/// A session opened or is closing.
#[derive(Clone, Debug)]
pub struct Session {
    pub id: Option<String>,
    pub path: Option<PathBuf>,
    /// `"new"` or `"resume"`.
    pub reason: String,
    /// How many messages a resume restored.
    pub restored: usize,
}

/// A session opened.
#[derive(Debug)]
pub struct SessionStart(pub Session);
impl Event for SessionStart {
    const NAME: &'static str = "code/session-start";
}
impl Emitted for SessionStart {}

/// A session is closing.
///
/// Every component's state is written back afterwards, so a listener that saves
/// on the way out is not too late.
#[derive(Debug)]
pub struct SessionEnd(pub Session);
impl Event for SessionEnd {
    const NAME: &'static str = "code/session-end";
}
impl Emitted for SessionEnd {}

/// What a listener decided about a tool that wants permission.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Permission {
    Allow,
    /// Allow, and stop asking about this exact call.
    AllowAlways,
    Deny,
}

impl Permission {
    #[must_use]
    pub fn parse(text: &str) -> Option<Permission> {
        match text {
            "allow" => Some(Permission::Allow),
            "allow_always" => Some(Permission::AllowAlways),
            "deny" => Some(Permission::Deny),
            // "ask" is *no* opinion, which is what `None` already means.
            _ => None,
        }
    }
}

/// A tool needs permission to run.
///
/// A **bail**: the first listener with an opinion decides and the rest do not
/// run, because a second opinion on a settled question is a second question for
/// the user. Failure is closed — a guard that raised has not approved anything,
/// and this is the announcement people subscribe to when they mean to be
/// careful.
#[derive(Clone, Debug)]
pub struct Ask {
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
}

impl Event for Ask {
    const NAME: &'static str = "code/permission";
    const FAILURE: Failure = Failure::Closed;
}
impl Bailed for Ask {
    type Out = Permission;
}

/// How a file came to change.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Change {
    Write,
    Edit,
}

impl Change {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Write => "write",
            Change::Edit => "edit",
        }
    }
}

/// A tool wrote to the workspace.
///
/// Observation only: the write has already happened, so a listener that
/// dislikes it wants `agent/tool-call`, which fires while there is still time.
#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: Change,
    pub before: Option<String>,
    pub after: String,
}

impl Event for FileChange {
    const NAME: &'static str = "code/file-change";
}
impl Emitted for FileChange {}

/// Something was shown to the user.
///
/// Not reentrant, and by nature: a listener that shows the user something would
/// announce itself. The inner announcement is dropped.
#[derive(Clone, Debug)]
pub struct Notice(pub String);

impl Event for Notice {
    const NAME: &'static str = "code/notice";
    const REENTRANT: bool = false;
}
impl Emitted for Notice {}

/// Every announcement this crate makes.
pub const CODE_EVENTS: &[&str] = &[
    SystemPrompt::NAME,
    Tick::NAME,
    SessionStart::NAME,
    SessionEnd::NAME,
    Ask::NAME,
    FileChange::NAME,
    Notice::NAME,
];
