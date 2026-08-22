//! The PID 1 reaper: collects the exit status of every child process that
//! ends up parented to apollod, not just the units it spawned itself.
//!
//! As PID 1, any process whose original parent dies gets reparented here
//! by the kernel — if nobody calls `waitpid` on it, it stays a zombie
//! forever. This uses the classic "block the signal, then `sigwait` for it
//! on a dedicated thread" pattern rather than an async signal handler, so
//! there are no async-signal-safety concerns to worry about in the reap
//! path itself.

use crate::supervisor::Event;
use nix::errno::Errno;
use nix::sys::signal::{SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::sync::mpsc;
use std::thread;

/// Blocks `SIGCHLD` for the calling thread. Signal masks are inherited by
/// new threads at creation time, so this must run on the main thread
/// before any other thread is spawned — including before any unit
/// processes are started, so there's no window where a fast-exiting
/// child's SIGCHLD is delivered under the default (ignored) disposition
/// and never makes it into this process's pending set.
pub fn block_sigchld() -> anyhow::Result<()> {
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    set.thread_block()?;
    Ok(())
}

/// Spawns the dedicated reaper thread and returns immediately.
pub fn spawn(events_tx: mpsc::Sender<Event>) {
    thread::spawn(move || run(events_tx));
}

fn run(events_tx: mpsc::Sender<Event>) {
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    loop {
        if let Err(e) = set.wait() {
            eprintln!("apollod: sigwait(SIGCHLD) failed: {e}");
            continue;
        }
        drain(&events_tx);
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
