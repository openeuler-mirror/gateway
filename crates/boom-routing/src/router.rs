use arc_swap::ArcSwap;
use boom_core::provider::Provider;
use boom_core::types::{Message, Tool};
use std::sync::Arc;

use crate::auto_router::AutoRouter;
use crate::policy::{SchedulePolicy, Selection};
use crate::{AliasStore, DeploymentStore};

/// Type-erased scheduling policy wrapper for ArcSwap compatibility.
struct PolicyHolder {
    inner: Arc<dyn SchedulePolicy>,
}

impl std::ops::Deref for PolicyHolder {
    type Target = Arc<dyn SchedulePolicy>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Type-erased classifier wrapper for ArcSwap compatibility.
struct ClassifierHolder {
    inner: Option<Arc<AutoRouter>>,
}

/// Unified routing decision engine.
///
/// Owns references to DeploymentStore, AliasStore, a SchedulePolicy,
/// and an optional AutoRouter classifier, providing a single entry
/// point for all routing logic.
pub struct Router {
    deployment_store: Arc<DeploymentStore>,
    alias_store: Arc<AliasStore>,
    policy: ArcSwap<PolicyHolder>,
    classifier: ArcSwap<ClassifierHolder>,
}

impl Router {
    pub fn new(
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        policy: Arc<dyn SchedulePolicy>,
    ) -> Self {
        Self {
            deployment_store,
            alias_store,
            policy: ArcSwap::from_pointee(PolicyHolder { inner: policy }),
            classifier: ArcSwap::from_pointee(ClassifierHolder { inner: None }),
        }
    }

    /// Create a Router with an optional auto router classifier.
    pub fn with_classifier(
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        policy: Arc<dyn SchedulePolicy>,
        classifier: Option<Arc<AutoRouter>>,
    ) -> Self {
        Self {
            deployment_store,
            alias_store,
            policy: ArcSwap::from_pointee(PolicyHolder { inner: policy }),
            classifier: ArcSwap::from_pointee(ClassifierHolder { inner: classifier }),
        }
    }

    /// Hot-swap the scheduling policy (e.g. on config reload).
    pub fn set_policy(&self, policy: Arc<dyn SchedulePolicy>) {
        self.policy.store(Arc::new(PolicyHolder { inner: policy }));
    }

    /// Current scheduling policy name (e.g. "kvc_aware" / "key_affinity" /
    /// "round_robin"). Used for DFX logging so each request record shows which
    /// policy handled it.
    pub fn policy_name(&self) -> String {
        self.policy.load().inner.name().to_string()
    }

    /// Sweep expired key-affinity bindings on the current policy (idle-TTL
    /// hygiene). Delegates through the ArcSwap so it always targets the
    /// post-reload policy. Returns the number of bindings removed.
    pub fn sweep_affinity(&self, now: std::time::Instant) -> usize {
        self.policy.load().inner.sweep_expired(now)
    }

    /// Outcome notification: delegates to the current policy's `on_outcome`
    /// (post-ArcSwap, same pattern as sweep_affinity). Called by the
    /// rollback chain after a successful fallback so key_affinity can
    /// migrate the binding to the deployment that actually served the
    /// request. Idempotent; no-op for policies without outcome state.
    pub fn notify_outcome(
        &self,
        key_hash: Option<&str>,
        model: &str,
        deployment_id: Option<&str>,
        success: bool,
    ) {
        self.policy
            .load()
            .inner
            .on_outcome(key_hash, model, deployment_id, success);
    }

    /// Snapshot of per-deployment affinity binding counts for
    /// logging/observability.
    pub fn affinity_binding_counts(&self) -> Vec<(String, u64)> {
        self.policy.load().inner.affinity_binding_counts()
    }

    /// Hot-swap the auto router classifier (e.g. on config reload).
    pub fn set_classifier(&self, classifier: Option<Arc<AutoRouter>>) {
        self.classifier
            .store(Arc::new(ClassifierHolder { inner: classifier }));
    }

    /// Resolve a model name through alias mapping.
    /// Returns the target model name if `model` is a registered alias.
    pub fn resolve_model(&self, model: &str) -> Option<String> {
        self.alias_store.resolve(model)
    }

    /// Resolve the request model to the actual deployment model_name
    /// (after alias resolution). Returns the original name if it's a direct
    /// deployment or a wildcard.
    pub fn resolve_model_name(&self, model: &str) -> String {
        if self.deployment_store.contains(model) {
            return model.to_string();
        }
        if let Some(target) = self.alias_store.resolve(model) {
            return target;
        }
        model.to_string()
    }

