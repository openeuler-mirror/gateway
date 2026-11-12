//! L1+L2 decision: identify the error, gate the pool, pick the target
//! tier, run the second select through the router. Pure — no AppState,
//! no side effects; every exit is a named [`FallbackDecision`] reason.

use boom_core::error::GatewayError;
use boom_core::provider::Provider;
use boom_routing::Router;
use std::sync::Arc;

/// Routing context for the fallback's second select — everything the
/// normal first select used, so the retry rides the identical policy
/// pipeline (just on a tier-confined candidate list).
pub struct RollbackRoute<'a> {
    pub model: &'a str,
    pub key_hash: Option<&'a str>,
    pub input_chars: u64,
    pub prefix_bytes: &'a [u8],
    /// Full online candidate pool for the model (the same list the first
    /// select used).
    pub candidates: &'a [Arc<dyn Provider>],
}

/// Decision outcome from [`select_fallback`] — every exit is a named
/// reason so the stats can count exactly why fallbacks do or don't fire.
pub enum FallbackDecision {
    /// A fallback target was selected; retry should proceed.
    Target(Arc<dyn Provider>),
    /// L1 miss: not an identified context error.
    MissIdentify,
    /// L2 gate: pool not participating (not enrolled / mixed / single tier).
    PoolInert,
    /// L2 gate: pool active but no larger tier online.
    NoTarget,
}

impl FallbackDecision {
    pub fn target(self) -> Option<Arc<dyn Provider>> {
        match self {
            Self::Target(p) => Some(p),
            _ => None,
        }
    }
}

/// L1 + L2 decision: identify the error, gate the pool, pick the target
/// tier, run the second select through the router (full policy pipeline).
/// Every exit is a named [`FallbackDecision`] reason. Flat: identify →
/// gates → tier → one select; no recursion.
pub fn select_fallback(
    error: &GatewayError,
    patterns: &[String],
    route: &RollbackRoute<'_>,
    router: &Router,
    failed_provider: &Arc<dyn Provider>,
) -> FallbackDecision {
    // ── L1: identification (status gate + code/message patterns). ──
    let m = error.context_error_match(patterns);
    if !m.matched {
        return FallbackDecision::MissIdentify;
    }

    // ── L2 gates: pool participation (all-or-nothing on both fields). ──
    let failed_max = failed_provider.max_context_tokens();
    let mut tiers = std::collections::BTreeSet::new();
    for c in route.candidates {
        if !c.rollback_enabled() {
            return FallbackDecision::PoolInert;
        }
        match c.max_context_tokens() {
            Some(v) => {
                tiers.insert(v);
            }
            None => return FallbackDecision::PoolInert,
        }
    }
    if tiers.len() < 2 {
        return FallbackDecision::PoolInert; // single tier: nowhere to fall back to
    }

    // ── Target tier: strictly larger than the failed tier; smallest tier
    // satisfying the parsed request size, else the largest. ──
    let larger: Vec<u64> = match failed_max {
        Some(fm) => tiers.iter().copied().filter(|&v| v > fm).collect(),
        // Failed provider not in the pool anymore (disabled mid-flight?):
        // treat every tier as eligible.
        None => tiers.iter().copied().collect(),
    };
    if larger.is_empty() {
        return FallbackDecision::NoTarget;
    }
    let target_tier = match m.requested_tokens {
        Some(r) => larger
            .iter()
            .copied()
            .find(|&v| v >= r) // smallest satisfying tier
            .unwrap_or(*larger.last().unwrap()), // exceeds all → largest anyway
        None => *larger.last().unwrap(), // unparsed → largest (design §5)
    };

    // ── Tier candidates, excluding the failed deployment itself. ──
    let tier_candidates: Vec<Arc<dyn Provider>> = route
        .candidates
        .iter()
        .filter(|c| c.max_context_tokens() == Some(target_tier))
        .filter(|c| !Arc::ptr_eq(c, failed_provider))
        .cloned()
        .collect();
    if tier_candidates.is_empty() {
        return FallbackDecision::NoTarget;
    }

    // ── Second select: the FULL policy pipeline on the tier's list. ──
    match router.select_with_candidates(
        route.model,
        &tier_candidates,
        route.key_hash,
        route.input_chars,
        route.prefix_bytes,
    ) {
        Some(selection) => FallbackDecision::Target(selection.provider),
        None => FallbackDecision::NoTarget,
    }
}


/// Cheap pre-check: can a rollback possibly happen for this pool? Used by
/// the handlers to decide whether to keep a request clone for the retry
/// (the only per-request cost of the feature when it can't fire).
/// Everything is re-checked by [`select_fallback`] after the actual error
/// arrives — this is purely the clone-cost gate.
pub fn capable(patterns: &[String], candidates: &[Arc<dyn Provider>]) -> bool {
    if patterns.is_empty() || candidates.len() < 2 {
        return false;
    }
    let mut tiers = std::collections::BTreeSet::new();
    for c in candidates {
        if !c.rollback_enabled() {
            return false;
        }
        match c.max_context_tokens() {
            Some(v) => {
                tiers.insert(v);
            }
            None => return false, // mixed pool → inert (all-or-nothing gate)
        }
    }
    tiers.len() >= 2
}

