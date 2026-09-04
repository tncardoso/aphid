//! The per-session tool that sends a file through an attachment-capable gateway.

use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::Toolbox;
use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::{ToolCx, ToolOutcome, tool_fn};
use aphid_code::plugins::permissions::{Permissions, Risk};
use aphid_code::tools::Workspace;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::server::AttachmentSender;

pub const NAME: &str = "send_attachment";

/// Installs the attachment tool only for one gateway session.
pub struct AttachmentComponent {
    workspace: Workspace,
    session: String,
    destination: String,
    sender: AttachmentSender,
    permissions: Arc<Permissions>,
    limit: u64,
    tools: Arc<Toolbox>,
}

impl AttachmentComponent {
    #[must_use]
    pub fn new(
        workspace: Workspace,
        session: String,
        destination: String,
        sender: AttachmentSender,
        permissions: Arc<Permissions>,
        limit: u64,
        composition: &Composition,
    ) -> Self {
        Self {
            workspace,
            session,
            destination,
            sender,
            permissions,
            limit,
            tools: Arc::clone(&composition.tools),
        }
    }
}

impl Component for AttachmentComponent {
    fn name(&self) -> &str {
        "gateway-attachment"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.tools.contribute_scoped(
            ctx,
            self.session.clone(),
            Arc::new(tool(
                self.workspace.clone(),
                self.session.clone(),
                self.destination.clone(),
                self.sender.clone(),
                self.permissions.clone(),
                self.limit,
            )),
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct Params {
    path: String,
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Clone)]
struct Target {
    session: String,
    destination: String,
}

/// Build the gateway-specific attachment tool.
#[must_use]
pub fn tool(
    workspace: Workspace,
    session: String,
    destination: String,
    sender: AttachmentSender,
    permissions: Arc<Permissions>,
    limit: u64,
) -> impl aphid_agent::ToolHandler {
    tool_fn(
        NAME,
        "Send a file to the user in this gateway conversation. Use this only when the user explicitly asks for the file or asks for it to be delivered.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A file path inside an allowed workspace read root." },
                "caption": { "type": "string", "description": "An optional caption sent with the file." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |params: Params, cx: ToolCx| {
            let workspace = workspace.clone();
            let target = Target {
                session: session.clone(),
                destination: destination.clone(),
            };
            let sender = sender.clone();
            let permissions = permissions.clone();
            async move {
                send(
                    &workspace,
                    &target,
                    &sender,
                    &permissions,
                    limit,
                    params,
                    cx,
                )
                .await
            }
        },
    )
    .sequential()
}

async fn send(
    workspace: &Workspace,
    target: &Target,
    sender: &AttachmentSender,
    permissions: &Permissions,
    limit: u64,
    params: Params,
    cx: ToolCx,
) -> ToolOutcome {
    if cx.cancelled() {
        return ToolOutcome::error("attachment cancelled before it was sent");
    }
    let first = match read_file(workspace, &params.path, limit).await {
        Ok(file) => file,
        Err(error) => return ToolOutcome::error(error),
    };
    let File { path, bytes, hash } = first;
    let size = bytes.len();
    drop(bytes);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment")
        .to_owned();
    let caption = params.caption.clone();
    let short_hash = &hash[..12];
    let summary = format!(
        "send {name} ({size} bytes, sha256 {short_hash}) from {} to {}{}",
        workspace.display(&path),
        target.destination,
        caption
            .as_deref()
            .map(|caption| format!(" with caption: {caption}"))
            .unwrap_or_default(),
    );
    if let Some(blocked) = permissions.verdict_with(sender, NAME, &summary, Risk::Mutate) {
        return ToolOutcome::error(blocked.reason).terminating();
    }
    if cx.cancelled() {
        return ToolOutcome::error("attachment cancelled before it was sent");
    }

    // Do not retain file bytes while a person considers the confirmation. A
    // second read must be identical to the one that was approved.
    let second = match read_file(workspace, &params.path, limit).await {
        Ok(file) => file,
        Err(error) => return ToolOutcome::error(error),
    };
    if second.hash != hash || second.bytes.len() != size {
        return ToolOutcome::error("the attachment changed while permission was pending");
    }
    if cx.cancelled() {
        return ToolOutcome::error("attachment cancelled before it was sent");
    }

    let data = base64::engine::general_purpose::STANDARD.encode(second.bytes);
    let result =
        tokio::task::block_in_place(|| sender.send(&target.session, name.clone(), data, caption));
    match result {
        Ok(()) => ToolOutcome::text(format!("sent {name}")),
        Err(error) => ToolOutcome::error(format!("could not send {name}: {error}")),
    }
}

struct File {
    path: PathBuf,
    bytes: Vec<u8>,
    hash: String,
}

async fn read_file(workspace: &Workspace, input: &str, limit: u64) -> Result<File, String> {
    if limit == 0 {
        return Err("attachments are disabled for this gateway".to_owned());
    }
    let path = workspace.resolve_read(input)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("could not inspect {input}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{input} is not a regular file"));
    }
    if metadata.len() > limit {
        return Err(format!(
            "{input} is {} bytes, above the attachment limit of {limit} bytes",
            metadata.len()
        ));
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {input}: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "{input} grew above the attachment limit while it was read"
        ));
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(File { path, bytes, hash })
}
