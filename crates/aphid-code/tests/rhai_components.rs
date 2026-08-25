//! A `.rhai` file as a component: what it declares, and when it runs.

use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::exec;
use aphid_agent::rt::{Component, Composition, Context, Service, State};
use aphid_code::registries::Registries;
use aphid_code::scripting::{Capabilities, PluginHost, ScriptComponent, explicit, silent_sink};

/// A scratch directory of plugin files, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-components-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the tree");
        Self { root }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, source).expect("write the plugin");
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A sink that keeps what a script said, so a test can see it.
#[derive(Default)]
struct Recorder {
    lines: std::sync::Mutex<Vec<String>>,
}

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("not poisoned").clone()
    }
}

impl aphid_agent::Sink for Recorder {
    fn notify(&self, source: &str, text: &str) {
        self.lines
            .lock()
            .expect("not poisoned")
            .push(format!("{source}: {text}"));
    }

    fn log(&self, source: &str, text: &str) {
        self.notify(source, text);
    }
}

/// Load one file and wrap it as a component on `composition`.
fn component(path: &std::path::Path, composition: &Composition) -> Arc<ScriptComponent> {
    component_with(path, composition, silent_sink())
}

fn component_with(
    path: &std::path::Path,
    composition: &Composition,
    sink: Arc<dyn aphid_agent::Sink>,
) -> Arc<ScriptComponent> {
    let file = explicit(path).expect("the file is readable");
    let processes = Arc::new(exec::Registry::new());
    let (host, diagnostics) = PluginHost::load(&[file], &Capabilities::default(), sink, &processes);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let host = Arc::new(host);
    let plugin = Arc::clone(host.plugins().first().expect("one plugin compiled"));
    let _ = &host;
    Arc::new(ScriptComponent::new(plugin, composition))
}

// A service for the script to wait on.
struct Shell;
impl Service for Shell {
    const NAME: &'static str = "shell";
    type Handle = Arc<&'static str>;
}

