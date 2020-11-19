use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::RwLock;
use regex::bytes::RegexSet;

use crate::config::StatsdDuplicateTo;
use crate::shard::{statsrelay_compat_hash, Ring};
use crate::statsd::StatsdPDU;
use crate::statsd_client::StatsdClient;

use log::warn;

struct StatsdBackend {
    conf: StatsdDuplicateTo,
    ring: RwLock<Ring<StatsdClient>>,
    input_filter: Option<RegexSet>,
    warning_log: AtomicU64,
}

impl StatsdBackend {
    fn new(conf: &StatsdDuplicateTo) -> anyhow::Result<Self> {
        let mut filters: Vec<String> = Vec::new();

        // This is ugly, sorry
        if conf.input_blacklist.is_some() {
            filters.push(conf.input_blacklist.as_ref().unwrap().clone());
        }
        if conf.input_filter.is_some() {
            filters.push(conf.input_filter.as_ref().unwrap().clone());
        }
        let input_filter = if filters.len() > 0 {
            Some(RegexSet::new(filters).unwrap())
        } else {
            None
        };

        let mut ring: Ring<StatsdClient> = Ring::new();

        // Use the same backend for the same endpoint address, caching the lookup locally
        let mut memoize: HashMap<String, StatsdClient> = HashMap::new();
        for endpoint in &conf.shard_map {
            if let Some(client) = memoize.get(endpoint) {
                ring.push(client.clone())
            } else {
                let client = StatsdClient::new(endpoint.as_str(), 100000);
                memoize.insert(endpoint.clone(), client.clone());
                ring.push(client);
            }
        }

        let backend = StatsdBackend {
            conf: conf.clone(),
            ring: RwLock::new(ring),
            input_filter: input_filter,
            warning_log: AtomicU64::new(0),
        };

        Ok(backend)
    }

    fn reload_backends(&mut self, shard_map: Vec<String>) -> anyhow::Result<()> {
        let mut new_ring: Ring<StatsdClient> = Ring::new();
        let mut memoize: HashMap<String, StatsdClient> = HashMap::new();

        // Capture the old ring contents into a memoization map by endpoint,
        // letting us re-use any old client connections and buffers. Note we
        // won't start tearing down connections until the memoization buffer and
        // old ring are both dropped.
        {
            let ring_read = self.ring.read();
            for i in 0..self.ring.read().len() {
                let client = ring_read.pick_from(i as u32);
                memoize.insert(String::from(client.endpoint()), client.clone());
            }
        }
        for shard in shard_map {
            if let Some(client) = memoize.get(&shard) {
                new_ring.push(client.clone());
            } else {
                new_ring.push(StatsdClient::new(shard.as_str(), 100000));
            }
        }
        self.ring.write().swap(new_ring);
        Ok(())
    }

    fn provide_statsd_pdu(&self, pdu: &StatsdPDU) {
        if !self
            .input_filter
            .as_ref()
            .map_or(true, |inf| inf.is_match(pdu.name()))
        {
            return;
        }

        let ring_read = self.ring.read();
        let code = match ring_read.len() {
            0 => return, // In case of nothing to send, do nothing
            1 => 1 as u32,
            _ => statsrelay_compat_hash(pdu),
        };
        let client = ring_read.pick_from(code);
        let mut sender = client.sender();

        // Assign prefix and/or suffix
        let pdu_clone: StatsdPDU;
        if self.conf.prefix.is_some() || self.conf.suffix.is_some() {
            pdu_clone = pdu.with_prefix_suffix(
                self.conf
                    .prefix
                    .as_ref()
                    .map(|p| p.as_bytes())
                    .unwrap_or_default(),
                self.conf
                    .suffix
                    .as_ref()
                    .map(|s| s.as_bytes())
                    .unwrap_or_default(),
            );
        } else {
            pdu_clone = pdu.clone();
        }
        match sender.try_send(pdu_clone) {
            Err(_e) => {
                let count = self
                    .warning_log
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 1000 == 0 {
                    warn!(
                        "error pushing to queue full (endpoint {}, total failures {})",
                        client.endpoint(),
                        count
                    );
                }
            }
            Ok(_) => (),
        }
    }
}

struct BackendsInner {
    statsd: Vec<StatsdBackend>,
}

impl BackendsInner {
    fn new() -> Self {
        BackendsInner { statsd: vec![] }
    }

    fn add_statsd_backend(&mut self, c: &StatsdDuplicateTo) {
        self.statsd.push(StatsdBackend::new(c).unwrap());
    }

    fn provide_statsd_pdu(&self, pdu: StatsdPDU) {
        let _result: Vec<_> = self
            .statsd
            .iter()
            .map(|backend| backend.provide_statsd_pdu(&pdu))
            .collect();
    }
}

///
/// Backends provides a cloneable contaner for various protocol backends,
/// handling logic like sharding, sampling, and other detectors.
///
#[derive(Clone)]
pub struct Backends {
    inner: Arc<RwLock<BackendsInner>>,
}

impl Backends {
    pub fn new() -> Self {
        Backends {
            inner: Arc::new(RwLock::new(BackendsInner::new())),
        }
    }

    pub fn add_statsd_backend(&self, c: &StatsdDuplicateTo) {
        self.inner.write().add_statsd_backend(c);
    }

    pub fn provide_statsd_pdu(&self, pdu: StatsdPDU) {
        self.inner.read().provide_statsd_pdu(pdu)
    }
}
