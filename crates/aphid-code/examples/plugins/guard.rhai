//! Keeps the model away from files that are maintained by hand.

const PROTECTED = ["CHANGELOG", "LICENSE", ".env"];

fn apply(ctx) {
    on("agent/tool-call", |tool| {
        if tool.name != "write" && tool.name != "edit" {
            return;
        }

        for name in global::PROTECTED {
            if tool.arguments.contains(name) {
                return block(name + " is maintained by hand");
            }
        }
    });
}
