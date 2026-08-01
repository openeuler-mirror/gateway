use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// Latency histogram bucket upper bounds in milliseconds (+Inf is implicit).
/// Covers short probes up to long streaming LLM requests (60s).
const DURATION_BUCKETS_MS: [u64; 13] = [
    5, 10, 25, 50, 100, 250, 500, 1_000, 2_000, 5_000, 10_000, 30_000, 60_000,
];

/// Lightweight in-process counters exposed as a Prometheus text-format
/// endpoint. Relaxed ordering is fine: totals only need monotonicity.
pub(crate) struct Metrics {
    started: Instant,
    pub(crate) requests_total: AtomicU64,
    pub(crate) blocked_total: AtomicU64,
    pub(crate) redirects_total: AtomicU64,
    pub(crate) proxied_total: AtomicU64,
    pub(crate) body_rejected_total: AtomicU64,
    pub(crate) status_2xx_total: AtomicU64,
    pub(crate) status_3xx_total: AtomicU64,
    pub(crate) status_4xx_total: AtomicU64,
    pub(crate) status_5xx_total: AtomicU64,
    pub(crate) upstream_errors_total: AtomicU64,
    pub(crate) retries_total: AtomicU64,
    pub(crate) response_bytes_total: AtomicU64,
    /// Per-bucket counts (non-cumulative; the last slot is the +Inf bucket).
    /// Rendered as a cumulative Prometheus histogram.
    duration_buckets: [AtomicU64; DURATION_BUCKETS_MS.len() + 1],
    duration_sum_ms: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            started: Instant::now(),
            requests_total: AtomicU64::new(0),
            blocked_total: AtomicU64::new(0),
            redirects_total: AtomicU64::new(0),
            proxied_total: AtomicU64::new(0),
            body_rejected_total: AtomicU64::new(0),
            status_2xx_total: AtomicU64::new(0),
            status_3xx_total: AtomicU64::new(0),
            status_4xx_total: AtomicU64::new(0),
            status_5xx_total: AtomicU64::new(0),
            upstream_errors_total: AtomicU64::new(0),
            retries_total: AtomicU64::new(0),
            response_bytes_total: AtomicU64::new(0),
            duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_sum_ms: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    /// Record the end-to-end request latency of a proxied request.
    pub(crate) fn observe_duration_ms(&self, ms: u128) {
        let ms = ms as u64;
        let idx = DURATION_BUCKETS_MS
            .iter()
            .position(|&le| ms <= le)
            .unwrap_or(DURATION_BUCKETS_MS.len());
        self.duration_buckets[idx].fetch_add(1, Relaxed);
        self.duration_sum_ms.fetch_add(ms, Relaxed);
    }

    pub(crate) fn render(&self, unhealthy_backends: usize) -> String {
        let mut out = format!(
            "# HELP gateway_lb_requests_total Total requests received by the LB.\n\
             # TYPE gateway_lb_requests_total counter\n\
             gateway_lb_requests_total {}\n\
             gateway_lb_blocked_total {}\n\
             gateway_lb_redirects_total {}\n\
             gateway_lb_proxied_total {}\n\
             gateway_lb_body_rejected_total {}\n\
             gateway_lb_status_2xx_total {}\n\
             gateway_lb_status_3xx_total {}\n\
             gateway_lb_status_4xx_total {}\n\
             gateway_lb_status_5xx_total {}\n\
             gateway_lb_upstream_errors_total {}\n\
             gateway_lb_retries_total {}\n\
             gateway_lb_response_bytes_total {}\n\
             # HELP gateway_lb_unhealthy_backends Backends currently marked unhealthy.\n\
             # TYPE gateway_lb_unhealthy_backends gauge\n\
             gateway_lb_unhealthy_backends {}\n\
             # HELP gateway_lb_uptime_seconds Seconds since process start.\n\
             # TYPE gateway_lb_uptime_seconds gauge\n\
             gateway_lb_uptime_seconds {}",
            self.requests_total.load(Relaxed),
            self.blocked_total.load(Relaxed),
            self.redirects_total.load(Relaxed),
            self.proxied_total.load(Relaxed),
            self.body_rejected_total.load(Relaxed),
            self.status_2xx_total.load(Relaxed),
            self.status_3xx_total.load(Relaxed),
            self.status_4xx_total.load(Relaxed),
            self.status_5xx_total.load(Relaxed),
            self.upstream_errors_total.load(Relaxed),
            self.retries_total.load(Relaxed),
            self.response_bytes_total.load(Relaxed),
            unhealthy_backends,
            self.started.elapsed().as_secs(),
        );
        // Cumulative latency histogram.
        out.push_str(
            "\n# HELP gateway_lb_request_duration_ms Upstream request latency in milliseconds.\n\
             # TYPE gateway_lb_request_duration_ms histogram\n",
        );
        let mut cum = 0u64;
        for (i, &le) in DURATION_BUCKETS_MS.iter().enumerate() {
            cum += self.duration_buckets[i].load(Relaxed);
            out.push_str(&format!(
                "gateway_lb_request_duration_ms_bucket{{le=\"{le}\"}} {cum}\n"
            ));
        }
        cum += self.duration_buckets[DURATION_BUCKETS_MS.len()].load(Relaxed);
        out.push_str(&format!(
            "gateway_lb_request_duration_ms_bucket{{le=\"+Inf\"}} {cum}\n"
        ));
        out.push_str(&format!(
            "gateway_lb_request_duration_ms_sum {}\n",
            self.duration_sum_ms.load(Relaxed)
        ));
        out.push_str(&format!("gateway_lb_request_duration_ms_count {cum}"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_reports_counters() {
        let m = Metrics::default();
        m.requests_total.fetch_add(3, Relaxed);
        m.blocked_total.fetch_add(1, Relaxed);
        m.proxied_total.fetch_add(2, Relaxed);
        m.status_2xx_total.fetch_add(5, Relaxed);
        m.upstream_errors_total.fetch_add(1, Relaxed);
        m.retries_total.fetch_add(3, Relaxed);
        m.response_bytes_total.fetch_add(4096, Relaxed);
        m.observe_duration_ms(3);
        m.observe_duration_ms(120);
        m.observe_duration_ms(200_000); // beyond the last bucket -> +Inf
        let out = m.render(2);
        assert!(out.contains("gateway_lb_requests_total 3"));
        assert!(out.contains("gateway_lb_blocked_total 1"));
        assert!(out.contains("gateway_lb_proxied_total 2"));
        assert!(out.contains("gateway_lb_status_2xx_total 5"));
        assert!(out.contains("gateway_lb_upstream_errors_total 1"));
        assert!(out.contains("gateway_lb_retries_total 3"));
        assert!(out.contains("gateway_lb_response_bytes_total 4096"));
        assert!(out.contains("gateway_lb_unhealthy_backends 2"));
        // Histogram: 3ms hits the <=5 bucket (and every later one cumulatively).
        assert!(out.contains("gateway_lb_request_duration_ms_bucket{le=\"5\"} 1"));
        assert!(out.contains("gateway_lb_request_duration_ms_bucket{le=\"100\"} 1"));
        assert!(out.contains("gateway_lb_request_duration_ms_bucket{le=\"250\"} 2"));
        assert!(out.contains("gateway_lb_request_duration_ms_bucket{le=\"+Inf\"} 3"));
        assert!(out.contains("gateway_lb_request_duration_ms_sum 200123"));
        assert!(out.contains("gateway_lb_request_duration_ms_count 3"));
    }
}
