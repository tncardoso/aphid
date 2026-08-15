//! A session todo list. Use /todo <prompt> to run a prompt in todo mode,
//! and /todo-status to open or close the right-hand todo panel.
//! The list lives in memory for this session only.

/// The prompt prefix for todo mode.
///
/// A function and not a constant, because a command body is called without the
/// script's scope and `global::` reaches nothing from there.
fn todo_mode() {
    "You are in TODO MODE. Use the `todo_list`, `todo_add`, \
`todo_done` and `todo_clear` tools to keep the todo list current: list it \
before you start, add tasks as you identify them, mark each done as you finish \
it, and clear it when you start over. Keep your answer short."
}

/// The whole state, with defaults filled in.
fn store() {
    let s = state();
    #{
        items: if "items" in s { s.items } else { [] },
        next:  if "next"  in s { s.next }  else { 1 },
        open:  if "open"  in s { s.open }  else { false }
    }
}

/// Replace the in-memory state. Nothing here is written to disk.
fn remember(s) {
    state(s);
}

/// The list as markdown, pending first.
fn format_todos(items) {
    let pending = [];
    let done = [];
    for item in items {
        if item.done { done.push(item); } else { pending.push(item); }
    }

    let out = "## Pending\n";
    if pending.is_empty() {
        out += "_(none)_\n";
    } else {
        for item in pending {
            out += "- [ ] " + item.id + ". " + item.task + "\n";
        }
    }

    out += "\n## Done\n";
    if done.is_empty() {
        out += "_(none)_\n";
    } else {
        for item in done {
            out += "- [x] " + item.id + ". " + item.task + "\n";
        }
    }

    out
}

/// A trimmed copy. Rhai's own `trim` works in place and returns nothing.
fn trimmed(text) {
    let out = text;
    out.trim();
    out
}

register_surface(#{
    name: "todo",
    placement: #{ kind: "side", side: "right" },
    render: |s| {
        let open = if "open" in s { s.open } else { false };
        if !open { return (); }
        let items = if "items" in s { s.items } else { [] };
        #{ type: "text", text: format_todos(items) }
    }
});

register_command(#{
    name: "todo",
    description: "Run a prompt in todo mode (uses the todo tools).",
    run: |args| {
        let text = trimmed(args);
        if text == "" {
            return notice("usage: /todo <prompt>");
        }
        prompt(todo_mode() + "\n\n" + text);
        notice("todo mode: " + text)
    }
});

register_command(#{
    name: "todo-status",
    description: "Open or close the todo panel. /todo-status on|off.",
    run: |args| {
        let arg = trimmed(args);
        if arg == "on" || arg == "off" || arg == "" {
            let s = store();
            if arg == "on" {
                s.open = true;
            } else if arg == "off" {
                s.open = false;
            } else {
                s.open = !s.open;
            }
            remember(s);
            notice("todo panel " + if s.open { "on" } else { "off" })
        } else {
            notice("usage: /todo-status on|off")
        }
    }
});

register_tool(#{
    name: "todo_list",
    description: "List the current todos as markdown, pending first.",
    parameters: #{ type: "object", properties: #{}, required: [] },
    execute: |args| { format_todos(store().items) }
});

register_tool(#{
    name: "todo_add",
    description: "Add a task to the todo list. Returns the updated list.",
    parameters: #{
        type: "object",
        properties: #{ task: #{ type: "string", description: "The task to add." } },
        required: ["task"]
    },
    execute: |args| {
        let task = args.task;
        if task == "" {
            return #{ content: "task is empty", is_error: true };
        }

        let s = store();
        let id = s.next;
        s.items.push(#{ id: id, task: task, done: false });
        s.next = id + 1;
        remember(s);

        "Added task " + id + ". " + task + ".\n\n" + format_todos(s.items)
    }
});

register_tool(#{
    name: "todo_clear",
    description: "Remove every pending and done task from the todo list. Returns the updated list.",
    parameters: #{ type: "object", properties: #{}, required: [] },
    execute: |args| {
        let s = store();
        s.items = [];
        s.next = 1;
        remember(s);
        "Cleared the todo list.\n\n" + format_todos(s.items)
    }
});

register_tool(#{
    name: "todo_done",
    description: "Mark a task as done by id. Returns the updated list.",
    parameters: #{
        type: "object",
        properties: #{ id: #{ type: "integer", description: "The task id to mark done." } },
        required: ["id"]
    },
    execute: |args| {
        let target = args.id;
        let s = store();
        let found = false;
        let i = 0;
        while i < s.items.len() {
            if s.items[i].id == target {
                s.items[i].done = true;
                found = true;
            }
            i += 1;
        }

        if !found {
            return #{ content: "no task with id " + target, is_error: true };
        }

        remember(s);
        "Marked task " + target + " done.\n\n" + format_todos(s.items)
    }
});
