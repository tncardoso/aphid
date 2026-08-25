//! Slash commands written in Rhai.
//!
//! ```rhai
//! command(#{
//!     name: "review",
//!     description: "Ask for a review of the working tree.",
//!     run: |args| {
//!         let diff = exec("git diff").stdout;
//!         if diff == "" { return notice("nothing to review"); }
//!         prompt("Review this diff:\n" + diff);
//!         notice("reviewing…")
//!     }
//! });
//! ```
//!
//! A handler returns what the user should read: a `notice`, a bare string, or an
//! array of them. To steer the model it calls `prompt`, which is the one way to
//! send text to it from anywhere — a hook and a tool reach for the same
//! function.

use rhai::{Dynamic, FnPtr, Map};

/// A command declaration collected while a script loaded.
#[derive(Clone)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    pub body: FnPtr,
}

/// What running a command should do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Show this to the user.
    Notice(String),
}

impl std::fmt::Debug for Registered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registered")
            .field("invocation", &self.invocation)
            .field("plugin", &self.plugin)
            .finish()
    }
}

/// Read a `command` declaration.
///
/// # Errors
///
/// Returns the reason it was refused, which the script sees as a runtime error.
pub(crate) fn spec(declaration: &Map) -> Result<CommandSpec, String> {
    let name = declaration
        .get("name")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "a command needs a `name`".to_owned())?;

    let name = name.trim_start_matches('/').to_owned();
    if name.is_empty() || name.split_whitespace().count() > 1 {
        return Err(format!("`{name}` is not a usable command name"));
    }

    let body = declaration
        .get("run")
        .and_then(|value| value.clone().try_cast::<FnPtr>())
        .ok_or_else(|| format!("command `{name}` needs a `run` function"))?;

    let description = declaration
        .get("description")
        .filter(|value| value.is_string())
        .map_or_else(String::new, std::string::ToString::to_string);

    Ok(CommandSpec {
        name,
        description,
        body,
    })
}

/// Read what a handler returned.
///
/// A bare string is a notice, which is what a command that only reports wants
/// and saves it wrapping every answer.
#[must_use]
pub fn actions(value: &Dynamic) -> Vec<Action> {
    if value.is_unit() {
        return Vec::new();
    }

    if value.is_array() {
        return value
            .clone()
            .into_array()
            .unwrap_or_default()
            .iter()
            .flat_map(actions)
            .collect();
    }

    if value.is_map() {
        let map: Map = value.clone().cast();
        let kind = map.get("verdict").map(std::string::ToString::to_string);
        let text = map
            .get("reason")
            .map(std::string::ToString::to_string)
            .unwrap_or_default();

        return match kind.as_deref() {
            Some("notice") => vec![Action::Notice(text)],
            _ => Vec::new(),
        };
    }

    vec![Action::Notice(value.to_string())]
}

/// One command as the harness sees it, after collisions are resolved.
#[derive(Clone)]
pub struct Registered {
    /// What the user types, without the slash.
    pub invocation: String,
    /// The name the script asked for.
    pub name: String,
    pub description: String,
    /// Which loaded plugin owns it.
    pub plugin: String,
    /// What to call to run it.
    pub spec: CommandSpec,
}

/// Every command on offer, with its invocation worked out.
///
/// Two components may both want `/review`. Rather than one of them silently
/// losing, both are kept and the later gets a numeric suffix — `/review` and
/// `/review:2` — so a plugin the user installed always has some way to be
/// reached.
///
/// Read from the registry, not from the files that compiled: a component that
/// is waiting on a service, or was unloaded, has offered nothing.
#[must_use]
pub fn registered(
    commands: &crate::registries::Registry<crate::registries::Command>,
) -> Vec<Registered> {
    let mut registered: Vec<Registered> = Vec::new();

    for entry in commands.entries() {
        let taken = registered
            .iter()
            .filter(|existing| existing.name == entry.spec.name)
            .count();
        let invocation = if taken == 0 {
            entry.spec.name.clone()
        } else {
            format!("{}:{}", entry.spec.name, taken + 1)
        };

        registered.push(Registered {
            invocation,
            name: entry.spec.name.clone(),
            description: entry.spec.description.clone(),
            plugin: entry.source.clone(),
            spec: entry.spec,
        });
    }

    registered
}

impl super::host::PluginHost {
    /// Run the command a user typed, if some component is offering it.
    ///
    /// `None` means nothing owns that name, which is what tells the caller to
    /// report an unknown command.
    #[must_use]
    pub fn run_command(
        &self,
        commands: &crate::registries::Registry<crate::registries::Command>,
        invocation: &str,
        args: &str,
    ) -> Option<Vec<Action>> {
        let found = registered(commands)
            .into_iter()
            .find(|command| command.invocation == invocation)?;

        let plugin = self
            .plugins()
            .iter()
            .find(|plugin| plugin.name() == found.plugin)?;

        Some(plugin.run_command(&found.spec, args))
    }
}
