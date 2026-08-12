//! `aphid colony`: the hub agents talk in.
//!
//! Four verbs. `run` is the relay with a terminal attached to it, which is what
//! a person wants; `serve` is the relay alone, for a machine that is only
//! carrying messages; `list` says which hubs exist; `keys` prints the two
//! public keys an agent needs to be told about.
//!
//! Detaching it from a terminal is the shell's job — `nohup`, a service manager
//! — and not the hub's, exactly as with an alate.

use std::process::ExitCode;

use aphid_colony::config::Config;
use aphid_colony::home::{DEFAULT_NAME, Home};
use aphid_colony::identity;
use aphid_colony::relay::{Options, Relay};
use aphid_colony::store::Store;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run the colony and open a terminal on it
    Run(Args),
    /// Run the colony with no terminal
    Serve(Args),
    /// Show the colonies on this machine
    List,
    /// Print this colony's public keys
    Keys(Args),
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Which colony, by name
    #[arg(long, short, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
}

pub async fn run(command: Command) -> ExitCode {
    match command {
        Command::Run(args) => start(&args.name, true).await,
        Command::Serve(args) => start(&args.name, false).await,
        Command::List => list(),
        Command::Keys(args) => keys(&args.name),
    }
}

/// Open the home, the configuration and the two keys.
fn open(name: &str) -> Result<(Home, Config), String> {
    let home = Home::open(name).map_err(|error| error.to_string())?;
    let config = Config::load(&home.config_file()).map_err(|error| error.to_string())?;
    Ok((home, config))
}

async fn start(name: &str, terminal: bool) -> ExitCode {
    let (home, config) = match open(name) {
        Ok(both) => both,
        Err(error) => return fail(&error),
    };

    let address = match config.address() {
        Ok(address) => address,
        Err(error) => return fail(&error),
    };
    let store = match Store::open(&home.database()) {
        Ok(store) => store,
        Err(error) => return fail(&error.to_string()),
    };
    let keys = match identity::open(&home.relay_key()) {
        Ok(keys) => keys,
        Err(error) => return fail(&error.to_string()),
    };

    let mut relay = match Relay::bind(Options {
        address,
        store,
        keys,
        channels: config.channels.clone(),
        history: config.history,
    })
    .await
    {
        Ok(relay) => relay,
        Err(error) => return fail(&error.to_string()),
    };

    let url = format!("ws://{}", relay.address());

    if !terminal {
        println!("colony {name} is listening on {url}");
        println!("anything that can reach it may publish and read");
        relay.joined().await;
        return ExitCode::SUCCESS;
    }

    let human = match identity::open(&home.human_key()) {
        Ok(keys) => keys,
        Err(error) => return fail(&error.to_string()),
    };

    // The relay is hosted in this process, so the terminal ending ends it and
    // the relay ending has to end the terminal. `tui::run` owns both.
    match aphid_colony::tui::run(aphid_colony::tui::Options {
        url,
        keys: human,
        name: config.name.clone(),
        relay: Some(relay),
    })
    .await
    {
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
        println!("no colonies yet. `aphid colony run` makes one called {DEFAULT_NAME}");
        return ExitCode::SUCCESS;
    }

    for name in names {
        let listen = Config::load(&root.join(&name).join("colony.json"))
            .map(|config| config.listen)
            .unwrap_or_else(|_| "?".to_owned());
        println!("{name:<20} ws://{listen}");
    }
    ExitCode::SUCCESS
}

/// The public keys an agent has to be told about.
///
/// Reading a key file makes it if it is not there, which is what makes this the
/// command to run before configuring the first alate.
fn keys(name: &str) -> ExitCode {
    let (home, config) = match open(name) {
        Ok(both) => both,
        Err(error) => return fail(&error),
    };

    for (what, path) in [("relay", home.relay_key()), ("you", home.human_key())] {
        match identity::open(&path) {
            Ok(keys) => println!("{what:<6} {}", keys.public_key().to_hex()),
            Err(error) => return fail(&error.to_string()),
        }
    }
    println!("listen ws://{}", config.listen);
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("aphid: {message}");
    ExitCode::FAILURE
}
