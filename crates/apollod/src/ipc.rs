//! The control socket: one `apolloctl` connection in, one request read, one
//! response written, connection closed. Simple request/response, no
//! persistent sessions.

use crate::supervisor::Event;
use anyhow::Context;
use apollo_proto::Request;
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
