use bytes::{BufMut, Bytes, BytesMut};
use memchr::memchr;
use stream_cancel::{Trigger, Tripwire};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use std::sync::Arc;
use std::time::Duration;

use crate::stats;
use crate::statsd_proto::Pdu;

use log::{info, warn};

pub struct StatsdClient {
    sender: mpsc::Sender<Pdu>,
    inner: Arc<StatsdClientInner>,
}

struct StatsdClientInner {
    endpoint: String,
    sender: mpsc::Sender<Pdu>,
    _trig: Trigger,
}

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_DELAY: Duration = Duration::from_millis(500);
const SEND_THRESHOLD: usize = 10 * 1024;
const INITIAL_BUF_CAPACITY: usize = SEND_THRESHOLD + 1024;

impl StatsdClient {
    pub fn new(stats: stats::Scope, endpoint: &str, channel_buffer: usize) -> Self {
        // Currently, we need this tripwire to abort connection looping. This can probably be refactored
        let (trig, trip) = Tripwire::new();
        let (sender, recv) = mpsc::channel::<Pdu>(channel_buffer);
        let inner = StatsdClientInner {
            endpoint: endpoint.to_string(),
            sender: sender.clone(),
            _trig: trig,
        };
        let eps = String::from(endpoint);
        let (ticker_sender, ticker_recv) = mpsc::channel::<bool>(1);
        tokio::spawn(ticker(eps.clone(), ticker_sender));
        tokio::spawn(client_task(stats, eps, trip, recv, ticker_recv));
        StatsdClient {
            inner: Arc::new(inner),
            sender,
        }
    }

    pub fn sender(&self) -> mpsc::Sender<Pdu> {
        self.sender.clone()
    }

    pub fn endpoint(&self) -> &str {
        self.inner.endpoint.as_str()
    }
}

impl Clone for StatsdClient {
    fn clone(&self) -> Self {
        StatsdClient {
            inner: self.inner.clone(),
            sender: self.inner.sender.clone(),
        }
    }
}

