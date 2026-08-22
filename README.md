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
distro](#testing-safely-on-a-real-distro). Getty and shutdown handling
are still future work; see [Roadmap](#roadmap).

## Layout

```
crates/
  apollo-proto/   shared IPC types and wire protocol (Request/Response, framing)
  apollod/        the supervisor daemon
  apolloctl/      the CLI control client
examples/services/ sample unit files for local testing
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
  a pid belongs to.
- **`reaper.rs`** — real PID-1-style reaping: `SIGCHLD` is blocked on the
  main thread before anything else happens (so all later threads inherit
  it blocked), and a dedicated thread synchronously `sigwait()`s for it,
  draining every ready exit with `waitpid(-1, WNOHANG)` on each wakeup.
  This reaps *every* child that ends up parented to apollod — not just
  units it started — which is what makes it correct to run as actual
  PID 1, where any orphaned process on the system can be reparented here.
- **`mounts.rs`** — early-boot filesystem setup: `/proc`, `/sys`,
  devtmpfs on `/dev`, `/dev/pts`, `/dev/shm`, `/run` (tmpfs), cgroup2 on
  `/sys/fs/cgroup`, then `mount -a` + `swapon -a` against `/etc/fstab`
  and setting the hostname from `/etc/hostname`. Gated in `main.rs` on
  `is_pid1()` (`getpid() == 1`) — a no-op on every dev/test invocation,
  since none of it makes sense unless apollod actually is the init
  process for this boot.
- **`ipc.rs`** — accepts connections on the control socket; each
  connection is one `Request` in, one `Response` out.
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
stop/start/restart/status).

## Current limitations

These are expected at this milestone, not bugs:

- **Stopping `apollod` does not stop its supervised children.** They're
  independent processes once spawned; killing the daemon just orphans
  them (reparented to whatever the system's real PID 1 is, and reaped by
  *that*, not by apollod — apollod isn't PID 1 yet in this dev-testing
  setup). Graceful shutdown sequencing hasn't been built yet (Roadmap
  step 5); no handling of `apollod`'s own SIGTERM/SIGINT at all yet. Kill
  test services manually when experimenting.
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
  own, as seen in the examples above.

## Roadmap

Ordered toward the concrete goal of booting a real distro (Fedora, in a
VM) to a usable login prompt and shutting it down cleanly:

1. ~~Daemon/CLI split with a control socket~~ (done)
2. ~~Real PID 1 reaping~~ (done) — `SIGCHLD` blocked on the main thread
   before any other thread or unit process exists, a dedicated thread
   `sigwait()`s on it, and each wakeup drains every ready exit with
   `waitpid(-1, WNOHANG)` (signals don't queue 1:1, so one wakeup can mean
   several exits). Reaps *any* child parented to apollod, known unit or
   not. Still open from this step: apollod doesn't yet guard against
   panics unwinding out of a thread, and there's no handling yet of
   apollod's own termination signals (SIGTERM/SIGINT/Ctrl-Alt-Del) — that
   lands with shutdown handling in step 5.
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
4. **Getty**, so booting actually produces a login prompt: a unit
   running `agetty` on `tty1` (and a serial console for headless VM
   testing), `restart = "always"`.
5. **`apolloctl reboot` / `poweroff` / `halt`**, with apollod stopping
   units in reverse dependency order, unmounting, syncing, then calling
   `reboot(2)` directly. On Fedora, `reboot`/`poweroff`/`halt`/`shutdown`
   are symlinks to `systemctl`, which needs systemd-PID1's D-Bus socket —
   none of that exists once apollo is PID 1, so without this there is no
   clean shutdown path for the VM at all.
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
bug that hangs boot just means resetting the VM, not reinstalling it.

## License

MIT
