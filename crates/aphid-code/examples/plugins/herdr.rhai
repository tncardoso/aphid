//! Reports this session to Herdr, so its sidebar shows the pane as working,
//! blocked or idle instead of unknown. Does nothing when aphid runs outside
//! Herdr. Settings go in `.aphid/plugins/herdr.json`, and in a home directory,
//! which also covers headless sessions.

// Herdr is a terminal workspace manager for coding agents, and aphid is not
// one of the agent kinds it knows, so it cannot detect it. A pane that reports
// its own state is the supported way in: `herdr pane report-agent`, documented
// at <https://herdr.dev/docs/integrations/>. The report is taken back when the
// session ends, so no row is left for a session that is gone.
//
// Two of the rules Herdr gives integrations are followed here. `idle` is
// reported when the session is ready for input, and `blocked` when a permission
// question is on the screen. No `--seq` is sent: it exists to order reports that
// can arrive out of order, and `exec` is synchronous on one plugin thread, so
// these cannot arrive out of order. Worse, a source that has once used `--seq`
// has every later report without one ignored, which is a silent failure this
// reporter has no use for.
//
// The settings, and what they do:
//
// | Setting | Default | Result |
// | --- | --- | --- |
// | `enabled` | `true` | `false` makes the plugin do nothing |
// | `pane` | the pane herdr started | report about another pane |
// | `bin` | `HERDR_BIN_PATH`, then `herdr` | a different herdr binary |
// | `source` | `custom:aphid` | the authority the report is filed under |
// | `agent` | `aphid` | the label in the sidebar |
// | `message` | `true` | send the line under the state |
// | `tools` | `true` | name the tool while working |
// | `heartbeat` | `60` | seconds between repeats, `0` for none |
// | `notify` | `true` | a herdr notice when a run ends |
// | `verbose` | `false` | log every report and every failure |

// ------------------------------------------------------------------ settings

/// The settings, with their defaults under them.
///
/// A function and not a constant, because a listener is called without the
/// script's scope and a constant is not reachable from one.
fn settings() {
    let c = config();
    #{
        enabled: c.enabled ?? true,
        pane: c.pane ?? "",
        bin: c.bin ?? "",
        source: c.source ?? "custom:aphid",
        agent: c.agent ?? "aphid",
        message: c.message ?? true,
        tools: c.tools ?? true,
        heartbeat: c.heartbeat ?? 60,
        notify: c.notify ?? true,
        verbose: c.verbose ?? false,
    }
}

// ---------------------------------------------------------------- text craft

/// A trimmed copy. Rhai's own `trim` works in place and returns nothing.
fn trimmed(text) {
    let out = text;
    out.trim();
    out
}

/// One line, because the sidebar is one line.
fn one_line(text) {
    let out = text;
    out.replace("\n", " ");
    out.replace("\r", " ");
    out.trim();
    out
}

/// `text` as one shell word.
///
/// A permission summary holds a path, and a path can hold a space or a quote.
/// The command this makes is a string that bash reads, so the value is quoted
/// rather than trusted.
fn quoted(text) {
    let out = text;
    out.replace("'", "'\\''");
    "'" + out + "'"
}

// ------------------------------------------------------------------- herdr

/// What herdr put in the environment of this pane, in one process.
///
/// `exec` is a shell, so the shell reads the values where they are. Nothing is
/// carried through Rhai that the shell can expand itself.
fn environment() {
    let seen = exec("printf '%s|%s|%s' \"$HERDR_ENV\" \"$HERDR_PANE_ID\" \"$HERDR_BIN_PATH\"");
    if seen.status != 0 || seen.stdout == "" {
        return #{ env: "", pane: "", bin: "" };
    }

    let parts = seen.stdout.split('|');
    if parts.len() < 3 {
        return #{ env: "", pane: "", bin: "" };
    }

    #{
        env: trimmed(parts[0]),
        pane: trimmed(parts[1]),
        bin: trimmed(parts[2]),
    }
}

/// Whether the binary is there at all.
///
/// `--version` reads the binary and does not need the server, so this answers
/// one question: is there a herdr to talk to. Without it, a machine that has no
/// herdr would raise inside every listener instead of staying quiet.
fn herdr_answers(bin) {
    try {
        exec(quoted(bin) + " --version").status == 0
    } catch (error) {
        false
    }
}

/// Run one command, and let nothing it does reach the caller.
///
/// A listener that raises is shown to the user and dropped. `code/permission`
/// is worse: a listener that raises there **denies the tool**. A reporter has
/// no business deciding anything, so every call is caught.
fn safely_run(command) {
    try {
        exec(command);
    } catch (error) {
        if settings().verbose {
            log("herdr: " + error);
        }
    }
}

// ---------------------------------------------------------------- reporting

