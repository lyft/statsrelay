use bytes::{BufMut, BytesMut};
use memchr::memchr;
use statsdproto::statsd::StatsdPDU;
use stream_cancel::Tripwire;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::select;
use tokio::time::timeout;

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};

use crate::backends::Backends;
use crate::config::StatsdServerConfig;
use crate::stats;

const TCP_READ_TIMEOUT: Duration = Duration::from_secs(62);
const READ_BUFFER: usize = 8192;

struct UdpServer {
    shutdown_gate: Arc<AtomicBool>,
}

impl Drop for UdpServer {
    fn drop(&mut self) {
        self.shutdown_gate.store(true, Relaxed);
    }
}

impl UdpServer {
    fn new() -> Self {
        UdpServer {
            shutdown_gate: Arc::new(AtomicBool::new(false)),
        }
    }

    fn udp_worker(
        &mut self,
        stats: stats::Scope,
        bind: String,
        backends: Backends,
    ) -> std::thread::JoinHandle<()> {
        let socket = UdpSocket::bind(bind.as_str()).unwrap();

        let processed_lines = stats.counter("processed_lines").unwrap();
        let incoming_bytes = stats.counter("incoming_bytes").unwrap();
        // We set a small timeout to allow aborting the UDP server if there is no
        // incoming traffic.
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        info!("statsd udp server running on {}", bind);
        let gate = self.shutdown_gate.clone();
        std::thread::spawn(move || {
            loop {
                if gate.load(Relaxed) {
                    break;
                }
                let mut buf = BytesMut::with_capacity(65535);

                match socket.recv_from(&mut buf[..]) {
                    Ok((size, _remote)) => {
                        incoming_bytes.inc_by(size as f64);
                        let mut r = process_buffer_newlines(&mut buf);
                        processed_lines.inc_by(r.len() as f64);
                        for p in r.drain(..) {
                            backends.provide_statsd_pdu(p);
                        }
                        match StatsdPDU::new(buf.clone().freeze()) {
                            Some(p) => backends.provide_statsd_pdu(p),
                            None => (),
                        }
                        buf.clear();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                    Err(e) => warn!("udp receiver error {:?}", e),
                }
            }
            info!("terminating statsd udp");
        })
    }
}

fn process_buffer_newlines(buf: &mut BytesMut) -> Vec<StatsdPDU> {
    let mut ret: Vec<StatsdPDU> = Vec::new();
    loop {
        match memchr(b'\n', &buf) {
            None => break,
            Some(newline) => {
                let mut incoming = buf.split_to(newline + 1);
                if incoming[incoming.len() - 2] == b'\r' {
                    incoming.truncate(incoming.len() - 2);
                } else {
                    incoming.truncate(incoming.len() - 1);
                }
                let frozen = incoming.freeze();
                if frozen == "status" {
                    // Consume a line consisting of just the word status, and do not produce a PDU
                    continue;
                }
                StatsdPDU::new(frozen).map(|f| ret.push(f));
            }
        };
    }
    return ret;
}

async fn client_handler<T>(
    stats: stats::Scope,
    peer: String,
    mut tripwire: Tripwire,
    mut socket: T,
    backends: Backends,
) where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BytesMut::with_capacity(READ_BUFFER);
    let incoming_bytes = stats.counter("incoming_bytes").unwrap();
    let disconnects = stats.counter("disconnects").unwrap();
    let processed_lines = stats.counter("lines").unwrap();

    loop {
        if buf.remaining_mut() < READ_BUFFER {
            buf.reserve(READ_BUFFER);
        }
        let result = select! {
            r = timeout(TCP_READ_TIMEOUT, socket.read_buf(&mut buf)) => {
                match r {
                    Err(_e)  => Err(std::io::Error::new(ErrorKind::TimedOut, "read timeout")),
                    Ok(Err(e)) => Err(e),
                    Ok(Ok(r)) => Ok(r),
                }
            },
            _ = &mut tripwire => Err(std::io::Error::new(ErrorKind::Other, "shutting down")),
        };

        match result {
            Ok(bytes) if buf.is_empty() && bytes == 0 => {
                debug!("closing reader (empty buffer, eof) {}", peer);
                break;
            }
            Ok(bytes) if bytes == 0 => {
                let mut r = process_buffer_newlines(&mut buf);
                processed_lines.inc_by(r.len() as f64);

                for p in r.drain(..) {
                    backends.provide_statsd_pdu(p);
                }
                let remaining = buf.clone().freeze();
                match StatsdPDU::new(remaining) {
                    Some(p) => {
                        backends.provide_statsd_pdu(p);
                        ()
                    }
                    None => (),
                };
                debug!("remaining {:?}", buf);
                debug!("closing reader {}", peer);
                break;
            }
            Ok(bytes) => {
                incoming_bytes.inc_by(bytes as f64);

                let mut r = process_buffer_newlines(&mut buf);
                processed_lines.inc_by(r.len() as f64);
                for p in r.drain(..) {
                    backends.provide_statsd_pdu(p);
                }
            }
            Err(e) if e.kind() == ErrorKind::Other => {
                // Ignoring the results of the write call here
                let _ = timeout(
                    Duration::from_secs(1),
                    socket.write_all(b"server closing due to shutdown, goodbye\n"),
                )
                .await;
                break;
            }
            Err(e) if e.kind() == ErrorKind::TimedOut => {
                debug!("read timeout, closing {}", peer);
                break;
            }
            Err(e) => {
                debug!("socket error {:?} {}", e, peer);
                break;
            }
        }
    }
    disconnects.inc();
}

