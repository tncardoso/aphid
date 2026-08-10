//! Adds /review, which asks the model to look at the working tree.

register_command(#{
    name: "review",
    description: "Ask for a review of the uncommitted changes.",
    run: |args| {
        let target = if args == "" { "" } else { " -- " + args };
        let diff = exec("git diff" + target).stdout;

        if diff.trim() == "" {
            return notice("nothing to review");
        }

        prompt("Review this diff. Report only real problems.\n\n```diff\n" + diff + "\n```");
        notice("reviewing " + diff.split('\n').len() + " lines of diff")
    }
});
