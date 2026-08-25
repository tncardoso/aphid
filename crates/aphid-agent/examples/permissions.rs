//! A component that gates and rewrites tool calls, and a run that exercises it.
//!
//! This is the shape a real permission gate takes: listen for a tool request,
//! refuse what should not run, patch what should run differently, and annotate
//! results on the way back. It uses the scripted backend, so it needs no API
//! key:
//!
//! ```text
//! cargo run -p aphid-agent --example permissions
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{
    Agent, Blocked, ToolContent, ToolCx, ToolOutcome, ToolRequest, ToolResult, tool_fn,
};
use aphid_core::{Role, providers::deepseek};

/// Blocks destructive shell commands, keeps every command inside a workspace,
/// and marks results it touched.
struct Permissions {
    workspace: String,
    blocked: Arc<AtomicUsize>,
    composition: Composition,
}

impl Component for Permissions {
    fn name(&self) -> &str {
        "permissions"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();

        // The component brings its own tool, so a composition that mounts it
        // gets `bash` without anybody wiring one up — and loses it again if the
        // component is ever unloaded.
        self.composition.tools.contribute(
            ctx,
            Arc::new(tool_fn(
                "bash",
                "Run a shell command.",
                serde_json::json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }),
                |args: serde_json::Value, _cx: ToolCx| async move {
                    let command = args["command"].as_str().unwrap_or_default();
                    // A real handler would spawn a shell here.
                    ToolOutcome::text(format!("$ {command}\n(pretend output)"))
                },
            )),
        );

        let workspace = self.workspace.clone();
        let blocked = Arc::clone(&self.blocked);
        self.composition
            .bus
            .on::<ToolRequest>(owner, move |request| {
                if request.name != "bash" {
                    return;
                }

                let Ok(mut args) = serde_json::from_str::<serde_json::Value>(&request.arguments)
                else {
                    request.refuse(Blocked::new("arguments were not valid JSON"));
                    return;
                };
                let command = args["command"].as_str().unwrap_or_default().to_owned();

                if command.contains("rm -rf") || command.contains("sudo") {
                    blocked.fetch_add(1, Ordering::Relaxed);
                    // The model reads this reason and can try something else.
                    request.refuse(Blocked::new(format!(
                        "`{command}` is destructive and was refused"
                    )));
                    return;
                }

                // Patch rather than refuse: pin the command to the workspace. Later
                // listeners see this edit, and nothing re-validates it against the
                // schema.
                args["command"] = serde_json::Value::String(format!("cd {workspace} && {command}"));
                request.arguments = args.to_string();
            });

        let workspace = self.workspace.clone();
        self.composition.bus.on::<ToolResult>(owner, move |result| {
            if result.name != "bash" || result.is_error {
                return;
            }
            // Result listeners chain: each sees the previous one's edits.
            result.content.push(ToolContent::Text(format!(
                "\n[audited by permissions, workspace {workspace}]"
            )));
        });

        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let blocked = Arc::new(AtomicUsize::new(0));

    // What the model "says": first a refused command, then an allowed one.
    let (backend, _script) = scripted([
        Turn::call("call_1", "bash", r#"{"command":"rm -rf /"}"#),
        Turn::call("call_2", "bash", r#"{"command":"ls -la"}"#),
        Turn::text("Listed the workspace."),
    ]);

    let composition = Composition::new();
    composition
        .plug(Permissions {
            workspace: "/srv/project".to_owned(),
            blocked: Arc::clone(&blocked),
            composition: composition.clone(),
        })
        .await
        .expect("the gate has no dependencies and no schema");

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .system("You are a careful shell operator.")
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("clean up and then list the directory").await;

    for message in agent.transcript().iter() {
        let body: String = message
            .content()
            .filter_map(|content| content.text())
            .collect();
        let label = match message.role() {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::ToolResult => "tool",
        };
        let flag = match message.tool_result() {
            Some(meta) if meta.is_error => " (error)",
            _ => "",
        };
        println!("{label:>9}{flag}: {}", body.replace('\n', "\n           "));
    }

    println!(
        "\n{} turns, {} call(s) refused, stopped with {:?}",
        outcome.turns,
        blocked.load(Ordering::Relaxed),
        outcome.stop
    );
}
