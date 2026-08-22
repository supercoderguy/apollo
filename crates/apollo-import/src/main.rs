mod runit;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Converts service definitions from other init systems into apollo unit
/// files (`*.toml`) that can be dropped into an apollod config directory.
/// A one-time, offline conversion tool — doesn't talk to a running
/// apollod, doesn't need root itself (only whatever permissions reading
/// the source and writing the destination require).
#[derive(Parser, Debug)]
#[command(name = "apollo-import")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Convert runit service directories (each holding an executable
    /// `run` script — e.g. Void Linux's `/etc/sv/`) into apollo units.
    Runit {
        /// Directory containing one subdirectory per runit service — the
        /// real, final location. The generated units point straight at
        /// each service's `run` script there; nothing is copied.
        src: PathBuf,

        /// Directory to write the generated `*.toml` unit files into.
        dest: PathBuf,

        /// Overwrite a unit file that already exists at the destination.
        #[arg(long)]
        force: bool,

        /// Also convert services with a runit `down` file (starts
        /// disabled). Off by default: a `down` file usually marks one of
        /// several mutually-exclusive alternatives for the same role
        /// (e.g. several agetty variants for consoles that may not exist
        /// on this machine), and apollo has no "loaded but not started"
        /// state — converting one means it auto-starts immediately,
        /// which crash-loops if it isn't actually applicable here.
        #[arg(long)]
        include_down: bool,

        /// Remove every existing `*.toml` file in `dest` before
        /// converting — use this for a full re-import so a service
        /// dropped from this run (e.g. now correctly skipped for having
        /// a `down` file) doesn't leave its old generated unit behind,
        /// still present and still auto-starting. Removes *any* `.toml`
        /// there, not just ones this tool generated — don't point `dest`
        /// at a directory with hand-written units mixed in if using this.
        #[arg(long)]
        clean: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Runit {
            src,
            dest,
            force,
            include_down,
            clean,
        } => {
            let summary = runit::convert(&src, &dest, force, include_down, clean)?;
            report(&summary);
        }
    }
    Ok(())
}

fn report(summary: &runit::Summary) {
    for path in &summary.cleaned {
        println!("removed (--clean): {path}");
    }
    for name in &summary.imported {
        println!("imported: {name}");
    }
    for (name, reason) in &summary.skipped {
        eprintln!("skipped {name}: {reason}");
    }
    for (name, note) in &summary.notes {
        eprintln!("note ({name}): {note}");
    }
    println!(
        "{} imported, {} skipped",
        summary.imported.len(),
        summary.skipped.len()
    );
}
