use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// Lightweight in-process counters exposed as a Prometheus text-format
/// endpoint. Relaxed ordering is fine: totals only need monotonicity.
pub(crate) struct Metrics {
    started: Instant,
    pub(crate) requests_total: AtomicU64,
    pub(crate) blocked_total: AtomicU64,
    pub(crate) redirects_total: AtomicU64,
    pub(crate) proxied_total: AtomicU64,
    pub(crate) body_rejected_total: AtomicU64,
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
        }
    }
}

impl Metrics {
    pub(crate) fn render(&self, unhealthy_backends: usize) -> String {
        format!(
            "# HELP gateway_lb_requests_total Total requests received by the LB.\n\
             # TYPE gateway_lb_requests_total counter\n\
             gateway_lb_requests_total {}\n\
             gateway_lb_blocked_total {}\n\
             gateway_lb_redirects_total {}\n\
             gateway_lb_proxied_total {}\n\
             gateway_lb_body_rejected_total {}\n\
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
            unhealthy_backends,
            self.started.elapsed().as_secs(),
        )
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
        let out = m.render(2);
        assert!(out.contains("gateway_lb_requests_total 3"));
        assert!(out.contains("gateway_lb_blocked_total 1"));
        assert!(out.contains("gateway_lb_proxied_total 2"));
        assert!(out.contains("gateway_lb_unhealthy_backends 2"));
    }
}
