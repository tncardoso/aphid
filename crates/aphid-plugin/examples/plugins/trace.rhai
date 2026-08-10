//! Reports every tool call and what the run cost.

fn on_tool_call(tool) {
    notify(tool.name + " " + tool.arguments);
}

fn on_run_end(cx, outcome) {
    notify(
        outcome.turns + " turns, " +
        outcome.input + " in / " + outcome.output + " out, stopped: " + outcome.stop
    );
}
