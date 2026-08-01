use arc_swap::ArcSwap;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::HealthCheckConfig;
use crate::proxy::ConfigSnapshot;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Cap on concurrent probe worker threads per cycle; a large pool reuses at
/// most this many OS threads instead of spawning one per backend.
const MAX_PROBE_WORKERS: usize = 8;

/// (prev_consecutive_failures, probe_healthy) -> (new_failures, mark_unhealthy)
fn classify_health(prev_failures: u32, probe_healthy: bool, fail_threshold: u32) -> (u32, bool) {
    if probe_healthy {
        (0, false)
    } else {
        let n = prev_failures + 1;
        (n, n >= fail_threshold)
    }
}

/// HTTP GET probe: open the connection, send a minimal request, and require the
/// configured status code in the response head. Reads until the end of headers
/// (bounded), so slow/hung backends fail within PROBE_TIMEOUT.
fn probe_http(addr: SocketAddr, path: &str, expected: u16) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let host = addr.ip().to_string();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut buf = [0u8; 4096];
    let mut got = 0;
    loop {
        match stream.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => {
                got += n;
                if got >= buf.len() || buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return false,
        }
    }
    let head = std::str::from_utf8(&buf[..got]).unwrap_or("");
    head.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        == Some(expected)
}

fn probe_once(addr: SocketAddr, check: &HealthCheckConfig) -> bool {
    match &check.path {
        Some(path) => probe_http(addr, path, check.expected_status),
        None => std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok(),
    }
}

/// Background TCP prober: every PROBE_INTERVAL, probe each known backend in
/// parallel and publish a fresh `unhealthy` set. It is the SOLE writer of
/// `unhealthy`, so a plain `.store()` is safe. Rebuilding from the current
/// `backends` each cycle also self-prunes backends that were removed from the
/// config.
pub(crate) fn run_health_probe(
    snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    unhealthy: Arc<ArcSwap<HashSet<SocketAddr>>>,
) {
    std::thread::spawn(move || {
        let mut failures: HashMap<SocketAddr, u32> = HashMap::new();
        loop {
            let (backends, check) = {
                let snap = snapshot.load();
                (snap.backends.clone(), snap.config.health_check.clone())
            };
            let interval = Duration::from_secs(check.interval_secs.max(1));
            let threshold = check.fail_threshold.max(1);

            let results: Vec<(SocketAddr, bool)> = if backends.is_empty() {
                Vec::new()
            } else {
                // Probe backends concurrently, but cap the number of OS threads
                // so a large pool does not churn threads every cycle.
                let workers = backends.len().min(MAX_PROBE_WORKERS);
                std::thread::scope(|s| {
                    let chunk_size = backends.len().div_ceil(workers);
                    let mut handles = Vec::with_capacity(workers);
                    for chunk in backends.chunks(chunk_size) {
                        let check = &check;
                        handles.push(s.spawn(move || {
                            chunk
                                .iter()
                                .map(|&addr| (addr, probe_once(addr, check)))
                                .collect::<Vec<_>>()
                        }));
                    }
                    handles
                        .into_iter()
                        .flat_map(|h| h.join().expect("probe thread panicked"))
                        .collect()
                })
            };
            // Drop counters for backends no longer in the config.
            failures.retain(|addr, _| backends.contains(addr));
            let mut new_unhealthy = HashSet::new();
            for (addr, healthy) in results {
                let prev = *failures.get(&addr).unwrap_or(&0);
                let (nf, mark) = classify_health(prev, healthy, threshold);
                failures.insert(addr, nf);
                if mark {
                    new_unhealthy.insert(addr);
                }
            }
            unhealthy.store(Arc::new(new_unhealthy));
            std::thread::sleep(interval);
        }
    });
}

/// Per-request routing context. `request_filter` resolves the route once and
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_health_threshold_and_recovery() {
        assert_eq!(classify_health(0, true, 3), (0, false));
        assert_eq!(classify_health(1, false, 3), (2, false));
        assert_eq!(classify_health(2, false, 3), (3, true));
        assert_eq!(classify_health(9, true, 3), (0, false));
        // Custom threshold: a single failure can mark unhealthy immediately.
        assert_eq!(classify_health(0, false, 1), (1, true));
    }

    #[test]
    fn probe_once_alive_then_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(probe_once(addr, &HealthCheckConfig::default()));
        drop(listener);
        assert!(!probe_once(addr, &HealthCheckConfig::default()));
    }
}
