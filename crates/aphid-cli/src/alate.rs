//! `aphid alate`: the resident agent.
//!
//! Two processes and three verbs. `run` is the alate itself, in the foreground;
//! `attach` is a terminal on a running one; `list` says which exist and which
//! are awake. Detaching it from a terminal is the shell's job — `nohup`, a
//! service manager — and not the agent's.

use std::process::ExitCode;

use aphid_alate::config::Config;
use aphid_alate::daemon::{self, Options};
use aphid_alate::gateway::is_listening;
use aphid_alate::home::{DEFAULT_NAME, Home};
use aphid_alate::tui;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run the alate in this terminal
    Run(Args),
    /// Open a terminal on a running alate
    Attach(Args),
    /// Open a window on a running alate
    Gui(GuiArgs),
    /// Show the alates on this machine
    List,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Which alate, by name
    #[arg(long, short, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
}

/// `aphid alate gui`, with or without a verb.
///
/// Without one it opens the window, which takes the main thread. With one it is
/// a remote control for the window that is already open — the form to bind to a
/// key in a window manager.
#[derive(Debug, clap::Args)]
pub struct GuiArgs {
    #[command(subcommand)]
    pub command: Option<GuiCommand>,
    /// Which alate, by name. Without one, the alate the window last watched.
    ///
    /// Global, so it can be given before or after the verb.
    #[arg(long, short, value_name = "NAME", global = true)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum GuiCommand {
    /// Expand the window, or collapse it
    Toggle,
    /// Bring the window forward
    Show,
    /// Swap between the console and the companion column
    Mode,
    /// Close the window. The alate keeps running
    Quit,
}

pub async fn run(command: Command) -> ExitCode {
    match command {
        Command::Run(args) => start(&args.name).await,
        Command::Attach(args) => attach(&args.name).await,
        Command::Gui(args) => gui(args).await,
        Command::List => list(),
    }
}

async fn start(name: &str) -> ExitCode {
    let home = match Home::open(name) {
        Ok(home) => home,
        Err(error) => return fail(&error.to_string()),
    };
    let config = match Config::load(&home.config_file()) {
        Ok(config) => config,
        Err(error) => return fail(&error.to_string()),
    };

    // The daemon says when it is awake, because it is the only one that knows.
    match daemon::run(Options {
        home,
        config,
        model: None,
        stream_fn: None,
        sessions_dir: aphid_code::session::sessions_dir(),
    })
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

async fn attach(name: &str) -> ExitCode {
    let home = match Home::open(name) {
        Ok(home) => home,
        Err(error) => return fail(&error.to_string()),
    };
    match tui::run(&home).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error.to_string()),
    }
}

/// Open the window, or bring the one that is open forward.
///
/// This is not `async` and does not run under the alate's runtime: GPUI owns
/// the process main thread and runs an event loop of its own on it, so `main`
/// sends the verbless form here before Tokio starts, exactly as it already does
/// for `aphid gui`. The window builds a runtime of its own for the connection.
pub fn window(args: GuiArgs) -> ExitCode {
    #[cfg(feature = "gui")]
    {
        match aphid_alate::gui::run(args.name) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        }
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = args;
        fail(NO_GUI)
    }
}

/// Say one thing to the window that is open.
///
/// The form without a verb never reaches here: it takes the main thread, so
/// `main` sends it before the runtime starts, the way `aphid gui` already goes.
async fn gui(args: GuiArgs) -> ExitCode {
    #[cfg(feature = "gui")]
    {
        use aphid_alate::gui::control::Command as Order;

        let Some(command) = args.command else {
            return fail("`aphid alate gui` opens the window and takes this thread");
        };
        let order = match command {
            GuiCommand::Toggle => Order::Toggle,
            GuiCommand::Show => Order::Show,
            GuiCommand::Mode => Order::Mode,
            GuiCommand::Quit => Order::Quit,
        };
        // A name given with a verb points the open window at another alate
        // before the verb runs, which is what makes `gui show --name notes`
        // one gesture rather than two.
        if let Some(name) = args.name
            && let Err(error) = aphid_alate::gui::control_one(Order::Instance { name }).await
        {
            return fail(&error);
        }
        match aphid_alate::gui::control_one(order).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        }
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = args;
        fail(NO_GUI)
    }
}

/// What a build without a window says when asked for one.
#[cfg(not(feature = "gui"))]
const NO_GUI: &str = "this build has no graphical interface. \
     Reinstall with `cargo install aphid-ai --features gui`";

fn list() -> ExitCode {
    let root = match Home::root_dir() {
        Ok(root) => root,
        Err(error) => return fail(&error.to_string()),
    };
    let names = match Home::list_in(&root) {
        Ok(names) => names,
        Err(error) => return fail(&error.to_string()),
    };

    if names.is_empty() {
        println!("no alates yet. `aphid alate run` makes one called {DEFAULT_NAME}");
        return ExitCode::SUCCESS;
    }

    for name in names {
        let socket = root.join(&name).join("gateway.sock");
        let state = if is_listening(&socket) {
            "awake"
        } else {
            "asleep"
        };
        println!("{name:<20} {state}");
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("aphid: {message}");
    ExitCode::FAILURE
}