/// Tell herdr what the session is doing, and remember that it was told.
///
/// One `herdr` call, and only when the state or the line under it changed. A
/// report per tool call would be a `herdr` process for every step of the work,
/// with nothing on the screen to show for it, and `/ps` lists what the runtime
/// started. A failed report is not a report: the memory is cleared, so the next
/// change tries again.
fn say(state_name, text, force) {
    let memory = state();
    let line = one_line(text);
    if !force && memory.sent_state == state_name && memory.sent_text == line {
        return;
    }

    let cfg = settings();
    let command = quoted(memory.bin) + " pane report-agent " + quoted(memory.pane);
    command += " --source " + quoted(cfg.source) + " --agent " + quoted(cfg.agent);
    command += " --state " + state_name;
    if cfg.message && line != "" {
        command += " --message " + quoted(line);
    }

    let outcome = exec(command);
    if outcome.status == 0 {
        memory.sent_state = state_name;
        memory.sent_text = line;
    } else {
        memory.sent_state = "";
        memory.sent_text = "";
    }
    state(memory);

    if cfg.verbose {
        let verdict = if outcome.status == 0 { "ok" } else { "failed" };
        log("herdr: " + state_name + " " + verdict + " (" + line + ")");
    }
}

/// The same as `say`, with a failure kept away from the caller.
fn safely(state_name, text, force) {
    try {
        say(state_name, text, force);
    } catch (error) {
        if settings().verbose {
            log("herdr: " + error);
        }
    }
}

/// What to write under `idle` when a run stops.
fn ending(outcome) {
    let error = outcome.error ?? "";
    if error != "" {
        return "error: " + one_line(error);
    }
    if outcome.stop != "stop" {
        return "stopped: " + outcome.stop;
    }

    let turns = outcome.turns;
    if turns == 1 {
        return "done, 1 turn";
    }
    return "done, " + turns + " turns";
}

/// A notice from herdr for a run that ended while you were somewhere else.
fn ring(outcome) {
    let memory = state();
    let cfg = settings();
    let command = quoted(memory.bin) + " notification show " + quoted(cfg.agent);
    command += " --body " + quoted(ending(outcome)) + " --sound done";
    safely_run(command);
}

/// Take the report back, so the sidebar keeps no row for a session that has
/// ended.
fn release() {
    let memory = state();
    let cfg = settings();
    if memory.pane != "" {
        let command = quoted(memory.bin) + " pane release-agent " + quoted(memory.pane);
        command += " --source " + quoted(cfg.source) + " --agent " + quoted(cfg.agent);
        safely_run(command);
    }

    memory.sent_state = "";
    memory.sent_text = "";
    state(memory);
}

// --------------------------------------------------------------------- load

fn apply(ctx) {
    let cfg = settings();
    if !cfg.enabled {
        return;
    }

    let where = environment();
    let named = cfg.pane != "";
    if !named && where.env != "1" {
        // A plain terminal. Silence is the whole point of this exit.
        if cfg.verbose {
            log("herdr: aphid is not in a herdr pane, so nothing is reported");
        }
        return;
    }

    let pane = if named { cfg.pane } else { where.pane };
    if pane == "" {
        return;
    }

    let bin = if cfg.bin != "" { cfg.bin } else if where.bin != "" { where.bin } else { "herdr" };
    if !herdr_answers(bin) {
        log("herdr: " + bin + " did not answer, so nothing is reported");
        return;
    }

    // The pane and the binary are resolved once, here. A listener reads them
    // from the plugin's memory rather than running a shell to find them again.
    state(#{
        pane: pane,
        bin: bin,
        sent_state: "",
        sent_text: "",
        ticks: 0,
    });

    on("code/session-start", |session| {
        safely("idle", "", false);
    });

    on("agent/run-start", |cx| {
        safely("working", "", false);
    });

    // A tool that is running means the wait is over, whether the run is
    // starting or a permission question was just answered.
    on("agent/tool-progress", |id, tool, chunk| {
        if settings().tools {
            safely("working", tool, false);
        }
    });

    on("agent/tool-result", |result| {
        let text = if settings().tools { result.name } else { "" };
        safely("working", text, false);
    });

    on("code/permission", |ask| {
        // The summary is already the thing that was asked about: a command, or
        // `write <path>`. It is also what the dialog on the screen says.
        safely("blocked", ask.summary, false);
    });

    on("agent/run-end", |cx, outcome| {
        safely("idle", ending(outcome), false);
        if settings().notify {
            ring(outcome);
        }
    });

    // The terminal interface ticks four times a second. A report is repeated
    // every `heartbeat` seconds, because a herdr server that restarts forgets
    // what it was told, and nothing else would say it again.
    on("code/tick", || {
        let cfg = settings();
        let memory = state();
        if cfg.heartbeat <= 0 || memory.sent_state == "" {
            return;
        }

        let ticks = memory.ticks + 1;
        if ticks * 250 < cfg.heartbeat * 1000 {
            memory.ticks = ticks;
            state(memory);
            return;
        }

        memory.ticks = 0;
        state(memory);
        safely(memory.sent_state, memory.sent_text, true);
    });

    on("code/session-end", |session| {
        release();
    });

    // Belt and braces: `/reload` unloads the plugin without a session ending,
    // and a row for a session that is gone would stay in the sidebar.
    effect(|| {}, || { release(); });
}
