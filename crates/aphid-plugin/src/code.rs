//! The hooks a coding harness dispatches.
//!
//! These have no bit in the agent's [`Interest`](aphid_agent::Interest) set,
//! because the agent loop has no idea what a permission or a file change is.
//! They are typed methods on [`PluginHost`] instead, called by whichever
//! harness knows about such things — which keeps coding concepts out of the
//! loop crate without a second trait and a second dispatch table to keep in
//! step with the first.

use std::path::Path;

use rhai::Map;

use crate::host::PluginHost;

/// What a script decided about a tool that wants permission.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Permission {
    Allow,
    /// Allow, and stop asking about this exact call.
    AllowAlways,
    Deny,
    /// No opinion: fall through to whatever would have decided anyway.
    Ask,
}

impl Permission {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "allow" => Some(Permission::Allow),
            "allow_always" => Some(Permission::AllowAlways),
            "deny" => Some(Permission::Deny),
            "ask" => Some(Permission::Ask),
            _ => None,
        }
    }
}

/// What a session hook is told.
#[derive(Clone, Debug)]
pub struct SessionInfo<'a> {
    pub id: Option<&'a str>,
    pub path: Option<&'a Path>,
    /// `"new"` or `"resume"`.
    pub reason: &'a str,
    /// How many messages a resume restored.
    pub restored: usize,
}

/// How a file came to change.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Change {
    Write,
    Edit,
}

impl Change {
    fn as_str(self) -> &'static str {
        match self {
            Change::Write => "write",
            Change::Edit => "edit",
        }
    }
}

impl PluginHost {
    /// Let scripts add to or replace the system prompt.
    ///
    /// Fires once, while the harness is being built, so it is the only hook that
    /// runs before an agent exists. Hooks chain: each sees the previous one's
    /// edits, and a replacement is what the next hook is shown.
    pub fn system_prompt(&self, prompt: &mut String) {
        for plugin in self.defining("on_system_prompt") {
            let Some(returned) = plugin.call("on_system_prompt", (prompt.clone(),)) else {
                continue;
            };
            let Some(patch) = crate::host::map_of(&returned) else {
                continue;
            };

            if let Some(text) = patch.get("replace").filter(|value| value.is_string()) {
                *prompt = text.to_string();
            }
            if let Some(text) = patch.get("append").filter(|value| value.is_string()) {
                prompt.push_str("\n\n");
                prompt.push_str(&text.to_string());
            }
        }
    }

    /// Time passed.
    ///
    /// The only hook the agent does not cause, so it is what a plugin watching
    /// something outside the session — a file, a queue, a clock — is woken by.
    /// A tick that is still running is not started again: a slow hook then
    /// costs its own time and not a growing queue of ticks behind it.
    pub fn tick(&self) {
        if self.enter_tick() {
            return;
        }
        for plugin in self.defining("on_tick") {
            plugin.call("on_tick", ());
        }
        self.leave_tick();
    }

    /// A session opened.
    pub fn session_start(&self, info: &SessionInfo<'_>) {
        self.session("on_session_start", info);
    }

    /// A session is closing. Every plugin's state is written back afterwards,
    /// so a hook that saves on the way out is not too late.
    pub fn session_end(&self, info: &SessionInfo<'_>) {
        self.session("on_session_end", info);
        self.flush();
    }

    fn session(&self, hook: &str, info: &SessionInfo<'_>) {
        for plugin in self.defining(hook) {
            let mut payload = Map::new();
            payload.insert("id".into(), info.id.map_or(rhai::Dynamic::UNIT, Into::into));
            payload.insert(
                "path".into(),
                info.path.map_or(rhai::Dynamic::UNIT, |path| {
                    path.display().to_string().into()
                }),
            );
            payload.insert("reason".into(), info.reason.into());
            payload.insert(
                "restored".into(),
                i64::try_from(info.restored).unwrap_or(i64::MAX).into(),
            );
            plugin.call(hook, (payload,));
        }
    }

    /// Ask the scripts about a tool that needs permission.
    ///
    /// The first script with an opinion decides, and `None` means none had one.
    /// A script that raises **denies**: like `on_tool_call`, a guard that failed
    /// has not approved anything, and this is the hook people reach for when
    /// they mean to be careful.
    #[must_use]
    pub fn permission(&self, tool: &str, summary: &str, risk: &str) -> Option<Permission> {
        for plugin in self.defining("on_permission") {
            let mut payload = Map::new();
            payload.insert("tool".into(), tool.into());
            payload.insert("summary".into(), summary.into());
            payload.insert("risk".into(), risk.into());

            let Some(returned) = plugin.call("on_permission", (payload,)) else {
                return Some(Permission::Deny);
            };

            let verdict = if returned.is_string() {
                Permission::parse(&returned.to_string())
            } else {
                crate::host::map_of(&returned)
                    .and_then(|map| map.get("verdict").map(std::string::ToString::to_string))
                    .and_then(|text| Permission::parse(&text))
            };

            match verdict {
                Some(Permission::Ask) | None => {}
                Some(decided) => return Some(decided),
            }
        }

        None
    }

    /// A tool wrote to the workspace.
    ///
    /// Observation only: the write has already happened, so a hook that dislikes
    /// it wants `on_tool_call`, which fires while there is still time.
    pub fn file_change(&self, path: &Path, change: Change, before: Option<&str>, after: &str) {
        for plugin in self.defining("on_file_change") {
            let mut payload = Map::new();
            payload.insert("path".into(), path.display().to_string().into());
            payload.insert("kind".into(), change.as_str().into());
            payload.insert(
                "before".into(),
                before.map_or(rhai::Dynamic::UNIT, Into::into),
            );
            payload.insert("after".into(), after.into());
            plugin.call("on_file_change", (payload,));
        }
    }

    /// Something was shown to the user.
    ///
    /// Re-entrant by nature — a hook that calls `notify` would trigger itself —
    /// so the host drops any notice raised while this is already running.
    pub fn notice(&self, text: &str) {
        if self.enter_notice() {
            return;
        }
        for plugin in self.defining("on_notify") {
            plugin.call("on_notify", (text.to_owned(),));
        }
        self.leave_notice();
    }

    /// Write every plugin's state back.
    pub fn flush(&self) {
        for plugin in self.plugins() {
            plugin.flush();
        }
    }
}
