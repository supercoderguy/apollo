# apollo

A Linux init system, written in Rust.

## Scope and design decisions

apollo is being built as a **minimal PID 1**, not a full systemd
replacement: process supervision, service start/stop/restart, and basic
dependency ordering — not socket activation, cgroup resource control, or a
built-in logging daemon. It's a clean-slate design; it does not aim for
compatibility with systemd `.service` files. Service definitions are TOML.

The daemon/CLI split — `apollod` (the supervisor) and `apolloctl` (its
control client) talking over a Unix socket — is in place, developed and
tested as an ordinary process rather than as PID 1. apollod now also does
real PID-1-style reaping and early filesystem setup (mounts, fstab,
hostname — see Architecture below); the mounting code only actually runs
when apollod is PID 1, so it hasn't been exercised against a real kernel
yet, only built and reviewed — see [Testing safely on a real
distro](#testing-safely-on-a-real-distro). Getty units are in place too
(`examples/getty/`), so a real boot should reach a login prompt, and
`apolloctl reboot`/`poweroff`/`halt` (plus SIGTERM/SIGINT sent to apollod
directly) do a full graceful shutdown — see [Shutdown](#shutdown) below.
See [Roadmap](#roadmap) for what's still ahead. See [Tested On](#tested-on)
to make sure that Apollo has been tested and works on your distro.

## Layout

```
crates/
  apollo-proto/   shared IPC types and wire protocol (Request/Response, framing)
  apollod/        the supervisor daemon
  apolloctl/      the CLI control client
examples/services/ toy unit files for local dev testing (no root needed)
examples/getty/    real getty units meant for an actual /etc/apollo/services
```

### Architecture

`apollod` is structured as a single-threaded actor: a `Supervisor` owns a
`HashMap<String, UnitRuntime>` of all unit state, and it is the *only*
thing that ever touches that map. Everything else — the control-socket
listener, per-connection handler threads, and the reaper thread — only
ever communicates with it by sending an `Event` over an `mpsc` channel
and (for commands) waiting on a reply channel. This avoids locking in the
core logic entirely; see `crates/apollod/src/supervisor.rs`.

Concretely:
- **`config.rs`** — loads `*.toml` unit files from a directory, and
  topologically sorts them by their `after` field to decide start order.
  An `after` name that doesn't match a loaded unit (e.g. a not-yet-modeled
  `network.target`) is treated as already satisfied.
- **`registry.rs`** — `UnitRuntime`, the state kept for one loaded unit
  (parsed config + current state + pid + restart bookkeeping).
- **`supervisor.rs`** — the event loop and all state transitions. Units
  are tracked only by pid; process exits arrive as `Event::ProcessExited`
  from the reaper (below), and `handle_exit` looks up which unit (if any)
  a pid belongs to. Every spawned unit gets a `pre_exec` hook that resets
  its inherited (blocked) signal mask before `exec` — `fork()` carries
  apollod's own blocked `SIGCHLD`/`SIGTERM`/`SIGINT` into every child, and
  without undoing that, `SIGTERM` (e.g. from `apolloctl stop`) would just
  sit pending against a mask nothing in that process ever unblocks,
  silently ignored forever. Also owns shutdown: see
  [Shutdown](#shutdown) below.
- **`reaper.rs`** — one dedicated thread doing two jobs. Real PID-1-style
  reaping: `SIGCHLD`/`SIGTERM`/`SIGINT` are blocked on the main thread
  before anything else happens (so all later threads, including every
  spawned unit — see the note on `pre_exec` below — inherit them
  blocked), and this thread synchronously `sigwait()`s for them, draining
  every ready exit with `waitpid(-1, WNOHANG)` on each `SIGCHLD` wakeup.
  This reaps *every* child that ends up parented to apollod — not just
  units it started — which is what makes it correct to run as actual
  PID 1, where any orphaned process on the system can be reparented here.
  The same thread turns `SIGTERM`/`SIGINT` (what the kernel sends PID 1
  on a plain `kill`/Ctrl-Alt-Del) into an `Event::Shutdown`, then keeps
  running — it must not exit early just because a shutdown started, since
  `Supervisor::shutdown` depends on it continuing to reap units as they're
  stopped one by one.
- **`mounts.rs`** — early-boot filesystem setup: first, a default `PATH`
  if the environment doesn't already have one (PID 1 as exec'd by the
  kernel typically has *no* environment at all — without this, every
  bare-name command apollod or any unit shells out to would fail to
  resolve). Then `/proc`, `/sys`, devtmpfs on `/dev`, `/dev/pts`,
  `/dev/shm`, `/run` (tmpfs), cgroup2 on `/sys/fs/cgroup`, then `mount -a`
  + `swapon -a` against `/etc/fstab` and setting the hostname from
  `/etc/hostname`. Gated in `main.rs` on `is_pid1()` (`getpid() == 1`) —
  a no-op on every dev/test invocation, since none of it makes sense
  unless apollod actually is the init process for this boot.
- **`ipc.rs`** — accepts connections on the control socket; each
  connection is one `Request` in, one `Response` out.
- **`main.rs`** — wires the above together, and is where PID 1's
  never-exit constraint is actually enforced: `boot()` (the real startup
  sequence) runs inside `catch_unwind`, and both a panic and a normal
  setup `Err` funnel into `fatal()`, which holds the process open instead
  of exiting when running as PID 1 (see Roadmap step 2).
- **`apollo-proto`** — `Request`/`Response`/`UnitInfo` types shared by
  `apollod` and `apolloctl`, plus length-prefixed JSON framing
  (`write_message`/`read_message`) over the socket.

### Unit files

One `*.toml` file per service, in a directory apollod is pointed at:

```toml
name = "sshd"
exec = ["/usr/sbin/sshd", "-D"]
restart = "on-failure"   # "no" (default) | "always" | "on-failure"
after = ["network.target"]

[env]
SOME_VAR = "value"
```

- `exec` is an argv array — no shell involved unless you put one there
  yourself (`["/bin/sh", "-c", "..."]`).
- `after` only orders startup among *other loaded units*; there's no
  notion of targets/runlevels yet.
- Restart attempts are capped at 5 per unit per daemon run (see
  `MAX_RESTARTS` in `supervisor.rs`) to avoid burning CPU on a crash loop.
  A unit that hits the cap is marked `failed` and left alone.

### Getty

`examples/getty/` has two units meant to actually be deployed (copy them
into `/etc/apollo/services/`, not just used as dev-testing toys):
`getty-tty1` (a virtual-console login prompt — `agetty ... tty1`) and
`getty-serial` (a serial-console one, for a headless VM — `agetty -L
ttyS0 ...`). Both are ordinary `restart = "always"` units; no getty-
specific code exists in apollod, since the generic restart-on-exit
mechanism already covers it — `agetty` exec's into `login` on successful
auth (keeping the same pid), and when that session ends the pid exits and
apollod respawns a fresh `agetty`, same as it would for any other
long-running unit.

One real, current wrinkle: apollod's own log lines (`eprintln!`) go to
whatever console it inherited, same as every unit's stdout/stderr right
now (no log capture yet — Roadmap step 9). If that happens to be the same
physical console a getty is attached to, apollod's own output can
interleave with the login prompt. Cosmetic, not a functional problem —
but it goes away once step 9 gives units (and probably apollod's own
diagnostics) somewhere else to write.

### Shutdown

`apolloctl reboot`/`poweroff`/`halt` (and SIGTERM/SIGINT sent to apollod
directly — SIGINT reboots, matching the kernel's Ctrl-Alt-Del-to-PID-1
convention; SIGTERM powers off) all funnel into one sequence in
`Supervisor::shutdown`:

1. Reply `Ok` to the caller immediately, *before* doing anything slow —
   stopping every unit can take seconds, and there's no reason to leave
   `apolloctl` hanging for it.
2. Stop every unit in the reverse of its start order, one at a time,
   waiting for each to actually exit (SIGTERM, then SIGKILL after a 5s
   timeout) before moving to the next — mirroring dependency order
   exactly reversed, the same way `after` decided start order.
3. Only if apollod is actually PID 1: `sync()`, `umount -a -r` against
   whatever `/etc/fstab` mounted, then `reboot(2)`.
4. Otherwise (dev/test mode): skip step 3 entirely and just exit. This
   isn't a testing shortcut bolted on afterward — it's the same
   `is_pid1()` gate `mounts.rs` uses, because syncing and unmounting the
   *real* filesystems of whatever machine a dev-testing apollod instance
   happens to be running on is never correct, regardless of testing
   concerns.

Verified in dev mode: all three `apolloctl` subcommands, plus SIGTERM and
SIGINT sent directly to apollod, correctly stop every unit (confirmed via
the log and process list, not just the reply) and exit — consistently
within milliseconds when a unit responds to SIGTERM immediately, as all
the example units do. What's *not* exercised here, and can't be: the
actual `sync`/`umount`/`reboot(2)` steps, gated behind `is_pid1()` same as
`mounts.rs` — that's for the Fedora VM.

## Building

```sh
cargo build
```

## Running locally (not as PID 1)

`apollod` defaults to `/etc/apollo/services` and
`/run/apollo/control.sock`, which need root. For local development,
override both:

```sh
./target/debug/apollod \
  --config-dir ./examples/services \
  --socket /tmp/apollo.sock &

./target/debug/apolloctl --socket /tmp/apollo.sock list
./target/debug/apolloctl --socket /tmp/apollo.sock status ping
./target/debug/apolloctl --socket /tmp/apollo.sock restart ping
./target/debug/apolloctl --socket /tmp/apollo.sock stop ping
```

`examples/services/` has three units to try this against: `hello` (a
one-shot), `greeter` (a one-shot that runs `after = ["hello"]`), and
`ping` (a `restart = "always"` loop, useful for exercising
stop/start/restart/status). `examples/getty/` can be pointed at the same
way (`--config-dir ./examples/getty`) to try the getty units against
this machine's own `/dev/tty1`/`/dev/ttyS0` — safe to do; `agetty` just
sits blocked waiting for a login, same as `ping` sits in its loop.

## Current limitations

These are expected at this milestone, not bugs:

- **SIGKILL still bypasses everything.** `kill -9 apollod` (or any signal
  other than SIGTERM/SIGINT) skips the graceful shutdown sequence
  entirely and just orphans apollod's children — same as the old
  "stopping apollod doesn't stop its children" caveat, just narrower now
  that plain SIGTERM/SIGINT are handled. Expected (SIGKILL can't be
  caught by anything, ever), but worth remembering when force-killing a
  test instance.
- Early mounts (`mounts.rs`) are implemented but only ever run when
  apollod is PID 1, so they've had a code review and a clean build, not a
  real boot yet — that happens in the Fedora VM, not here.
- Units are tracked only by the single pid apollod spawned. A unit that
  forks further children (double-forking daemons, in particular) is
  invisible to apollo beyond that first pid — `stop` won't reach them,
  and their exit doesn't affect the unit's tracked state. Process-group-
  or cgroup-based tracking (Roadmap step 6) fixes this.
- `after` is the only dependency relation; no targets, no `wants`/
  `requires` distinction, no parallelism control beyond what the
  topological order allows (units are currently started serially, in
  order — not fanned out in parallel per dependency level).
- No log capture — child stdout/stderr currently just inherit apollod's
  own, as seen in the examples above. For a getty unit specifically, this
  means apollod's own log lines can visually interleave with the login
  prompt on the same console (see the Getty section above).

## Roadmap

Ordered toward the concrete goal of booting a real distro (Fedora, in a
VM) to a usable login prompt and shutting it down cleanly:

1. ~~Daemon/CLI split with a control socket~~ (done)
2. ~~Real PID 1 reaping~~ (done) — `SIGCHLD` blocked on the main thread
   before any other thread or unit process exists, a dedicated thread
   `sigwait()`s on it, and each wakeup drains every ready exit with
   `waitpid(-1, WNOHANG)` (signals don't queue 1:1, so one wakeup can mean
   several exits). Reaps *any* child parented to apollod, known unit or
   not. (apollod's own termination signals — SIGTERM/SIGINT/Ctrl-Alt-Del —
   are now handled too; see step 5.) A main-thread panic or setup error is
   now also caught rather than allowed to exit the process — see
   `main.rs::fatal` — since PID 1 exiting panics the *kernel*, not just
   apollod: `main()` wraps `boot()` in `catch_unwind`, and both a panic
   and a normal early-setup `Err` funnel into the same fallback, which
   parks the main thread forever instead of exiting when PID 1 (still
   needs a reset to recover, but that's the same recovery model already
   in use for this whole testing phase — see below — not a new risk this
   adds). In dev/test mode this is invisible: exits normally, same as
   before. Verified in dev mode for both the `Err` path (bad
   `--config-dir`: exits 1, doesn't hang) and, with a temporary forced
   `panic!()` plus a temporary `is_pid1()` override to actually exercise
   the hang branch without needing to be real PID 1: confirmed apollod
   holds instead of exiting, then reverted both changes.
3. ~~Early mounts~~ (implemented, unverified against a real kernel) —
   `/proc`, `/sys`, devtmpfs on `/dev`, `/dev/pts`, `/dev/shm`, `/run`
   (tmpfs), and cgroup2 (unified) on `/sys/fs/cgroup` — most modern
   daemons, including udev, assume the last one is already there. Then
   `mount -a` + `swapon -a` against `/etc/fstab` (shell out to
   util-linux's own binaries rather than reimplementing fstab parsing),
   and set the hostname from `/etc/hostname`. A failure on any one step
   logs and moves on rather than aborting the rest of boot. Gated on
   `is_pid1()`, so this has only been exercised as a no-op so far — real
   verification happens booting the Fedora VM (see Testing below).
4. ~~Getty~~ (done) — `examples/getty/getty-tty1.toml` and
   `getty-serial.toml`, plain `restart = "always"` units, no dedicated
   code needed. Verified in dev mode: apollod starts both against real
   `/dev/tty1`/`/dev/ttyS0` device nodes, they sit correctly blocked
   waiting for a login (confirmed `agetty` itself works stand-alone with
   a hard timeout first), and restart/stop behave exactly like any other
   long-running unit. What's *not* verified outside a real boot is the
   actual interactive login flow end-to-end — that's for the Fedora VM.
5. ~~`apolloctl reboot` / `poweroff` / `halt`~~ (done) — see
   [Shutdown](#shutdown) above for the full sequence. On Fedora,
   `reboot`/`poweroff`/`halt`/`shutdown` are symlinks to `systemctl`,
   which needs systemd-PID1's D-Bus socket — none of that exists once
   apollo is PID 1, so without this there would be no clean shutdown path
   for the VM at all. Also closes out step 2's remaining item: apollod's
   own SIGTERM/SIGINT are now handled (reaper.rs). Found and fixed two
   real bugs building this — worth recording since they're the kind of
   thing that only shows up under actual testing, not code review:
   forked units silently ignored SIGTERM entirely (they inherit apollod's
   own blocked signal mask across `fork()`, since `exec()` doesn't reset
   it — fixed with a `pre_exec` hook), and the reaper thread was
   `return`ing right after dispatching a signal-triggered shutdown,
   which meant nothing was left reaping `SIGCHLD` for the rest of the
   sequence — every unit stopped during that window sat as an unreaped
   zombie instead of being detected as exited.
6. **Process-group-based `stop`.** Currently only the immediate pid
   apollo spawned is signalled; anything that double-forks/daemonizes
   escapes tracking entirely. `setsid()` each unit's child and signal its
   whole process group (`kill(-pgid, ...)`) instead of just the one pid.
   A cgroup-per-unit approach (kill via `cgroup.kill`) is the more robust
   long-term fix and falls out naturally once cgroup2 is mounted (step 3).
7. **udev.** devtmpfs alone gives raw device nodes but not permissions,
   persistent `/dev/disk/by-uuid` symlinks, or hotplug handling. Start
   `systemd-udevd` as a unit (it runs fine under a non-systemd PID 1) and
   run `udevadm trigger --action=add && udevadm settle` for the initial
   coldplug.
8. **D-Bus + NetworkManager**, for a networked (SSH-reachable) VM:
   units for `dbus-broker` (or `dbus-daemon`) and `NetworkManager`,
   `after = ["dbus"]`.
9. **Log capture.** Redirect each unit's stdout/stderr to
   `/var/log/apollo/<name>.log` instead of inheriting apollod's own —
   needed for debugging boot issues on a console you can't scroll back.
10. Parallel startup within a dependency level.

**Explicitly deferred:** SELinux policy loading. Fedora runs enforcing by
default; getting policy load right at the correct point in early boot
(before anything creates mislabeled files) is a substantial chunk of work
on its own. Test with `enforcing=0` on the kernel cmdline (or
`/etc/selinux/config` set to permissive) rather than blocking on this.

### Testing safely on a real distro

Don't replace `/sbin/init`. Test via a one-time GRUB edit adding
`init=/path/to/apollod` to the kernel command line — that boots apollo
for that session only, and a normal reboot goes back to systemd. This is
the standard way alt-init projects (runit, OpenRC, etc.) get tested: a
bug that hangs boot just means resetting the VM, not reinstalling it —
whether the hang is the fail-safe above holding on purpose, or an actual
kernel panic from something SIGKILL bypassed, the recovery is the same
either way.

Steps, for the Fedora VM:

1. **Build on the VM itself**, not by copying a binary built elsewhere —
   a binary linked against this dev machine's glibc isn't guaranteed to
   run against Fedora's. `git clone`/copy the repo in, install Rust
   (`rustup` or `dnf install cargo`), `cargo build --release`.
2. **SELinux**: boot Fedora normally first and either set
   `/etc/selinux/config` to `permissive`, or plan to add `enforcing=0` on
   the kernel cmdline in step 4 — SELinux policy loading isn't
   implemented (see Explicitly deferred, above) and enforcing mode will
   just get in the way of evaluating apollo itself.
3. **Install unit files**: `sudo mkdir -p /etc/apollo/services` and copy
   in `examples/getty/getty-tty1.toml` (graphical/framebuffer console —
   e.g. virt-manager's default "Console" view) and/or `getty-serial.toml`
   (serial console — e.g. QEMU `-serial mon:stdio`, or virt-manager with
   a serial device added and set to a text console). Either or both;
   whichever matches how you'll actually be watching the VM. Start with
   *just* these — no other services — to keep the first boot's variables
   down to apollod itself.
4. **Boot**: at the GRUB menu, press `e` on the normal boot entry, find
   the line starting `linux`, and append `init=/path/to/apollod` (and
   `enforcing=0` if you didn't set permissive mode already) — then
   `Ctrl-X`/F10 to boot *that edit only*, not save it.
5. **Watch**: apollod's own log lines and the getty login prompt share
   the same console right now (see Getty, above) — expect them
   interleaved, that's cosmetic. Log in, and from that shell,
   `apolloctl list`/`status` should work against the real default socket
   (`/run/apollo/control.sock`) with no `--socket` override needed.
   **No networking will be up** (no dbus/NetworkManager units yet — step
   8), so this is console-only; SSH won't reach it.
6. **Shut down**: `apolloctl poweroff` (or `reboot`, or `halt`) from that
   shell. A `reboot` is safe to try without complication — the GRUB edit
   from step 4 was one-time, so the *next* boot goes back to systemd
   automatically, not back into apollo.

## Tested On
| Distro     | Tester             | Status        |
|------------|--------------------|---------------|
| Void Linux | Linux User Lucario | WORKING!      |
| Fedora     | Linux User Lucario | Does not boot |

## License

GNU GPL v3 (see [LICENSE.md](LICENSE.md) for full license)
