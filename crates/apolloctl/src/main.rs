use anyhow::Context;
use apollo_proto::{Request, Response, UnitInfo, DEFAULT_SOCKET_PATH};
use clap::{Parser, Subcommand};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Control client for the apollo init daemon.
#[derive(Parser)]
#[command(name = "apolloctl")]
struct Cli {
    /// Path to apollod's control socket.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List all known service units and their state.
    List,
    /// Show the status of a single unit.
    Status { name: String },
    /// Start a unit.
    Start { name: String },
    /// Stop a running unit.
    Stop { name: String },
    /// Stop and restart a unit.
    Restart { name: String },
    /// Stop every unit and reboot the machine.
    Reboot,
    /// Stop every unit and power the machine off.
    Poweroff,
    /// Stop every unit and halt the machine (without powering it off).
    Halt,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let socket_path = cli
        .socket
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));

    // Shutdown commands get a more informative success message than the
    // generic "ok" below, since unlike the other commands the requested
    // action (stopping every unit, then possibly the whole machine going
    // down) is still very much in progress when apolloctl gets its reply.
    let ok_message = match &cli.command {
        Command::Reboot => "apollod is stopping all units and rebooting",
        Command::Poweroff => "apollod is stopping all units and powering off",
        Command::Halt => "apollod is stopping all units and halting",
        _ => "ok",
    };

    let req = match cli.command {
        Command::List => Request::ListUnits,
        Command::Status { name } => Request::Status { name },
        Command::Start { name } => Request::Start { name },
        Command::Stop { name } => Request::Stop { name },
        Command::Restart { name } => Request::Restart { name },
        Command::Reboot => Request::Reboot,
        Command::Poweroff => Request::Poweroff,
        Command::Halt => Request::Halt,
    };

    let mut stream = UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "connecting to apollod at {} (is it running?)",
            socket_path.display()
        )
    })?;
    apollo_proto::write_message(&mut stream, &req).context("sending request")?;
    let resp: Response = apollo_proto::read_message(&mut stream).context("reading response")?;

    match resp {
        Response::Ok => println!("{ok_message}"),
        Response::Error(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Response::Units(units) => print_units(&units),
        Response::Unit(u) => print_units(std::slice::from_ref(&u)),
    }
    Ok(())
}

fn print_units(units: &[UnitInfo]) {
    println!("{:<16} {:<10} {:>8}  DETAIL", "NAME", "STATE", "PID");
    for u in units {
        let pid = u.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        let detail = u.exit_status.as_deref().unwrap_or("");
        println!("{:<16} {:<10} {:>8}  {}", u.name, u.state, pid, detail);
    }
}
