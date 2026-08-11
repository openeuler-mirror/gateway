use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use crate::config::{Config, LbMode};

const VNODES: usize = 150;

/// Stable 64-bit FNV-1a. Deterministic across processes and Rust versions,
/// unlike `std::collections::hash_map::DefaultHasher`, so multiple LB
/// instances always agree on the ring layout.
pub(crate) fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Consistent-hash ring over a route's backends.
#[derive(Debug, Clone)]
pub(crate) struct Ring {
    nodes: Vec<(u64, SocketAddr)>,
}

impl Ring {
    fn new(backends: &[SocketAddr]) -> Self {
        let mut nodes = Vec::with_capacity(backends.len() * VNODES);
        // Keyed by address (not by index), so inserting/removing a backend
        // elsewhere in the list does not reshuffle every other backend's vnodes.
        for addr in backends {
            for vi in 0..VNODES {
                nodes.push((hash_str(&format!("{addr}#{vi}")), *addr));
            }
        }
        nodes.sort_unstable_by_key(|(h, _)| *h);
        Ring { nodes }
    }

    /// Clockwise from hash(key): return the first node that is neither
    /// unhealthy nor already attempted this request. If every remaining node is
    /// unhealthy, still prefer an untried node (best-effort); only when all
    /// nodes were attempted does it fall back to the start node.
    fn pick(
        &self,
        key: &str,
        unhealthy: &HashSet<SocketAddr>,
        attempted: &[SocketAddr],
    ) -> SocketAddr {
        debug_assert!(!self.nodes.is_empty(), "Ring must have >=1 backend");
        let h = hash_str(key);
        let n = self.nodes.len();
        let start = match self.nodes.binary_search_by_key(&h, |(p, _)| *p) {
            Ok(i) => i,
            Err(i) => i % n,
        };
        let mut fallback: Option<SocketAddr> = None;
        for off in 0..n {
            let (_, addr) = self.nodes[(start + off) % n];
            if !attempted.contains(&addr) {
                if fallback.is_none() {
                    fallback = Some(addr);
                }
                if !unhealthy.contains(&addr) {
                    return addr;
                }
            }
        }
        fallback.unwrap_or(self.nodes[start].1)
    }
}

/// Primary-standby selection: the first healthy backend in list order, so all
/// traffic prefers `backends[0]` and only fails over when earlier ones are down.
/// Untried-but-unhealthy backends are still preferred over retrying an already
/// attempted one; only when every backend was attempted does it fall back to
/// `backends[0]`.
pub(crate) fn pick_primary(
    backends: &[SocketAddr],
    unhealthy: &HashSet<SocketAddr>,
    attempted: &[SocketAddr],
) -> SocketAddr {
    let mut fallback: Option<SocketAddr> = None;
    for addr in backends {
        if !attempted.contains(addr) {
            if fallback.is_none() {
                fallback = Some(*addr);
            }
            if !unhealthy.contains(addr) {
                return *addr;
            }
        }
    }
    fallback.unwrap_or(backends[0])
}

/// Active-active selection with a graceful fallback: consistent-hash via the
/// ring when present; if the ring is missing for `idx` (e.g. a config reload
/// invalidated the index between resolve and lookup), degrade to ordered
/// primary selection so routing never panics.
pub(crate) fn pick_active_active(
    idx: usize,
    backends: &[SocketAddr],
    rings: &HashMap<usize, Ring>,
    key: &str,
    unhealthy: &HashSet<SocketAddr>,
    attempted: &[SocketAddr],
) -> SocketAddr {
    match rings.get(&idx) {
        Some(ring) => ring.pick(key, unhealthy, attempted),
        None => pick_primary(backends, unhealthy, attempted),
    }
}

