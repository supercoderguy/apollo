//! The supervisor event loop.
//!
//! All unit state lives in a `HashMap` owned by [`Supervisor::run`] and is
//! only ever touched from that one thread — an actor, not a shared,
//! lock-guarded registry. IPC handler threads and the global PID-1 reaper
//! thread (`reaper.rs`) talk to it exclusively through [`Event`]s over an
//! `mpsc` channel, so there's no locking anywhere in the core logic.

use crate::config::{RestartPolicy, UnitConfig};
use crate::mounts;
use crate::registry::UnitRuntime;
use anyhow::Context;
use apollo_proto::{Request, Response, UnitState};
use nix::sys::reboot::RebootMode;
use nix::sys::signal::{self, SigSet, Signal};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How many times a unit may be auto-restarted (by its restart policy)
/// before apollod gives up and marks it Failed. Prevents a crash-looping
/// service from burning CPU forever. Manual `apolloctl restart` is exempt.
const MAX_RESTARTS: u32 = 5;

/// Minimum delay between a unit exiting and an auto-restart of it (policy-
/// driven, not a manual `apolloctl restart` — that stays immediate).
/// Without this, a unit that fails instantly on every attempt (a typo'd
/// path, a device that doesn't exist, a port/lockfile already held by
/// something else) burns through all `MAX_RESTARTS` attempts in one tight
/// burst — no real safety issue, MAX_RESTARTS still bounds it, but it's a
/// flood of forked processes and log lines in well under a second. Found
/// on a real boot: several imported units crash-looping simultaneously
/// scrolled the console faster than it could be read.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// How long [`Supervisor::shutdown`] waits for a unit to exit after
/// SIGTERM before giving up and sending SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub enum Event {
    /// A request from an `apolloctl` connection, with a channel to send
    /// the reply back on.
    Command {
        req: Request,
        resp_tx: mpsc::Sender<Response>,
    },
    /// A child process exited and was reaped by the global PID-1 reaper
    /// (`reaper.rs`). This fires for *every* reaped process, not just
    /// units apollod started — [`Supervisor::handle_exit`] looks up
    /// whether `pid` belongs to a known unit and ignores it if not (e.g.
    /// an orphan reparented from elsewhere on the system).
    ProcessExited {
        pid: u32,
        status: String,
        success: bool,
    },
    /// apollod itself was asked to go down — via SIGTERM/SIGINT
    /// (`reaper.rs`), not an `apolloctl` connection, so there's no
    /// `resp_tx` to reply on.
    Shutdown(ShutdownMode),
    /// A policy-driven auto-restart's backoff (`RESTART_BACKOFF`) has
    /// elapsed — see `Supervisor::schedule_restart`.
    RestartDue(String),
}

#[derive(Debug, Clone, Copy)]
pub enum ShutdownMode {
    Reboot,
    Poweroff,
    Halt,
}

impl ShutdownMode {
    fn reboot_mode(self) -> RebootMode {
        match self {
            ShutdownMode::Reboot => RebootMode::RB_AUTOBOOT,
            ShutdownMode::Poweroff => RebootMode::RB_POWER_OFF,
            ShutdownMode::Halt => RebootMode::RB_HALT_SYSTEM,
        }
    }

    fn from_request(req: &Request) -> Option<Self> {
        match req {
            Request::Reboot => Some(ShutdownMode::Reboot),
            Request::Poweroff => Some(ShutdownMode::Poweroff),
            Request::Halt => Some(ShutdownMode::Halt),
            _ => None,
        }
    }
}

impl fmt::Display for ShutdownMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ShutdownMode::Reboot => "reboot",
            ShutdownMode::Poweroff => "poweroff",
            ShutdownMode::Halt => "halt",
        };
        f.write_str(s)
    }
}

