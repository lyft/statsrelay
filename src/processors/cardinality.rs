use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime};

use super::super::config;
use super::super::statsd_proto::Event;
use super::{Output, Processor};
use crate::backends::Backends;
use crate::stats::{Counter, Scope};

use ahash::AHasher;
use cuckoofilter::{self, CuckooFilter};
use parking_lot::Mutex;

use log::warn;

struct TimeBoundedCuckoo<H>
where
    H: Hasher + Default,
{
    filter: CuckooFilter<H>,
    valid_until: SystemTime,
}

impl<H> TimeBoundedCuckoo<H>
where
    H: Hasher + Default,
{
    fn new(valid_until: SystemTime) -> Self {
        TimeBoundedCuckoo {
            filter: CuckooFilter::with_capacity(cuckoofilter::DEFAULT_CAPACITY),
            valid_until,
        }
    }
}

struct MultiCuckoo<H>
where
    H: Hasher + Default,
{
    buckets: usize,
    window: Duration,
    filters: Vec<TimeBoundedCuckoo<H>>,
}

impl<H> MultiCuckoo<H>
where
    H: Hasher + Default,
{
    fn new(buckets: usize, window: &Duration) -> Self {
        assert!(buckets > 0);
        let now = SystemTime::now();
        let cuckoos: Vec<_> = (1..(buckets + 1))
            .map(|bucket| TimeBoundedCuckoo::new(now + (*window * bucket as u32)))
            .collect();
        MultiCuckoo {
            buckets,
            window: *window,
            filters: cuckoos,
        }
    }

    fn len(&self) -> usize {
        self.filters[0].filter.len()
    }

    fn contains<T: ?Sized + Hash>(&self, data: &T) -> bool {
        self.filters[0].filter.contains(data)
    }

    fn add<T: ?Sized + Hash>(&mut self, data: &T) -> Result<(), cuckoofilter::CuckooError> {
        let results: Result<Vec<_>, _> = self
            .filters
            .iter_mut()
            .map(|filter| filter.filter.add(data))
            .collect();
        results.map(|_| ())
    }

    fn rotate(&mut self, with_time: SystemTime) {
        if self.filters[0].valid_until.duration_since(with_time).is_err() {
            // duration_since returns err if the given is later then the valid_until time, aka expired
            self.filters.remove(0);
            self.filters.push(TimeBoundedCuckoo::new(
                with_time + (self.window * (self.buckets + 1) as u32),
            ));
        }
    }
}

pub struct Cardinality {
    route: Vec<config::Route>,
    filter: Mutex<MultiCuckoo<AHasher>>,
    limit: usize,
    counter_flagged_metrics: Counter,
}

impl Cardinality {
    pub fn new(scope: Scope, from_config: &config::processor::Cardinality) -> Self {
        let window = Duration::from_secs(from_config.rotate_after_seconds);
        let flagged_metrics = scope.counter("flagged_metrics").unwrap();
        Cardinality {
            route: from_config.route.clone(),
            filter: Mutex::new(MultiCuckoo::new(from_config.buckets, &window)),
            limit: from_config.size_limit as usize,
            counter_flagged_metrics: flagged_metrics,
        }
    }

    fn rotate(&self) {
        self.filter.lock().rotate(SystemTime::now())
    }
}

impl Processor for Cardinality {
    fn provide_statsd(&self, sample: &Event) -> Option<Output> {
        let mut filter = self.filter.lock();
        let contains = filter.contains(sample);
        if !contains && filter.len() > self.limit {
            self.counter_flagged_metrics.inc();
            warn!("metric flagged for cardinality limits: {:?}", sample);
            return None;
        }
        let _ = filter.add(sample);
        Some(Output {
            route: self.route.as_ref(),
            new_events: None,
        })
    }

    fn tick(&self, _time: std::time::SystemTime, _backends: &Backends) {
        self.rotate();
    }
}

#[cfg(test)]
pub mod test {
    use std::vec;

    use crate::statsd_proto::{Id, Owned, Type};

    use super::*;

    #[test]
    fn cuckoo_simple_contains() {
        let a = "a".to_string();
        let b = "b".to_string();

        let mut mc: MultiCuckoo<AHasher> = MultiCuckoo::new(2, &Duration::from_secs(60));

        mc.add(&a).unwrap();
        assert!(!mc.contains(&b));
        assert!(mc.contains(&a));
        mc.add(&b).unwrap();
        assert!(mc.contains(&b));
    }

    #[test]
    fn cuckoo_simple_rotate() {
        let a = "a".to_string();
        let b = "b".to_string();

        let now = SystemTime::now();
        let mut mc: MultiCuckoo<AHasher> = MultiCuckoo::new(2, &Duration::from_secs(60));

        mc.add(&a).unwrap();
        assert!(!mc.contains(&b));
        assert!(mc.contains(&a));
        mc.add(&b).unwrap();
        assert!(mc.contains(&b));
        // Rotate once, add only a
        mc.rotate(now + Duration::from_secs(61));
        assert!(mc.contains(&a));
        assert!(mc.contains(&b));
        assert!(mc.len() == 2);
        mc.add(&a).unwrap();
        // Rotate again, b should drop out
        mc.rotate(now + Duration::from_secs(122));
        assert!(mc.contains(&a));
        assert!(!mc.contains(&b));
        assert!(mc.len() == 1);
    }

    #[test]
    fn test_cardinality_limit() {
        let names: Vec<Event> = (0..400)
            .map(|val| {
                let id = Id {
                    name: format!("metric.{}", val as u32).as_bytes().to_vec(),
                    mtype: Type::Counter,
                    tags: vec![],
                };
                Event::Parsed(Owned::new(id, 1.0, None))
            })
            .collect();

        let config = config::processor::Cardinality {
            size_limit: 100_usize,
            rotate_after_seconds: 10,
            buckets: 2,
            route: vec![],
        };
        let scope = crate::stats::Collector::default().scope("test");
        let filter = Cardinality::new(scope, &config);
        for name in &names[0..101] {
            assert!(filter.provide_statsd(name).is_some());
        }
        for name in &names[101..] {
            assert!(
                filter.provide_statsd(name).is_none(),
                "sample {:?} was allowed",
                name
            );
        }
    }
}
