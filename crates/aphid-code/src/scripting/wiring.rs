//! Where a script's `ctx` reaches the composition.
//!
//! An engine is built when a file is compiled, which is before the fiber that
//! will run it exists. So the functions a script calls — `provide`, `effect`,
//! `call` — close over this instead, and the component fills it in for exactly
//! as long as `apply` is running.
//!
//! That window is the point, not an implementation detail. Registering is only
//! legal during `apply`, because that is the only moment the runtime can attach
//! what you register to the fiber that will revert it. A `provide` from inside
//! a hook has no owner and would leak, so it is refused and says why.

use std::sync::{Arc, Mutex};

use aphid_agent::rt::{Composition, Context, Disposer};

use crate::registries::{Command, Commands, Surface, Surfaces, Tools};
use rhai::{Array, Dynamic, EvalAltResult, FnPtr, Map};

use super::facade::{Facade, ScriptService};
use super::script::ScriptPlugin;

/// The live context, while there is one.
#[derive(Default)]
pub(crate) struct Wiring {
    open: Mutex<Option<Open>>,
}

struct Open {
    ctx: Context,
    composition: Composition,
}

impl Wiring {
    #[must_use]
    pub(crate) fn new() -> Arc<Wiring> {
        Arc::default()
    }

    /// Open the window.
    pub(crate) fn begin(&self, ctx: &Context, composition: &Composition) {
        if let Ok(mut slot) = self.open.lock() {
            *slot = Some(Open {
                ctx: ctx.clone(),
                composition: composition.clone(),
            });
        }
    }

    pub(crate) fn end(&self) {
        if let Ok(mut slot) = self.open.lock() {
            *slot = None;
        }
    }

    fn context(&self, what: &str) -> Result<Context, Box<EvalAltResult>> {
        self.with(what, |open| open.ctx.clone())
    }

    fn with<T>(&self, what: &str, read: impl FnOnce(&Open) -> T) -> Result<T, Box<EvalAltResult>> {
        self.open
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(read))
            .ok_or_else(|| {
                format!(
                    "`{what}` can only be called from `apply`: outside it there is no \
                     component to undo the registration when the plugin unloads"
                )
                .into()
            })
    }
}

/// Teach an engine the three functions that reach the composition.
///
/// `plugin` is filled in by the component once the script is compiled, because
/// a service a script provides has to be called back in that script's own
/// engine.
pub(crate) fn register(
    engine: &mut rhai::Engine,
    wiring: &Arc<Wiring>,
    owner: &Arc<Mutex<Option<Arc<ScriptPlugin>>>>,
) {
    // provide(name, #{ method: || … })
    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "provide",
        move |name: &str, methods: Map| -> Result<(), Box<EvalAltResult>> {
            let ctx = wired.context("provide")?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            let service: Arc<dyn Facade> = Arc::new(ScriptService::new(plugin, methods));
            // Leaked because a coeffect key is `&'static str` and this one came
            // from a file. There are tens of them and they live as long as the
            // process.
            let key: &'static str = Box::leak(name.to_owned().into_boxed_str());
            ctx.provide_dyn(key, Arc::new(service));
            Ok(())
        },
    );

    // invoke(service, method, [args])
    //
    // Not `call`: Rhai already has one on `FnPtr`, and a second would shadow
    // it. It is the same collision the plugin documentation warns about for
    // parameter names.
    let wired = Arc::clone(wiring);
    engine.register_fn(
        "invoke",
        move |name: &str, method: &str, args: Array| -> Result<Dynamic, Box<EvalAltResult>> {
            let ctx = wired.context("invoke")?;
            let key: &'static str = Box::leak(name.to_owned().into_boxed_str());
            let facade = ctx
                .probe_dyn(key)
                .and_then(|binding| binding.value::<Arc<dyn Facade>>())
                .ok_or_else(|| {
                    Box::<EvalAltResult>::from(format!(
                        "no service `{name}` is available here — either nothing provides it, \
                         or it is a service scripts cannot reach"
                    ))
                })?;
            facade.call(method, args).map_err(Into::into)
        },
    );

    // on(event, |payload| { … })
    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "on",
        move |event: &str, body: FnPtr| -> Result<(), Box<EvalAltResult>> {
            let (ctx, composition) =
                wired.with("on", |open| (open.ctx.clone(), open.composition.clone()))?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            super::subscribe::on(&composition, ctx.uid(), &plugin, event, body).map_err(Into::into)
        },
    );

    // tool(#{ … }), command(#{ … }), surface(#{ … })
    //
    // Through the services rather than into a table of the plugin's own, so
    // what a component contributes leaves when it does — and so a component
    // that never loaded has contributed nothing.
    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "tool",
        move |declaration: Map| -> Result<(), Box<EvalAltResult>> {
            let (ctx, _) = wired.with("tool", |open| (open.ctx.clone(), ()))?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            let spec = super::tool::spec(&declaration).map_err(EvalAltResult::from)?;
            let handler: Arc<dyn aphid_agent::ToolHandler> =
                Arc::new(super::tool::ScriptTool::new(plugin, spec));
            ctx.get::<Tools>()
                .map_err(|error| EvalAltResult::from(error.to_string()))?
                .contribute(&ctx, handler);
            Ok(())
        },
    );

    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "command",
        move |declaration: Map| -> Result<(), Box<EvalAltResult>> {
            let (ctx, _) = wired.with("command", |open| (open.ctx.clone(), ()))?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            let spec = super::command::spec(&declaration).map_err(EvalAltResult::from)?;
            ctx.get::<Commands>()
                .map_err(|error| EvalAltResult::from(error.to_string()))?
                .contribute(
                    &ctx,
                    Command {
                        spec,
                        source: plugin.name().to_owned(),
                    },
                );
            Ok(())
        },
    );

    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "surface",
        move |declaration: Map| -> Result<(), Box<EvalAltResult>> {
            let (ctx, _) = wired.with("surface", |open| (open.ctx.clone(), ()))?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            let spec = super::surface::spec(&declaration).map_err(EvalAltResult::from)?;
            plugin.seed_surface(&spec);
            ctx.get::<Surfaces>()
                .map_err(|error| EvalAltResult::from(error.to_string()))?
                .contribute(
                    &ctx,
                    Surface {
                        spec,
                        source: plugin.name().to_owned(),
                    },
                );
            Ok(())
        },
    );

    // effect(|| { … }) and effect(|| { … }, || { … })
    let wired = Arc::clone(wiring);
    let holder = Arc::clone(owner);
    engine.register_fn(
        "effect",
        move |setup: FnPtr, teardown: FnPtr| -> Result<(), Box<EvalAltResult>> {
            let ctx = wired.context("effect")?;
            let plugin = holder
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| Box::<EvalAltResult>::from("the plugin is not loaded"))?;

            let acquiring = Arc::clone(&plugin);
            ctx.effect(move || {
                let _ = acquiring.call_fn(&setup, ());
                Disposer::sync(move || {
                    let _ = plugin.call_fn(&teardown, ());
                })
            });
            Ok(())
        },
    );
}
