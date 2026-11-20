//! Cluster scheduling policy group (`boom_router_policy`, single row).
//!
//! CLAUDE.md §7 three-domain split: these six fields are cluster-visible
//! scheduling behavior — mixed values across instances dilute KVC hit rates
//! and make enforcement depend on which instance a request lands on — so
//! the DB row is authoritative and edits broadcast via
//! `boom_state::CATEGORY_ROUTER_POLICY`. `kvc_max_blocks` (the KVC trie's
//! LRU eviction watermark) belongs here too: it gates eviction behavior,
//! not real memory (a trie block is a hash + counter, ~bytes), so the old
//! "memory budget, per-instance heterogeneous" argument for keeping it in
//! YAML was retired — instances disagreeing on it just drift KVC hit
//! behavior.
//!
//! No in-memory mirror on purpose: the effective values live in the
//! caller's `RouterSettings` (boom-main overlays the adopted row onto the
//! config before building the scheduling policy, and swaps it into the
//! live inner config) — a second copy here would be a drift risk, not a
//! convenience.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// The DB-authoritative scheduling policy group. Field names mirror the
/// YAML `router_settings` counterparts (`kvc_router_ttl_secs` /
/// `kvc_max_blocks` ← `kvc_aware.router_ttl_secs` / `kvc_aware.max_blocks`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterPolicy {
    #[serde(default = "default_schedule_policy")]
    pub schedule_policy: String,
    #[serde(default)]
    pub key_affinity_context_threshold: u64,
    /// `None` (or 0) = key→deployment bindings never expire.
    #[serde(default)]
    pub key_affinity_ttl_secs: Option<u64>,
    #[serde(default = "default_rebalance_threshold")]
    pub rebalance_threshold: u8,
    /// 0 disables the TTL prune (LRU-only). Mirrors
    /// `kvc_aware.router_ttl_secs` YAML validation: finite and >= 0.
    #[serde(default = "default_router_ttl_secs")]
    pub kvc_router_ttl_secs: f64,
    /// KVC trie LRU capacity. Mirrors `kvc_aware.max_blocks` YAML
    /// validation: must be > 0.
    #[serde(default = "default_kvc_max_blocks")]
    pub kvc_max_blocks: usize,
}

fn default_schedule_policy() -> String {
    "round_robin".to_string()
}

fn default_rebalance_threshold() -> u8 {
    20
}

fn default_router_ttl_secs() -> f64 {
    1200.0
}

fn default_kvc_max_blocks() -> usize {
    500_000
}

impl Default for RouterPolicy {
    fn default() -> Self {
        Self {
            schedule_policy: default_schedule_policy(),
            key_affinity_context_threshold: 0,
            key_affinity_ttl_secs: None,
            rebalance_threshold: default_rebalance_threshold(),
            kvc_router_ttl_secs: default_router_ttl_secs(),
            kvc_max_blocks: default_kvc_max_blocks(),
        }
    }
}

impl RouterPolicy {
    /// Extract the group from the YAML-side settings (used as the first-boot
    /// seed and to compare "effective old" vs "adopted new").
    pub fn from_settings(rs: &boom_config::RouterSettings) -> Self {
        Self {
            schedule_policy: rs.schedule_policy.clone(),
            key_affinity_context_threshold: rs.key_affinity_context_threshold,
            key_affinity_ttl_secs: rs.key_affinity_ttl_secs,
            rebalance_threshold: rs.rebalance_threshold,
            kvc_router_ttl_secs: rs.kvc_aware.router_ttl_secs,
            kvc_max_blocks: rs.kvc_aware.max_blocks,
        }
    }

    /// Overlay the group onto YAML-side settings (only these six fields —
    /// the rest of `RouterSettings` stays instance-owned).
    pub fn apply_to(&self, rs: &mut boom_config::RouterSettings) {
        rs.schedule_policy = self.schedule_policy.clone();
        rs.key_affinity_context_threshold = self.key_affinity_context_threshold;
        rs.key_affinity_ttl_secs = self.key_affinity_ttl_secs;
        rs.rebalance_threshold = self.rebalance_threshold;
        rs.kvc_aware.router_ttl_secs = self.kvc_router_ttl_secs;
        rs.kvc_aware.max_blocks = self.kvc_max_blocks;
    }

