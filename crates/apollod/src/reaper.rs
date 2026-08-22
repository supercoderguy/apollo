//! The PID 1 signal-handling thread: reaps the exit status of every child
//! process that ends up parented to apollod (not just units it spawned
//! itself), and turns SIGTERM/SIGINT — apollod's own termination signals —
//! into a graceful shutdown.
//!
//! As PID 1, any process whose original parent dies gets reparented here
//! by the kernel — if nobody calls `waitpid` on it, it stays a zombie
//! forever. Likewise, PID 1 is who the kernel sends SIGINT to on
//! Ctrl-Alt-Del, and a plain `kill`/`reboot`/`poweroff`/`shutdown`
//! targeting PID 1 shows up as SIGTERM. This uses the classic "block the
//! signals, then `sigwait` for them on a dedicated thread" pattern rather
//! than async signal handlers, so there are no async-signal-safety
//! concerns to worry about in the reap path itself.

use crate::supervisor::{Event, ShutdownMode};
use nix::errno::Errno;
use nix::sys::signal::{SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::sync::mpsc;
use std::thread;

fn signal_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    set
}

/// Blocks `SIGCHLD`/`SIGTERM`/`SIGINT` for the calling thread. Signal
/// masks are inherited by new threads at creation time, so this must run
/// on the main thread before any other thread is spawned — including
/// before any unit processes are started, so there's no window where a
/// fast-exiting child's SIGCHLD is delivered under the default (ignored)
/// disposition and never makes it into this process's pending set.
pub fn block_signals() -> anyhow::Result<()> {
    signal_set().thread_block()?;
    Ok(())
}

/// Spawns the dedicated signal-handling thread and returns immediately.
pub fn spawn(events_tx: mpsc::Sender<Event>) {
    thread::spawn(move || run(events_tx));
}

fn run(events_tx: mpsc::Sender<Event>) {
    let set = signal_set();
    let mut shutdown_sent = false;
    loop {
        match set.wait() {
            Ok(Signal::SIGCHLD) => drain(&events_tx),
            // Deliberately *not* `return`ing after dispatching a shutdown:
            // this thread has to keep draining SIGCHLD for the rest of the
            // process's life, since Supervisor::shutdown (running on the
            // main thread) depends on ProcessExited events to know when
            // each unit it's stopping has actually exited. If this thread
            // stopped here, every unit stopped during shutdown would sit
            // as an unreaped zombie until the whole process exits instead
            // of being detected as exited.
            Ok(Signal::SIGTERM) if !shutdown_sent => {
                eprintln!("apollod: received SIGTERM, shutting down");
                let _ = events_tx.send(Event::Shutdown(ShutdownMode::Poweroff));
                shutdown_sent = true;
            }
            Ok(Signal::SIGINT) if !shutdown_sent => {
                eprintln!("apollod: received SIGINT (Ctrl-Alt-Del), rebooting");
                let _ = events_tx.send(Event::Shutdown(ShutdownMode::Reboot));
                shutdown_sent = true;
            }
            // A repeat SIGTERM/SIGINT once shutdown is already underway:
            // ignore it, the sequence is already in progress.
            Ok(Signal::SIGTERM | Signal::SIGINT) => {}
            Ok(other) => eprintln!("apollod: sigwait woke for unexpected signal {other}"),
            Err(e) => eprintln!("apollod: sigwait failed: {e}"),
        }
    }
}

/// A single SIGCHLD delivery can stand for more than one exit — signals
/// don't queue 1:1 with events — so keep reaping with `WNOHANG` until
/// there's genuinely nothing left ready.
fn drain(events_tx: &mpsc::Sender<Event>) {
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(WaitStatus::Exited(pid, code)) => {
                report(events_tx, pid, format!("exit status: {code}"), code == 0);
            }
            Ok(WaitStatus::Signaled(pid, sig, _core_dumped)) => {
                report(events_tx, pid, format!("signal: {sig}"), false);
            }
            // Stopped / Continued / ptrace events: not an exit, nothing to
            // report, but keep draining in case more are queued.
            Ok(_) => {}
            Err(Errno::ECHILD) => break, // no children left at all
            Err(e) => {
                eprintln!("apollod: waitpid failed: {e}");
                break;
            }
        }
    }
}

fn report(events_tx: &mpsc::Sender<Event>, pid: Pid, status: String, success: bool) {
    let _ = events_tx.send(Event::ProcessExited {
        pid: pid.as_raw() as u32,
        status,
        success,
    });
}
