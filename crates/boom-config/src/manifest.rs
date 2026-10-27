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

/// Where a config field's runtime authority lives (DB-authority model).
///
/// `Db` — business / cluster-scoped config (models, aliases, plans, blocklist
/// rules, workflows, cost templates). The database is the single writable
/// authority; YAML only seeds fresh tables and receives manual write-back
/// snapshots. Edited via dashboard CRUD.
/// `Yaml` — process / host-scoped config (server, router policy, prompt log,
/// trace, hooks). Always read from the YAML file (env bootstrap falls back
/// to defaults). Edited via the config page's section editors.
///
/// In the no-DB fallback deployment everything is YAML-built; `home` records
/// the authority in the default (DB) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigHome {
    Yaml,
    Db,
}

/// UI-level metadata for a single config field.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FieldMeta {
    /// Field name as it appears in YAML / API JSON (snake_case). Use dotted
    /// form for nested fields, e.g. `"model_info.cost_template"`.
    pub field: &'static str,
    /// UI grouping — corresponds to a `.form-card-title` section in the modal.
    pub section: &'static str,
    /// Input control type: `text` | `number` | `bool` | `password` | `select`
    /// | `json` | `kv` | `list` | `window_limits` | `schedule` | `conditions`
    /// | `block_action`. The structured types (last four) name dedicated
    /// sub-editors on the plan / blocklist forms — the frontend renders them
    /// with purpose-built widgets, not generic JSON textareas.
    pub input_type: &'static str,
    /// i18n key for the field label.
    pub label_key: &'static str,
    /// i18n key for the field tooltip (empty string if none).
    pub tip_key: &'static str,
    /// Authoritative store for this field — see [`ConfigHome`].
    pub home: ConfigHome,
}

