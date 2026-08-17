//! A minimal interactive side panel. Run /panel to open or close it.
//!
//! A surface is a small app of its own: `init` says what its model starts as,
//! `update` takes the model and a message and returns the new model, and `view`
//! turns the model into what you see. None of the three reaches around the
//! others.

register_command(#{
    name: "panel",
    description: "Open or close the example panel.",
    run: |args| {
        let s = surface_state("panel");
        s.open = !s.open;
        surface_state("panel", s);
        notice("example panel " + if s.open { "on" } else { "off" })
    }
});

register_surface(#{
    name: "panel",
    placement: #{ kind: "side", side: "right" },

    // Whatever was open last session stays open: a persisted value wins over
    // a default, so this says what a key means and not what it is.
    init: || #{ open: false, selected: 0, items: ["first", "second", "third"] },

    update: |s, msg| {
        if msg.kind == "key" {
            if msg.code == "up" {
                s.selected = (s.selected + s.items.len() - 1) % s.items.len();
            }
            if msg.code == "down" {
                s.selected = (s.selected + 1) % s.items.len();
            }
            if msg.code == "esc" {
                // The model did not change, so this returns the action alone.
                return "release_focus";
            }
        }
        s
    },

    view: |s| {
        if !s.open { return (); }
        #{
            type: "cols",
            children: [
                #{ type: "text", text: "Use up and down, Esc to return." },
                #{ type: "list", id: "items", items: s.items, selected: s.selected }
            ]
        }
    }
});
