//! What a script is allowed to do, and the functions that let it.
//!
//! Rhai's standard library is pure: a script can compute, but it cannot reach
//! the filesystem, a shell or the network unless the host hands it a function
//! that does. Everything registered here is therefore a deliberate grant, and
//! [`Capabilities`] is where a caller decides which grants to make.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rhai::{Engine, EvalAltResult, Map};

use crate::worker::{Job, Worker};

/// How long `exec` and `http` may take before they are given up on.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// How much a script may compute in one hook before it is stopped.
///
/// Generous for anything sane and still bounded, so a runaway loop in a plugin
/// ends the hook rather than the session.
pub const DEFAULT_MAX_OPERATIONS: u64 = 5_000_000;

/// What a script may do.
///
/// The default grants nothing but computation: a caller opts in to each
/// capability, and a host built with [`Capabilities::default`] cannot touch
/// anything outside itself.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// Where a relative path starts. `None` disables the filesystem.
    pub root: Option<PathBuf>,
    /// Whether `fs::*` may leave `root`.
    ///
    /// A host that grants `exec` has already granted this in all but name: `sh`
    /// reads and writes anywhere. Confining the file functions then buys no
    /// safety and only makes them weaker than the shell the same plugin holds,
    /// so a coding session grants both together.
    pub unconfined: bool,
    /// Whether `fs::write` is allowed. Reading is granted by `root` alone.
    pub write: bool,
    /// Whether `exec` is allowed.
    pub exec: bool,
    /// Whether `http::*` is allowed.
    pub http: bool,
    pub timeout: Duration,
    pub max_operations: u64,
    /// Directories searched for `<name>.json`, first hit wins.
    pub config_dirs: Vec<PathBuf>,
    /// Where `save_state` is persisted. `None` keeps state for the session only.
    pub state_dir: Option<PathBuf>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            root: None,
            unconfined: false,
            write: false,
            exec: false,
            http: false,
            timeout: DEFAULT_TIMEOUT,
            max_operations: DEFAULT_MAX_OPERATIONS,
            config_dirs: Vec::new(),
            state_dir: None,
        }
    }
}

impl Capabilities {
    /// Everything, with `root` as where a relative path starts. What the coding
    /// harness grants: the agent can already run a shell, so withholding one
    /// from a plugin the user installed on purpose buys nothing.
    #[must_use]
    pub fn full(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let plugins = root.join(".aphid").join("plugins");
        Self {
            root: Some(root),
            unconfined: true,
            write: true,
            exec: true,
            http: true,
            config_dirs: vec![plugins.clone()],
            state_dir: Some(plugins.join("state")),
            ..Self::default()
        }
    }

    /// Also read settings from a home directory, behind the project's.
    #[must_use]
    pub fn with_home(mut self, home: &Path) -> Self {
        self.config_dirs.push(home.join(".aphid").join("plugins"));
        self
    }
}

/// Where a script's `log`, `notify` and `prompt` output goes.
pub trait Sink: Send + Sync + 'static {
    /// Something the user should see. The terminal UI renders these as notices.
    fn notify(&self, plugin: &str, text: &str);

    /// Something only a developer wants. Defaults to standard error, which is
    /// where the terminal UI is not drawing.
    fn log(&self, plugin: &str, text: &str) {
        eprintln!("[{plugin}] {text}");
    }

    /// Text for the model, as if the user had typed it.
    ///
    /// Defaults to doing nothing, because a front end with no prompt queue —
    /// headless, or a caller embedding the agent — has nowhere to put it. The
    /// terminal UI puts it in the same queue a typed line goes to.
    fn prompt(&self, plugin: &str, text: &str) {
        let _ = (plugin, text);
    }
}

/// The sink for a host nobody is watching.
#[derive(Copy, Clone, Debug, Default)]
pub struct Silent;

impl Sink for Silent {
    fn notify(&self, _plugin: &str, _text: &str) {}
    fn log(&self, _plugin: &str, _text: &str) {}
}

/// Resolve a script-supplied path against the root a relative path starts from.
///
/// A relative path always lands under `root`, so `fs_read("src/lib.rs")` means
/// the workspace whatever else is granted. `unconfined` decides what happens
/// with the rest: with it, a path may go anywhere; without it, the guard is
/// lexical like the one the file tools use — `..` is rejected outright rather
/// than normalised, so no amount of cleverness with symlinks turns a relative
/// path into an escape.
///
/// # Errors
///
/// Fails when the filesystem is not granted, or, when confined, the path is
/// outside the root or any component is `..`.
pub fn resolve(root: Option<&Path>, unconfined: bool, path: &str) -> Result<PathBuf, String> {
    let Some(root) = root else {
        return Err("the filesystem is not available to this plugin".to_owned());
    };

    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };

    if unconfined {
        return Ok(joined);
    }

    if joined
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!("`{path}` climbs out of the workspace"));
    }
    if !joined.starts_with(root) {
        return Err(format!("`{path}` is outside the workspace"));
    }

    Ok(joined)
}

fn fail(message: String) -> Box<EvalAltResult> {
    message.into()
}