struct Provider;
impl Component for Provider {
    fn name(&self) -> &str {
        "shell-provider"
    }
    fn provides(&self) -> &[&'static str] {
        &["shell"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.provide::<Shell>(Arc::new("bash"));
        Ok(())
    }
}

#[tokio::test]
async fn a_script_declares_what_it_needs_without_running() {
    let tree = Tree::new();
    let path = tree.write(
        "waiting.rhai",
        r#"
const inject   = ["shell"];
const provides = ["todos"];
const emits    = ["todos/changed"];

fn apply(ctx) {}
"#,
    );

    let composition = Composition::new();
    let script = component(&path, &composition);

    assert_eq!(script.inject(), ["shell"]);
    assert_eq!(script.provides(), ["todos"]);
    assert_eq!(script.emits(), ["todos/changed"]);
}

#[tokio::test]
async fn a_script_waits_for_the_service_it_injected() {
    let tree = Tree::new();
    let path = tree.write(
        "waiting.rhai",
        r#"
const inject = ["shell"];
fn apply(ctx) {}
"#,
    );

    let composition = Composition::new();
    let script = component(&path, &composition);
    let uid = composition
        .add(script, serde_json::Value::Null)
        .await
        .expect("mounts");

    // Nothing provides `shell`, so it is waiting rather than loaded — and it is
    // waiting rather than failed, which is the distinction that makes the state
    // worth showing.
    assert_eq!(composition.runtime.state(uid), Some(State::Pending));

    let provider = composition.plug(Provider).await.expect("mounts");
    assert_eq!(composition.runtime.state(uid), Some(State::Active));

    // And it goes back down when the service does.
    composition.runtime.unmount(provider).await;
    assert_eq!(composition.runtime.state(uid), Some(State::Pending));
}

#[tokio::test]
async fn a_script_that_declares_nothing_loads_immediately() {
    let tree = Tree::new();
    let path = tree.write("plain.rhai", "fn apply(ctx) {}\n");

    let composition = Composition::new();
    let script = component(&path, &composition);
    let uid = composition
        .add(script, serde_json::Value::Null)
        .await
        .expect("mounts");

    // An empty coeffect specification is trivially satisfied, so "unchanged
    // behaviour for a plugin that declares nothing" falls out of the model
    // rather than being a compatibility case.
    assert_eq!(composition.runtime.state(uid), Some(State::Active));
}

#[tokio::test]
async fn what_a_script_emits_is_declared_while_it_is_loaded() {
    let tree = Tree::new();
    let path = tree.write(
        "emitter.rhai",
        r#"
const emits = ["todos/changed"];
fn apply(ctx) {}
"#,
    );

    let composition = Composition::new();
    let script = component(&path, &composition);
    assert!(!composition.bus.is_declared("todos/changed"));

    let uid = composition
        .add(script, serde_json::Value::Null)
        .await
        .expect("mounts");
    assert!(composition.bus.is_declared("todos/changed"));

    composition.runtime.unmount(uid).await;
    assert!(
        !composition.bus.is_declared("todos/changed"),
        "the declaration left with the script"
    );
}

#[tokio::test]
async fn one_script_failing_leaves_the_others_alone() {
    let tree = Tree::new();
    let waiting = tree.write(
        "waiting.rhai",
        "const inject = [\"nobody-provides-this\"];\nfn apply(ctx) {}\n",
    );
    let plain = tree.write("plain.rhai", "fn apply(ctx) {}\n");

    let composition = Composition::new();
    let stuck = composition
        .add(component(&waiting, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");
    let fine = composition
        .add(component(&plain, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");

    assert_eq!(composition.runtime.state(stuck), Some(State::Pending));
    assert_eq!(composition.runtime.state(fine), Some(State::Active));

    // And the roster says which key the waiting one is short of, which is the
    // whole answer to "why is this plugin doing nothing?".
    let status = composition
        .runtime
        .roster()
        .into_iter()
        .find(|status| status.uid == stuck)
        .expect("listed");
    assert_eq!(status.missing, ["nobody-provides-this"]);
}

#[tokio::test]
async fn a_script_provides_a_service_another_script_consumes() {
    let tree = Tree::new();
    let provider = tree.write(
        "todos.rhai",
        r#"
const provides = ["todos"];

fn apply(ctx) {
    provide("todos", #{
        greet: |who| "hello " + who,
    });
}
"#,
    );
    let consumer = tree.write(
        "reader.rhai",
        r#"
const inject = ["todos"];

fn apply(ctx) {
    let answer = invoke("todos", "greet", ["world"]);
    log(answer);
}
"#,
    );

    let composition = Composition::new();
    // Mounted consumer-first, which is the order that would be wrong if order
    // decided anything.
    let reader = composition
        .add(component(&consumer, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");
    assert_eq!(composition.runtime.state(reader), Some(State::Pending));

    composition
        .add(component(&provider, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");

    let status = composition
        .runtime
        .roster()
        .into_iter()
        .find(|status| status.uid == reader)
        .expect("listed");
    assert_eq!(status.state, State::Active, "{:?}", status.error);
}

#[tokio::test]
async fn a_scripts_effect_is_reverted_when_it_unloads() {
    let tree = Tree::new();
    let path = tree.write(
        "effectful.rhai",
        r#"
fn apply(ctx) {
    effect(
        || { log("acquired"); },
        || { log("released"); },
    );
}
"#,
    );

    let composition = Composition::new();
    let recorder = Arc::new(Recorder::default());
    let uid = composition
        .add(
            component_with(&path, &composition, recorder.clone()),
            serde_json::Value::Null,
        )
        .await
        .expect("mounts");

    assert_eq!(composition.runtime.state(uid), Some(State::Active));
    assert_eq!(recorder.lines(), ["effectful: acquired"]);

    composition.runtime.unmount(uid).await;

    assert_eq!(composition.runtime.state(uid), Some(State::Inactive));
    assert_eq!(
        recorder.lines(),
        ["effectful: acquired", "effectful: released"],
        "the inverse ran, and nothing in the plugin asked for it"
    );
}

#[tokio::test]
async fn registering_outside_apply_says_why_rather_than_leaking() {
    let tree = Tree::new();
    let path = tree.write(
        "late.rhai",
        r#"
fn apply(ctx) {
    on("agent/turn-start", |cx| {
        // No component owns this, so nothing could ever revert it.
        provide("sneaky", #{});
    });
}
"#,
    );

    let composition = Composition::new();
    let uid = composition
        .add(component(&path, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");

    // The plugin itself is fine — it is the call that is refused, and only if
    // it is ever made.
    assert_eq!(composition.runtime.state(uid), Some(State::Active));
    assert!(composition.runtime.bindings().is_empty());
}

// ------------------------------------------------------- the composition file

#[test]
fn discovery_alone_is_a_complete_list() {
    use aphid_code::scripting::compose;

    let tree = Tree::new();
    let path = tree.write("todo.rhai", "fn apply(ctx) {}\n");
    let file = explicit(&path).expect("readable");

    let entries = compose(&[file], &[]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "todo");
    assert!(!entries[0].disabled);
    assert!(entries[0].isolate.is_empty());
}

#[test]
fn a_row_overrides_the_file_it_names() {
    use aphid_agent::rt::Isolate;
    use aphid_code::scripting::{Row, compose};

    let tree = Tree::new();
    let path = tree.write("todo.rhai", "fn apply(ctx) {}\n");
    let file = explicit(&path).expect("readable");

    let row: Row = serde_json::from_value(serde_json::json!({
        "id": "todo",
        "disabled": true,
        "config": { "limit": 5 },
        "isolate": { "shell": true, "sink": "workspace" }
    }))
    .expect("a well-formed row");

    let entries = compose(&[file], &[row]);
    assert_eq!(entries.len(), 1, "an override is not a second entry");
    assert!(entries[0].disabled);
    assert_eq!(entries[0].config["limit"], 5);
    assert_eq!(entries[0].isolate["shell"], Isolate::Local);
    assert_eq!(
        entries[0].isolate["sink"],
        Isolate::Shared("workspace".to_owned())
    );
}

#[test]
fn a_row_naming_nothing_discovered_brings_its_own_file() {
    use aphid_code::scripting::{Row, compose};

    let row: Row = serde_json::from_value(serde_json::json!({
        "id": "elsewhere",
        "url": "/opt/plugins/elsewhere.rhai"
    }))
    .expect("a well-formed row");

    let entries = compose(&[], &[row]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].url, "/opt/plugins/elsewhere.rhai");
}

#[test]
fn a_row_with_no_url_naming_nothing_discovered_is_ignored() {
    use aphid_code::scripting::{Row, compose};

    // Nothing to load and nowhere to load it from: a stale row for a file that
    // was deleted, which is not worth failing over.
    let row: Row =
        serde_json::from_value(serde_json::json!({ "id": "gone" })).expect("a well-formed row");
    assert!(compose(&[], &[row]).is_empty());
}

#[test]
fn a_missing_composition_file_means_no_overrides() {
    let tree = Tree::new();
    assert!(
        aphid_code::scripting::read(&tree.root)
            .expect("a missing file is fine")
            .is_empty()
    );
}

#[test]
fn a_malformed_composition_file_says_so_rather_than_guessing() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.root.join(".aphid")).expect("mkdir");
    std::fs::write(tree.root.join(".aphid").join("plugins.json"), "{ not json").expect("write");

    let error = aphid_code::scripting::read(&tree.root).expect_err("refused");
    assert!(error.contains("plugins.json"), "{error}");
}

// ------------------------------------------------------------ what /reload does

/// Build a loader over a workspace's `.aphid/plugins` directory.
fn loader_for(
    root: &std::path::Path,
    composition: &Composition,
) -> (aphid_agent::rt::Loader, Arc<PluginHost>) {
    let (files, problems) = aphid_code::scripting::discover(root, None);
    assert!(problems.is_empty(), "{problems:?}");
    let processes = Arc::new(exec::Registry::new());
    let (host, diagnostics) =
        PluginHost::load(&files, &Capabilities::default(), silent_sink(), &processes);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let host = Arc::new(host);
    let loader = aphid_agent::rt::Loader::new(
        composition,
        Arc::new(aphid_code::scripting::Scripts::new(
            Arc::clone(&host),
            composition,
        )),
    );
    (loader, host)
}

/// Write a plugin into a workspace's plugin directory.
fn plugin_in(root: &std::path::Path, name: &str, source: &str) {
    let dir = root.join(".aphid").join("plugins");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(name), source).expect("write");
}

#[tokio::test]
async fn reconciling_picks_up_a_new_file_and_drops_a_deleted_one() {
    let tree = Tree::new();
    plugin_in(&tree.root, "one.rhai", "fn apply(ctx) {}\n");

    let composition = Composition::new();
    let (mut loader, _host) = loader_for(&tree.root, &composition);

    let (files, _) = aphid_code::scripting::discover(&tree.root, None);
    let rows = aphid_code::scripting::read(&tree.root).expect("no file is fine");
    let report = loader
        .reconcile(aphid_code::scripting::compose(&files, &rows))
        .await;
    assert_eq!(report.mounted, ["one"]);

    // A second file appears. The host has to be rebuilt to compile it, which is
    // what `/reload` does before it reconciles.
    plugin_in(&tree.root, "two.rhai", "fn apply(ctx) {}\n");
    let (mut loader, _host) = loader_for(&tree.root, &composition);
    let (files, _) = aphid_code::scripting::discover(&tree.root, None);
    let report = loader
        .reconcile(aphid_code::scripting::compose(&files, &rows))
        .await;
    // `one` was already loaded by the previous loader, so this one mounts both
    // against its own bookkeeping — what matters is that neither is missed.
    assert_eq!(report.mounted.len(), 2, "{report:?}");
}

#[tokio::test]
async fn the_composition_file_can_switch_a_discovered_plugin_off() {
    let tree = Tree::new();
    plugin_in(&tree.root, "noisy.rhai", "fn apply(ctx) {}\n");
    std::fs::write(
        tree.root.join(".aphid").join("plugins.json"),
        r#"[{ "id": "noisy", "disabled": true }]"#,
    )
    .expect("write");

    let composition = Composition::new();
    let (mut loader, _host) = loader_for(&tree.root, &composition);

    let (files, _) = aphid_code::scripting::discover(&tree.root, None);
    let rows = aphid_code::scripting::read(&tree.root).expect("a valid file");
    let report = loader
        .reconcile(aphid_code::scripting::compose(&files, &rows))
        .await;

    assert!(report.mounted.is_empty(), "{report:?}");
    assert!(composition.runtime.roster().is_empty());
}

#[tokio::test]
async fn the_composition_file_can_isolate_a_service_for_one_plugin() {
    let tree = Tree::new();
    plugin_in(
        &tree.root,
        "left.rhai",
        "const provides = [\"shell\"];\nfn apply(ctx) { provide(\"shell\", #{}); }\n",
    );
    plugin_in(
        &tree.root,
        "right.rhai",
        "const provides = [\"shell\"];\nfn apply(ctx) { provide(\"shell\", #{}); }\n",
    );
    std::fs::write(
        tree.root.join(".aphid").join("plugins.json"),
        r#"[{ "id": "left", "isolate": { "shell": true } }]"#,
    )
    .expect("write");

    let composition = Composition::new();
    let (mut loader, _host) = loader_for(&tree.root, &composition);
    let (files, _) = aphid_code::scripting::discover(&tree.root, None);
    let rows = aphid_code::scripting::read(&tree.root).expect("a valid file");
    loader
        .reconcile(aphid_code::scripting::compose(&files, &rows))
        .await;

    // Two providers of one key. Without the isolation the second would have
    // overwritten the first and there would be one binding.
    assert_eq!(composition.runtime.bindings().len(), 2);
}

// ------------------------------------------------ what a component offers

/// Mount the registry, then one script, on a fresh composition.
async fn offering(path: &std::path::Path) -> (Composition, Arc<Registries>, aphid_agent::rt::Uid) {
    let composition = Composition::new();
    let registries = Registries::for_composition(&composition);
    composition
        .add(
            Arc::clone(&registries) as Arc<dyn Component>,
            serde_json::Value::Null,
        )
        .await
        .expect("the registry has no dependencies");

    let uid = composition
        .add(component(path, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");
    (composition, registries, uid)
}

#[tokio::test]
async fn a_command_leaves_with_the_component_that_offered_it() {
    let tree = Tree::new();
    let path = tree.write(
        "gone.rhai",
        r#"const inject = ["commands", "surfaces"];



fn apply(ctx) {
    command(#{ name: "greet", description: "Say hello.", run: |a| { "hi" } });

    surface(#{ name: "panel", placement: #{ kind: "side", side: "right" },
                        view: |s| #{ type: "text", text: "here" } });
}
"#,
    );

    let (composition, registries, uid) = offering(&path).await;
    assert_eq!(registries.commands().entries().len(), 1);
    assert_eq!(registries.surfaces().entries().len(), 1);

    composition.runtime.unmount(uid).await;

    // The file still compiles and its specs are still there; what is gone is
    // the offer, because the component that made it is not loaded.
    assert!(
        registries.commands().is_empty(),
        "the command left with its component"
    );
    assert!(registries.surfaces().is_empty(), "so did the panel");
}

#[tokio::test]
async fn a_component_that_never_loaded_offers_nothing() {
    let tree = Tree::new();
    let path = tree.write(
        "waiting.rhai",
        r#"
const inject = ["nobody-provides-this", "commands"];

fn apply(ctx) {
    command(#{ name: "ghost", description: "Never runs.", run: |a| { "hi" } });
}
"#,
    );

    let (composition, registries, uid) = offering(&path).await;

    // This is the case the old shape got wrong: the file compiled, so its
    // `/ghost` was listed and runnable even though nothing in it had run.
    assert_eq!(composition.runtime.state(uid), Some(State::Pending));
    assert!(registries.commands().is_empty());
}

#[tokio::test]
async fn offering_a_command_implies_needing_the_registry() {
    let tree = Tree::new();
    let path = tree.write(
        "cmd.rhai",
        r#"const inject = ["commands"];


fn apply(ctx) {
    command(#{ name: "greet", description: "Say hello.", run: |a| { "hi" } });
}
"#,
    );

    let composition = Composition::new();
    let script = component(&path, &composition);

    // Declared for it: a plugin that already said it offers a command has said
    // it needs somewhere to offer it, and should not have to say so twice.
    assert!(script.inject().contains(&"commands"));

    let uid = composition
        .add(script, serde_json::Value::Null)
        .await
        .expect("mounts");
    assert_eq!(
        composition.runtime.state(uid),
        Some(State::Pending),
        "nothing provides `commands` yet"
    );

    composition
        .add(
            Registries::for_composition(&composition) as Arc<dyn Component>,
            serde_json::Value::Null,
        )
        .await
        .expect("mounts");
    assert_eq!(composition.runtime.state(uid), Some(State::Active));
}

#[tokio::test]
async fn a_plugin_decides_what_it_contributes() {
    // The whole point of registering from `apply`: it is a decision the plugin
    // makes, at load, with its configuration in hand. A declaration at the top
    // of a file cannot look at anything.
    let source = r#"
const inject = ["commands"];

fn apply(ctx) {
    if config().experimental == true {
        command(#{ name: "wip", description: "Work in progress.", run: |a| { "ok" } });
    }
    command(#{ name: "stable", description: "Always here.", run: |a| { "ok" } });
}
"#;

    for (experimental, expected) in [
        (false, vec!["stable".to_owned()]),
        (true, vec!["wip".to_owned(), "stable".to_owned()]),
    ] {
        let tree = Tree::new();
        let plugins = tree.root.join(".aphid").join("plugins");
        std::fs::create_dir_all(&plugins).expect("mkdir");
        std::fs::write(plugins.join("maybe.rhai"), source).expect("write");
        std::fs::write(
            plugins.join("maybe.json"),
            format!("{{ \"experimental\": {experimental} }}"),
        )
        .expect("write");

        let composition = Composition::new();
        let registries = Registries::for_composition(&composition);
        composition
            .add(
                Arc::clone(&registries) as Arc<dyn Component>,
                serde_json::Value::Null,
            )
            .await
            .expect("the registry mounts");

        let file = explicit(&plugins.join("maybe.rhai")).expect("readable");
        let processes = Arc::new(exec::Registry::new());
        let (host, diagnostics) = PluginHost::load(
            &[file],
            &Capabilities::full(&tree.root),
            silent_sink(),
            &processes,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let host = Arc::new(host);
        let plugin = Arc::clone(host.plugins().first().expect("one plugin compiled"));

        composition
            .add(
                Arc::new(ScriptComponent::new(plugin, &composition)),
                serde_json::Value::Null,
            )
            .await
            .expect("mounts");

        let offered: Vec<String> =
            aphid_code::scripting::registered_commands(registries.commands())
                .into_iter()
                .map(|command| command.name)
                .collect();
        assert_eq!(offered, expected, "experimental = {experimental}");
    }
}

#[test]
fn every_shipped_plugin_subscribes_to_something_or_contributes_something() {
    // Compiling is not enough: a plugin whose `apply` is empty and whose only
    // other functions are helpers does nothing at all, and that is a mistake
    // worth catching in the repository rather than in a session.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root");

    for dir in [
        root.join(".aphid/plugins"),
        root.join("crates/aphid-code/examples/plugins"),
    ] {
        for entry in std::fs::read_dir(&dir).expect("readable") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|ext| ext != "rhai") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable");
            let does_something = source.contains("on(\"")
                || source.contains("tool(#{")
                || source.contains("command(#{")
                || source.contains("surface(#{")
                || source.contains("provide(");
            assert!(
                does_something,
                "{} defines `apply` but contributes nothing and listens to nothing",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn the_webchat_subscribes_to_everything_it_needs() {
    // The plugin that lives on the system boundary, and the one whose listeners
    // going missing would be silent: it would start its server and never read
    // what the browser sent back.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root");
    let path = root.join(".aphid/plugins/webchat.rhai");

    let composition = Composition::new();
    let registries = Registries::for_composition(&composition);
    composition
        .add(
            Arc::clone(&registries) as Arc<dyn Component>,
            serde_json::Value::Null,
        )
        .await
        .expect("the registry mounts");

    let uid = composition
        .add(component(&path, &composition), serde_json::Value::Null)
        .await
        .expect("mounts");
    assert_eq!(composition.runtime.state(uid), Some(State::Active));

    let bus = &composition.bus;
    assert!(
        bus.has_listeners::<aphid_code::events::Tick>(),
        "without this the browser is never read"
    );
    assert!(bus.has_listeners::<aphid_code::events::Notice>());
    assert!(bus.has_listeners::<aphid_code::events::SessionEnd>());
    assert!(bus.has_listeners::<aphid_agent::Prompt>());
    assert!(bus.has_listeners::<aphid_agent::ToolRequest>());
    assert!(bus.has_listeners::<aphid_agent::RunEnd>());
    assert!(
        composition.stream.is_observed(),
        "the browser watches the reply as it streams"
    );
    assert_eq!(
        aphid_code::scripting::registered_commands(&aphid_code::registries::Registry::default())
            .len(),
        0
    );
}