    /// Content-aware model resolution: auto router classification
    /// followed by normal alias resolution.
    ///
    /// Returns the resolved model name for routing (provider selection,
    /// inflight, logging). If the classifier is disabled or the model
    /// doesn't match the virtual name, falls back to `resolve_model_name`.
    pub async fn resolve_request_model(
        &self,
        model: &str,
        messages: &[Message],
        tools: &Option<Vec<Tool>>,
    ) -> String {
        let classifier = self.classifier.load();
        if let Some(ref hr) = classifier.inner {
            if let Some(target) = hr.classify(model, messages, tools).await {
                return target;
            }
        }
        self.resolve_model_name(model)
    }

    /// Check if a model name is the auto router's virtual model.
    pub fn is_auto_virtual_model(&self, model: &str) -> bool {
        self.classifier
            .load()
            .inner
            .as_ref()
            .map_or(false, |c| c.model_name() == model)
    }

    /// KV-cache aware routing: resolves candidates then delegates to
    /// `select_with_context()` with token IDs. Returns a [`Selection`] whose
    /// `kv_hit_ratio` lets the gateway choose full vs incremental reporting.
    pub fn select_provider_with_prefix(
        &self,
        model: &str,
        key_hash: Option<&str>,
        input_chars: u64,
        prefix_bytes: &[u8],
    ) -> Option<Selection> {
        let candidates = self.resolve_candidates(model)?;
        self.policy.load().inner.select_with_context(model, &candidates, key_hash, input_chars, prefix_bytes)
    }

    /// Public access to candidate providers for a model.
    pub fn candidates_for(&self, model: &str) -> Option<Vec<Arc<dyn Provider>>> {
        self.resolve_candidates(model)
    }

    /// KV-aware selection with pre-resolved candidates.
    pub fn select_with_candidates(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        prefix_bytes: &[u8],
    ) -> Option<Selection> {
        if candidates.is_empty() {
            return None;
        }
        self.policy
            .load()
            .inner
            .select_with_context(model, candidates, key_hash, input_chars, prefix_bytes)
    }

    /// Resolve model name to a list of candidate providers.
    fn resolve_candidates(&self, model: &str) -> Option<Vec<Arc<dyn Provider>>> {
        // Exact match first.
        if let Some(ps) = self.deployment_store.get_providers(model) {
            if !ps.is_empty() {
                return Some(ps);
            }
            return None;
        }

        // Alias resolution.
        if let Some(target) = self.alias_store.resolve(model) {
            if let Some(ps) = self.deployment_store.get_providers(&target) {
                if !ps.is_empty() {
                    return Some(ps);
                }
                return None;
            }
            return None;
        }

        // Fallback to catch-all "*" — only for completely unknown model names.
        self.deployment_store.get_providers("*")
    }

    /// Check if a model name has deployments registered.
    pub fn is_model_configured(&self, model: &str) -> bool {
        self.deployment_store.contains(model)
    }

    /// Whether a model is public (visibility=public). Public models bypass
    /// all per-key permission checks.
    pub fn is_public_model(&self, model: &str) -> bool {
        self.deployment_store.is_public_model(model)
    }

    /// Whether a model is private (team ACL attached). Private models bypass
    /// the normal key/team whitelist rules — authorization is solely the
    /// team ACL check in [`Router::team_can_access`].
    pub fn is_private_model(&self, model: &str) -> bool {
        self.deployment_store.is_private_model(model)
    }

    /// Team ACL check for private models. Returns true for public models;
    /// for private models it is the only authorization criterion.
    pub fn team_can_access(&self, model: &str, team_id: Option<&str>) -> bool {
        self.deployment_store.team_can_access(model, team_id)
    }

    /// Return all model names that should be visible in the model list.
    ///
    /// Empty groups are filtered out: delete/rename paths deliberately keep an
    /// empty key under the old model_name (to suppress wildcard fallthrough),
    /// so without this filter deleted models would linger in /v1/models until
    /// the next full reload. A model with zero routable deployments (deleted,
    /// renamed-away, or all deployments disabled) is not listable.
    pub fn visible_model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .deployment_store
            .model_names()
            .into_iter()
            .filter(|k| k != "*" && self.deployment_store.deployment_count(k) > 0)
            .collect();

        for alias_name in self.alias_store.visible_names() {
            if !names.contains(&alias_name) {
                names.push(alias_name);
            }
        }

        // Include auto router virtual model name if enabled.
        if let Some(ref hr) = self.classifier.load().inner {
            let name = hr.model_name().to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }

        names
    }
}
