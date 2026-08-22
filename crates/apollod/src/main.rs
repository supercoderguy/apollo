mod config;
mod ipc;
mod registry;
mod supervisor;

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;
use std::thread;

/// Apollo init system supervisor daemon.
///
/// Not yet run as PID 1 — this milestone is the daemon/CLI split
/// (apollod + apolloctl over a control socket). See README.md for the
/// roadmap toward actually booting a system.
#[derive(Parser, Debug)]
#[command(name = "apollod")]
struct Args {
    /// Directory containing `*.toml` service unit files.
    #[arg(long, default_value = "/etc/apollo/services")]
    config_dir: PathBuf,

    /// Path to the Unix control socket apolloctl connects to.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let socket_path = args
        .socket
        .unwrap_or_else(|| PathBuf::from(apollo_proto::DEFAULT_SOCKET_PATH));

    let configs = config::load_units(&args.config_dir)
        .with_context(|| format!("loading unit files from {}", args.config_dir.display()))?;
    let order = config::resolve_start_order(&configs)?;
    eprintln!("apollod: loaded {} unit(s)", configs.len());

    let (mut sup, events_tx) = supervisor::Supervisor::new();
    sup.load(configs);
    sup.start_all(&order);

    // The IPC server runs on its own thread; the supervisor's event loop
    // stays on the main thread and is the only thing that ever mutates
    // unit state.
    thread::spawn(move || {
        if let Err(e) = ipc::serve(&socket_path, events_tx) {
            eprintln!("apollod: control socket failed: {e:#}");
        }
    });

    sup.run();
    Ok(())
}