    /// API-boundary validation, stricter than the YAML path (which warns and
    /// falls back to round_robin on an unknown policy): reject the write so
    /// the bad value never becomes the cluster-wide authority.
    pub fn validate(&self) -> Result<(), String> {
        const KNOWN: [&str; 5] = ["", "round_robin", "shuffle", "key_affinity", "kvc_aware"];
        if !KNOWN.contains(&self.schedule_policy.as_str()) {
            return Err(format!(
                "schedule_policy must be one of round_robin / shuffle / key_affinity / kvc_aware, got '{}'",
                self.schedule_policy
            ));
        }
        if self.rebalance_threshold == 0 || self.rebalance_threshold > 100 {
            return Err(format!(
                "rebalance_threshold must be 1..=100 (percentage), got {}",
                self.rebalance_threshold
            ));
        }
        if !self.kvc_router_ttl_secs.is_finite() || self.kvc_router_ttl_secs < 0.0 {
            return Err(format!(
                "kvc_router_ttl_secs must be finite and >= 0 (0 disables TTL prune), got {}",
                self.kvc_router_ttl_secs
            ));
        }
        if self.kvc_max_blocks == 0 {
            return Err(format!(
                "kvc_max_blocks must be > 0, got {}",
                self.kvc_max_blocks
            ));
        }
        Ok(())
    }
}

/// Read the single `boom_router_policy` row, seeding it from the YAML-side
/// group on first contact (fresh DB). After that the row is authoritative —
/// the YAML values are seed-only. No `ON CONFLICT` (GaussDB): a lost insert
/// race with a concurrently-booting instance is resolved by re-reading the
/// winner's row, same tolerance as `boom_core::gaussdb_upsert!`.
///
/// A row whose `kvc_max_blocks` is NULL predates the column (one-time
/// migration window): backfill it from the seed's value so the operator's
/// pre-migration setting survives instead of being reset to the default.
pub async fn read_or_seed(
    pool: &PgPool,
    seed: &RouterPolicy,
) -> Result<RouterPolicy, sqlx::Error> {
    if let Some((row, max_blocks_raw)) = read_row_with_raw_max_blocks(pool).await? {
        return match max_blocks_raw {
            Some(v) if v > 0 => Ok(row),
            // NULL = pre-column row: backfill the pre-migration (YAML) value.
            None => {
                backfill_max_blocks_db(pool, seed.kvc_max_blocks).await?;
                tracing::info!(
                    kvc_max_blocks = seed.kvc_max_blocks,
                    "Backfilled boom_router_policy.kvc_max_blocks from pre-migration value"
                );
                Ok(RouterPolicy {
                    kvc_max_blocks: seed.kvc_max_blocks,
                    ..row
                })
            }
            // Dirty (<= 0): heal to the default, not the seed.
            Some(_) => {
                backfill_max_blocks_db(pool, default_kvc_max_blocks()).await?;
                tracing::warn!(
                    kvc_max_blocks = default_kvc_max_blocks(),
                    "Healed dirty boom_router_policy.kvc_max_blocks to default"
                );
                Ok(RouterPolicy {
                    kvc_max_blocks: default_kvc_max_blocks(),
                    ..row
                })
            }
        };
    }
    match insert_row(pool, seed).await {
        Ok(()) => {
            tracing::info!(
                schedule_policy = %seed.schedule_policy,
                "Initialized router policy row from YAML defaults"
            );
            Ok(seed.clone())
        }
        // Lost the race — adopt whatever the winner wrote.
        Err(_) => read_row_with_raw_max_blocks(pool)
            .await?
            .map(|(row, raw)| {
                let kvc_max_blocks = raw
                    .filter(|v| *v > 0)
                    .map(|v| v as usize)
                    .unwrap_or(seed.kvc_max_blocks);
                RouterPolicy {
                    kvc_max_blocks,
                    ..row
                }
            })
            .ok_or_else(|| {
                sqlx::Error::RowNotFound
            }),
    }
}

