//! `aphid colony`: the hub agents talk in.
//!
//! Two processes and four verbs, the shape [`crate::alate`] already has.
//! `serve` is the hub itself, in the foreground; `attach` is a terminal on a
//! running one; `list` says which hubs exist; `keys` prints the two public keys
//! an agent needs to be told about.
//!
//! The hub and the terminal are **separate processes**, and that is the point
//! of a hub. It has to outlive any one terminal, and several terminals have to
//! be able to watch it at once — neither of which is possible when one process
//! holds both.
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
    /// Run the colony in this terminal
    Serve(Args),
    /// Open a terminal on a running colony
    Attach(AttachArgs),
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

#[derive(Debug, clap::Args)]
pub struct AttachArgs {
    #[command(flatten)]
    pub colony: Args,
    /// The colony to attach to. The one this home names when absent.
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,
}

pub async fn run(command: Command) -> ExitCode {
    match command {
        Command::Serve(args) => serve(&args.name).await,
        Command::Attach(args) => attach(&args.colony.name, args.relay.as_deref()).await,
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

/// The hub itself. It runs until it is stopped, and nothing it serves ends it.
async fn serve(name: &str) -> ExitCode {
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

    println!("colony {name} is listening on ws://{}", relay.address());
    println!("anything that can reach it may publish and read");
    println!("attach a terminal with `aphid colony attach --name {name}`");
    relay.joined().await;
    ExitCode::SUCCESS
}

/// A terminal on a hub that is already running.
///
/// It binds nothing and hosts nothing. Several of these can watch one colony at
/// once, and closing one leaves the colony and the others alone.
async fn attach(name: &str, relay: Option<&str>) -> ExitCode {
    let (home, config) = match open(name) {
        Ok(both) => both,
        Err(error) => return fail(&error),
    };

    // `--relay` is what points a terminal at a colony on another machine. With
    // no flag the home says where its own colony listens, which is how
    // `aphid alate attach` finds its socket.
    let url = match relay
        .map(str::to_owned)
        .map(Ok)
        .unwrap_or_else(|| config.url())
    {
        Ok(url) => url,
        Err(error) => return fail(&error),
    };

    let human = match identity::open(&home.human_key()) {
        Ok(keys) => keys,
        Err(error) => return fail(&error.to_string()),
    };

    match aphid_colony::tui::run(aphid_colony::tui::Options {
        url,
        keys: human,
        name: config.name.clone(),
    })
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        // A colony that is not there is the ordinary mistake of starting two
        // processes in one order, so the refusal names the other one.
        Err(error) if error.kind() == std::io::ErrorKind::NotConnected => fail(&format!(
            "{error}.\n       Start it with `aphid colony serve --name {name}`"
        )),
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
        println!("no colonies yet. `aphid colony serve` makes one called {DEFAULT_NAME}");
        return ExitCode::SUCCESS;
    }

    for name in names {
        // The address to dial, and not the one it binds: this line is here to
        // be read and copied into a `--relay`.
        let url = Config::load(&root.join(&name).join("colony.json"))
            .map_err(|error| error.to_string())
            .and_then(|config| config.url())
            .unwrap_or_else(|_| "?".to_owned());
        println!("{name:<20} {url}");
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
    match config.url() {
        Ok(url) => println!("at     {url}"),
        Err(error) => return fail(&error),
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("aphid: {message}");
    ExitCode::FAILURE
}
