//! Stops a run that has gone on too long.
//!
//! The limit is configuration rather than a constant, so `.aphid/plugins/budget.json`
//! can raise it without editing the plugin.

fn apply(ctx) {
    let limit = config().max_tool_calls ?? 40;

    on("agent/turn-end", |cx, turn| {
        if turn.tool_calls > limit {
            cx.note("Stopping: this turn asked for too many tools at once.");
            return stop();
        }
    });
}
