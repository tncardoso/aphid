//! One loaded `.rhai` file.
//!
//! A plugin declares its hooks by defining functions with known names — there is
//! no registration call and no manifest. The host reads the compiled AST once at
//! load time and keeps the names it recognises, so the
//! agent's existing per-hook subscriber lists do the gating and a plugin that
//! defines only `on_tool_call` costs nothing on every other hook.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rhai::{AST, Dynamic, Engine, FnPtr, FuncArgs, Map, Scope};

use aphid_agent::Sink;

use super::caps::Capabilities;
use super::command::{Action, CommandSpec};
use super::discover::PluginFile;
use super::store::Store;
use super::worker::Worker;

/// Every hook name the host knows, and the interest it implies.
///
/// The list exists so that a typo is **reported** rather than silently ignored.
/// A function named `on_permissio` is not a hook nobody happened to call; it is
/// a plugin that will never do anything, and saying so is the difference
/// between a five-second fix and an afternoon.
const HOOKS: &[&str] = &[
    // Where a plugin says what it contributes and what it listens to.
    //
    // The only one left. Everything a plugin used to name a function for is now
    // subscribed to from here — see [`subscribe`](super::subscribe).
    "apply",
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
    hooks: Vec<String>,
    /// What this plugin declared about itself, read from the compiled source
    /// before any of it ran.
    declares: Declares,
    /// Where `provide`, `effect` and `call` reach the composition. Open only
    /// while `apply` runs.
    wiring: Arc<super::wiring::Wiring>,
    /// Handed back to the engine's closures once this exists, because a service
    /// a script provides is called in that script's own engine.
    self_ref: Arc<Mutex<Option<Arc<ScriptPlugin>>>>,
    sink: Arc<dyn Sink>,
    store: Arc<Store>,
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

        let store = Arc::new(Store::load(caps.state_dir.as_deref(), &file.name));

        let mut engine = Engine::new();
        super::caps::register(&mut engine, &file.name, caps, sink, worker, &store);

        // Filled in for the length of `apply`, which is the only time a
        // registration is legal.
        let wiring = super::wiring::Wiring::new();
        let self_ref: Arc<Mutex<Option<Arc<ScriptPlugin>>>> = Arc::default();
        super::wiring::register(&mut engine, &wiring, &self_ref);
        engine.register_type_with_name::<ScriptCtx>("Ctx");

        let ast = engine
            .compile(&text)
            .map_err(|error| format!("does not compile: {error}"))?;

        let mut scope = Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .map_err(|error| format!("failed while loading: {error}"))?;

        let hooks = declared(&ast);
        let declares = Declares::read(&ast);

        Ok(Self {
            name: file.name.clone(),
            description: file.description.clone(),
            path: file.path.clone(),
            project: file.project,
            engine,
            ast,
            scope: Mutex::new(scope),
            hooks,
            declares,
            wiring,
            self_ref,
            sink: Arc::clone(sink),
            store,
        })
    }

    /// Fill in each surface's defaults under whatever was persisted.
    ///
    /// Persisted values win: `init` says what a key means when nothing has set
    /// it yet, which is what saves every surface from writing out the
    /// `if "open" in s { s.open } else { false }` dance.
    pub(crate) fn seed_surface(&self, spec: &super::surface::SurfaceSpec) {
        let Some(init) = &spec.init else { return };
        let Ok(defaults) = self.call_fn(init, ()) else {
            return;
        };
        let Some(defaults) = defaults.try_cast::<Map>() else {
            return;
        };

        let mut state = self.store.surface_get(&spec.name);
        let mut seeded = false;
        for (key, value) in defaults {
            state.entry(key).or_insert_with(|| {
                seeded = true;
                value
            });
        }
        if seeded {
            self.store.surface_set(&spec.name, state);
        }
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

    /// Point the engine's closures back at the loaded plugin.
    ///
    /// A service a script provides is a set of its own functions, and calling
    /// one has to happen in the engine they were compiled into — so the plugin
    /// has to be able to reach itself. It cannot do that at construction, since
    /// it does not exist yet.
    pub fn wire(self: &Arc<Self>) {
        if let Ok(mut slot) = self.self_ref.lock() {
            *slot = Some(Arc::clone(self));
        }
    }

    /// Run `apply`, with registration allowed for the length of the call.
    ///
    /// # Errors
    ///
    /// Whatever the script raised. A component that cannot apply is `FAILED`
    /// rather than quietly half-loaded.
    pub fn apply(
        &self,
        ctx: &aphid_agent::rt::Context,
        composition: &aphid_agent::rt::Composition,
    ) -> Result<(), String> {
        if !self.defines("apply") {
            return Ok(());
        }
        self.wiring.begin(ctx, composition);
        let outcome = {
            let mut scope = match self.scope.lock() {
                Ok(scope) => scope,
                Err(poisoned) => poisoned.into_inner(),
            };
            self.engine
                .call_fn::<Dynamic>(&mut scope, &self.ast, "apply", (ScriptCtx,))
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        self.wiring.end();
        outcome
    }

    /// The services this plugin requires, provides, and the events it emits.
    #[must_use]
    pub fn declares(&self) -> &Declares {
        &self.declares
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

    /// The in-memory state this plugin holds, as a copy.
    #[must_use]
    pub fn state(&self) -> Map {
        self.store.get()
    }

    /// One surface's own model, as a copy.
    #[must_use]
    pub fn surface_state(&self, name: &str) -> Map {
        self.store.surface_get(name)
    }

    /// Replace one surface's model.
    pub fn set_surface_state(&self, name: &str, state: Map) {
        self.store.surface_set(name, state);
    }

    /// The current state version, used to invalidate cached renders.
    #[must_use]
    pub fn state_version(&self) -> u64 {
        self.store.version()
    }

    /// Run one of this plugin's commands.
    ///
    /// The spec comes from the registry rather than from the plugin, because a
    /// command belongs to the *component* that offered it. A failure is
    /// reported and yields nothing to do: a command the user typed is not worth
    /// ending a session over.
    #[must_use]
    pub fn run_command(&self, spec: &CommandSpec, args: &str) -> Vec<Action> {
        let name = &spec.name;
        match self.call_fn(&spec.body, (args.to_owned(),)) {
            Ok(value) => super::command::actions(&value),
            Err(error) => {
                self.sink
                    .notify(&self.name, &format!("/{name} failed: {error}"));
                Vec::new()
            }
        }
    }

    /// Call a function this plugin handed out, such as a tool body.
    ///
    /// Unlike [`ScriptPlugin::call`] the error is returned rather than reported:
    /// a tool's failure belongs in its result, where the model will read it.
    ///
    /// # Errors
    ///
    /// Propagates whatever the script raised.
    pub fn call_fn(&self, body: &FnPtr, args: impl FuncArgs) -> Result<Dynamic, String> {
        body.call::<Dynamic>(&self.engine, &self.ast, args)
            .map_err(|error| error.to_string())
    }

    /// Report something to this plugin's sink.
    pub(crate) fn report(&self, text: &str) {
        self.sink.notify(&self.name, text);
    }

    /// Write this plugin's state back, if it changed.
    ///
    /// A failure is the plugin's to hear about, not the session's to stop for.
    pub fn flush(&self) {
        if let Err(error) = self.store.flush() {
            self.sink
                .notify(&self.name, &format!("could not save state: {error}"));
        }
    }

    /// Call a hook.
    ///
    /// `None` means the hook did not run — it is not defined, the scope was
    /// poisoned, or the script raised. A raise is reported through the sink and
    /// then swallowed: the caller decides whether that failure is open or
    /// closed, because only the caller knows whether the hook was guarding
    /// anything.
    /// Call a hook that takes the run context.
    ///
    /// Nothing is applied afterwards: what a script asks for through `cx` is
    /// recorded on the run itself, and the loop applies it once the
    /// announcement has finished going round.
    pub fn call_with_cx(
        &self,
        hook: &str,
        run: &aphid_agent::Run,
        extra: Option<Dynamic>,
    ) -> Option<Dynamic> {
        let cx = super::cx::ScriptCx::new(run);
        match extra {
            Some(extra) => self.call(hook, (cx, extra)),
            None => self.call(hook, (cx,)),
        }
    }

    /// Where this plugin's output goes.
    #[must_use]
    pub fn sink(&self) -> &Arc<dyn Sink> {
        &self.sink
    }

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

/// What `apply` is handed.
///
/// Carries nothing: the functions a script calls on it — `provide`, `effect`,
/// `invoke` — are engine functions that reach the composition through
/// [`Wiring`](super::wiring::Wiring), because they have to work whatever Rhai
/// does with the value on the way in. The type exists so that `apply(ctx)`
/// reads the way the rest of the model does.
#[derive(Clone, Copy, Debug)]
pub struct ScriptCtx;

/// Read the hooks a compiled script defines.
///
/// Anything not on the known list is ignored: a plugin is free to define helper
/// functions, and they are the overwhelming majority of what a real one holds.
/// What a plugin says about itself, before it says anything else.
///
/// Read out of the **compiled source**, not out of a run: the body cannot
/// declare `inject`, because the body must not run until `inject` is satisfied.
/// Rhai evaluates a `const` initialiser at compile time when it is a literal,
/// so `const inject = ["sink"];` is readable without executing a line.
///
/// ```rhai
/// const inject   = ["sink"];        // wait for these
/// const provides = ["todos"];       // offer these
/// const emits    = ["todos/changed"]; // announce these
/// ```
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Declares {
    pub inject: Vec<String>,
    pub provides: Vec<String>,
    pub emits: Vec<String>,
}

impl Declares {
    fn read(ast: &AST) -> Declares {
        let mut declares = Declares::default();
        for (name, is_const, value) in ast.iter_literal_variables(true, false) {
            if !is_const {
                continue;
            }
            let target = match name {
                "inject" => &mut declares.inject,
                "provides" => &mut declares.provides,
                "emits" => &mut declares.emits,
                _ => continue,
            };
            *target = names(&value);
        }
        declares
    }
}

/// The strings in a literal array, ignoring anything that is not one.
///
/// A declaration with a number in it is a mistake worth stepping over rather
/// than refusing the whole file for: the other names in it are still true.
fn names(value: &Dynamic) -> Vec<String> {
    value
        .clone()
        .try_cast::<rhai::Array>()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.clone().try_cast::<rhai::ImmutableString>())
        .map(|name| name.to_string())
        .collect()
}

fn declared(ast: &AST) -> Vec<String> {
    let mut hooks = Vec::new();

    for function in ast.iter_functions() {
        let Some(name) = HOOKS.iter().find(|name| **name == function.name) else {
            continue;
        };
        hooks.push((*name).to_owned());
    }

    hooks.sort_unstable();
    hooks.dedup();
    hooks
}
