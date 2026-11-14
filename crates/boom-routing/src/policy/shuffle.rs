use boom_core::provider::Provider;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::SchedulePolicy;

/// Shuffle scheduling: pick a candidate uniformly at random per request.
/// Stateless load-blind baseline, equivalent to litellm's `simple-shuffle`.
///
/// Randomness comes from a global atomic counter hashed through splitmix64 —
/// no RNG state to share, each concurrent request draws a distinct value,
/// and picks are uniformly distributed over candidates. Distribution evens
/// out statistically (law of large numbers) rather than per-window like
/// round_robin.
pub struct ShufflePolicy {
    counter: AtomicU64,
}

impl ShufflePolicy {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

/// splitmix64 finalizer — turns a sequential counter into a well-distributed
/// u64 (uniform across the whole range, no visible correlation between
/// consecutive requests).
#[inline]
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

impl SchedulePolicy for ShufflePolicy {
    fn select(
        &self,
        _model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }
        let draw = self.counter.fetch_add(1, Ordering::Relaxed);
        let idx = (mix64(draw) % candidates.len() as u64) as usize;
        Some(candidates[idx].clone())
    }

    fn name(&self) -> &str {
        "shuffle"
    }
}

impl Default for ShufflePolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse};
    use boom_core::GatewayError;
    use std::collections::HashMap;

    struct FakeProvider;
    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            unimplemented!("distribution test only inspects Arc identity")
        }
        async fn chat_stream(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<boom_core::types::ChatStream, GatewayError> {
            unimplemented!()
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn models(&self) -> &[String] {
            &[]
        }
    }

    #[test]
    fn test_shuffle_picks_uniformly_over_many_draws() {
        let policy = ShufflePolicy::new();
        let candidates: Vec<Arc<dyn Provider>> = (0..3)
            .map(|_| Arc::new(FakeProvider) as Arc<dyn Provider>)
            .collect();
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for _ in 0..30_000 {
            let picked = policy
                .select("m", &candidates, None, 0)
                .expect("non-empty candidates");
            let idx = candidates
                .iter()
                .position(|c| std::sync::Arc::ptr_eq(c, &picked))
                .expect("picked from candidates");
            *counts.entry(idx).or_insert(0) += 1;
        }
        // 3 candidates × 30k draws → expect ~10k each; allow ±5% tolerance.
        for (idx, count) in &counts {
            assert!(
                (9_500..=10_500).contains(count),
                "candidate {idx} got {count}/30000 draws — distribution skewed"
            );
        }
        assert_eq!(counts.len(), 3, "every candidate must be picked sometimes");
    }

    #[test]
    fn test_shuffle_empty_and_single() {
        let policy = ShufflePolicy::new();
        assert!(policy.select("m", &[], None, 0).is_none());
        let single: Vec<Arc<dyn Provider>> = vec![Arc::new(FakeProvider)];
        assert!(policy.select("m", &single, None, 0).is_some());
    }

    #[test]
    fn test_shuffle_name() {
        assert_eq!(ShufflePolicy::new().name(), "shuffle");
    }
}