async fn backfill_max_blocks_db(
    pool: &PgPool,
    kvc_max_blocks: usize,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE boom_router_policy SET kvc_max_blocks = $1 WHERE id = 1"#,
    )
    .bind(kvc_max_blocks as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Validate + persist + broadcast. The bump lives here (store write-method
/// pattern, CLAUDE.md §12): callers cannot forget it. Memory application
/// (policy/trie rebuild) is the caller's job — it needs boom-main state.
pub async fn write_policy_db(pool: &PgPool, policy: &RouterPolicy) -> Result<(), String> {
    policy
        .validate()
        .map_err(|e| format!("invalid router policy: {e}"))?;
    let upsert = async {
        boom_core::gaussdb_upsert!(
            pool,
            || {
                sqlx::query(
                    r#"UPDATE boom_router_policy
                       SET schedule_policy = $1,
                           key_affinity_context_threshold = $2,
                           key_affinity_ttl_secs = $3,
                           rebalance_threshold = $4,
                           kvc_router_ttl_secs = $5,
                           kvc_max_blocks = $6,
                           updated_at = NOW()
                       WHERE id = 1"#,
                )
                .bind(&policy.schedule_policy)
                .bind(policy.key_affinity_context_threshold as i64)
                .bind(policy.key_affinity_ttl_secs.map(|v| v as i64))
                .bind(policy.rebalance_threshold as i16)
                .bind(policy.kvc_router_ttl_secs)
                .bind(policy.kvc_max_blocks as i64)
            },
            || {
                sqlx::query(
                    r#"INSERT INTO boom_router_policy
                       (id, schedule_policy, key_affinity_context_threshold,
                        key_affinity_ttl_secs, rebalance_threshold, kvc_router_ttl_secs,
                        kvc_max_blocks)
                       VALUES (1, $1, $2, $3, $4, $5, $6)"#,
                )
                .bind(&policy.schedule_policy)
                .bind(policy.key_affinity_context_threshold as i64)
                .bind(policy.key_affinity_ttl_secs.map(|v| v as i64))
                .bind(policy.rebalance_threshold as i16)
                .bind(policy.kvc_router_ttl_secs)
                .bind(policy.kvc_max_blocks as i64)
            }
        )?;
        boom_state::revision::bump_best_effort(
            pool,
            boom_state::revision::CATEGORY_ROUTER_POLICY,
        )
        .await;
        Ok::<(), sqlx::Error>(())
    };
    upsert.await.map_err(|e| format!("write boom_router_policy: {e}"))
}

/// Read the row plus the raw (possibly NULL/dirty) `kvc_max_blocks` column,
/// so the caller can distinguish "column not yet migrated" from a value.
/// The returned `RouterPolicy` carries the sanitized max_blocks (default on
/// NULL/dirty); the raw value is authoritative for the backfill decision.
async fn read_row_with_raw_max_blocks(
    pool: &PgPool,
) -> Result<Option<(RouterPolicy, Option<i64>)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, Option<i64>, i16, f64, Option<i64>)>(
        r#"SELECT schedule_policy, key_affinity_context_threshold,
                  key_affinity_ttl_secs, rebalance_threshold, kvc_router_ttl_secs,
                  kvc_max_blocks
           FROM boom_router_policy WHERE id = 1"#,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(schedule_policy, ctx, ttl, rebalance, kvc_ttl, max_blocks_raw)| {
            let kvc_max_blocks = max_blocks_raw
                .filter(|v| *v > 0)
                .map(|v| v as usize)
                .unwrap_or_else(default_kvc_max_blocks);
            (
                RouterPolicy {
                    schedule_policy,
                    key_affinity_context_threshold: ctx.max(0) as u64,
                    key_affinity_ttl_secs: ttl.filter(|v| *v > 0).map(|v| v as u64),
                    rebalance_threshold: rebalance.clamp(0, 100) as u8,
                    kvc_router_ttl_secs: if kvc_ttl.is_finite() && kvc_ttl >= 0.0 {
                        kvc_ttl
                    } else {
                        default_router_ttl_secs()
                    },
                    kvc_max_blocks,
                },
                max_blocks_raw,
            )
        },
    ))
}

