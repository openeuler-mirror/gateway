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

    // ── L2: participation set = DECLARED instances only ──
    // `max_context_tokens` is the sole participation signal (the pool
    // toggle was removed). Undeclared instances are never TARGETS; but a
    // context error that fired on an UNDECLARED instance is still rescued
    // — the request fell to it via normal routing and the declared subset
    // may fit it (the "救" scheme). Semantics:
    //   failed instance declared (tier T) → strictly larger tiers eligible
    //   failed instance undeclared            → ALL declared tiers eligible
    //   declared subset empty                 → PoolInert (defensive;
    //                                          the capable() precheck
    //                                          normally routes these to
    //                                          the handler's else branch)
    let failed_max = failed_provider.max_context_tokens();
    let mut tiers = std::collections::BTreeSet::new();
    for c in route.candidates {
        if let Some(v) = c.max_context_tokens() {
            tiers.insert(v);
        }
    }
    if tiers.is_empty() {
        return FallbackDecision::PoolInert;
    }

    let larger: Vec<u64> = match failed_max {
        Some(fm) => tiers.iter().copied().filter(|&v| v > fm).collect(),
        // Undeclared-origin rescue (first-class, not the old defensive
        // branch): pick the smallest declared tier satisfying the parsed
        // request size, else the largest.
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

    // ── Tier candidates: declared instances of the target tier only
    // (undeclared are never targets), excluding the failed deployment
    // itself. For an undeclared-origin rescue the failed instance is not
    // in any declared tier, so the ptr_eq guard is a no-op. ──
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
    if patterns.is_empty() {
        return false;
    }
    // Declaration is the sole participation signal (pool toggle removed).
    // A pool can roll back SOMETHING when either:
    //   - a declared instance can fall up (≥2 distinct declared tiers), or
    //   - an undeclared instance exists whose failures can be rescued by
    //     the declared subset (even a single declared tier suffices).
    // All-undeclared pools and all-declared-single-tier pools can't.
    let mut tiers = std::collections::BTreeSet::new();
    let mut undeclared = false;
    for c in candidates {
        match c.max_context_tokens() {
            Some(v) => {
                tiers.insert(v);
            }
            None => {
                undeclared = true;
            }
        }
    }
    !tiers.is_empty() && (tiers.len() >= 2 || undeclared)
}

