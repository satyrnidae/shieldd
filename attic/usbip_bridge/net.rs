//! USB/IP TCP plumbing. The threads here only move bytes; all protocol state
//! lives on the main thread, which gets everything through one channel.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::Msg;

/// Accepts USB/IP clients until `quit` is set.
pub fn listen(addr: SocketAddr, tx: Sender<Msg>, quit: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    listener.set_nonblocking(true)?;
    let handle = std::thread::Builder::new().name("usbip-accept".into()).spawn(move || {
        let mut next_id = 0u64;
        while !quit.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    next_id += 1;
                    if let Err(e) = start_reader(next_id, stream, peer, &tx) {
                        warn!("client {peer}: {e:#}");
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    warn!("accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    })?;
    Ok(handle)
}

fn start_reader(id: u64, stream: TcpStream, peer: SocketAddr, tx: &Sender<Msg>) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    let mut reader = stream.try_clone()?;
    let tx2 = tx.clone();
    std::thread::Builder::new().name(format!("usbip-rx-{id}")).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx2.send(Msg::Data(id, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx2.send(Msg::Closed(id));
    })?;
    debug!(id, %peer, "client connected");
    let _ = tx.send(Msg::Accepted(id, Client { id, stream, buf: Vec::new(), imported: false }));
    Ok(())
}

/// One USB/IP connection, owned by the main thread.
pub struct Client {
    pub id: u64,
    stream: TcpStream,
    /// Bytes received but not parsed yet
    pub buf: Vec<u8>,
    pub imported: bool,
}

impl Client {
    pub fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Unblocks the reader thread, which then exits on its own
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
