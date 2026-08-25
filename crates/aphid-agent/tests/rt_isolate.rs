//! Realms: one key, independent bindings.

use std::sync::Arc;

use aphid_agent::rt::{Component, Context, Realm, Realms, Runtime, Service, State};
use serde_json::Value;

struct Shell;
impl Service for Shell {
    const NAME: &'static str = "shell";
    type Handle = Arc<&'static str>;
}

struct Provider(&'static str);
impl Component for Provider {
    fn name(&self) -> &str {
        self.0
    }
    fn provides(&self) -> &[&'static str] {
        &["shell"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.provide::<Shell>(Arc::new(self.0));
        Ok(())
    }
}

struct Consumer {
    seen: Arc<std::sync::Mutex<Vec<&'static str>>>,
}
impl Component for Consumer {
    fn name(&self) -> &str {
        "consumer"
    }
    fn inject(&self) -> &[&'static str] {
        &["shell"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.seen.lock().expect("ok").push(*ctx.need::<Shell>());
        Ok(())
    }
}

/// Two subtrees, each with its own `shell`, mounted from one parent.
struct Split {
    seen: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl Component for Split {
    fn name(&self) -> &str {
        "split"
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let left = ctx.isolate("shell");
        left.mount(Arc::new(Provider("bash")), Value::Null)?;
        left.mount(
            Arc::new(Consumer {
                seen: Arc::clone(&self.seen),
            }),
            Value::Null,
        )?;

        let right = ctx.isolate("shell");
        right.mount(Arc::new(Provider("fish")), Value::Null)?;
        right.mount(
            Arc::new(Consumer {
                seen: Arc::clone(&self.seen),
            }),
            Value::Null,
        )?;
        Ok(())
    }
}

#[tokio::test]
async fn each_scope_resolves_the_key_to_its_own_provider() {
    let rt = Runtime::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    rt.mount(
        Arc::new(Split {
            seen: Arc::clone(&seen),
        }),
        Value::Null,
    )
    .expect("mounts");
    rt.settle().await;

    let mut got = seen.lock().expect("ok").clone();
    got.sort_unstable();
    // Without the indirection the second provider would have overwritten the
    // first and both consumers would have seen the same thing.
    assert_eq!(got, ["bash", "fish"]);
    assert_eq!(rt.bindings().len(), 2);
}

#[tokio::test]
async fn a_reassignment_carries_a_fibers_own_binding_with_it() {
    let rt = Runtime::new();

    // One fiber that both provides and is isolated: the binding is its own, so
    // moving the fiber has to move the binding.
    let uid = rt
        .mount(Arc::new(Provider("bash")), Value::Null)
        .expect("mounts");
    rt.settle().await;

    let before = rt.bindings();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].0, Realm::Root("shell"));

    let realm = Realm::local("shell");
    rt.reassign(uid, Realms::assigned([("shell", realm.clone())]))
        .await;

    let after = rt.bindings();
    assert_eq!(after.len(), 1, "one binding, moved rather than duplicated");
    assert_eq!(after[0].0, realm);
    assert_eq!(
        after[0].1, before[0].1,
        "the same provider still provides it"
    );
}

#[tokio::test]
async fn a_dependent_moved_away_from_its_provider_loses_it() {
    let rt = Runtime::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    rt.mount(Arc::new(Provider("bash")), Value::Null)
        .expect("mounts");
    let consumer = rt
        .mount(
            Arc::new(Consumer {
                seen: Arc::clone(&seen),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;
    assert_eq!(rt.state(consumer), Some(State::Active));

    // The consumer alone moves to a realm nobody provides.
    rt.reassign(
        consumer,
        Realms::assigned([("shell", Realm::local("shell"))]),
    )
    .await;

    assert_eq!(
        rt.state(consumer),
        Some(State::Pending),
        "it is looking somewhere the provider is not"
    );
}

#[tokio::test]
async fn two_entries_naming_one_realm_share_a_binding() {
    let rt = Runtime::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    let provider = rt
        .mount(Arc::new(Provider("bash")), Value::Null)
        .expect("mounts");
    let consumer = rt
        .mount(
            Arc::new(Consumer {
                seen: Arc::clone(&seen),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;

    // A realm named by both: moving them together keeps them together, which is
    // the difference between a shared realm and a private one.
    rt.reassign(
        provider,
        Realms::assigned([("shell", Realm::shared("shell", "workspace"))]),
    )
    .await;
    rt.reassign(
        consumer,
        Realms::assigned([("shell", Realm::shared("shell", "workspace"))]),
    )
    .await;

    assert_eq!(rt.state(consumer), Some(State::Active));
    assert_eq!(rt.bindings().len(), 1);
}

#[tokio::test]
async fn reassigning_to_the_same_realms_changes_nothing() {
    let rt = Runtime::new();
    let uid = rt
        .mount(Arc::new(Provider("bash")), Value::Null)
        .expect("mounts");
    rt.settle().await;

    let before = rt.bindings();
    rt.reassign(uid, Realms::root()).await;
    assert_eq!(rt.bindings(), before);
}