/// Register every granted capability on an engine.
///
/// One engine per plugin, so each closure captures that plugin's own name and
/// its share of the host — no thread-local or call-context plumbing is needed to
/// work out who is calling.
pub(crate) fn register(
    engine: &mut Engine,
    plugin: &str,
    caps: &Capabilities,
    sink: &Arc<dyn Sink>,
    worker: &Arc<Worker>,
    store: &Arc<crate::store::Store>,
) {
    engine.set_max_operations(caps.max_operations);
    engine.set_max_call_levels(64);
    engine.set_max_string_size(8 * 1024 * 1024);
    engine.set_max_array_size(100_000);
    engine.set_max_map_size(100_000);

    crate::cx::register(engine);
    register_verdicts(engine);
    register_output(engine, plugin, sink);
    register_fs(engine, caps);
    register_exec(engine, plugin, caps, worker);
    register_http(engine, caps, worker);
    register_storage(engine, plugin, caps, store);
}

/// A plugin's settings and its memory.
///
/// Both are functions rather than variables because a Rhai script function
/// cannot see the enclosing scope — it is closed over its parameters and nothing
/// else, so there is no `state` variable a hook could reach.
fn register_storage(
    engine: &mut Engine,
    plugin: &str,
    caps: &Capabilities,
    store: &Arc<crate::store::Store>,
) {
    let settings = crate::store::config(&caps.config_dirs, plugin);
    engine.register_fn("config", move || settings.clone());

    let reader = Arc::clone(store);
    engine.register_fn("state", move || reader.get());

    let writer = Arc::clone(store);
    engine.register_fn("save_state", move |state: Map| writer.set(state));
}

/// The values a hook returns to steer the run.
///
/// Plain maps under the hood, so a script can build one by hand, but naming them
/// keeps the intent legible at the call site.
fn register_verdicts(engine: &mut Engine) {
    fn verdict(kind: &str, text: &str) -> Map {
        let mut map = Map::new();
        map.insert("verdict".into(), kind.into());
        map.insert("reason".into(), text.into());
        map
    }

    engine.register_fn("block", |reason: &str| verdict("block", reason));
    engine.register_fn("block_and_stop", |reason: &str| {
        verdict("block_and_stop", reason)
    });
    engine.register_fn("reject", |reason: &str| verdict("reject", reason));
    engine.register_fn("stop", || verdict("stop", ""));
    engine.register_fn("allow", || verdict("allow", ""));
    engine.register_fn("notice", |text: &str| verdict("notice", text));
}

fn register_output(engine: &mut Engine, plugin: &str, sink: &Arc<dyn Sink>) {
    let name = plugin.to_owned();
    let target = Arc::clone(sink);
    engine.register_fn("notify", move |text: &str| target.notify(&name, text));

    let name = plugin.to_owned();
    let target = Arc::clone(sink);
    engine.register_fn("log", move |text: &str| target.log(&name, text));

    // A call rather than something a command returns, so a hook, a tool and a
    // command all send text to the model the same way.
    let name = plugin.to_owned();
    let target = Arc::clone(sink);
    engine.register_fn("prompt", move |text: &str| target.prompt(&name, text));
}

fn register_fs(engine: &mut Engine, caps: &Capabilities) {
    let free = caps.unconfined;

    let root = caps.root.clone();
    engine.register_fn("fs_read", move |path: &str| {
        let path = resolve(root.as_deref(), free, path).map_err(fail)?;
        std::fs::read_to_string(&path)
            .map_err(|error| fail(format!("could not read {}: {error}", path.display())))
    });

    let root = caps.root.clone();
    engine.register_fn("fs_exists", move |path: &str| {
        resolve(root.as_deref(), free, path).is_ok_and(|path| path.exists())
    });

    let root = caps.root.clone();
    engine.register_fn("fs_list", move |path: &str| {
        let path = resolve(root.as_deref(), free, path).map_err(fail)?;
        let entries = std::fs::read_dir(&path)
            .map_err(|error| fail(format!("could not list {}: {error}", path.display())))?;
        let mut names: Vec<rhai::Dynamic> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string().into())
            .collect();
        names.sort_by_key(rhai::Dynamic::to_string);
        Ok::<_, Box<EvalAltResult>>(names)
    });

    let root = caps.root.clone();
    let allowed = caps.write;
    engine.register_fn("fs_write", move |path: &str, contents: &str| {
        if !allowed {
            return Err(fail(
                "writing files is not available to this plugin".to_owned(),
            ));
        }
        let path = resolve(root.as_deref(), free, path).map_err(fail)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| fail(format!("could not create {}: {error}", parent.display())))?;
        }
        std::fs::write(&path, contents)
            .map_err(|error| fail(format!("could not write {}: {error}", path.display())))
    });
}

