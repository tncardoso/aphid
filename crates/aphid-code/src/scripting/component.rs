//! One `.rhai` file, as a component.
//!
//! A fiber per file, not one for the whole directory. That is what makes a
//! script's `inject` mean anything: it waits for what it declared, loads when
//! that arrives, and unloads again if it goes away — on its own, without
//! taking the other scripts with it.
//!
//! Everything it contributes happens in its `apply` and is reverted when it
//! unloads: its listeners, its tools, its commands, its surfaces. Nothing about
//! that is written down by the plugin author.

use std::sync::Arc;

use aphid_agent::rt::{Component, Composition, Context, Disposer};

use super::script::ScriptPlugin;

/// One compiled script, mounted.
pub struct ScriptComponent {
    plugin: Arc<ScriptPlugin>,
    composition: Composition,
    inject: Vec<&'static str>,
    provides: Vec<&'static str>,
    emits: Vec<&'static str>,
}

/// Declarations are `&'static str` inside the runtime, and a script's are read
/// from a file. Leaking them is the honest trade: there are tens of names, they
/// live as long as the process, and the alternative is a lifetime on every key
/// in the coeffect store.
fn intern(names: &[String]) -> Vec<&'static str> {
    names
        .iter()
        .map(|name| &*Box::leak(name.clone().into_boxed_str()))
        .collect()
}

impl ScriptComponent {
    #[must_use]
    pub fn new(plugin: Arc<ScriptPlugin>, composition: &Composition) -> Self {
        let declares = plugin.declares();
        Self {
            inject: intern(&declares.inject),
            provides: intern(&declares.provides),
            emits: intern(&declares.emits),
            plugin,
            composition: composition.clone(),
        }
    }
}

impl Component for ScriptComponent {
    fn name(&self) -> &str {
        self.plugin.name()
    }

    fn inject(&self) -> &[&'static str] {
        &self.inject
    }

    fn provides(&self) -> &[&'static str] {
        &self.provides
    }

    fn emits(&self) -> &[&'static str] {
        &self.emits
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();
        let plugin = Arc::clone(&self.plugin);

        self.composition.bus.declare(&self.emits);

        // Everything a script registers through `ctx` happens here, and only
        // here: this is the one call the runtime can attach the registrations
        // to, and a `provide` from a hook would have no owner to revert it.
        plugin.apply(ctx, &self.composition)?;

        let composition = self.composition.clone();
        let emits = self.emits.clone();
        ctx.effect(move || {
            Disposer::sync(move || {
                super::subscribe::unsubscribe(&composition, owner);
                composition.bus.undeclare(&emits);
            })
        });
        Ok(())
    }
}
