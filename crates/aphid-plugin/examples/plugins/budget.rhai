//! Stops a run that has gone on too long.

const MAX_TOOL_CALLS = 40;

fn on_turn_end(cx, turn) {
    if turn.tool_calls > global::MAX_TOOL_CALLS {
        cx.note("Stopping: this turn asked for too many tools at once.");
        return stop();
    }
}
