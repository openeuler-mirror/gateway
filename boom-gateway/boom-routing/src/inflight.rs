use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Per-model in-flight metrics.
#[derive(Debug)]
struct InFlightMetrics {
    request_count: AtomicU64,
    input_chars: AtomicU64,
}

/// Per-deployment in-flight metrics (model + deployment_id composite key).
#[derive(Debug)]
struct DeploymentMetrics {
    request_count: AtomicU64,
    input_chars: AtomicU64,
}

/// Per-key in-flight metrics within a deployment.
#[derive(Debug)]
struct InFlightKeyMetrics {
    request_count: AtomicU64,
}

/// Tracks in-flight requests per model and per deployment.
///
/// Lives at `AppState` level — survives config reloads.
/// Used by scheduling policies for load-aware routing and exposed
/// via the dashboard for operational visibility.
#[derive(Debug)]
pub struct InFlightTracker {
    /// model_name → metrics
    metrics: DashMap<String, InFlightMetrics>,
    /// `{model_name}\0{deployment_id}` → metrics (per-deployment tracking).
    deployment_metrics: DashMap<String, DeploymentMetrics>,
    /// `{model_name}\0{deployment_id}\0{key_alias_or_hash}` → per-key metrics.
    /// Used for dashboard tooltip showing which keys have active requests.
    key_metrics: DashMap<String, InFlightKeyMetrics>,
}

/// Snapshot of in-flight stats for one model.
#[derive(Debug)]
pub struct InFlightStat {
    pub model: String,
    pub inflight_requests: u64,
    pub inflight_input_chars: u64,
}

/// Snapshot of in-flight stats for one deployment.
#[derive(Debug, Clone)]
pub struct DeploymentInFlightStat {
    pub model: String,
    pub deployment_id: String,
    pub inflight_requests: u64,
    pub inflight_input_chars: u64,
    /// Per-key breakdown for dashboard tooltip.
    pub key_stats: Vec<InFlightKeyStat>,
}

/// Per-key in-flight stats within a deployment.
#[derive(Debug, Clone)]
pub struct InFlightKeyStat {
    pub key_alias: String,
    pub request_count: u64,
}

/// RAII guard that decrements in-flight metrics on Drop.
///
/// Create via [`InFlightGuard::new`] before forwarding a request;
/// the counters are released automatically whether the request
/// succeeds, fails, or the stream is dropped mid-way.
pub struct InFlightGuard {
    tracker: Arc<InFlightTracker>,
    model: String,
    input_chars: u64,
    /// Optional deployment_id for per-deployment tracking.
    deployment_key: Option<String>,
    /// Optional key-level tracking key.
    key_tracking_key: Option<String>,
}

impl InFlightTracker {
    pub fn new() -> Self {
        Self {
            metrics: DashMap::new(),
            deployment_metrics: DashMap::new(),
            key_metrics: DashMap::new(),
        }
    }