pub(crate) fn build_rings(config: &Config) -> HashMap<usize, Ring> {
    config
        .routes
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            // Active-standby routes select by ordered failover, no ring needed.
            if r.mode == LbMode::ActiveActive {
                r.backends.as_ref().map(|bs| (i, Ring::new(bs)))
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn collect_all_backends(config: &Config) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in &config.default_backends {
        if seen.insert(*a) {
            out.push(*a);
        }
    }
    for r in &config.routes {
        if let Some(list) = &r.backends {
            for a in list {
                if seen.insert(*a) {
                    out.push(*a);
                }
            }
        }
        if let Some(a) = r.backend {
            if seen.insert(a) {
                out.push(a);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_same_key_same_backend_and_skip_unhealthy() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let a = ring.pick("user-key-1", &HashSet::new(), &[]);
        let b = ring.pick("user-key-1", &HashSet::new(), &[]);
        assert_eq!(a, b, "same key must map to same backend");

        let mut un = HashSet::new();
        un.insert(a);
        let c = ring.pick("user-key-1", &un, &[]);
        assert_ne!(c, a, "must skip unhealthy primary");
        assert!(!un.contains(&c));

        let all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let d = ring.pick("user-key-1", &all, &[]);
        assert!(
            all.contains(&d),
            "all-unhealthy best-effort still returns a node"
        );
    }

    #[test]
    fn ring_consistency_on_eviction() {
        let backends: Vec<SocketAddr> = (8001..=8004)
            .map(|p| format!("127.0.0.1:{p}").parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let full: HashMap<String, SocketAddr> = (0..2000)
            .map(|i| {
                let k = format!("k{i}");
                (k.clone(), ring.pick(&k, &HashSet::new(), &[]))
            })
            .collect();
        let mut un = HashSet::new();
        un.insert(backends[1]);
        let on_evicted = full.values().filter(|a| **a == backends[1]).count();
        let moved = full
            .iter()
            .filter(|(k, orig)| ring.pick(k, &un, &[]) != **orig)
            .count();
        assert_eq!(
            moved, on_evicted,
            "only keys on the evicted node should move"
        );
    }

    #[test]
    fn ring_retry_excludes_attempted_backends() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let first = ring.pick("key", &HashSet::new(), &[]);
        let second = ring.pick("key", &HashSet::new(), &[first]);
        assert_ne!(
            second, first,
            "retry must avoid the already attempted backend"
        );

        let third = ring.pick("key", &HashSet::new(), &[first, second]);
        assert!(
            third != first && third != second,
            "third pick avoids both attempts"
        );

        // All backends attempted (and all unhealthy): best-effort fallback
        // still returns a node instead of panicking.
        let all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let _ = ring.pick("key", &all, &backends);
    }

    #[test]
    fn ring_untried_backend_preferred_over_unhealthy_when_all_unhealthy() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let all_unhealthy: HashSet<SocketAddr> = backends.iter().copied().collect();
        let first = ring.pick("key", &all_unhealthy, &[]);
        // Retry should prefer the untried backend even though it is marked
        // unhealthy, rather than re-hitting the attempted one.
        let second = ring.pick("key", &all_unhealthy, &[first]);
        assert_ne!(first, second);
    }

    #[test]
    fn hash_str_is_stable_and_distributes() {
        let a = hash_str("user-key-1");
        let b = hash_str("user-key-1");
        assert_eq!(a, b, "same input must hash identically (process-stable)");
        let c = hash_str("user-key-2");
        assert_ne!(a, c);
        // FNV-1a is a known vector: hash of "abc" is deterministic.
        assert_eq!(hash_str("abc"), 0xe71f_a219_0541_574b);
    }

    #[test]
    fn ring_layout_is_index_independent() {
        // Vnodes are keyed by address only: inserting a backend at the front of
        // the list must not move keys that already map to the other backends.
        let a = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"];
        let b = [
            "127.0.0.1:8000",
            "127.0.0.1:8001",
            "127.0.0.1:8002",
            "127.0.0.1:8003",
        ];
        let ra = Ring::new(&a.iter().map(|s| s.parse().unwrap()).collect::<Vec<_>>());
        let rb = Ring::new(&b.iter().map(|s| s.parse().unwrap()).collect::<Vec<_>>());
        let moved = (0..2000)
            .filter(|i| {
                let k = format!("k{i}");
                ra.pick(&k, &HashSet::new(), &[]) != rb.pick(&k, &HashSet::new(), &[])
            })
            .count();
        // Only keys that land on the new node (1/4 of the ring on average) may
        // move; an index-dependent layout would move ~3/4 of keys.
        assert!(moved < 1000, "too many keys moved: {moved}");
    }

    #[test]
    fn pick_primary_prefers_healthiest_in_order() {
        let bs: Vec<SocketAddr> = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // all healthy -> primary (backends[0])
        assert_eq!(pick_primary(&bs, &HashSet::new(), &[]), bs[0]);
        // primary down -> first standby in order
        let mut un = HashSet::new();
        un.insert(bs[0]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[1]);
        // first two down -> third
        un.insert(bs[1]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[2]);
        // all down -> best-effort primary (still attempt backends[0])
        un.insert(bs[2]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[0]);

        // Retry: attempted primary must be skipped even when it is healthy.
        assert_eq!(pick_primary(&bs, &HashSet::new(), &[bs[0]]), bs[1]);
        // All unhealthy but only primary attempted -> try standby next.
        assert_eq!(pick_primary(&bs, &un, &[bs[0]]), bs[1]);
        // Everything attempted -> fall back to primary.
        assert_eq!(pick_primary(&bs, &un, &bs), bs[0]);
    }

    #[test]
    fn pick_active_active_uses_ring_or_falls_back() {
        let bs: Vec<SocketAddr> = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let mut rings: HashMap<usize, Ring> = HashMap::new();
        rings.insert(7, Ring::new(&bs));
        let un = HashSet::new();

        // ring present -> consistent-hash pick (deterministic per key)
        let a = pick_active_active(7, &bs, &rings, "some-key", &un, &[]);
        let b = pick_active_active(7, &bs, &rings, "some-key", &un, &[]);
        assert_eq!(a, b, "same key via ring -> same backend");
        assert!(bs.contains(&a));

        // ring missing for idx (e.g. stale after a reload) -> graceful fallback
        // to ordered primary selection instead of panicking.
        let c = pick_active_active(999, &bs, &rings, "some-key", &un, &[]);
        assert_eq!(
            c, bs[0],
            "missing ring falls back to pick_primary -> backends[0]"
        );

        // Retry excludes the first pick.
        let d = pick_active_active(7, &bs, &rings, "some-key", &un, &[a]);
        assert_ne!(d, a, "retry must pick a different backend");
    }

    #[test]
    fn build_rings_and_collect_backends() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: a.com
    backend: "10.0.0.1:80"
  - host: b.com
    backends: ["10.0.0.2:80", "10.0.0.3:80"]
  - host: c.com
    backends: ["10.0.0.2:80", "10.0.0.4:80"]
"#;
        let cfg = Config::from_str(yaml).unwrap();
        let rings = build_rings(&cfg);
        assert!(rings.contains_key(&1) && rings.contains_key(&2));
        assert!(!rings.contains_key(&0), "single-backend route has no ring");
        let all = collect_all_backends(&cfg);
        let set: HashSet<SocketAddr> = all.iter().copied().collect();
        assert_eq!(
            set.len(),
            5,
            "default backend + single route backend + two lists, deduped"
        );
        assert!(set.contains(&"10.0.0.2:80".parse().unwrap()));
        assert!(
            set.contains(&"127.0.0.1:8080".parse().unwrap()),
            "default backend probed"
        );
        assert!(
            set.contains(&"10.0.0.1:80".parse().unwrap()),
            "single route backend probed"
        );
    }
}
