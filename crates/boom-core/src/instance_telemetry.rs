//! Instance telemetry snapshot shared via `boom_instance_state.snapshot`.
//!
//! Written by each gateway instance every few seconds (boom-main's telemetry
//! task), read by any instance's dashboard to render peer state. The wire
//! format must stay compatible across mixed-version clusters: every field
//! is `#[serde(default)]`, so an old writer's snapshot deserializes with
//! defaults and an old reader ignores unknown fields. To add a new
//! sync'd realtime state, add a defaulted field here, fill it in the
//! writer, and consume it in the reader — no DDL, no migration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Per-instance coarse telemetry. Deployments with no FlowControl config
/// and no in-flight activity are omitted — readers treat missing keys as
/// zero. Keyed by deployment_id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceTelemetry {
    #[serde(default)]
    pub inflight: HashMap<String, InflightStat>,
}

/// Counters only — no per-user detail (queued key lists, dispatched key
/// stats) crosses instances.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InflightStat {
    /// Queued requests (regular + VIP).
    #[serde(default)]
    pub fc_queue: usize,
    #[serde(default)]
    pub in_reqs: u64,
    #[serde(default)]
    pub in_context: u64,
    /// Configured caps (0 when unknown) so the dashboard can show
    /// utilization ratios.
    #[serde(default)]
    pub in_reqs_max: u32,
    #[serde(default)]
    pub in_context_max: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = InstanceTelemetry {
            inflight: HashMap::from([("dep-1".into(), InflightStat {
                fc_queue: 2,
                in_reqs: 5,
                in_context: 9000,
                in_reqs_max: 10,
                in_context_max: 100000,
            })]),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: InstanceTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.inflight["dep-1"].in_reqs, 5);
        assert_eq!(back.inflight["dep-1"].fc_queue, 2);
    }

    #[test]
    fn missing_fields_and_sections_default() {
        // Old/foreign writer shapes must not break the reader.
        let t: InstanceTelemetry = serde_json::from_str("{}").unwrap();
        assert!(t.inflight.is_empty());

        let t: InstanceTelemetry =
            serde_json::from_str(r#"{"inflight":{"dep-1":{"in_reqs":3}}}"#).unwrap();
        assert_eq!(t.inflight["dep-1"].in_reqs, 3);
        assert_eq!(t.inflight["dep-1"].fc_queue, 0);
        assert_eq!(t.inflight["dep-1"].in_context_max, 0);

        // Unknown fields from a NEWER writer are ignored by an older reader.
        let t: InstanceTelemetry =
            serde_json::from_str(r#"{"inflight":{},"future_state":{"a":1}}"#).unwrap();
        assert!(t.inflight.is_empty());
    }
}