async fn insert_row(pool: &PgPool, policy: &RouterPolicy) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO boom_router_policy
           (id, schedule_policy, key_affinity_context_threshold,
            key_affinity_ttl_secs, rebalance_threshold, kvc_router_ttl_secs,
            kvc_max_blocks)
           VALUES (1, $1, $2, $3, $4, $5, $6)"#,
    )
    .bind(&policy.schedule_policy)
    .bind(policy.key_affinity_context_threshold as i64)
    .bind(policy.key_affinity_ttl_secs.map(|v| v as i64))
    .bind(policy.rebalance_threshold as i16)
    .bind(policy.kvc_router_ttl_secs)
    .bind(policy.kvc_max_blocks as i64)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DB columns and the manifest must describe the same six-field
    /// group (CLAUDE.md §9 single-source-of-truth, cross-crate half).
    #[test]
    fn router_policy_group_matches_manifest_db_homes() {
        let manifest = boom_config::manifest::router_settings_fields();
        let expect_db = [
            "schedule_policy",
            "key_affinity_context_threshold",
            "key_affinity_ttl_secs",
            "rebalance_threshold",
            "kvc_aware.router_ttl_secs",
            "kvc_aware.max_blocks",
        ];
        for f in &expect_db {
            let m = manifest
                .iter()
                .find(|m| m.field == *f)
                .unwrap_or_else(|| panic!("manifest missing router field {f}"));
            assert!(
                matches!(m.home, boom_config::manifest::ConfigHome::Db),
                "{f} should be home: Db (kvc_aware.max_blocks joined the group)"
            );
        }
    }

    /// The serde default here must mirror the YAML default in boom-config,
    /// or a PUT omitting the field would silently reset clusters to a
    /// different capacity than fresh YAML installs get.
    #[test]
    fn kvc_max_blocks_default_matches_config_default() {
        assert_eq!(
            default_kvc_max_blocks(),
            boom_config::KvcAwareSettings::default().max_blocks
        );
    }

    #[test]
    fn validate_rejects_unknown_policy_and_bad_ranges() {
        let mut p = RouterPolicy::default();
        assert!(p.validate().is_ok());
        p.schedule_policy = "least_request".into();
        assert!(p.validate().is_err());
        p = RouterPolicy::default();
        p.rebalance_threshold = 0;
        assert!(p.validate().is_err());
        p = RouterPolicy::default();
        p.rebalance_threshold = 101;
        assert!(p.validate().is_err());
        p = RouterPolicy::default();
        p.kvc_router_ttl_secs = -1.0;
        assert!(p.validate().is_err());
        p.kvc_router_ttl_secs = 0.0; // 0 = TTL disabled, valid
        assert!(p.validate().is_ok());
        p = RouterPolicy::default();
        p.kvc_max_blocks = 0;
        assert!(p.validate().is_err());
    }

    #[test]
    fn from_settings_then_apply_to_round_trips() {
        let mut rs = boom_config::RouterSettings::default();
        rs.schedule_policy = "kvc_aware".into();
        rs.key_affinity_context_threshold = 1200;
        rs.key_affinity_ttl_secs = Some(3600);
        rs.rebalance_threshold = 35;
        rs.kvc_aware.router_ttl_secs = 600.0;
        rs.kvc_aware.max_blocks = 123_456;
        // Unrelated fields must survive the overlay untouched.
        rs.enable_workflow = true;

        let p = RouterPolicy::from_settings(&rs);
        let mut out = boom_config::RouterSettings::default();
        p.apply_to(&mut out);
        assert_eq!(out.schedule_policy, "kvc_aware");
        assert_eq!(out.key_affinity_context_threshold, 1200);
        assert_eq!(out.key_affinity_ttl_secs, Some(3600));
        assert_eq!(out.rebalance_threshold, 35);
        assert_eq!(out.kvc_aware.router_ttl_secs, 600.0);
        assert_eq!(out.kvc_aware.max_blocks, 123_456);
        assert!(!out.enable_workflow);
    }
}