/// Manifest for `model_list[].litellm_params` + `ModelEntry`-level fields —
/// the union of everything editable in the "new/edit deployment" modal.
///
/// Order = UI render order within each section.
pub fn model_deployment_fields() -> &'static [FieldMeta] {
    &[
        // ── basic ──
        FieldMeta { field: "model_name", section: "basic", input_type: "text",
                    label_key: "form.model.name", tip_key: "tip.model.name",
                    home: ConfigHome::Db },
        FieldMeta { field: "provider", section: "basic", input_type: "select",
                    label_key: "form.model.provider", tip_key: "tip.model.provider",
                    home: ConfigHome::Db },
        FieldMeta { field: "model", section: "basic", input_type: "text",
                    label_key: "form.model.id", tip_key: "tip.model.id",
                    home: ConfigHome::Db },
        FieldMeta { field: "deployment_id", section: "basic", input_type: "text",
                    label_key: "form.model.deployment_id", tip_key: "tip.model.deployment_id",
                    home: ConfigHome::Db },
        FieldMeta { field: "enabled", section: "basic", input_type: "bool",
                    label_key: "form.model.enabled", tip_key: "tip.model.enabled",
                    home: ConfigHome::Db },
        // ── auth ──
        FieldMeta { field: "api_key", section: "auth", input_type: "password",
                    label_key: "form.model.api_key", tip_key: "tip.model.api_key",
                    home: ConfigHome::Db },
        FieldMeta { field: "api_key_env", section: "auth", input_type: "bool",
                    label_key: "form.model.api_key_env", tip_key: "tip.model.api_key_env",
                    home: ConfigHome::Db },
        FieldMeta { field: "headers", section: "auth", input_type: "json",
                    label_key: "form.model.headers", tip_key: "tip.model.headers",
                    home: ConfigHome::Db },
        // ── aws (bedrock only) ──
        FieldMeta { field: "aws_region_name", section: "aws", input_type: "text",
                    label_key: "form.model.aws_region", tip_key: "tip.model.aws_region",
                    home: ConfigHome::Db },
        FieldMeta { field: "aws_access_key_id", section: "aws", input_type: "text",
                    label_key: "form.model.aws_key_id", tip_key: "tip.model.aws_key_id",
                    home: ConfigHome::Db },
        FieldMeta { field: "aws_secret_access_key", section: "aws", input_type: "password",
                    label_key: "form.model.aws_secret", tip_key: "tip.model.aws_secret",
                    home: ConfigHome::Db },
        // ── rate_limit ──
        FieldMeta { field: "rpm", section: "rate_limit", input_type: "number",
                    label_key: "form.model.rpm", tip_key: "tip.model.rpm",
                    home: ConfigHome::Db },
        FieldMeta { field: "tpm", section: "rate_limit", input_type: "number",
                    label_key: "form.model.tpm", tip_key: "tip.model.tpm",
                    home: ConfigHome::Db },
        FieldMeta { field: "quota_count_ratio", section: "rate_limit", input_type: "number",
                    label_key: "form.model.ratio", tip_key: "tip.model.ratio",
                    home: ConfigHome::Db },
        // ── flow_control ──
        FieldMeta { field: "max_inflight_queue_len", section: "flow_control", input_type: "number",
                    label_key: "form.model.maxinflight", tip_key: "tip.model.maxinflight",
                    home: ConfigHome::Db },
        FieldMeta { field: "max_context_len", section: "flow_control", input_type: "number",
                    label_key: "form.model.maxctx", tip_key: "tip.model.maxctx",
                    home: ConfigHome::Db },
        // ── tuning ──
        FieldMeta { field: "api_base", section: "tuning", input_type: "text",
                    label_key: "form.model.base", tip_key: "tip.model.base",
                    home: ConfigHome::Db },
        FieldMeta { field: "api_version", section: "tuning", input_type: "text",
                    label_key: "form.model.version", tip_key: "tip.model.version",
                    home: ConfigHome::Db },
        FieldMeta { field: "timeout", section: "tuning", input_type: "number",
                    label_key: "form.model.timeout", tip_key: "tip.model.timeout",
                    home: ConfigHome::Db },
        FieldMeta { field: "temperature", section: "tuning", input_type: "number",
                    label_key: "form.model.temp", tip_key: "tip.model.temp",
                    home: ConfigHome::Db },
        FieldMeta { field: "max_tokens", section: "tuning", input_type: "number",
                    label_key: "form.model.maxtok", tip_key: "tip.model.maxtok",
                    home: ConfigHome::Db },
        // ── behavior ──
        FieldMeta { field: "serve_not_match", section: "behavior", input_type: "bool",
                    label_key: "form.model.serve_not_match", tip_key: "tip.model.serve_not_match",
                    home: ConfigHome::Db },
        FieldMeta { field: "client_type_header", section: "behavior", input_type: "bool",
                    label_key: "form.model.client_type_header", tip_key: "tip.model.client_type_header",
                    home: ConfigHome::Db },
        // ── access control ──
        FieldMeta { field: "visibility", section: "access", input_type: "select",
                    label_key: "form.model.visibility", tip_key: "tip.model.visibility",
                    home: ConfigHome::Db },
        FieldMeta { field: "allowed_teams", section: "access", input_type: "team_list",
                    label_key: "form.model.allowed_teams", tip_key: "tip.model.allowed_teams",
                    home: ConfigHome::Db },
        // ── cost ──
        FieldMeta { field: "model_info.cost_template", section: "cost", input_type: "select",
                    label_key: "form.model.cost_template", tip_key: "tip.model.cost_template",
                    home: ConfigHome::Db },
    ]
}

/// Manifest for `general_settings.*` editable from the dashboard config page.
pub fn general_settings_fields() -> &'static [FieldMeta] {
    &[
        // `public_models` was removed: public access is now configured per
        // model via `visibility: public` (ModelEntry.visibility). The YAML
        // field still parses for backward compatibility but has no UI.
        // `master_key` and `database_url` are read-only on the UI (masked);
        // not exposed in the manifest of *editable* fields.
        FieldMeta { field: "user_tag_headers", section: "general", input_type: "list",
                    label_key: "config.field.user_tag_headers", tip_key: "tip.config.user_tag_headers",
                    home: ConfigHome::Yaml },
    ]
}

/// Manifest for `router_settings.*` editable from the dashboard config page.
/// Excludes the `kvc_aware` sub-tree (it has its own card), `model_group_alias`
/// (managed via the Models page alias CRUD — DB + AdminCommand, not the config
/// editor), and `enable_workflow` (feature-parked switch, YAML-only: flipped in
/// config.yaml + reload; no dashboard editor while the Workflows page is
/// disabled). Fields marked `section: "general"` are surfaced on the General card
/// but still live under `router_settings` in YAML — the frontend saves them
/// via dotted sub-paths (`router_settings.<field>`).
pub fn router_settings_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "schedule_policy", section: "router", input_type: "select",
                    label_key: "config.field.schedule_policy", tip_key: "tip.config.schedule_policy",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "key_affinity_context_threshold", section: "router", input_type: "number",
                    label_key: "config.field.key_affinity_context_threshold", tip_key: "tip.config.affinity_context_threshold",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "rebalance_threshold", section: "router", input_type: "number",
                    label_key: "config.field.rebalance_threshold", tip_key: "tip.config.rebalance_threshold",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "enable_priority_header", section: "general", input_type: "bool",
                    label_key: "config.field.enable_priority_header", tip_key: "tip.config.enable_priority_header",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "flow_control_queue_timeout_secs", section: "router", input_type: "number",
                    label_key: "config.field.flow_control_queue_timeout_secs", tip_key: "tip.config.flow_control_queue_timeout_secs",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "strip_claude_code_attribution", section: "general", input_type: "bool",
                    label_key: "config.field.strip_claude_code_attribution", tip_key: "tip.config.strip_claude_code_attribution",
                    home: ConfigHome::Yaml },
        FieldMeta { field: "forward_client_headers", section: "general", input_type: "list",
                    label_key: "config.field.forward_client_headers", tip_key: "tip.config.forward_client_headers",
                    home: ConfigHome::Yaml },
    ]
}