/// Repeatedly try to form a connection to and endpoint with backoff. If the
/// tripwire is set, this function will then abort and return none.
async fn form_connection(
    stats: stats::Scope,
    endpoint: &str,
    mut connect_tripwire: Tripwire,
) -> Option<TcpStream> {
    let connections_made = stats.counter("connections_made").unwrap();
    let connections_failed = stats.counter("connections_failed").unwrap();
    loop {
        let connect_attempt = timeout(CONNECT_TIMEOUT, TcpStream::connect(endpoint));

        let stream = match select!(
            connect = connect_attempt => connect,
            _ = (&mut connect_tripwire) => {
                return None;
            },
        ) {
            Err(_e) => {
                warn!("connect timeout to {:?}", endpoint);
                connections_failed.inc();
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            Ok(Err(e)) => {
                warn!("connect error to {:?} error {:?}", endpoint, e);
                connections_failed.inc();
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            Ok(Ok(s)) => {
                info!("statsd client connect {:?}", endpoint);
                s
            }
        };
        connections_made.inc();
        return Some(stream);
    }
}

// Since statsd has no notion of when a message is actually received, we have to
// assume a buffer write is incomplete and just drop it here. This simply
// advances to the next newline in the buffer if found.
fn trim_to_next_newline(buf: &mut Bytes) {
    match memchr(b'\n', buf) {
        None => (),
        Some(pos) => {
            let _b = buf.split_to(pos + 1);
        }
    }
}

async fn client_sender(
    stats: stats::Scope,
    endpoint: String,
    connect_tripwire: Tripwire,
    mut recv: mpsc::Receiver<bytes::Bytes>,
) {
    let bytes_sent = stats.counter("bytes_sent").unwrap();
    let connections_aborted = stats.counter("connections_aborted").unwrap();

    let first_connect_tripwire = connect_tripwire.clone();
    let mut lazy_connect: Option<TcpStream> =
        form_connection(stats.clone(), endpoint.as_str(), first_connect_tripwire).await;

    loop {
        let mut buf = match recv.recv().await {
            None => {
                info!("sender task {} exiting", endpoint);
                return;
            }
            Some(p) => p,
        };
        loop {
            if buf.is_empty() {
                break;
            }
            let connect = match lazy_connect.as_mut() {
                None => {
                    let reconnect_tripwire = connect_tripwire.clone();
                    lazy_connect =
                        form_connection(stats.clone(), endpoint.as_str(), reconnect_tripwire).await;
                    if lazy_connect.is_none() {
                        // Early check to see if the tripwire is set and bail
                        info!("sender task {} exiting", endpoint);
                        return;
                    }
                    lazy_connect.as_mut().unwrap()
                }
                Some(c) => c,
            };
            // Write the buffer until success
            let result = connect.write_buf(&mut buf).await;
            match result {
                Ok(0) if !buf.is_empty() => {
                    // Write 0 error, abort the connection and try again
                    lazy_connect = None;
                    trim_to_next_newline(&mut buf);
                    connections_aborted.inc();
                    continue;
                }
                Ok(bytes) if buf.is_empty() => {
                    bytes_sent.inc_by(bytes as f64);
                    drop(buf);
                    break;
                }
                Ok(bytes) => {
                    bytes_sent.inc_by(bytes as f64);
                    continue;
                }
                Err(e) => {
                    warn!(
                        "write error {} - {:?}, reforming a connection with this buffer",
                        endpoint, e
                    );
                    trim_to_next_newline(&mut buf);
                    lazy_connect = None;
                    connections_aborted.inc();
                    continue;
                }
            };
        }
    }
}

///
/// Ticker is responsible for making sure the statsd channel emits a payload at
/// a particular rate (allowing for write combining). Due to an issue with
/// non-async mpsc try_send being used to trigger the primary sender queue, the
/// ticker is needed as opposed to a timeout() wrapper over a queue.recv, which
/// does not reliably get woken by try_send. The upside of this we also form one
/// less short lived timer, not that its really a major advantage.
async fn ticker(endpoint: String, sender: mpsc::Sender<bool>) {
    loop {
        sleep(SEND_DELAY).await;
        if sender.send(true).await.is_err() {
            info!("ticker task {} exiting", endpoint);
            return;
        }
    }
}

async fn client_task(
    stats: stats::Scope,
    endpoint: String,
    connect_tripwire: Tripwire,
    mut recv: mpsc::Receiver<Pdu>,
    mut ticker_recv: mpsc::Receiver<bool>,
) {
    let backoff_send = stats.counter("send_backoff").unwrap();
    let delayed_sends = stats.counter("delayed_sends").unwrap();
    let messages_queued = stats.counter("messages_queued").unwrap();

    let mut buf = BytesMut::with_capacity(INITIAL_BUF_CAPACITY);
    let (buf_sender, buf_recv) = mpsc::channel(10);
    tokio::spawn(client_sender(
        stats,
        endpoint.clone(),
        connect_tripwire,
        buf_recv,
    ));

    loop {
        let (pdu, timeout) = select! {
            p = recv.recv() => (p, false),
            _ = ticker_recv.recv() => (None, true),
        };

        match (pdu, timeout) {
            (Some(pdu), _) => {
                let pdu_bytes = pdu.as_bytes();
                if buf.remaining_mut() < pdu_bytes.len() {
                    buf.reserve(pdu_bytes.len() + 10);
                }
                buf.put(pdu_bytes);
                buf.put(b"\n".as_ref());
                messages_queued.inc();
                if buf.len() < SEND_THRESHOLD {
                    backoff_send.inc();
                    // Do not send now
                    continue;
                }
            }
            (None, false) => {
                if buf.is_empty() {
                    // No more queue, no more bytes, exit
                    info!("client task {} exiting", endpoint);
                    return;
                }
            }
            (None, true) if buf.is_empty() => {
                continue;
            }
            (None, true) => {
                delayed_sends.inc();
                // Timeout! Just go ahead and send whats in the buf now
            }
        };
        if buf_sender.send(buf.freeze()).await.is_err() {
            info!("client task {} exiting", endpoint);
            return;
        }
        buf = BytesMut::with_capacity(INITIAL_BUF_CAPACITY);
    }
}
