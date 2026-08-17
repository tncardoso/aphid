//! Interactive surfaces written in Rhai, read through the host.

use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::exec;
use aphid_plugin::{
    Capabilities, Placement, PluginHost, Side, Silent, SurfaceAction, SurfaceEvent, SurfaceRender,
    Widget, explicit,
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-surfaces-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        std::fs::write(root.join(".aphid").join("plugins").join("kit.rhai"), source)
            .expect("write the plugin");
        Self { root }
    }

    fn host(&self) -> Arc<PluginHost> {
        let file =
            explicit(&self.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
        let (host, diagnostics) = PluginHost::load(
            &[file],
            &Capabilities::full(&self.root),
            Arc::new(Silent),
            &Arc::new(exec::Registry::new()),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        Arc::new(host)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const PANEL: &str = r#"
register_surface(#{
    name: "panel",
    placement: #{ kind: "side", side: "right" },
    view: |state| {
        #{ type: "text", text: "hello" }
    }
});
"#;

#[test]
fn a_script_surface_is_listed_and_renders() {
    let fixture = Fixture::new(PANEL);
    let host = fixture.host();

    let listed = host.surfaces();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].plugin, "kit");
    assert_eq!(listed[0].name, "panel");
    assert_eq!(listed[0].placement, Placement::Side(Side::Right));
    assert!(!listed[0].interactive);

    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::Text { text, .. })) if text == "hello"
    ));
}

#[test]
fn a_view_returning_unit_closes_the_surface() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "left" },
            view: |state| { () }
        });
        "#,
    );
    let host = fixture.host();

    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Closed)
    ));
}

#[test]
fn an_update_receives_the_message_and_returns_actions() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            view: |state| { #{ type: "text", text: "x" } },
            update: |state, msg| {
                if msg.kind == "key" && msg.code == "enter" {
                    return ["consume", notice("pressed"), "release_focus"];
                }
                ()
            }
        });
        "#,
    );
    let host = fixture.host();
    assert!(host.surfaces()[0].interactive);

    let actions = host
        .surface_event(
            "kit",
            "panel",
            SurfaceEvent::Key {
                code: "enter".to_owned(),
                modifiers: vec![],
            },
        )
        .expect("the surface exists");

    assert_eq!(
        actions,
        vec![
            SurfaceAction::Consume,
            SurfaceAction::Notice("pressed".to_owned()),
            SurfaceAction::ReleaseFocus,
        ]
    );
}

#[test]
fn a_surface_without_on_event_yields_no_actions() {
    let fixture = Fixture::new(PANEL);
    let host = fixture.host();

    assert_eq!(
        host.surface_event(
            "kit",
            "panel",
            SurfaceEvent::Paste {
                text: "hello".to_owned()
            }
        ),
        Some(Vec::new())
    );
}

#[test]
fn a_malformed_surface_is_refused_at_load_time() {
    let fixture = Fixture::new(r#"register_surface(#{ name: "panel" });"#);
    let file =
        explicit(&fixture.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
    let (_host, diagnostics) = PluginHost::load(
        &[file],
        &Capabilities::full(&fixture.root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0].message.contains("placement"),
        "{diagnostics:?}"
    );
}

#[test]
fn a_reserved_placement_is_refused_with_a_clear_message() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "overlay" },
            view: |state| { #{ type: "text", text: "x" } }
        });
        "#,
    );
    let file =
        explicit(&fixture.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
    let (_host, diagnostics) = PluginHost::load(
        &[file],
        &Capabilities::full(&fixture.root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0].message.contains("reserved"),
        "{diagnostics:?}"
    );
}

#[test]
fn state_map_is_memory_only_and_bumps_the_version() {
    let fixture = Fixture::new(
        r#"
        register_command(#{
            name: "remember",
            description: "remember without saving",
            run: |args| { state(#{ open: true }); notice("remembered") }
        });
        register_command(#{
            name: "persist",
            description: "remember and save",
            run: |args| { save_state(#{ open: true }); notice("saved") }
        });
        "#,
    );
    let host = fixture.host();
    let before = host.state_version("kit").expect("loaded");

    let _ = host.run_command("remember", "");
    let after_memory = host.state_version("kit").expect("loaded");
    assert!(after_memory > before, "a memory write bumps the version");

    host.flush();
    let state_file = fixture
        .root
        .join(".aphid")
        .join("plugins")
        .join("state")
        .join("kit.json");
    assert!(!state_file.exists(), "a memory write does not persist");

    let _ = host.run_command("persist", "");
    host.flush();
    assert!(state_file.exists(), "save_state persists");
    assert!(
        host.state_version("kit").expect("loaded") > after_memory,
        "save_state also bumps the version"
    );
}

// ---------------------------------------------------------------------------
// The model, the update and the view.
// ---------------------------------------------------------------------------

#[test]
fn init_fills_in_what_was_never_set() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ open: false, chosen: 0 },
            view: |s| #{ type: "text", text: "open " + s.open + " chosen " + s.chosen }
        });
        "#,
    );
    let host = fixture.host();

    // No `if "open" in s` anywhere: the defaults are already there.
    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::Text { text, .. }))
            if text == "open false chosen 0"
    ));
}

