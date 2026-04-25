use arc_swap::ArcSwap;
use boom_core::provider::Provider;
use std::sync::Arc;

use crate::policy::SchedulePolicy;
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

/// Unified routing decision engine.
///
/// Owns references to DeploymentStore, AliasStore, and a SchedulePolicy,
/// providing a single entry point for all routing logic: provider selection,
/// alias resolution, model visibility, and model configuration checks.
pub struct Router {
    deployment_store: Arc<DeploymentStore>,
    alias_store: Arc<AliasStore>,
    policy: ArcSwap<PolicyHolder>,
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
        }
    }

    /// Hot-swap the scheduling policy (e.g. on config reload).
    pub fn set_policy(&self, policy: Arc<dyn SchedulePolicy>) {
        self.policy.store(Arc::new(PolicyHolder { inner: policy }));
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

    /// Core routing: exact match → alias resolution → wildcard "*".
    ///
    /// Resolves candidates then delegates to the SchedulePolicy for selection.
    pub fn select_provider(
        &self,
        model: &str,
        key_hash: Option<&str>,
        input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        let candidates = self.resolve_candidates(model)?;
        self.policy.load().inner.select(model, &candidates, key_hash, input_chars)
    }

    /// Resolve model name to a list of candidate providers.
    ///
    /// Priority: exact match → alias resolution → wildcard "*".
    ///
    /// Important: if the model IS configured (key exists in deployment_store)
    /// but all its deployments are down (empty provider list), we return None
    /// immediately — we do NOT fall through to the wildcard. This ensures that
    /// a user requesting "gpt-4" never gets silently routed to a different model.
    /// The wildcard catch-all only applies to completely unknown model names.
    fn resolve_candidates(&self, model: &str) -> Option<Vec<Arc<dyn Provider>>> {
        // Exact match first.
        if let Some(ps) = self.deployment_store.get_providers(model) {
            if !ps.is_empty() {
                return Some(ps);
            }
            // Model is configured but all deployments are down — don't fall through.
            return None;
        }

        // Alias resolution.
        if let Some(target) = self.alias_store.resolve(model) {
            if let Some(ps) = self.deployment_store.get_providers(&target) {
                if !ps.is_empty() {
                    return Some(ps);
                }
                // Alias target is configured but all down — don't fall through.
                return None;
            }
            // Alias exists but target has no deployment entry — misconfiguration.
            // Still don't fall through to wildcard.
            return None;
        }

        // Fallback to catch-all "*" — only for completely unknown model names.
        self.deployment_store.get_providers("*")
    }

    /// Check if a model name has deployments registered.
    pub fn is_model_configured(&self, model: &str) -> bool {
        self.deployment_store.contains(model)
    }

    /// Return all model names that should be visible in the model list.
    pub fn visible_model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .deployment_store
            .model_names()
            .into_iter()
            .filter(|k| k != "*")
            .collect();

        for alias_name in self.alias_store.visible_names() {
            if !names.contains(&alias_name) {
                names.push(alias_name);
            }
        }

        names
    }
}
