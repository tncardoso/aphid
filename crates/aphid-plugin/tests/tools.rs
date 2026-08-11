//! Tools written in Rhai, driven through a real agent run.

use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::exec;
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{Agent, ToolCx, ToolOutcome, tool_fn};
use aphid_core::{ContentRef, MessageRef, providers::deepseek};
use aphid_plugin::{Capabilities, PluginHost, Silent, explicit};
use serde::Deserialize;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-tools-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        std::fs::write(root.join(".aphid").join("plugins").join("kit.rhai"), source)
            .expect("write the plugin");
        Self { root }
    }

    fn host(&self) -> Arc<PluginHost> {
        let file =
            explicit(&self.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
        let (host, diagnostics) = PluginHost::load(
            &[file],
            &Capabilities::full(&self.root),
            Arc::new(Silent),
            &Arc::new(exec::Registry::new()),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        Arc::new(host)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text_of(message: MessageRef<'_>) -> String {
    message
        .content()
        .filter_map(|part| match part {
            ContentRef::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

const WORDCOUNT: &str = r#"
register_tool(#{
    name: "wordcount",
    description: "Count the words in a file.",
    parameters: #{
        type: "object",
        properties: #{ path: #{ type: "string" } },
        required: ["path"]
    },
    execute: |args| { fs_read(args.path).split(' ').len() }
});
"#;

#[tokio::test]
async fn a_script_tool_is_declared_and_runs() {
    let fixture = Fixture::new(WORDCOUNT);
    std::fs::write(fixture.root.join("poem.txt"), "one two three four").expect("write");

    let (backend, _script) = scripted([
        Turn::call("call_1", "wordcount", r#"{"path":"poem.txt"}"#),
        Turn::text("four words"),
    ]);

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .plugin_arc(fixture.host())
        .stream_fn(backend)
        .build();

    let declared: Vec<&str> = agent.tools().names().collect();
    assert_eq!(declared, ["wordcount"], "the provider is told about it");

    agent.prompt("count them").await;

    assert_eq!(text_of(agent.transcript().get(2).expect("a result")), "4");
}

#[tokio::test]
async fn a_script_tool_shadows_a_built_in_of_the_same_name() {
    let fixture = Fixture::new(
        r#"
        register_tool(#{
            name: "echo",
            description: "A better echo.",
            parameters: #{ type: "object" },
            execute: |args| { "from the script" }
        });
        "#,
    );

    #[derive(Deserialize)]
    struct Echo {
        value: String,
    }

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(tool_fn(
            "echo",
            "The original.",
            serde_json::json!({ "type": "object" }),
            |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(args.value) },
        ))
        .plugin_arc(fixture.host())
        .stream_fn(backend)
        .build();

    assert_eq!(agent.tools().len(), 1, "one name, one tool");

    agent.prompt("go").await;

    assert_eq!(
        text_of(agent.transcript().get(2).expect("a result")),
        "from the script"
    );
}

#[tokio::test]
async fn a_tool_that_raises_becomes_an_error_result() {
    let fixture = Fixture::new(
        r#"
        register_tool(#{
            name: "broken",
            description: "Always fails.",
            parameters: #{ type: "object" },
            execute: |args| { throw "no" }
        });
        "#,
    );

    let (backend, _script) = scripted([Turn::call("call_1", "broken", "{}"), Turn::text("noted")]);

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .plugin_arc(fixture.host())
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    assert_eq!(outcome.turns, 2, "the run carried on");
    let result = agent.transcript().get(2).expect("a result");
    assert!(
        result.tool_result().is_some_and(|meta| meta.is_error),
        "it is marked as an error the model can read"
    );
    assert!(text_of(result).contains("no"), "{}", text_of(result));
}

#[tokio::test]
async fn a_tool_can_return_content_and_details() {
    let fixture = Fixture::new(
        r#"
        register_tool(#{
            name: "rich",
            description: "Returns structure.",
            parameters: #{ type: "object" },
            execute: |args| { #{ content: "looked", details: #{ found: 3 } } }
        });
        "#,
    );

    let (backend, _script) = scripted([Turn::call("call_1", "rich", "{}"), Turn::text("ok")]);

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .plugin_arc(fixture.host())
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    let result = agent.transcript().get(2).expect("a result");
    assert_eq!(text_of(result), "looked");
    let details = result
        .tool_result()
        .and_then(|meta| meta.details.clone())
        .expect("details survived");
    assert_eq!(details["found"], 3);
}

#[test]
fn a_malformed_declaration_is_refused_at_load_time() {
    let fixture = Fixture::new(r#"register_tool(#{ description: "no name" });"#);

    let file =
        explicit(&fixture.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
    let (_host, diagnostics) = PluginHost::load(
        &[file],
        &Capabilities::full(&fixture.root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );

    assert_eq!(diagnostics.len(), 1, "the plugin did not load");
    assert!(
        diagnostics[0].message.contains("needs a `name`"),
        "{}",
        diagnostics[0].message
    );
}

#[test]
fn a_script_command_reports_and_steers() {
    use aphid_plugin::Action;

    let fixture = Fixture::new(
        r#"
        register_command(#{
            name: "review",
            description: "Ask for a review.",
            run: |args| {
                if args == "" { return notice("give me something to review"); }
                prompt("Review " + args + " please.");
                notice("reviewing " + args)
            }
        });
        "#,
    );
    let host = fixture.host();

    let listed = host.commands();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].invocation, "review");
    assert_eq!(listed[0].description, "Ask for a review.");

    assert_eq!(
        host.run_command("review", ""),
        Some(vec![Action::Notice(
            "give me something to review".to_owned()
        )])
    );
    // The prompt went to the sink as it was called; what comes back is only
    // what the user should read.
    assert_eq!(
        host.run_command("review", "src/lib.rs"),
        Some(vec![Action::Notice("reviewing src/lib.rs".to_owned())])
    );
    assert_eq!(host.run_command("nothing", ""), None, "no plugin owns it");
}

#[test]
fn colliding_command_names_both_stay_reachable() {
    use aphid_plugin::{Capabilities, Silent};

    let one = Fixture::new(r#"register_command(#{ name: "review", run: |args| { "from one" } });"#);
    let two = Fixture::new(r#"register_command(#{ name: "review", run: |args| { "from two" } });"#);

    let files = vec![
        explicit(&one.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable"),
        explicit(&two.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable"),
    ];
    // Both files are named `kit`, so name them apart the way discovery would.
    let mut files = files;
    files[1].name = "kit2".to_owned();

    let (host, diagnostics) = PluginHost::load(
        &files,
        &Capabilities::full(&one.root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    let listed = host.commands();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].invocation, "review");
    assert_eq!(
        listed[1].invocation, "review:2",
        "the later one is suffixed"
    );

    assert_eq!(
        host.run_command("review:2", ""),
        Some(vec![aphid_plugin::Action::Notice("from two".to_owned())])
    );
}

#[test]
fn a_command_that_raises_is_reported_and_does_nothing() {
    let fixture =
        Fixture::new(r#"register_command(#{ name: "boom", run: |args| { throw "nope" } });"#);
    let host = fixture.host();

    assert_eq!(
        host.run_command("boom", ""),
        Some(Vec::new()),
        "the command is known, it just produced nothing"
    );
}
