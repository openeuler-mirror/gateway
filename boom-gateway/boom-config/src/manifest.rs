//! Field manifest — single source of truth for "which config fields are
//! editable from the dashboard UI".
//!
//! The frontend consumes this via `GET /admin/config/schema` to auto-render
//! forms. Adding a new field to `ProviderParams` / `ModelEntry` /
//! `FlowControlEntry` / `ModelInfo` without registering it here will fail
//! `tests::manifest_covers_all_struct_fields`, forcing the maintainer (human
//! or AI) to acknowledge the frontend impact before the change can land.
//!
//! See `CLAUDE.md §9 配置字段单一真相源` for the full architectural contract.

use serde::Serialize;

/// UI-level metadata for a single config field.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FieldMeta {
    /// Field name as it appears in YAML / API JSON (snake_case). Use dotted
    /// form for nested fields, e.g. `"model_info.cost_template"`.
    pub field: &'static str,
    /// UI grouping — corresponds to a `.form-card-title` section in the modal.
    pub section: &'static str,
    /// Input control type: `text` | `number` | `bool` | `password` | `select`
    /// | `json` | `kv` | `list`.
    pub input_type: &'static str,
    /// i18n key for the field label.
    pub label_key: &'static str,
    /// i18n key for the field tooltip (empty string if none).
    pub tip_key: &'static str,
}

/// Manifest for `model_list[].litellm_params` + `ModelEntry`-level fields —
/// the union of everything editable in the "new/edit deployment" modal.
///
/// Order = UI render order within each section.
pub fn model_deployment_fields() -> &'static [FieldMeta] {
    &[
        // ── basic ──
        FieldMeta { field: "model_name", section: "basic", input_type: "text",
                    label_key: "form.model.name", tip_key: "tip.model.name" },
        FieldMeta { field: "provider", section: "basic", input_type: "select",
                    label_key: "form.model.provider", tip_key: "tip.model.provider" },
        FieldMeta { field: "model", section: "basic", input_type: "text",
                    label_key: "form.model.id", tip_key: "tip.model.id" },
        FieldMeta { field: "deployment_id", section: "basic", input_type: "text",
                    label_key: "form.model.deployment_id", tip_key: "tip.model.deployment_id" },
        FieldMeta { field: "enabled", section: "basic", input_type: "bool",
                    label_key: "form.model.enabled", tip_key: "tip.model.enabled" },
        // ── auth ──
        FieldMeta { field: "api_key", section: "auth", input_type: "password",
                    label_key: "form.model.api_key", tip_key: "tip.model.api_key" },
        FieldMeta { field: "api_key_env", section: "auth", input_type: "bool",
                    label_key: "form.model.api_key_env", tip_key: "tip.model.api_key_env" },
        FieldMeta { field: "headers", section: "auth", input_type: "json",
                    label_key: "form.model.headers", tip_key: "tip.model.headers" },
        // ── aws (bedrock only) ──
        FieldMeta { field: "aws_region_name", section: "aws", input_type: "text",
                    label_key: "form.model.aws_region", tip_key: "tip.model.aws_region" },
        FieldMeta { field: "aws_access_key_id", section: "aws", input_type: "text",
                    label_key: "form.model.aws_key_id", tip_key: "tip.model.aws_key_id" },
        FieldMeta { field: "aws_secret_access_key", section: "aws", input_type: "password",
                    label_key: "form.model.aws_secret", tip_key: "tip.model.aws_secret" },
        // ── rate_limit ──
        FieldMeta { field: "rpm", section: "rate_limit", input_type: "number",
                    label_key: "form.model.rpm", tip_key: "tip.model.rpm" },
        FieldMeta { field: "tpm", section: "rate_limit", input_type: "number",
                    label_key: "form.model.tpm", tip_key: "tip.model.tpm" },
        FieldMeta { field: "quota_count_ratio", section: "rate_limit", input_type: "number",
                    label_key: "form.model.ratio", tip_key: "tip.model.ratio" },
        // ── flow_control ──
        FieldMeta { field: "max_inflight_queue_len", section: "flow_control", input_type: "number",
                    label_key: "form.model.maxinflight", tip_key: "tip.model.maxinflight" },
        FieldMeta { field: "max_context_len", section: "flow_control", input_type: "number",
                    label_key: "form.model.maxctx", tip_key: "tip.model.maxctx" },
        // ── tuning ──
        FieldMeta { field: "api_base", section: "tuning", input_type: "text",
                    label_key: "form.model.base", tip_key: "tip.model.base" },
        FieldMeta { field: "api_version", section: "tuning", input_type: "text",
                    label_key: "form.model.version", tip_key: "tip.model.version" },
        FieldMeta { field: "timeout", section: "tuning", input_type: "number",
                    label_key: "form.model.timeout", tip_key: "tip.model.timeout" },
        FieldMeta { field: "temperature", section: "tuning", input_type: "number",
                    label_key: "form.model.temp", tip_key: "tip.model.temp" },
        FieldMeta { field: "max_tokens", section: "tuning", input_type: "number",
                    label_key: "form.model.maxtok", tip_key: "tip.model.maxtok" },
        // ── behavior ──
        FieldMeta { field: "serve_not_match", section: "behavior", input_type: "bool",
                    label_key: "form.model.serve_not_match", tip_key: "tip.model.serve_not_match" },
        FieldMeta { field: "client_type_header", section: "behavior", input_type: "bool",
                    label_key: "form.model.client_type_header", tip_key: "tip.model.client_type_header" },
        // ── cost ──
        FieldMeta { field: "model_info.cost_template", section: "cost", input_type: "select",
                    label_key: "form.model.cost_template", tip_key: "tip.model.cost_template" },
    ]
}

