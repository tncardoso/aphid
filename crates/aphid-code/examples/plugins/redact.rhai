//! Keeps anything that looks like a key out of the transcript.

fn apply(ctx) {
    on("agent/tool-result", |result| {
        if result.content.contains("sk-") || result.content.contains("BEGIN PRIVATE KEY") {
            return #{ content: "[redacted by the redact plugin]" };
        }
    });
}
