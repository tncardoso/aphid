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
    /// Show the alates on this machine
    List,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Which alate, by name
    #[arg(long, short, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
}

pub async fn run(command: Command) -> ExitCode {
    match command {
        Command::Run(args) => start(&args.name).await,
        Command::Attach(args) => attach(&args.name).await,
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
