//! Reports every tool call and what the run cost.

fn apply(ctx) {
    on("agent/tool-call", |tool| {
        notify(tool.name + " " + tool.arguments);
    });

    on("agent/run-end", |cx, outcome| {
        notify(
            outcome.turns + " turns, " +
            outcome.input + " in / " + outcome.output + " out, stopped: " + outcome.stop
        );
    });
}