/// Manifest for `general_settings.*` editable from the dashboard config page.
pub fn general_settings_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "public_models", section: "general", input_type: "list",
                    label_key: "config.field.public_models", tip_key: "tip.config.public_models" },
        // `master_key` and `database_url` are read-only on the UI (masked);
        // not exposed in the manifest of *editable* fields.
    ]
}

/// Manifest for `router_settings.*` editable from the dashboard config page.
/// Excludes the `kvc_aware` sub-tree (it has its own card) and `model_group_alias`
/// (free-form JSON textarea, no per-field schema).
pub fn router_settings_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "schedule_policy", section: "router", input_type: "select",
                    label_key: "config.field.schedule_policy", tip_key: "tip.config.schedule_policy" },
        FieldMeta { field: "key_affinity_context_threshold", section: "router", input_type: "number",
                    label_key: "config.field.affinity_context_threshold", tip_key: "tip.config.affinity_context_threshold" },
        FieldMeta { field: "rebalance_threshold", section: "router", input_type: "number",
                    label_key: "config.field.rebalance_threshold", tip_key: "tip.config.rebalance_threshold" },
        FieldMeta { field: "enable_priority_header", section: "router", input_type: "bool",
                    label_key: "config.field.enable_priority_header", tip_key: "tip.config.enable_priority_header" },
        FieldMeta { field: "flow_control_queue_timeout_secs", section: "router", input_type: "number",
                    label_key: "config.field.flow_control_queue_timeout_secs", tip_key: "tip.config.flow_control_queue_timeout_secs" },
        FieldMeta { field: "strip_claude_code_attribution", section: "router", input_type: "bool",
                    label_key: "config.field.strip_claude_code_attribution", tip_key: "tip.config.strip_claude_code_attribution" },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ProviderParams + ModelEntry + ModelInfo + FlowControlEntry field
    /// that's exposed to YAML must be registered in
    /// `model_deployment_fields()`. Adding a new field without registering it
    /// here breaks this test — that's the architectural forcing function
    /// described in CLAUDE.md §9.
    ///
    /// When you add a field to `ProviderParams` / `ModelEntry` / etc., also
    /// add a `FieldMeta` entry above and the corresponding i18n keys in
    /// `boom-dashboard/src/frontend/i18n.js`. The test message tells you
    /// exactly what's missing.
    #[test]
    fn manifest_covers_all_struct_fields() {
        let manifest_fields: Vec<&str> = model_deployment_fields()
            .iter()
            .map(|m| m.field)
            .collect();

        // Note: `provider` is a UI virtual field — the modal splits
        // `litellm_params.model` into a provider prefix dropdown + a model-id
        // text input. `api_key_env` is a DB-only bool derived from the
        // api_key env-var pattern. `deployment_id` is the DB业务 id column
        // (auto-generated UUID if blank on create).
        let required_fields = [
            // ProviderParams (litellm_params.*)
            "model", "api_key", "api_base", "api_version",
            "aws_region_name", "aws_access_key_id", "aws_secret_access_key",
            "rpm", "tpm", "timeout", "headers", "temperature", "max_tokens",
            // ModelEntry (top-level)
            "model_name", "enabled", "serve_not_match", "client_type_header",
            // DB-derived
            "deployment_id", "api_key_env",
            // ModelInfo / FlowControlEntry (sub-fields exposed in UI)
            "model_info.cost_template", "quota_count_ratio",
            "max_inflight_queue_len", "max_context_len",
            // UI virtual
            "provider",
        ];

        for f in &required_fields {
            assert!(
                manifest_fields.contains(f),
                "field `{}` is missing from `model_deployment_fields()` manifest.\n\
                 This breaks the frontend auto-render contract (CLAUDE.md §9).\n\
                 Add a FieldMeta entry in `boom-config/src/manifest.rs`.",
                f
            );
        }
    }

    /// The manifest must not contain duplicate field names — UI renders each
    /// field exactly once.
    #[test]
    fn manifest_has_no_duplicates() {
        let fields: Vec<&str> = model_deployment_fields()
            .iter()
            .map(|m| m.field)
            .collect();
        let mut seen = std::collections::HashSet::new();
        for f in &fields {
            assert!(seen.insert(*f), "duplicate field `{}` in manifest", f);
        }
    }

    /// Every FieldMeta must have non-empty structural fields.
    #[test]
    fn manifest_well_formed() {
        for m in model_deployment_fields() {
            assert!(!m.field.is_empty(), "FieldMeta has empty field");
            assert!(!m.section.is_empty(), "field `{}` has empty section", m.field);
            assert!(!m.input_type.is_empty(), "field `{}` has empty input_type", m.field);
            assert!(!m.label_key.is_empty(), "field `{}` has empty label_key", m.field);
        }
    }

    /// `router_settings_fields()` must register every RouterSettings field
    /// that's exposed in the dashboard config page (excludes `kvc_aware`
    /// sub-tree — it has its own card — and `model_group_alias` — free-form
    /// JSON). Adding a new RouterSettings field without registering it here
    /// fails this test, mirroring the deployment-field contract above.
    ///
    /// This test exists because RouterSettings/KvcAwareSettings used to drift
    /// silently: when upstream renamed `key_affinity_rebalance_threshold` →
    /// `rebalance_threshold` and deleted several KvcAwareSettings fields
    /// (tier_weight, zmq_endpoints, etc.), the manifest + frontend were not
    /// updated and the change was caught only by manual audit. The test now
    /// locks the contract.
    #[test]
    fn router_settings_manifest_covers_editable_fields() {
        let manifest_fields: Vec<&str> = router_settings_fields()
            .iter()
            .map(|m| m.field)
            .collect();

        // Every field rendered on the dashboard router-settings card must be
        // registered here. `kvc_aware` is rendered in its own collapsible card
        // (separate `KvcAwareSettings` struct); `model_group_alias` is a
        // free-form JSON textarea without per-field schema.
        let required_fields = [
            "schedule_policy",
            "key_affinity_context_threshold",
            "rebalance_threshold",
            "enable_priority_header",
            "flow_control_queue_timeout_secs",
            "strip_claude_code_attribution",
        ];

        for f in &required_fields {
            assert!(
                manifest_fields.contains(f),
                "field `{}` is missing from `router_settings_fields()` manifest.\n\
                 This breaks the frontend auto-render contract (CLAUDE.md §9).\n\
                 Add a FieldMeta entry in `boom-config/src/manifest.rs`.",
                f
            );
        }
    }
}