pub struct Supervisor {
    units: HashMap<String, UnitRuntime>,
    events_rx: mpsc::Receiver<Event>,
    /// Kept so [`Supervisor::schedule_restart`] can hand a clone to the
    /// short-lived backoff thread it spawns.
    events_tx: mpsc::Sender<Event>,
    /// The order units were started in at boot, so [`Supervisor::shutdown`]
    /// can stop them in the reverse of it.
    boot_order: Vec<String>,
    /// Directory each unit's stdout/stderr is captured into — see
    /// `spawn_unit`.
    log_dir: PathBuf,
}

impl Supervisor {
    /// Creates a new supervisor and returns it along with a sender clients
    /// (the IPC server, the reaper thread) can use to post it events.
    pub fn new(log_dir: PathBuf) -> (Self, mpsc::Sender<Event>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                units: HashMap::new(),
                events_rx: rx,
                events_tx: tx.clone(),
                boot_order: Vec::new(),
                log_dir,
            },
            tx,
        )
    }

    pub fn load(&mut self, configs: Vec<UnitConfig>) {
        for c in configs {
            self.units.insert(c.name.clone(), UnitRuntime::new(c));
        }
    }

    pub fn start_all(&mut self, order: &[String]) {
        self.boot_order = order.to_vec();
        for name in order {
            self.start_unit(name);
        }
    }

    /// Blocks, processing events, until the channel is closed (i.e. every
    /// sender — including our own retained clone — is dropped) or a
    /// shutdown is triggered, in which case [`Supervisor::shutdown`] ends
    /// the process directly and this never returns normally either way.
    pub fn run(mut self) {
        while let Ok(event) = self.events_rx.recv() {
            match event {
                Event::Command { req, resp_tx } => match ShutdownMode::from_request(&req) {
                    Some(mode) => {
                        // Reply first: the shutdown sequence can take
                        // several seconds (stopping every unit), and the
                        // caller shouldn't be left hanging for it — by the
                        // time it would matter, the machine is going down
                        // anyway.
                        let _ = resp_tx.send(Response::Ok);
                        self.shutdown(mode); // never returns
                    }
                    None => {
                        let resp = self.handle_request(req);
                        let _ = resp_tx.send(resp);
                    }
                },
                Event::ProcessExited {
                    pid,
                    status,
                    success,
                } => self.handle_exit(pid, status, success),
                Event::Shutdown(mode) => self.shutdown(mode),
                Event::RestartDue(name) => self.start_unit(&name),
            }
        }
    }

    fn handle_request(&mut self, req: Request) -> Response {
        match req {
            Request::ListUnits => {
                let mut units: Vec<_> = self.units.values().map(UnitRuntime::to_info).collect();
                units.sort_by(|a, b| a.name.cmp(&b.name));
                Response::Units(units)
            }
            Request::Status { name } => match self.units.get(&name) {
                Some(u) => Response::Unit(u.to_info()),
                None => Response::Error(format!("no such unit: {name}")),
            },
            Request::Start { name } => {
                if !self.units.contains_key(&name) {
                    return Response::Error(format!("no such unit: {name}"));
                }
                self.start_unit(&name);
                Response::Ok
            }
            Request::Stop { name } => match self.stop_unit(&name) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            Request::Restart { name } => {
                let (exists, running) = match self.units.get(&name) {
                    Some(u) => (true, u.pid.is_some()),
                    None => (false, false),
                };
                if !exists {
                    return Response::Error(format!("no such unit: {name}"));
                }
                if running {
                    if let Some(u) = self.units.get_mut(&name) {
                        u.pending_restart = true;
                    }
                    if let Err(e) = self.stop_unit(&name) {
                        return Response::Error(e);
                    }
                } else {
                    self.start_unit(&name);
                }
                Response::Ok
            }
            // Intercepted in run()'s match on Event::Command before it
            // ever calls handle_request, since replying has to happen
            // before the (possibly multi-second) shutdown sequence, not
            // after. Reachable only if that dispatch is ever changed to
            // stop doing so.
            Request::Reboot | Request::Poweroff | Request::Halt => {
                Response::Error("internal error: shutdown request reached handle_request".into())
            }
        }
    }

    fn start_unit(&mut self, name: &str) {
        let Some(unit) = self.units.get_mut(name) else {
            return;
        };
        if matches!(unit.state, UnitState::Running | UnitState::Stopping) {
            return;
        }
        match spawn_unit(name, &unit.config, &self.log_dir) {
            Ok(pid) => {
                unit.pid = Some(pid);
                unit.state = UnitState::Running;
                unit.user_stopped = false;
                eprintln!("apollod: started {name} (pid {pid})");
            }
            Err(e) => {
                unit.state = UnitState::Failed;
                unit.exit_status = Some(e.to_string());
                eprintln!("apollod: failed to start {name}: {e:#}");
            }
        }
    }

    fn stop_unit(&mut self, name: &str) -> Result<(), String> {
        let Some(unit) = self.units.get_mut(name) else {
            return Err(format!("no such unit: {name}"));
        };
        match unit.pid {
            Some(pid) => {
                unit.user_stopped = true;
                unit.state = UnitState::Stopping;
                signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
                    .map_err(|e| format!("sending SIGTERM to {name} (pid {pid}): {e}"))?;
                Ok(())
            }
            None => Err(format!("{name} is not running")),
        }
    }

    /// Stops every unit (reverse of `boot_order`, waiting for each to
    /// actually exit before moving to the next — mirroring dependency
    /// order exactly reversed), then — only if apollod is actually PID 1
    /// — syncs, unmounts, and calls `reboot(2)`. Never returns: the
    /// process always ends here, one way or another.
    ///
    /// The sync/unmount/reboot(2) steps are skipped entirely when not
    /// PID 1 (dev/test mode), same gating as `mounts::run` — there's no
    /// sense in a dev-testing apollod instance syncing and unmounting the
    /// *real* system it happens to be running on.
    fn shutdown(&mut self, mode: ShutdownMode) -> ! {
        eprintln!("apollod: {mode} requested, stopping units...");
        let order = std::mem::take(&mut self.boot_order);
        for name in order.iter().rev() {
            self.stop_and_wait(name, STOP_TIMEOUT);
        }

        if mounts::is_pid1() {
            eprintln!("apollod: syncing filesystems");
            nix::unistd::sync();

            eprintln!("apollod: unmounting filesystems");
            match Command::new("umount").args(["-a", "-r"]).status() {
                Ok(status) if status.success() => eprintln!("apollod: umount -a -r succeeded"),
                Ok(status) => eprintln!("apollod: umount -a -r exited with {status}"),
                Err(e) => eprintln!("apollod: failed to run umount: {e}"),
            }

            eprintln!("apollod: calling reboot(2) ({mode})");
            // reboot(2) only returns on failure — success means the
            // machine is already going down and never returns to us.
            let Err(e) = nix::sys::reboot::reboot(mode.reboot_mode());
            eprintln!("apollod: reboot(2) failed: {e}");
        } else {
            eprintln!(
                "apollod: not PID 1 — skipping sync/unmount/reboot(2) (dev/test mode); exiting instead"
            );
        }

        std::process::exit(0);
    }

    /// Sends SIGTERM to `name` (if running) and blocks — continuing to
    /// service other events in the meantime, most importantly other
    /// units' own exits — until it actually exits, or `timeout` passes
    /// and it's sent SIGKILL instead.
    fn stop_and_wait(&mut self, name: &str, timeout: Duration) {
        let Some(pid) = self.units.get(name).and_then(|u| u.pid) else {
            return; // not running, nothing to do
        };
        if let Err(e) = self.stop_unit(name) {
            eprintln!("apollod: {name}: {e}");
            return;
        }

        let mut deadline = Instant::now() + timeout;
        let mut escalated = false;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                if escalated {
                    eprintln!(
                        "apollod: {name} (pid {pid}) still hasn't exited after SIGKILL, giving up"
                    );
                    return;
                }
                eprintln!(
                    "apollod: {name} (pid {pid}) didn't stop within {timeout:?}, sending SIGKILL"
                );
                let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                escalated = true;
                deadline = Instant::now() + Duration::from_secs(2);
                continue;
            };
            match self.events_rx.recv_timeout(remaining) {
                Ok(Event::ProcessExited {
                    pid: exited_pid,
                    status,
                    success,
                }) => {
                    let is_this_one = exited_pid == pid;
                    self.handle_exit(exited_pid, status, success);
                    if is_this_one {
                        return;
                    }
                    // Some other unit (or an unrelated orphan) exited
                    // while we were waiting — handled above, keep waiting
                    // for the one we actually care about.
                }
                Ok(Event::Command { resp_tx, .. }) => {
                    let _ = resp_tx.send(Response::Error("apollod is shutting down".into()));
                }
                Ok(Event::Shutdown(_)) => {} // already shutting down, ignore
                Ok(Event::RestartDue(_)) => {} // ditto — don't restart into a shutdown
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {} // loop, re-check the deadline
            }
        }
    }

    /// Handles a reaped process exit. `pid` may not belong to any unit at
    /// all — the reaper reaps every child reparented to apollod as PID 1,
    /// not just units it started — in which case this is a no-op: the
    /// reaper has already done the one thing PID 1 owes it (`waitpid`).
    fn handle_exit(&mut self, pid: u32, status: String, success: bool) {
        let Some(name) = self.find_unit_by_pid(pid) else {
            return;
        };

        let should_restart;
        let pending_restart;
        {
            let Some(unit) = self.units.get_mut(&name) else {
                return;
            };
            unit.pid = None;
            unit.exit_status = Some(status.clone());
            pending_restart = std::mem::take(&mut unit.pending_restart);
            let policy_wants_restart = !unit.user_stopped
                && match unit.config.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => !success,
                    RestartPolicy::No => false,
                };
            should_restart =
                (pending_restart || policy_wants_restart) && unit.restart_count < MAX_RESTARTS;
            if should_restart {
                // Transient: start_unit() below will flip this to Running.
                // Without this, state stays Stopping and start_unit's
                // already-running guard refuses to respawn it.
                unit.restart_count += 1;
                unit.state = UnitState::Stopped;
            } else if unit.user_stopped {
                // A deliberate `stop` is reported by wait() as a signal
                // exit (success() == false), but that's not a failure.
                unit.state = UnitState::Stopped;
            } else {
                unit.state = if success {
                    UnitState::Stopped
                } else {
                    UnitState::Failed
                };
            }
        }

        if should_restart {
            if pending_restart {
                // A manual `apolloctl restart` — keep this immediate, same
                // as before; only policy-driven (crash-loop) restarts get
                // backed off.
                eprintln!("apollod: {name} exited ({status}), restarting");
                self.start_unit(&name);
            } else {
                eprintln!(
                    "apollod: {name} exited ({status}), restarting in {RESTART_BACKOFF:?}"
                );
                self.schedule_restart(&name);
            }
        } else {
            eprintln!("apollod: {name} exited ({status})");
        }
    }

    /// Respawns `name` after `RESTART_BACKOFF`, via a short-lived thread
    /// that sleeps then posts `Event::RestartDue` back onto the event
    /// channel — not a blocking sleep here in the event loop itself, which
    /// would stall every other unit and all IPC for the duration.
    /// `start_unit`'s own Running/Stopping guard makes this safe to act on
    /// even if something else (a manual `apolloctl start`/`stop`) already
    /// changed the unit's state by the time it lands.
    fn schedule_restart(&self, name: &str) {
        let tx = self.events_tx.clone();
        let name = name.to_string();
        thread::spawn(move || {
            thread::sleep(RESTART_BACKOFF);
            let _ = tx.send(Event::RestartDue(name));
        });
    }

    fn find_unit_by_pid(&self, pid: u32) -> Option<String> {
        self.units
            .iter()
            .find(|(_, u)| u.pid == Some(pid))
            .map(|(name, _)| name.clone())
    }
}

