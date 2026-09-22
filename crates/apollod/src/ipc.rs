//! The control socket: one `apolloctl` connection in, one request read, one
//! response written, connection closed. Simple request/response, no
//! persistent sessions.

use crate::supervisor::Event;
use anyhow::Context;
use apollo_proto::Request;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

pub fn serve(socket_path: &Path, events_tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding control socket {}", socket_path.display()))?;

    // Every request on this socket is trusted and executed unconditionally
    // (start/stop/restart a unit, reboot/poweroff/halt the machine) — there's
    // no per-request auth. Without restricting the socket's own permissions,
    // any local user could connect and control apollod. `UnixListener::bind`
    // creates it with umask-derived (typically world-connectable)
    // permissions, so lock it down to owner-only right after binding.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", socket_path.display()))?;

    eprintln!("apollod: listening on {}", socket_path.display());

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("apollod: accept error: {e}");
                continue;
            }
        };
        let tx = events_tx.clone();
        thread::spawn(move || {
            if let Err(e) = handle_conn(stream, tx) {
                eprintln!("apollod: connection error: {e:#}");
            }
        });
    }
    Ok(())
}

fn handle_conn(mut stream: UnixStream, events_tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
    let req: Request = apollo_proto::read_message(&mut stream).context("reading request")?;

    let (resp_tx, resp_rx) = mpsc::channel();
    events_tx
        .send(Event::Command { req, resp_tx })
        .map_err(|_| anyhow::anyhow!("supervisor loop is no longer running"))?;
    let resp = resp_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("supervisor loop dropped the response channel"))?;

    apollo_proto::write_message(&mut stream, &resp).context("writing response")?;
    Ok(())
}
