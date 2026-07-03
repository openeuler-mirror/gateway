use arc_swap::ArcSwap;
use boom_core::provider::Provider;
use boom_core::types::{Message, Tool};
use std::sync::Arc;

use crate::hybrid_router::HybridRouter;
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
    inner: Option<Arc<HybridRouter>>,
}

/// Unified routing decision engine.
///
/// Owns references to DeploymentStore, AliasStore, a SchedulePolicy,
/// and an optional HybridRouter classifier, providing a single entry
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

    /// Create a Router with an optional hybrid router classifier.
    pub fn with_classifier(
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        policy: Arc<dyn SchedulePolicy>,
        classifier: Option<Arc<HybridRouter>>,
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

    /// Hot-swap the hybrid router classifier (e.g. on config reload).
    pub fn set_classifier(&self, classifier: Option<Arc<HybridRouter>>) {
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

    /// Content-aware model resolution: hybrid router classification
    /// followed by normal alias resolution.
    ///
    /// Returns the resolved model name for routing (provider selection,
    /// inflight, logging). If the classifier is disabled or the model
    /// doesn't match the virtual name, falls back to `resolve_model_name`.
    pub fn resolve_request_model(
        &self,
        model: &str,
        messages: &[Message],
        tools: &Option<Vec<Tool>>,
    ) -> String {
        let classifier = self.classifier.load();
        if let Some(ref hr) = classifier.inner {
            if let Some(target) = hr.classify(model, messages, tools) {
                return target;
            }
        }
        self.resolve_model_name(model)
    }

    /// Check if a model name is the hybrid router's virtual model.
    pub fn is_hybrid_virtual_model(&self, model: &str) -> bool {
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
        token_ids: &[u32],
    ) -> Option<Selection> {
        let candidates = self.resolve_candidates(model)?;
        self.policy.load().inner.select_with_context(model, &candidates, key_hash, input_chars, token_ids)
    }

    /// Public access to candidate providers for a model. Used by KV-aware
    /// routing to obtain a deployment's vLLM endpoint for `/tokenize` BEFORE
    /// the routing decision (tokenization is model-level, identical across
    /// deployments of the same model, so any candidate suffices).
    pub fn candidates_for(&self, model: &str) -> Option<Vec<Arc<dyn Provider>>> {
        self.resolve_candidates(model)
    }

    /// KV-aware selection with pre-resolved candidates (pairs with
    /// `candidates_for` so the caller can tokenize via a candidate's vLLM
    /// between resolving candidates and selecting).
    pub fn select_with_candidates(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        token_ids: &[u32],
    ) -> Option<Selection> {
        if candidates.is_empty() {
            return None;
        }
        self.policy
            .load()
            .inner
            .select_with_context(model, candidates, key_hash, input_chars, token_ids)
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

        // Include hybrid router virtual model name if enabled.
        if let Some(ref hr) = self.classifier.load().inner {
            let name = hr.model_name().to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }

        names
    }
}