    /// Current in-flight stats for all models with active requests.
    pub fn get_stats(&self) -> Vec<InFlightStat> {
        self.metrics
            .iter()
            .filter(|r| r.value().request_count.load(Ordering::Relaxed) > 0)
            .map(|r| InFlightStat {
                model: r.key().clone(),
                inflight_requests: r.value().request_count.load(Ordering::Relaxed),
                inflight_input_chars: r.value().input_chars.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Get the total in-flight input chars for a specific model (O(1) lookup).
    pub fn get_model_input_chars(&self, model: &str) -> u64 {
        self.metrics
            .get(model)
            .map(|m| m.input_chars.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get the in-flight request count for a specific deployment (O(1) lookup).
    pub fn get_deployment_count(&self, model: &str, deployment_id: &str) -> u64 {
        let key = format!("{}\0{}", model, deployment_id);
        self.deployment_metrics
            .get(&key)
            .map(|m| m.request_count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Current in-flight stats for all deployments with active requests.
    pub fn get_stats_by_deployment(&self) -> Vec<DeploymentInFlightStat> {
        // Clean up any stale key_metrics entries (count == 0 but not yet removed).
        self.key_metrics.retain(|_, v| v.request_count.load(Ordering::Relaxed) > 0);

        self.deployment_metrics
            .iter()
            .filter(|r| r.value().request_count.load(Ordering::Relaxed) > 0)
            .filter_map(|r| {
                let key = r.key();
                let (model, deployment_id) = key.split_once('\0')?;

                // Collect per-key stats for this deployment.
                let prefix = format!("{}\0{}\0", model, deployment_id);
                let mut key_stats: Vec<InFlightKeyStat> = self
                    .key_metrics
                    .iter()
                    .filter(|kr| kr.key().starts_with(&prefix))
                    .filter_map(|kr| {
                        let count = kr.value().request_count.load(Ordering::Relaxed);
                        if count > 0 {
                            let key_alias = kr.key()[prefix.len()..].to_string();
                            Some(InFlightKeyStat {
                                key_alias,
                                request_count: count,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                // Sort by request count descending.
                key_stats.sort_by(|a, b| b.request_count.cmp(&a.request_count));

                Some(DeploymentInFlightStat {
                    model: model.to_string(),
                    deployment_id: deployment_id.to_string(),
                    inflight_requests: r.value().request_count.load(Ordering::Relaxed),
                    inflight_input_chars: r.value().input_chars.load(Ordering::Relaxed),
                    key_stats,
                })
            })
            .collect()
    }
}

impl Default for InFlightTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl InFlightGuard {
    /// Acquire an in-flight slot. The returned guard decrements the
    /// counters when dropped, so it **must** live until the request completes.
    ///
    /// For streaming responses, move the guard into a wrapper stream so it
    /// is dropped when the stream finishes (or the connection is torn down).
    pub fn new(tracker: Arc<InFlightTracker>, model: &str, input_chars: u64) -> Self {
        {
            let metrics = tracker
                .metrics
                .entry(model.to_string())
                .or_insert_with(|| InFlightMetrics {
                    request_count: AtomicU64::new(0),
                    input_chars: AtomicU64::new(0),
                });
            metrics.request_count.fetch_add(1, Ordering::Relaxed);
            metrics.input_chars.fetch_add(input_chars, Ordering::Relaxed);
        }

        Self {
            tracker,
            model: model.to_string(),
            input_chars,
            deployment_key: None,
            key_tracking_key: None,
        }
    }

    /// Acquire an in-flight slot with per-deployment tracking.
    /// In addition to model-level tracking, also tracks per `(model, deployment_id)`.
    pub fn new_for_deployment(
        tracker: Arc<InFlightTracker>,
        model: &str,
        deployment_id: &str,
        input_chars: u64,
    ) -> Self {
        Self::new_for_deployment_with_key(tracker, model, deployment_id, input_chars, None, "")
    }

    /// Acquire an in-flight slot with per-deployment and per-key tracking.
    pub fn new_for_deployment_with_key(
        tracker: Arc<InFlightTracker>,
        model: &str,
        deployment_id: &str,
        input_chars: u64,
        key_alias: Option<&str>,
        key_hash: &str,
    ) -> Self {
        // Model-level tracking.
        {
            let metrics = tracker
                .metrics
                .entry(model.to_string())
                .or_insert_with(|| InFlightMetrics {
                    request_count: AtomicU64::new(0),
                    input_chars: AtomicU64::new(0),
                });
            metrics.request_count.fetch_add(1, Ordering::Relaxed);
            metrics.input_chars.fetch_add(input_chars, Ordering::Relaxed);
        }

        // Deployment-level tracking.
        let deployment_key = format!("{}\0{}", model, deployment_id);
        {
            let dm = tracker
                .deployment_metrics
                .entry(deployment_key.clone())
                .or_insert_with(|| DeploymentMetrics {
                    request_count: AtomicU64::new(0),
                    input_chars: AtomicU64::new(0),
                });
            dm.request_count.fetch_add(1, Ordering::Relaxed);
            dm.input_chars.fetch_add(input_chars, Ordering::Relaxed);
        }

        // Key-level tracking.
        let key_id = key_alias.unwrap_or(key_hash);
        let key_tracking_key = format!("{}\0{}\0{}", model, deployment_id, key_id);
        {
            let km = tracker
                .key_metrics
                .entry(key_tracking_key.clone())
                .or_insert_with(|| InFlightKeyMetrics {
                    request_count: AtomicU64::new(0),
                });
            km.request_count.fetch_add(1, Ordering::Relaxed);
        }

        Self {
            tracker,
            model: model.to_string(),
            input_chars,
            deployment_key: Some(deployment_key),
            key_tracking_key: Some(key_tracking_key),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Decrement model-level metrics.
        if let Some(metrics) = self.tracker.metrics.get(&self.model) {
            metrics.request_count.fetch_sub(1, Ordering::Relaxed);
            metrics.input_chars.fetch_sub(self.input_chars, Ordering::Relaxed);
        }

        // Decrement deployment-level metrics.
        if let Some(ref dk) = self.deployment_key {
            if let Some(dm) = self.tracker.deployment_metrics.get(dk) {
                dm.request_count.fetch_sub(1, Ordering::Relaxed);
                dm.input_chars.fetch_sub(self.input_chars, Ordering::Relaxed);
            }
        }

        // Decrement key-level metrics. Remove entry if count reaches zero
        // to prevent unbounded DashMap growth.
        if let Some(ref kk) = self.key_tracking_key {
            let should_remove = self.tracker.key_metrics.get(kk).map_or(false, |km| {
                km.request_count.fetch_sub(1, Ordering::Relaxed) == 1
            });
            if should_remove {
                drop(self.tracker.key_metrics.get(kk));
                self.tracker.key_metrics.remove(kk);
            }
        }
    }
}
