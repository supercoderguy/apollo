# apollo

A Linux init system, written in Rust.

## Scope and design decisions

apollo is being built as a **minimal PID 1**, not a full systemd
replacement: process supervision, service start/stop/restart, and basic
dependency ordering — not socket activation, cgroup resource control, or a
built-in logging daemon. It's a clean-slate design; it does not aim for
compatibility with systemd `.service` files. Service definitions are TOML.

The current milestone is the **daemon/CLI split**: `apollod` (the
supervisor) and `apolloctl` (its control client) talking over a Unix
socket, developed and tested as an ordinary process rather than as PID 1.
Actually booting a system — mounting `/proc`, `/sys`, `/dev`, reaping
reparented orphans, running as PID 1 itself — is future work; see
[Roadmap](#roadmap).

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
listener, per-connection handler threads, and the per-child "wait for exit"
threads — only ever communicates with it by sending an `Event` over an
`mpsc` channel and (for commands) waiting on a reply channel. This avoids
locking in the core logic entirely; see `crates/apollod/src/supervisor.rs`.

Concretely:
- **`config.rs`** — loads `*.toml` unit files from a directory, and
  topologically sorts them by their `after` field to decide start order.
  An `after` name that doesn't match a loaded unit (e.g. a not-yet-modeled
  `network.target`) is treated as already satisfied.
- **`registry.rs`** — `UnitRuntime`, the state kept for one loaded unit
  (parsed config + current state + pid + restart bookkeeping).
- **`supervisor.rs`** — the event loop and all state transitions.
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
  them. Reaping reparented orphans is real-PID-1 behavior that hasn't been
  built yet (see Roadmap). Kill test services manually when experimenting.
- No graceful shutdown sequencing (stop units in reverse dependency order)
  yet — there's no handling of `apollod`'s own SIGTERM/SIGINT at all.
- No filesystem mounting, no real PID 1 responsibilities (reaping
  arbitrary reparented children via a SIGCHLD-driven `waitpid` loop,
  rather than one waiter thread per known child).
- `after` is the only dependency relation; no targets, no `wants`/
  `requires` distinction, no parallelism control beyond what the
  topological order allows (units are currently started serially, in
  order — not fanned out in parallel per dependency level).
- No log capture — child stdout/stderr currently just inherit apollod's
  own, as seen in the examples above.

## Roadmap

1. ~~Daemon/CLI split with a control socket~~ (this milestone)
2. Graceful shutdown: SIGTERM to `apollod` stops units in reverse
   dependency order before exiting.
3. Real PID 1 behavior: mount essential filesystems, reap arbitrary
   reparented orphans via `SIGCHLD` + a `waitpid(-1, WNOHANG)` loop
   instead of one thread per known child, boot to a login shell in a VM.
4. Parallel startup within a dependency level.
5. Structured logging / log capture per unit.

## License

MIT
