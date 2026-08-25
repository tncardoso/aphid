//! Tells the model which branch it is on, once per run.

fn apply(ctx) {
    on("agent/run-start", |cx| {
        let branch = exec("git rev-parse --abbrev-ref HEAD");
        if branch.status == 0 {
            // Rhai trims the string in place and gives back nothing, so this
            // has to be two steps.
            let name = branch.stdout;
            name.trim();
            cx.note("The current git branch is " + name + ".");
        }
    });
}
