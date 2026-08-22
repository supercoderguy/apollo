//! The supervisor event loop.
//!
//! All unit state lives in a `HashMap` owned by [`Supervisor::run`] and is
//! only ever touched from that one thread — an actor, not a shared,
//! lock-guarded registry. IPC handler threads and the global PID-1 reaper
//! thread (`reaper.rs`) talk to it exclusively through [`Event`]s over an
//! `mpsc` channel, so there's no locking anywhere in the core logic.

use crate::config::{RestartPolicy, UnitConfig};
use crate::registry::UnitRuntime;
use anyhow::Context;
use apollo_proto::{Request, Response, UnitState};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::process::Command;
use std::sync::mpsc;

/// How many times a unit may be auto-restarted (by its restart policy)
/// before apollod gives up and marks it Failed. Prevents a crash-looping
/// service from burning CPU forever. Manual `apolloctl restart` is exempt.
const MAX_RESTARTS: u32 = 5;

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
}

pub struct Supervisor {
    units: HashMap<String, UnitRuntime>,
    events_rx: mpsc::Receiver<Event>,
}

impl Supervisor {
    /// Creates a new supervisor and returns it along with a sender clients
    /// (the IPC server, the reaper thread) can use to post it events.
    pub fn new() -> (Self, mpsc::Sender<Event>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                units: HashMap::new(),
                events_rx: rx,
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
        for name in order {
            self.start_unit(name);
        }
    }

    /// Blocks, processing events, until the channel is closed (i.e. every
    /// sender — including our own retained clone — is dropped).
    pub fn run(mut self) {
        while let Ok(event) = self.events_rx.recv() {
            match event {
                Event::Command { req, resp_tx } => {
                    let resp = self.handle_request(req);
                    let _ = resp_tx.send(resp);
                }
                Event::ProcessExited {
                    pid,
                    status,
                    success,
                } => self.handle_exit(pid, status, success),
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
        }
    }

    fn start_unit(&mut self, name: &str) {
        let Some(unit) = self.units.get_mut(name) else {
            return;
        };
        if matches!(unit.state, UnitState::Running | UnitState::Stopping) {
            return;
        }
        match spawn_unit(name, &unit.config) {
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

    /// Handles a reaped process exit. `pid` may not belong to any unit at
    /// all — the reaper reaps every child reparented to apollod as PID 1,
    /// not just units it started — in which case this is a no-op: the
    /// reaper has already done the one thing PID 1 owes it (`waitpid`).
    fn handle_exit(&mut self, pid: u32, status: String, success: bool) {
        let Some(name) = self.find_unit_by_pid(pid) else {
            return;
        };

        let should_restart;
        {
            let Some(unit) = self.units.get_mut(&name) else {
                return;
            };
            unit.pid = None;
            unit.exit_status = Some(status.clone());
            let pending_restart = std::mem::take(&mut unit.pending_restart);
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
            eprintln!("apollod: {name} exited ({status}), restarting");
            self.start_unit(&name);
        } else {
            eprintln!("apollod: {name} exited ({status})");
        }
    }

    fn find_unit_by_pid(&self, pid: u32) -> Option<String> {
        self.units
            .iter()
            .find(|(_, u)| u.pid == Some(pid))
            .map(|(name, _)| name.clone())
    }
}

/// Spawns a unit's process and returns its pid. Only the pid is kept in
/// the registry — no `Child` handle is retained. As PID 1, apollod's
/// global reaper thread (`reaper.rs`) is what calls `waitpid` on every
/// child regardless of who spawned it, so there's no per-unit "wait for
/// exit" thread here anymore; stopping a unit means signalling that pid,
/// not holding onto a `Child`.
fn spawn_unit(name: &str, cfg: &UnitConfig) -> anyhow::Result<u32> {
    let (prog, args) = cfg
        .exec
        .split_first()
        .context("unit's `exec` list must not be empty")?;

    let mut cmd = Command::new(prog);
    cmd.args(args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning unit '{name}' ({prog})"))?;

    Ok(child.id())
}
