//! Tells the model which branch it is on, once per run.

fn on_run_start(cx) {
    let branch = exec("git rev-parse --abbrev-ref HEAD");
    if branch.status == 0 {
        cx.note("The current git branch is " + branch.stdout.trim() + ".");
    }
}
