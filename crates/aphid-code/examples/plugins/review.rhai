//! Adds /review, which asks the model to look at the working tree.

const inject = ["commands"];

fn apply(ctx) {
    command(#{
        name: "review",
        description: "Ask for a review of the uncommitted changes.",
        run: |args| {
            let target = if args == "" { "" } else { " -- " + args };
            let diff = exec("git diff" + target).stdout;
            // Rhai trims in place and gives back nothing.
            diff.trim();

            if diff == "" {
                return notice("nothing to review");
            }

            prompt("Review this diff. Report only real problems.\n\n```diff\n" + diff + "\n```");
            notice("reviewing " + diff.split('\n').len() + " lines of diff")
        }
    });
}