/// Opens (creating if needed, appending if not — so a restart's output
/// doesn't clobber what came before it) `<log_dir>/<name>.log`, returning
/// two independent handles to it (via `try_clone`, not opening it twice)
/// so a unit's stdout and stderr can be wired up as separate `Stdio`s that
/// still land in the same file and share its write offset — the same
/// effect as a shell's `2>&1`.
///
/// `Err` here isn't fatal to spawning the unit — see the fallback in
/// `spawn_unit` — so this is `Result` rather than something that panics or
/// gets unwrapped.
fn open_unit_log(name: &str, log_dir: &Path) -> std::io::Result<(File, File)> {
    let path = log_dir.join(format!("{name}.log"));
    let out = OpenOptions::new().create(true).append(true).open(&path)?;
    let err = out.try_clone()?;
    Ok((out, err))
}

/// Spawns a unit's process and returns its pid. Only the pid is kept in
/// the registry — no `Child` handle is retained. As PID 1, apollod's
/// global reaper thread (`reaper.rs`) is what calls `waitpid` on every
/// child regardless of who spawned it, so there's no per-unit "wait for
/// exit" thread here anymore; stopping a unit means signalling that pid,
/// not holding onto a `Child`.
fn spawn_unit(name: &str, cfg: &UnitConfig, log_dir: &Path) -> anyhow::Result<u32> {
    let (prog, args) = cfg
        .exec
        .split_first()
        .context("unit's `exec` list must not be empty")?;

    let mut cmd = Command::new(prog);
    cmd.args(args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &cfg.working_dir {
        cmd.current_dir(dir);
    }

    // Every unit defaults to inheriting apollod's own stdout/stderr, which
    // as PID 1 is whatever console the kernel handed it — meaning, without
    // this, *every* unit's output (plus every restart of it) lands on that
    // one console forever, indistinguishable from apollod's own log lines
    // and from whatever a getty/login shell on that same console is trying
    // to show. Redirect each unit's stdout/stderr into its own log file
    // instead; fall back to inheriting (the old behavior) only if the log
    // file itself can't be opened, e.g. `log_dir` doesn't exist and isn't
    // writable (typical of dev/test mode — see README) — a unit failing to
    // start over a logging problem alone would be a strange way to fail.
    match open_unit_log(name, log_dir) {
        Ok((out, err)) => {
            cmd.stdout(Stdio::from(out));
            cmd.stderr(Stdio::from(err));
        }
        Err(e) => {
            eprintln!(
                "apollod: couldn't open log file for {name} in {}: {e} (its output will go to the console instead)",
                log_dir.display()
            );
        }
    }

    // apollod blocks SIGCHLD/SIGTERM/SIGINT on itself for its own
    // sigwait()-based handling (reaper.rs) — but fork() carries that
    // blocked mask into every child, and exec() does *not* reset it.
    // Without clearing it here first, a spawned unit would silently
    // ignore SIGTERM (e.g. from `apolloctl stop`) forever, since the
    // signal just sits pending against a mask nothing in that process
    // ever unblocks.
    //
    // Safety: this closure runs in the forked child, after fork() and
    // before exec(), when the child is a single-threaded copy of this
    // process — only async-signal-safe operations are permitted, and
    // `pthread_sigmask` (what `thread_set_mask` calls) is safe to use
    // there.
    unsafe {
        cmd.pre_exec(|| {
            SigSet::empty().thread_set_mask()?;
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning unit '{name}' ({prog})"))?;

    Ok(child.id())
}