#[test]
fn what_was_already_there_wins_over_a_default() {
    let fixture = Fixture::new(
        r#"
        // Set before the surface is registered, as a restored state would be.
        state(#{ surfaces: #{ panel: #{ open: true } } });
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ open: false, chosen: 7 },
            view: |s| #{ type: "text", text: "open " + s.open + " chosen " + s.chosen }
        });
        "#,
    );
    let host = fixture.host();

    assert!(
        matches!(
            host.render_surface("kit", "panel"),
            Some(SurfaceRender::Widget(Widget::Text { ref text, .. }))
                if text == "open true chosen 7"
        ),
        "{:?}",
        host.render_surface("kit", "panel")
    );
}

#[test]
fn what_update_returns_is_what_view_sees() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ chosen: 0 },
            update: |s, msg| {
                if msg.kind == "key" && msg.code == "down" { s.chosen += 1; }
                s
            },
            view: |s| #{ type: "text", text: "chosen " + s.chosen }
        });
        "#,
    );
    let host = fixture.host();

    for _ in 0..3 {
        host.surface_event(
            "kit",
            "panel",
            SurfaceEvent::Key {
                code: "down".to_owned(),
                modifiers: vec![],
            },
        )
        .expect("the surface exists");
    }

    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::Text { text, .. })) if text == "chosen 3"
    ));
}

#[test]
fn an_update_can_change_the_model_and_ask_for_something() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ chosen: 0 },
            update: |s, msg| {
                s.chosen = 42;
                #{ state: s, cmd: [notice("moved"), send("again")] }
            },
            view: |s| #{ type: "text", text: "chosen " + s.chosen }
        });
        "#,
    );
    let host = fixture.host();

    let actions = host
        .surface_event("kit", "panel", SurfaceEvent::Paste { text: "x".into() })
        .expect("the surface exists");

    assert_eq!(
        actions,
        vec![
            SurfaceAction::Notice("moved".to_owned()),
            SurfaceAction::Send {
                name: "again".to_owned(),
                payload: aphid_core::Json::Null,
            },
        ]
    );
    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::Text { text, .. })) if text == "chosen 42"
    ));
}

#[test]
fn a_surface_hears_its_own_message() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ heard: "" },
            update: |s, msg| {
                if msg.kind == "msg" { s.heard = msg.name + ":" + msg.payload.n; }
                s
            },
            view: |s| #{ type: "text", text: s.heard }
        });
        "#,
    );
    let host = fixture.host();

    host.surface_event(
        "kit",
        "panel",
        SurfaceEvent::Msg {
            name: "again".to_owned(),
            payload: serde_json::json!({ "n": 3 }),
        },
    )
    .expect("the surface exists");

    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::Text { text, .. })) if text == "again:3"
    ));
}

#[test]
fn a_surface_written_for_the_old_shape_says_what_to_rename() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            render: |s| #{ type: "text", text: "x" },
            on_event: |event| "consume"
        });
        "#,
    );
    let file =
        explicit(&fixture.root.join(".aphid").join("plugins").join("kit.rhai")).expect("readable");
    let (_host, diagnostics) = PluginHost::load(
        &[file],
        &Capabilities::full(&fixture.root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );

    assert_eq!(diagnostics.len(), 1);
    let said = &diagnostics[0].message;
    assert!(said.contains("`view`"), "{said}");
    assert!(said.contains("update(state, msg)"), "{said}");
}

#[test]
fn a_tool_and_a_panel_of_one_plugin_share_the_panels_model() {
    let fixture = Fixture::new(
        r#"
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            init: || #{ items: [] },
            view: |s| #{ type: "list", items: s.items, selected: 0 }
        });
        register_tool(#{
            name: "add",
            description: "Add an item.",
            parameters: #{ type: "object", properties: #{}, required: [] },
            execute: |args| {
                let s = surface_state("panel");
                s.items.push("added");
                surface_state("panel", s);
                "ok"
            }
        });
        "#,
    );
    let host = fixture.host();

    // What the todo plugin does: a tool writes the very list the panel draws.
    let plugin = &host.plugins()[0];
    let mut state = plugin.surface_state("panel");
    state.insert(
        "items".into(),
        rhai::Dynamic::from_array(vec!["added".into()]),
    );
    plugin.set_surface_state("panel", state);

    assert!(matches!(
        host.render_surface("kit", "panel"),
        Some(SurfaceRender::Widget(Widget::List { items, .. })) if items == vec!["added".to_owned()]
    ));
}
