//! A minimal interactive side panel. Run /panel to open or close it.

fn store() {
    let s = state();
    #{
        open: if "open" in s { s.open } else { false },
        selected: if "selected" in s { s.selected } else { 0 },
        items: ["first", "second", "third"]
    }
}

fn remember(s) {
    state(s);
}

register_command(#{
    name: "panel",
    description: "Open or close the example panel.",
    run: |args| {
        let s = store();
        s.open = !s.open;
        remember(s);
        notice("example panel " + if s.open { "on" } else { "off" })
    }
});

register_surface(#{
    name: "panel",
    placement: #{ kind: "side", side: "right" },
    render: |s| {
        let open = if "open" in s { s.open } else { false };
        if !open { return (); }
        let selected = if "selected" in s { s.selected } else { 0 };
        #{
            type: "cols",
            children: [
                #{ type: "text", text: "Use up and down, Esc to return." },
                #{ type: "list", id: "items", items: ["first", "second", "third"], selected: selected }
            ]
        }
    },
    on_event: |event| {
        if event.kind == "key" {
            if event.code == "up" {
                let s = store();
                s.selected = (s.selected + s.items.len() - 1) % s.items.len();
                remember(s);
                return "consume";
            }
            if event.code == "down" {
                let s = store();
                s.selected = (s.selected + 1) % s.items.len();
                remember(s);
                return "consume";
            }
            if event.code == "esc" {
                return "release_focus";
            }
        }
    }
});