fn register_exec(engine: &mut Engine, plugin: &str, caps: &Capabilities, worker: &Arc<Worker>) {
    let allowed = caps.exec;
    let timeout = caps.timeout;
    let cwd = caps.root.clone();
    let runner = Arc::clone(worker);
    let origin = plugin.to_owned();

    engine.register_fn("exec", move |command: &str| {
        if !allowed {
            return Err(fail(
                "running commands is not available to this plugin".to_owned(),
            ));
        }

        let outcome = runner
            .run(Job::Exec {
                command: command.to_owned(),
                cwd: cwd.clone(),
                timeout,
                origin: origin.clone(),
            })
            .map_err(fail)?;

        let mut result = Map::new();
        result.insert("status".into(), outcome.status.into());
        result.insert("stdout".into(), outcome.stdout.into());
        result.insert("stderr".into(), outcome.stderr.into());
        Ok::<_, Box<EvalAltResult>>(result)
    });
}

fn register_http(engine: &mut Engine, caps: &Capabilities, worker: &Arc<Worker>) {
    fn respond(outcome: crate::worker::Outcome) -> Map {
        let mut result = Map::new();
        result.insert("status".into(), outcome.status.into());
        result.insert("body".into(), outcome.stdout.into());
        let headers: Map = outcome
            .headers
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        result.insert("headers".into(), headers.into());
        result
    }

    fn header_pairs(headers: &Map) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    let allowed = caps.http;
    let timeout = caps.timeout;
    let runner = Arc::clone(worker);
    engine.register_fn("http_get", move |url: &str| {
        if !allowed {
            return Err(fail("http is not available to this plugin".to_owned()));
        }
        let outcome = runner
            .run(Job::Http {
                method: "GET",
                url: url.to_owned(),
                body: None,
                headers: Vec::new(),
                timeout,
            })
            .map_err(fail)?;
        Ok::<_, Box<EvalAltResult>>(respond(outcome))
    });

    let allowed = caps.http;
    let timeout = caps.timeout;
    let runner = Arc::clone(worker);
    engine.register_fn("http_post", move |url: &str, body: &str, headers: Map| {
        if !allowed {
            return Err(fail("http is not available to this plugin".to_owned()));
        }
        let outcome = runner
            .run(Job::Http {
                method: "POST",
                url: url.to_owned(),
                body: Some(body.to_owned()),
                headers: header_pairs(&headers),
                timeout,
            })
            .map_err(fail)?;
        Ok::<_, Box<EvalAltResult>>(respond(outcome))
    });
}

/// Let a script declare tools while its body runs.
///
/// Separate from [`register`] because the registry it fills is per-load, not
/// per-capability: a tool is something the plugin *is*, not something it is
/// allowed to do.
pub(crate) fn register_tools(engine: &mut Engine, registry: &crate::tool::Registry) {
    let target = Arc::clone(registry);
    engine.register_fn("register_tool", move |declaration: Map| {
        let spec = crate::tool::spec(&declaration).map_err(fail)?;
        match target.lock() {
            Ok(mut specs) => {
                specs.retain(|existing| existing.declaration.name != spec.declaration.name);
                specs.push(spec);
                Ok::<_, Box<EvalAltResult>>(())
            }
            Err(_) => Err(fail("the tool registry is poisoned".to_owned())),
        }
    });
}

/// Let a script declare slash commands while its body runs.
pub(crate) fn register_commands(engine: &mut Engine, registry: &crate::command::Registry) {
    let target = Arc::clone(registry);
    engine.register_fn("register_command", move |declaration: Map| {
        let spec = crate::command::spec(&declaration).map_err(fail)?;
        match target.lock() {
            Ok(mut specs) => {
                specs.retain(|existing| existing.name != spec.name);
                specs.push(spec);
                Ok::<_, Box<EvalAltResult>>(())
            }
            Err(_) => Err(fail("the command registry is poisoned".to_owned())),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_lands_under_the_root() {
        let root = Path::new("/work");
        assert_eq!(
            resolve(Some(root), false, "src/lib.rs").expect("resolved"),
            Path::new("/work/src/lib.rs")
        );
        assert_eq!(
            resolve(Some(root), true, "src/lib.rs").expect("resolved"),
            Path::new("/work/src/lib.rs")
        );
    }

    #[test]
    fn climbing_out_is_refused_when_confined() {
        let root = Path::new("/work");
        assert!(resolve(Some(root), false, "../etc/passwd").is_err());
        assert!(resolve(Some(root), false, "src/../../etc/passwd").is_err());
        assert!(resolve(Some(root), false, "/etc/passwd").is_err());
    }

    #[test]
    fn an_absolute_path_inside_the_root_is_allowed() {
        let root = Path::new("/work");
        assert_eq!(
            resolve(Some(root), false, "/work/src/lib.rs").expect("resolved"),
            Path::new("/work/src/lib.rs")
        );
    }

    #[test]
    fn an_unconfined_plugin_reaches_outside_the_root() {
        let root = Path::new("/work");
        assert_eq!(
            resolve(Some(root), true, "/tmp/aphid-webchat/inbox").expect("resolved"),
            Path::new("/tmp/aphid-webchat/inbox")
        );
        assert!(resolve(Some(root), true, "../sibling/notes.md").is_ok());
    }

    #[test]
    fn no_root_means_no_filesystem() {
        assert!(resolve(None, false, "anything").is_err());
        assert!(resolve(None, true, "anything").is_err());
    }
}
