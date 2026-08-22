mod config;
mod ipc;
mod mounts;
mod reaper;
mod registry;
mod supervisor;

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;
use std::thread;

/// Apollo init system supervisor daemon.
///
/// Runnable as PID 1, or as an ordinary process for development (see
/// README.md — override `--config-dir`/`--socket` since the defaults live
/// under `/etc/apollo` and `/run`, which need root). See README.md for the
/// roadmap toward the rest of what a real boot needs (mounts, getty,
/// shutdown handling, ...).
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
    // Must happen before any other thread is spawned (including, below,
    // before any unit process is started): signal masks are inherited by
    // new threads at creation time, and this closes the window where a
    // fast-exiting child's SIGCHLD could be delivered under the default
    // (ignored) disposition instead of being left pending for the reaper.
    reaper::block_sigchld().context("blocking SIGCHLD")?;

    let args = Args::parse();
    let socket_path = args
        .socket
        .unwrap_or_else(|| PathBuf::from(apollo_proto::DEFAULT_SOCKET_PATH));

    // Only meaningful on an actual boot; a no-op check when running as an
    // ordinary process for development (see README.md).
    if mounts::is_pid1() {
        mounts::run();
    } else {
        eprintln!(
            "apollod: not PID 1 (pid {}), skipping early filesystem setup",
            std::process::id()
        );
    }

    let configs = config::load_units(&args.config_dir)
        .with_context(|| format!("loading unit files from {}", args.config_dir.display()))?;
    let order = config::resolve_start_order(&configs)?;
    eprintln!("apollod: loaded {} unit(s)", configs.len());

    let (mut sup, events_tx) = supervisor::Supervisor::new();
    sup.load(configs);

    // As PID 1, apollod is responsible for reaping every child that ends
    // up parented to it, not just the units it starts itself — start this
    // before start_all() spawns anything.
    reaper::spawn(events_tx.clone());

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