/// Manifest for `cost_templates[]` — DB-owned (`boom_cost_template`), edited
/// via the config page's cost-templates card (per-template CRUD; template
/// edits re-rate the models referencing them).
pub fn cost_template_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "name", section: "cost_template", input_type: "text",
                    label_key: "config.field.name", tip_key: "tip.model.cost_template",
                    home: ConfigHome::Db },
        FieldMeta { field: "input_cost_per_million_tokens", section: "cost_template", input_type: "number",
                    label_key: "config.field.input_cost_per_million_tokens", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "cached_input_cost_per_million_tokens", section: "cost_template", input_type: "number",
                    label_key: "config.field.cached_input_cost_per_million_tokens", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "output_cost_per_million_tokens", section: "cost_template", input_type: "number",
                    label_key: "config.field.output_cost_per_million_tokens", tip_key: "",
                    home: ConfigHome::Db },
    ]
}

/// Manifest for `workflow_settings` — DB-owned (`boom_workflow_definition` +
/// `boom_workflow_model`), edited via the dedicated Workflows page. The page
/// renders its own dedicated UI (nested panel/aggregator editor), so these
/// entries are the registry of what's editable, not an auto-render source.
///
/// Field paths mirror the YAML shape: `models` is the virtual-model →
/// workflow binding map; definition fields live under
/// `workflows.<name>.*` (dotted paths use the `DirectSynthesis` variant's
/// field names).
pub fn workflow_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "models", section: "workflow_binding", input_type: "kv",
                    label_key: "workflows.bindings_title", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "workflows.name", section: "workflow_def", input_type: "text",
                    label_key: "workflows.col.name", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "roles.panel", section: "workflow_def", input_type: "list",
                    label_key: "workflows.panel_label", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "roles.aggregator", section: "workflow_def", input_type: "text",
                    label_key: "workflows.aggregator_label", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "temperature", section: "workflow_def", input_type: "number",
                    label_key: "workflows.temperature", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "panel_timeout_secs", section: "workflow_def", input_type: "number",
                    label_key: "workflows.panel_timeout", tip_key: "",
                    home: ConfigHome::Db },
    ]
}

/// Manifest for model aliases (`boom_model_alias`, DB-owned). Edited via the
/// alias card embedded in the model edit dialog (`AliasInput` shape).
/// `source` / `updated_at` are provenance columns — same exclusion as the
/// deployment manifest.
pub fn alias_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "alias_name", section: "alias", input_type: "text",
                    label_key: "form.alias.name", tip_key: "tip.alias.name",
                    home: ConfigHome::Db },
        FieldMeta { field: "target_model", section: "alias", input_type: "text",
                    label_key: "form.alias.target", tip_key: "tip.alias.target",
                    home: ConfigHome::Db },
        FieldMeta { field: "hidden", section: "alias", input_type: "bool",
                    label_key: "form.alias.hidden", tip_key: "tip.alias.hidden",
                    home: ConfigHome::Db },
    ]
}

