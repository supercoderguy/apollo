mod config;
mod ipc;
mod mounts;
mod reaper;
mod registry;
mod supervisor;

use anyhow::Context;
use clap::Parser;
use std::panic;
use std::path::PathBuf;
use std::thread;

/// Apollo init system supervisor daemon.
///
/// Runnable as PID 1, or as an ordinary process for development (see
/// README.md — override `--config-dir`/`--socket` since the defaults live
/// under `/etc/apollo` and `/run`, which need root). Note for dev/test
/// use: SIGTERM and SIGINT (so also Ctrl-C) now trigger a graceful
/// shutdown sequence — stop every unit, then exit — rather than an
/// immediate kill, matching real PID 1 behavior; see `reaper.rs` and
/// `supervisor.rs::shutdown`. See README.md for the roadmap toward the
/// rest of what a real boot needs.
#[derive(Parser, Debug)]
#[command(name = "apollod")]
struct Args {
    /// Directory containing `*.toml` service unit files.
    #[arg(long, default_value = "/etc/apollo/services")]
    config_dir: PathBuf,

    /// Path to the Unix control socket apolloctl connects to.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Directory each unit's stdout/stderr is captured into, one
    /// `<name>.log` file per unit, instead of inheriting apollod's own
    /// console (see supervisor.rs::spawn_unit).
    #[arg(long, default_value = "/var/log/apollo")]
    log_dir: PathBuf,
}

fn main() {
    // Must happen before any other thread is spawned (including, inside
    // boot(), before any unit process is started): signal masks are
    // inherited by new threads at creation time, and this closes the
    // window where a fast-exiting child's SIGCHLD could be delivered
    // under the default (ignored) disposition instead of being left
    // pending for the reaper.
    if let Err(e) = reaper::block_signals() {
        fatal(e.context("blocking signals"));
    }

    // catch_unwind, not a plain call: as PID 1, this process must never
    // exit — for a panic anywhere during boot() any more than for a
    // normal setup error — without going through the deliberate,
    // controlled fallback in `fatal` below.
    match panic::catch_unwind(boot) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => fatal(e),
        Err(_) => fatal(anyhow::anyhow!(
            "apollod panicked (see the panic message above, if any)"
        )),
    }
}

/// The actual startup sequence. Returns only on a setup failure, before
/// `sup.run()` — once that starts, the process only ever ends via
/// `Supervisor::shutdown`'s `std::process::exit`, which terminates
/// directly rather than unwinding back through this function, so under
/// normal operation this call simply never returns.
fn boot() -> anyhow::Result<()> {
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

    // Best-effort: a unit whose log file can't be opened (e.g. this
    // directory doesn't exist and isn't writable — likely dev/test mode,
    // see README) just falls back to inheriting apollod's own console for
    // that one unit, handled per-unit in spawn_unit rather than aborting
    // boot over it here.
    if let Err(e) = std::fs::create_dir_all(&args.log_dir) {
        eprintln!(
            "apollod: couldn't create log directory {}: {e} (units will log to the console instead)",
            args.log_dir.display()
        );
    }

    let (mut sup, events_tx) = supervisor::Supervisor::new(args.log_dir);
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

/// A setup failure or panic that would otherwise take the whole process
/// down. As PID 1, that panics the *kernel* ("Attempted to kill init!"),
/// not just apollod — so instead of exiting, this parks the main thread
/// forever, holding PID 1 open. Whatever units had already started keep
/// running unsupervised (the reaper thread, on its own and unaffected by
/// a main-thread panic, keeps reaping them, so they don't zombie);
/// recovering from this still needs a reset, same as any other boot
/// failure in this project's tested workflow, but at least it surfaces as
/// a hang with a message on the console rather than a kernel panic.
///
/// In dev/test mode (not PID 1), none of that constraint applies — just
/// exit normally, as any CLI tool would.
fn fatal(e: anyhow::Error) -> ! {
    eprintln!("apollod: fatal error: {e:#}");
    if mounts::is_pid1() {
        eprintln!(
            "apollod: PID 1 must never exit — holding here instead of exiting (needs a reset to recover)"
        );
        loop {
            thread::park();
        }
    } else {
        std::process::exit(1);
    }
}
