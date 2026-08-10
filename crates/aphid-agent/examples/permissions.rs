//! A plugin that gates and rewrites tool calls, and a run that exercises it.
//!
//! This is the shape a real permission gate takes: watch `on_tool_call`, veto
//! what should not run, patch what should run differently, and annotate results
//! on the way back. It uses the scripted backend, so it needs no API key:
//!
//! ```text
//! cargo run -p aphid-agent --example permissions
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{
    Agent, Guard, Interest, PendingCall, Plugin, ResultCx, ToolContent, ToolCx, ToolHandler,
    ToolOutcome, tool_fn,
};
use aphid_core::{Role, providers::deepseek};

/// Blocks destructive shell commands, keeps every command inside a workspace,
/// and marks results it touched.
struct Permissions {
    workspace: String,
    blocked: Arc<AtomicUsize>,
}

impl Plugin for Permissions {
    fn name(&self) -> &str {
        "permissions"
    }

    fn interests(&self) -> Interest {
        Interest::TOOL_CALL | Interest::TOOL_RESULT
    }

    /// The plugin brings its own tool, so a host that loads it gets `bash`
    /// without wiring anything up.
    fn tools(&self) -> Vec<Arc<dyn ToolHandler>> {
        vec![Arc::new(tool_fn(
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
        ))]
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        if call.name() != "bash" {
            return Guard::Allow;
        }

        let Ok(mut args) = serde_json::from_str::<serde_json::Value>(call.arguments()) else {
            return Guard::block("arguments were not valid JSON");
        };
        let command = args["command"].as_str().unwrap_or_default().to_owned();

        if command.contains("rm -rf") || command.contains("sudo") {
            self.blocked.fetch_add(1, Ordering::Relaxed);
            // The model reads this reason and can try something else.
            return Guard::block(format!("`{command}` is destructive and was refused"));
        }

        // Patch rather than refuse: pin the command to the workspace. Later
        // plugins see this edit, and nothing re-validates it against the schema.
        args["command"] = serde_json::Value::String(format!("cd {} && {command}", self.workspace));
        call.set_arguments(args.to_string());

        Guard::Allow
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        if cx.name() != "bash" || outcome.is_error {
            return;
        }
        // Result hooks chain: each sees the previous one's edits.
        outcome.content.push(ToolContent::Text(format!(
            "\n[audited by permissions, workspace {}]",
            self.workspace
        )));
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

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .system("You are a careful shell operator.")
        .plugin(Permissions {
            workspace: "/srv/project".to_owned(),
            blocked: Arc::clone(&blocked),
        })
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