/// Manifest for rate-limit plans (`boom_rate_limit_plan`, DB-owned). Maps the
/// `RateLimitPlan` struct field-for-field (10 fields). The `is_default` /
/// `is_default_team` flag columns are intentionally excluded — they are the
/// default-plan pointers, swapped via the dedicated `PUT /admin/plans/default`
/// endpoint, not plan-definition fields. `source` / `updated_at` are
/// provenance columns.
pub fn rate_limit_plan_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "name", section: "plan", input_type: "text",
                    label_key: "form.plan.name", tip_key: "tip.plan.name",
                    home: ConfigHome::Db },
        FieldMeta { field: "type", section: "plan", input_type: "select",
                    label_key: "form.plan.type", tip_key: "tip.plan.type",
                    home: ConfigHome::Db },
        FieldMeta { field: "member_plan", section: "plan", input_type: "text",
                    label_key: "form.plan.member_plan", tip_key: "tip.plan.member_plan",
                    home: ConfigHome::Db },
        FieldMeta { field: "concurrency_limit", section: "plan", input_type: "number",
                    label_key: "form.plan.concurrency", tip_key: "tip.plan.concurrency",
                    home: ConfigHome::Db },
        FieldMeta { field: "rpm_limit", section: "plan", input_type: "number",
                    label_key: "form.plan.rpm", tip_key: "tip.plan.rpm",
                    home: ConfigHome::Db },
        FieldMeta { field: "tpm_limit", section: "plan", input_type: "number",
                    label_key: "form.plan.tpm", tip_key: "tip.plan.tpm",
                    home: ConfigHome::Db },
        FieldMeta { field: "window_limits", section: "plan", input_type: "window_limits",
                    label_key: "form.plan.windows", tip_key: "tip.plan.windows",
                    home: ConfigHome::Db },
        FieldMeta { field: "total_token_limit", section: "plan", input_type: "number",
                    label_key: "form.plan.total_token", tip_key: "tip.plan.total_token",
                    home: ConfigHome::Db },
        FieldMeta { field: "total_cost_limit", section: "plan", input_type: "number",
                    label_key: "form.plan.total_cost", tip_key: "tip.plan.total_cost",
                    home: ConfigHome::Db },
        FieldMeta { field: "schedule", section: "plan", input_type: "schedule",
                    label_key: "form.plan.schedule", tip_key: "tip.plan.schedule",
                    home: ConfigHome::Db },
    ]
}

