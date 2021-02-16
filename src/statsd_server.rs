use bytes::{BufMut, BytesMut};
use memchr::memchr;
use statsdproto::statsd::StatsdPDU;
use stream_cancel::Tripwire;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::time::timeout;

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn, debug};

use crate::backends::Backends;
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
                StatsdPDU::new(incoming.freeze()).map(|f| ret.push(f));
            }
        };
    }
    return ret;
}

async fn client_handler(
    stats: stats::Scope,
    mut tripwire: Tripwire,
    mut socket: TcpStream,
    backends: Backends,
) {
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
                debug!(
                    "closing reader (empty buffer, eof) {:?}",
                    socket.peer_addr()
                );
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
                debug!("closing reader {:?}", socket.peer_addr());
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
                debug!("read timeout, closing {:?}", socket.peer_addr());
                break;
            }
            Err(e) => {
                debug!("socket error {:?} from {:?}", e, socket.peer_addr());
                break;
            }
        }
    }
    disconnects.inc();
}

pub async fn run(stats: stats::Scope, tripwire: Tripwire, bind: String, backends: Backends) {
    //self.shutdown_trigger = Some(trigger);
    let listener = TcpListener::bind(bind.as_str()).await.unwrap();
    let mut udp = UdpServer::new();
    let bind_clone = bind.clone();
    let udp_join = udp.udp_worker(stats.scope("udp"), bind_clone, backends.clone());
    info!("statsd tcp server running on {}", bind);

    let accept_connections = stats.counter("accepts").unwrap();
    let accept_failures = stats.counter("accept_failures").unwrap();

    async move {
        loop {
            select! {
                _ = tripwire.clone() => {
                    info!("stopped tcp listener loop");
                    return
                }
                socket_res = listener.accept() => {

                match socket_res {
                    Ok((socket, _)) => {
                        debug!("accepted connection from {:?}", socket.peer_addr());
                        accept_connections.inc();
                        tokio::spawn(client_handler(stats.scope("connections"), tripwire.clone(), socket, backends.clone()));
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
}
