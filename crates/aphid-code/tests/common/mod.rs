//! Shared test support.
//!
//! Commands and panels are offered by *loaded components*, not by files that
//! compiled, so a test that wants to see them has to mount the plugins. This
//! bundles the two halves — the host, for the state and the scripts, and the
//! registry, for what they are currently offering — behind the method names the
//! tests already used.

#![allow(dead_code)]

use std::ops::Deref;
use std::sync::Arc;

use aphid_agent::rt::{Component, Composition};
use aphid_code::registries::Registries;
use aphid_code::scripting::{
    Action, PluginHost, Registered, RegisteredSurface, ScriptComponent, registered_commands,
    registered_surfaces,
};

/// A host whose plugins are mounted, and what they offer.
pub struct Loaded {
    pub host: Arc<PluginHost>,
    pub registries: Arc<Registries>,
    pub composition: Composition,
}

impl Loaded {
    /// Mount every compiled plugin on a fresh composition.
    pub fn new(host: Arc<PluginHost>) -> Loaded {
        let composition = Composition::new();
        let registries = Registries::for_composition(&composition);
        composition
            .mount(
                Arc::clone(&registries) as Arc<dyn Component>,
                serde_json::Value::Null,
            )
            .expect("the registry has no dependencies");
        for plugin in host.plugins() {
            composition
                .mount(
                    Arc::new(ScriptComponent::new(Arc::clone(plugin), &composition)),
                    serde_json::Value::Null,
                )
                .expect("a plugin mounts");
        }
        block_on(composition.runtime.settle());

        Loaded {
            host,
            registries,
            composition,
        }
    }

    pub fn commands(&self) -> Vec<Registered> {
        registered_commands(self.registries.commands())
    }

    pub fn surfaces(&self) -> Vec<RegisteredSurface> {
        registered_surfaces(self.registries.surfaces())
    }

    pub fn run_command(&self, invocation: &str, args: &str) -> Option<Vec<Action>> {
        self.host
            .run_command(self.registries.commands(), invocation, args)
    }
}

/// Everything else still lives on the host.
impl Deref for Loaded {
    type Target = PluginHost;

    fn deref(&self) -> &PluginHost {
        &self.host
    }
}

/// Drive a future to completion on the current thread.
///
/// Nothing here awaits anything real — mounting a plugin with no dependencies
/// never waits — so this never blocks.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    loop {
        if let std::task::Poll::Ready(value) = Future::poll(future.as_mut(), &mut cx) {
            return value;
        }
    }
}
