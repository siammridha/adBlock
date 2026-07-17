//! LRU cache for DNS responses with TTL tracking.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hickory_proto::op::{Message, Query, ResponseCode};
use lru::LruCache;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct CacheStatus {
    pub entries: usize,
    pub capacity: usize,
    pub min_ttl_secs: u32,
    pub max_ttl_secs: u32,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    name: String,
    qtype: u16,
    qclass: u16,
}

impl Key {
    pub fn of(query: &Query) -> Self {
        Self {
            name: query.name().to_ascii().to_ascii_lowercase(),
            qtype: query.query_type().into(),
            qclass: query.query_class().into(),
        }
    }
}

struct Entry {
    response: Message,
    stored: Instant,
    expires: Instant,
}

const EMPTY_RESPONSE_TTL: Duration = Duration::from_secs(30);

pub struct DnsCache {
    entries: Mutex<Option<LruCache<Key, Entry>>>,
    min_ttl: AtomicU32,
    max_ttl: AtomicU32,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl DnsCache {
    pub fn new(capacity: usize, min_ttl: u32, max_ttl: u32) -> Self {
        Self {
            entries: Mutex::new(NonZeroUsize::new(capacity).map(LruCache::new)),
            min_ttl: AtomicU32::new(min_ttl),
            max_ttl: AtomicU32::new(max_ttl.max(1)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn set_config(&self, capacity: usize, min_ttl: u32, max_ttl: u32) {
        self.min_ttl.store(min_ttl, Ordering::Relaxed);
        self.max_ttl.store(max_ttl.max(1), Ordering::Relaxed);
        let mut entries = self.entries.lock().expect("dns cache lock");
        match (entries.as_mut(), NonZeroUsize::new(capacity)) {
            (Some(cache), Some(cap)) => cache.resize(cap),
            (None, Some(cap)) => *entries = Some(LruCache::new(cap)),
            (_, None) => *entries = None,
        }
    }

    pub fn get(&self, key: &Key) -> Option<Message> {
        let hit = {
            let mut guard = self.entries.lock().expect("dns cache lock");
            guard.as_mut().and_then(|entries| match entries.get(key) {
                Some(e) if e.expires > Instant::now() => {
                    let age = e.stored.elapsed().as_secs() as u32;
                    let mut msg = e.response.clone();
                    for r in records_mut(&mut msg) {
                        r.ttl = r.ttl.saturating_sub(age).max(1);
                    }
                    Some(msg)
                }
                Some(_) => {
                    entries.pop(key);
                    None
                }
                None => None,
            })
        };
        match &hit {
            Some(_) => self.hits.fetch_add(1, Ordering::Relaxed),
            None => self.misses.fetch_add(1, Ordering::Relaxed),
        };
        hit
    }

    pub fn put(&self, key: Key, response: &Message) {
        if response.metadata.truncation
            || !matches!(
                response.metadata.response_code,
                ResponseCode::NoError | ResponseCode::NXDomain
            )
        {
            return;
        }
        let min_ttl = self.min_ttl.load(Ordering::Relaxed);
        let max_ttl = self.max_ttl.load(Ordering::Relaxed).max(min_ttl);
        let mut response = response.clone();
        let mut shortest: Option<u32> = None;
        for r in records_mut(&mut response) {
            r.ttl = r.ttl.clamp(min_ttl, max_ttl);
            shortest = Some(shortest.map_or(r.ttl, |s| s.min(r.ttl)));
        }
        let now = Instant::now();
        let expires = match shortest {
            Some(ttl) => now + Duration::from_secs(u64::from(ttl)),
            None => now + EMPTY_RESPONSE_TTL,
        };
        let mut guard = self.entries.lock().expect("dns cache lock");
        if let Some(entries) = guard.as_mut() {
            entries.put(key, Entry { response, stored: now, expires });
        }
    }

    pub fn clear(&self) -> usize {
        let mut guard = self.entries.lock().expect("dns cache lock");
        let Some(entries) = guard.as_mut() else { return 0 };
        let n = entries.len();
        entries.clear();
        n
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("dns cache lock")
            .as_ref()
            .map_or(0, LruCache::len)
    }

    pub fn capacity(&self) -> usize {
        self.entries
            .lock()
            .expect("dns cache lock")
            .as_ref()
            .map_or(0, |e| e.cap().get())
    }

    pub fn min_ttl(&self) -> u32 {
        self.min_ttl.load(Ordering::Relaxed)
    }

    pub fn max_ttl(&self) -> u32 {
        self.max_ttl.load(Ordering::Relaxed)
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> CacheStatus {
        CacheStatus {
            entries: self.len(),
            capacity: self.capacity(),
            min_ttl_secs: self.min_ttl(),
            max_ttl_secs: self.max_ttl(),
            hits: self.hits(),
            misses: self.misses(),
        }
    }
}

fn records_mut(msg: &mut Message) -> impl Iterator<Item = &mut hickory_proto::rr::Record> {
    msg.answers
        .iter_mut()
        .chain(msg.authorities.iter_mut())
        .chain(msg.additionals.iter_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::OpCode;
    use hickory_proto::rr::rdata::A;
    use hickory_proto::rr::{Name, RData, Record, RecordType};
    use std::str::FromStr;

    fn query(name: &str, qtype: RecordType) -> Query {
        Query::query(Name::from_str(name).unwrap(), qtype)
    }

    fn response(name: &str, ttl: u32) -> Message {
        let mut msg = Message::response(1, OpCode::Query);
        msg.add_answer(Record::from_rdata(
            Name::from_str(name).unwrap(),
            ttl,
            RData::A(A::new(93, 184, 216, 34)),
        ));
        msg
    }

    #[test]
    fn hit_returns_stored_response_and_counts() {
        let cache = DnsCache::new(8, 0, 3600);
        let q = query("example.com.", RecordType::A);
        assert!(cache.get(&Key::of(&q)).is_none());
        cache.put(Key::of(&q), &response("example.com.", 300));
        let hit = cache.get(&Key::of(&q)).expect("cached");
        assert_eq!(hit.answers.len(), 1);
        assert!(hit.answers[0].ttl <= 300 && hit.answers[0].ttl > 0);
        assert_eq!((cache.hits(), cache.misses()), (1, 1));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn keys_are_case_insensitive_but_type_specific() {
        let cache = DnsCache::new(8, 0, 3600);
        let q = query("Example.COM.", RecordType::A);
        cache.put(Key::of(&q), &response("example.com.", 300));
        assert!(cache.get(&Key::of(&query("example.com.", RecordType::A))).is_some());
        assert!(cache.get(&Key::of(&query("example.com.", RecordType::AAAA))).is_none());
    }

    #[test]
    fn ttl_clamps_apply_on_store() {
        let cache = DnsCache::new(8, 60, 120);
        let q = query("short.example.", RecordType::A);
        cache.put(Key::of(&q), &response("short.example.", 5));
        assert!(cache.get(&Key::of(&q)).unwrap().answers[0].ttl >= 59);
        let q = query("long.example.", RecordType::A);
        cache.put(Key::of(&q), &response("long.example.", 999_999));
        assert!(cache.get(&Key::of(&q)).unwrap().answers[0].ttl <= 120);
    }

    #[test]
    fn servfail_and_truncated_are_not_cached() {
        let cache = DnsCache::new(8, 0, 3600);
        let q = query("down.example.", RecordType::A);
        let mut bad = response("down.example.", 300);
        bad.metadata.response_code = ResponseCode::ServFail;
        cache.put(Key::of(&q), &bad);
        assert!(cache.get(&Key::of(&q)).is_none());
        let mut trunc = response("down.example.", 300);
        trunc.metadata.truncation = true;
        cache.put(Key::of(&q), &trunc);
        assert!(cache.get(&Key::of(&q)).is_none());
    }

    #[test]
    fn zero_capacity_disables_the_cache() {
        let cache = DnsCache::new(0, 0, 3600);
        let q = query("example.com.", RecordType::A);
        cache.put(Key::of(&q), &response("example.com.", 300));
        assert!(cache.get(&Key::of(&q)).is_none());
        assert_eq!((cache.len(), cache.capacity()), (0, 0));
    }

    #[test]
    fn set_config_resizes_live_keeping_entries_and_counters() {
        let cache = DnsCache::new(8, 0, 3600);
        for name in ["a.example.", "b.example.", "c.example."] {
            cache.put(Key::of(&query(name, RecordType::A)), &response(name, 300));
        }
        assert!(cache.get(&Key::of(&query("a.example.", RecordType::A))).is_some());

        cache.set_config(2, 60, 120);
        assert_eq!((cache.len(), cache.capacity()), (2, 2));
        assert_eq!(cache.hits(), 1);
        assert_eq!((cache.min_ttl(), cache.max_ttl()), (60, 120));
        cache.put(Key::of(&query("d.example.", RecordType::A)), &response("d.example.", 5));
        assert!(cache.get(&Key::of(&query("d.example.", RecordType::A))).unwrap().answers[0].ttl >= 59);

        cache.set_config(0, 60, 120);
        assert_eq!((cache.len(), cache.capacity()), (0, 0));
        cache.put(Key::of(&query("e.example.", RecordType::A)), &response("e.example.", 300));
        assert!(cache.get(&Key::of(&query("e.example.", RecordType::A))).is_none());
        cache.set_config(4, 60, 120);
        assert_eq!((cache.len(), cache.capacity()), (0, 4));
    }

    #[test]
    fn lru_evicts_oldest_when_full() {
        let cache = DnsCache::new(2, 0, 3600);
        for name in ["a.example.", "b.example.", "c.example."] {
            cache.put(Key::of(&query(name, RecordType::A)), &response(name, 300));
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&Key::of(&query("a.example.", RecordType::A))).is_none());
        assert!(cache.get(&Key::of(&query("c.example.", RecordType::A))).is_some());
    }
}
