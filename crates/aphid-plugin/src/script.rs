//! One loaded `.rhai` file.
//!
//! A plugin declares its hooks by defining functions with known names — there is
//! no registration call and no manifest. The host reads the compiled AST once at
//! load time and turns the names it recognises into an [`Interest`] set, so the
//! agent's existing per-hook subscriber lists do the gating and a plugin that
//! defines only `on_tool_call` costs nothing on every other hook.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aphid_agent::Interest;
use rhai::{AST, Dynamic, Engine, FuncArgs, Scope};

use crate::caps::{Capabilities, Sink};
use crate::discover::PluginFile;
use crate::worker::Worker;

/// Every hook name the host knows, and the interest it implies.
///
/// Coding-harness hooks are recognised here too, so a typo in one of them is
/// reported rather than silently ignored, even though the agent loop has no
/// interest bit to give them.
const HOOKS: &[(&str, Option<Interest>)] = &[
    ("on_prompt", Some(Interest::PROMPT)),
    ("on_run_start", Some(Interest::RUN_START)),
    ("on_turn_start", Some(Interest::TURN_START)),
    ("on_event", Some(Interest::EVENT)),
    ("on_message", Some(Interest::MESSAGE)),
    ("on_tool_call", Some(Interest::TOOL_CALL)),
    ("on_tool_progress", Some(Interest::TOOL_PROGRESS)),
    ("on_tool_result", Some(Interest::TOOL_RESULT)),
    ("on_turn_end", Some(Interest::TURN_END)),
    ("on_run_end", Some(Interest::RUN_END)),
    ("on_system_prompt", None),
    ("on_session_start", None),
    ("on_session_end", None),
    ("on_permission", None),
    ("on_file_change", None),
    ("on_notify", None),
    ("on_request", None),
];

/// A compiled plugin, ready to be called.
///
/// The engine is per-plugin rather than shared: capability functions then close
/// over this plugin's own name and sink, which is far simpler than threading a
/// call context through Rhai to work out who is calling.
pub struct ScriptPlugin {
    name: String,
    description: Option<String>,
    path: PathBuf,
    project: bool,
    engine: Engine,
    ast: AST,
    scope: Mutex<Scope<'static>>,
    interests: Interest,
    hooks: Vec<String>,
    sink: Arc<dyn Sink>,
}

impl ScriptPlugin {
    /// Compile a plugin file and run its top-level body once.
    ///
    /// The body runs at load time so a script can set up whatever it needs
    /// before the first hook fires. A failure here is the plugin's, not the
    /// session's: the caller records it as a diagnostic and carries on.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, does not compile, or its top-level
    /// body raises.
    pub fn load(
        file: &PluginFile,
        caps: &Capabilities,
        sink: &Arc<dyn Sink>,
        worker: &Arc<Worker>,
    ) -> Result<Self, String> {
        let text = std::fs::read_to_string(&file.path)
            .map_err(|error| format!("could not read: {error}"))?;

        let mut engine = Engine::new();
        crate::caps::register(&mut engine, &file.name, caps, sink, worker);

        let ast = engine
            .compile(&text)
            .map_err(|error| format!("does not compile: {error}"))?;

        let mut scope = Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .map_err(|error| format!("failed while loading: {error}"))?;

        let (interests, hooks) = declared(&ast);

        Ok(Self {
            name: file.name.clone(),
            description: file.description.clone(),
            path: file.path.clone(),
            project: file.project,
            engine,
            ast,
            scope: Mutex::new(scope),
            interests,
            hooks,
            sink: Arc::clone(sink),
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Whether this plugin came from the workspace rather than the home
    /// directory.
    #[must_use]
    pub fn project(&self) -> bool {
        self.project
    }

    #[must_use]
    pub fn interests(&self) -> Interest {
        self.interests
    }

    /// The hook names this plugin defines, in the order the host knows them.
    #[must_use]
    pub fn hooks(&self) -> &[String] {
        &self.hooks
    }

    /// Whether this plugin defines a given hook.
    #[must_use]
    pub fn defines(&self, hook: &str) -> bool {
        self.hooks.iter().any(|name| name == hook)
    }

    /// Call a hook.
    ///
    /// `None` means the hook did not run — it is not defined, the scope was
    /// poisoned, or the script raised. A raise is reported through the sink and
    /// then swallowed: the caller decides whether that failure is open or
    /// closed, because only the caller knows whether the hook was guarding
    /// anything.
    pub fn call(&self, hook: &str, args: impl FuncArgs) -> Option<Dynamic> {
        if !self.defines(hook) {
            return None;
        }

        let mut scope = self.scope.lock().ok()?;
        match self
            .engine
            .call_fn::<Dynamic>(&mut scope, &self.ast, hook, args)
        {
            Ok(value) => Some(value),
            Err(error) => {
                self.sink
                    .notify(&self.name, &format!("{hook} failed: {error}"));
                None
            }
        }
    }
}

impl std::fmt::Debug for ScriptPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptPlugin")
            .field("name", &self.name)
            .field("hooks", &self.hooks)
            .finish()
    }
}

/// Read the hooks a compiled script defines.
///
/// Anything not on the known list is ignored: a plugin is free to define helper
/// functions, and they are the overwhelming majority of what a real one holds.
fn declared(ast: &AST) -> (Interest, Vec<String>) {
    let mut interests = Interest::empty();
    let mut hooks = Vec::new();

    for function in ast.iter_functions() {
        let Some((name, interest)) = HOOKS.iter().find(|(name, _)| *name == function.name) else {
            continue;
        };
        if let Some(interest) = interest {
            interests |= *interest;
        }
        hooks.push((*name).to_owned());
    }

    hooks.sort_unstable();
    hooks.dedup();
    (interests, hooks)
}