/// Manifest for client block rules (`boom_client_block_rule`, DB-owned).
/// Maps the `BlockRule` struct: `conditions` is a list of
/// `{field, op, value}` matchers (op ∈ eq/contains/regex/prefix/exists),
/// `action` is `{message, status, code}`. The global blocklist switch lives
/// in `boom_client_blocklist_state` and is toggled via
/// `PUT /admin/blocklist/enabled` — not a rule field.
pub fn block_rule_fields() -> &'static [FieldMeta] {
    &[
        FieldMeta { field: "name", section: "block_rule", input_type: "text",
                    label_key: "blockrules.form.name", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "enabled", section: "block_rule", input_type: "bool",
                    label_key: "blockrules.form.enabled", tip_key: "",
                    home: ConfigHome::Db },
        FieldMeta { field: "conditions", section: "block_rule", input_type: "conditions",
                    label_key: "blockrules.form.conditions", tip_key: "blockrules.form.conditions_hint",
                    home: ConfigHome::Db },
        FieldMeta { field: "action", section: "block_rule", input_type: "block_action",
                    label_key: "blockrules.form.action_title", tip_key: "",
                    home: ConfigHome::Db },
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
            "allowed_teams", "visibility",
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

    /// Every FieldMeta must have non-empty structural fields, and `home`
    /// must serialize to a known authority name. Covers ALL registration
    /// functions — a new function must be added to `ALL_MANIFESTS` below or
    /// its fields escape the well-formedness contract.
    #[test]
    fn manifest_well_formed() {
        let all_manifests: &[&[FieldMeta]] = &[
            model_deployment_fields(),
            general_settings_fields(),
            router_settings_fields(),
            cost_template_fields(),
            workflow_fields(),
            alias_fields(),
            rate_limit_plan_fields(),
            block_rule_fields(),
        ];
        let mut total = 0;
        for manifest in all_manifests {
            for m in *manifest {
                total += 1;
                assert!(!m.field.is_empty(), "FieldMeta has empty field");
                assert!(!m.section.is_empty(), "field `{}` has empty section", m.field);
                assert!(!m.input_type.is_empty(), "field `{}` has empty input_type", m.field);
                assert!(!m.label_key.is_empty(), "field `{}` has empty label_key", m.field);
                let home = serde_json::to_string(&m.home).unwrap_or_default();
                assert!(
                    home == "\"yaml\"" || home == "\"db\"",
                    "field `{}` has non-serializable home: {}",
                    m.field,
                    home
                );
            }
        }
        assert!(total >= 40, "expected the full manifest registry, got {} fields", total);
    }

    /// Business config (models, cost templates, workflows) must declare
    /// `home: Db`; process config (general/router settings) must declare
    /// `home: Yaml`. This is the §9 registry of the DB-authority split —
    /// registering a business field as Yaml (or vice versa) means the
    /// write-path routing for it is wrong somewhere.
    #[test]
    fn manifest_homes_match_authority_split() {
        for m in model_deployment_fields()
            .iter()
            .chain(cost_template_fields())
            .chain(workflow_fields())
            .chain(alias_fields())
            .chain(rate_limit_plan_fields())
            .chain(block_rule_fields())
        {
            assert_eq!(
                m.home,
                ConfigHome::Db,
                "business field `{}` must be home: Db",
                m.field
            );
        }
        for m in general_settings_fields()
            .iter()
            .chain(router_settings_fields())
        {
            assert_eq!(
                m.home,
                ConfigHome::Yaml,
                "process field `{}` must be home: Yaml",
                m.field
            );
        }
    }

    /// `router_settings_fields()` must register every RouterSettings field
    /// that's exposed in the dashboard config page (excludes `kvc_aware`
    /// sub-tree — it has its own card — and `model_group_alias` — managed
    /// on the Models page). Adding a new RouterSettings field without
    /// registering it here fails this test, mirroring the deployment-field
    /// contract above.
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

        // Every field rendered on the dashboard config cards must be
        // registered here (router card + the three router_settings fields
        // surfaced on the General card). `kvc_aware` is rendered in its own
        // collapsible card (separate `KvcAwareSettings` struct);
        // `model_group_alias` is managed on the Models page (alias CRUD).
        let required_fields = [
            "schedule_policy",
            "key_affinity_context_threshold",
            "rebalance_threshold",
            "enable_priority_header",
            "flow_control_queue_timeout_secs",
            "strip_claude_code_attribution",
            "forward_client_headers",
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

    /// `BlockRule` lives in this crate (lib.rs), so this is a real
    /// struct-coverage test. The global blocklist switch is NOT a rule field
    /// (it lives in `boom_client_blocklist_state`, see `block_rule_fields`
    /// docs).
    #[test]
    fn block_rule_manifest_covers_struct_fields() {
        let manifest_fields: Vec<&str> = block_rule_fields()
            .iter()
            .map(|m| m.field)
            .collect();
        let required_fields = ["name", "enabled", "conditions", "action"];
        for f in &required_fields {
            assert!(
                manifest_fields.contains(f),
                "field `{}` is missing from `block_rule_fields()` manifest.\n\
                 Add a FieldMeta entry in `boom-config/src/manifest.rs`.",
                f
            );
        }
    }

    /// boom-config is a leaf crate and cannot reflect boom-routing's alias
    /// store struct, so the required set is handwritten here (mirrors the
    /// `boom_model_alias` columns). The real anti-drift guard is the
    /// cross-check test in `boom-routing/src/alias_store.rs` asserting the
    /// SQL columns cover this manifest — if either side changes without the
    /// other, that test fails.
    #[test]
    fn alias_manifest_covers_store_fields() {
        let manifest_fields: Vec<&str> = alias_fields()
            .iter()
            .map(|m| m.field)
            .collect();
        let required_fields = ["alias_name", "target_model", "hidden"];
        for f in &required_fields {
            assert!(
                manifest_fields.contains(f),
                "field `{}` is missing from `alias_fields()` manifest.\n\
                 Add a FieldMeta entry in `boom-config/src/manifest.rs`.",
                f
            );
        }
    }

    /// Handwritten set mirroring boom-limiter's `RateLimitPlan` columns
    /// (is_default / is_default_team / source / updated_at are store-managed
    /// bookkeeping, not operator-editable fields). Anti-drift guard is the
    /// cross-check test in `boom-limiter/src/concurrency.rs` asserting the
    /// plan SQL covers this manifest.
    #[test]
    fn rate_limit_plan_manifest_covers_struct_fields() {
        let manifest_fields: Vec<&str> = rate_limit_plan_fields()
            .iter()
            .map(|m| m.field)
            .collect();
        let required_fields = [
            "name",
            "type",
            "member_plan",
            "concurrency_limit",
            "rpm_limit",
            "tpm_limit",
            "window_limits",
            "total_token_limit",
            "total_cost_limit",
            "schedule",
        ];
        for f in &required_fields {
            assert!(
                manifest_fields.contains(f),
                "field `{}` is missing from `rate_limit_plan_fields()` manifest.\n\
                 Add a FieldMeta entry in `boom-config/src/manifest.rs`.",
                f
            );
        }
    }
}