/// Wrapper type to adapt an optional listener and either return an accept
/// future, or a pending future which never returns. This wrapper is needed to
/// work around that .accept() is an opaque impl Future type, so can't be
/// readily mixed into a stream.
async fn optional_accept(
    listener: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, unix::SocketAddr)> {
    if let Some(listener) = listener {
        listener.accept().await
    } else {
        futures::future::pending().await
    }
}

pub async fn run(
    stats: stats::Scope,
    tripwire: Tripwire,
    config: StatsdServerConfig,
    backends: Backends,
) {
    let tcp_listener = TcpListener::bind(config.bind.as_str()).await.unwrap();
    info!("statsd tcp server running on {}", config.bind);

    let unix_listener = config.socket.as_ref().map(|socket| {
        let unix = UnixListener::bind(socket.as_str()).unwrap();
        info!("statsd unix server running on {}", socket);
        unix
    });

    // Spawn the threaded, non-async blocking UDP server
    let mut udp = UdpServer::new();
    let udp_join = udp.udp_worker(stats.scope("udp"), config.bind.clone(), backends.clone());

    let accept_connections = stats.counter("accepts").unwrap();
    let accept_connections_unix = stats.counter("accepts_unix").unwrap();
    let accept_failures = stats.counter("accept_failures").unwrap();
    let accept_failures_unix = stats.counter("accept_failures_unix").unwrap();

    async move {
        loop {
            select! {
                _ = tripwire.clone() => {
                    info!("stopped stream listener loop");
                    return
                }
                // Wrap the unix acceptor for different stats
                unix_res = optional_accept(unix_listener.as_ref()) => {
                    match unix_res {
                        Ok((socket,_)) => {
                            let peer_addr = format!("{:?}", socket.peer_addr());
                            debug!("accepted unix connection from {:?}", socket.peer_addr());
                            accept_connections_unix.inc();
                            tokio::spawn(client_handler(stats.scope("connections_unix"), peer_addr, tripwire.clone(), socket, backends.clone()));
                        }
                        Err(err) => {
                            accept_failures_unix.inc();
                            info!("unix accept error = {:?}", err);
                        }
                    }
                }
                socket_res = tcp_listener.accept() => {

                    match socket_res {
                        Ok((socket,_)) => {
                            let peer_addr = format!("{:?}", socket.peer_addr());
                            debug!("accepted connection from {:?}", socket.peer_addr());
                            accept_connections.inc();
                            tokio::spawn(client_handler(stats.scope("connections"), peer_addr, tripwire.clone(), socket, backends.clone()));
                        }
                        Err(err) => {
                            accept_failures.inc();
                            info!("accept error = {:?}", err);
                        }
                    }
                }
            }
        }
    }
    .await;
    drop(udp);
    // The socket file descriptor is not removed on teardown. Lets remove it if enabled.
    if let Some(socket) = config.socket.as_ref() {
        let _ = std::fs::remove_file(socket);
    }
    tokio::task::spawn_blocking(move || {
        udp_join.join().unwrap();
    })
    .await
    .unwrap();
}

#[cfg(test)]
pub mod test {
    use super::*;
    #[test]
    fn test_process_buffer_no_newlines() {
        let mut b = BytesMut::new();
        // Validate we don't consume non-newlines
        b.put_slice(b"hello");
        let r = process_buffer_newlines(&mut b);
        assert!(r.len() == 0);
        assert!(b.split().as_ref() == b"hello");
    }

    #[test]
    fn test_process_buffer_newlines() {
        let mut b = BytesMut::new();
        // Validate we don't consume newlines, but not a remnant
        b.put_slice(b"hello:1|c\nhello:1|c\nhello2");
        let r = process_buffer_newlines(&mut b);
        assert!(r.len() == 2);
        assert!(b.split().as_ref() == b"hello2");
    }

    #[test]
    fn test_process_buffer_cr_newlines() {
        let mut found = 0;
        let mut b = BytesMut::new();
        // Validate we don't consume newlines, but not a remnant
        b.put_slice(b"hello:1|c\r\nhello:1|c\nhello2");
        let r = process_buffer_newlines(&mut b);
        for w in r {
            assert!(w.pdu_type() == b"c");
            assert!(w.name() == b"hello");
            found += 1
        }
        assert_eq!(2, found);
        assert!(b.split().as_ref() == b"hello2");
    }

    #[test]
    fn test_process_buffer_status() {
        let mut found = 0;
        let mut b = BytesMut::new();
        // Validate we don't consume newlines, but not a remnant
        b.put_slice(b"status\r\nhello:1|c\nhello2");
        let r = process_buffer_newlines(&mut b);
        for w in r {
            assert!(w.pdu_type() == b"c");
            assert!(w.name() == b"hello");
            found += 1
        }
        assert_eq!(1, found);
        assert!(b.split().as_ref() == b"hello2");
    }
}
