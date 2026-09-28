//! boom-gatekeeper — client blocklist rule engine.
//!
//! Owns the `boom_client_block_rule` table (DDL in [`migrations`]), the
//! in-memory [`BlockRuleStore`] (survives config reloads), and the matching
//! engine ([`matcher`]). Rules are evaluated after authentication, before
//! model access checks — see `boom-main`'s `check_client_block_rules`.
//!
//! Rule semantics: conditions inside a rule are ANDed; rules are evaluated in
//! `name` lexicographic order and the first enabled matching rule wins.
//! Header conditions are reordered ahead of body conditions at compile time
//! (cheap short-circuit first).

pub mod matcher;
pub mod migrations;
pub mod store_db;

pub use store_db::{BlockRuleInput, BlockRuleRow};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use boom_config::{BlockAction, BlockRule, MatchCondition};
use serde_json::Value;

// Re-export the config-side rule types so consumers (boom-dashboard, which
// must NOT depend on boom-config per CLAUDE.md) can reference them through
// this crate alone.
pub use boom_config::{BlockAction as ConfigBlockAction, BlockOp, BlockRule as ConfigBlockRule, MatchCondition as ConfigMatchCondition};

/// A rule with its regexes pre-compiled and conditions reordered for cheap
/// short-circuiting. Built via [`BlockRuleStore::compile_rule`].
pub struct CompiledRule {
    pub name: String,
    pub enabled: bool,
    pub conditions: Vec<CompiledCondition>,
    pub action: BlockAction,
}

pub struct CompiledCondition {
    pub cond: MatchCondition,
    pub regex: Option<regex::Regex>,
}

/// Cap diagnostic values so a whole-body match or a huge field doesn't bloat
/// the debug entry (char-boundary-safe).
fn truncate_debug_value(s: &str) -> String {
    const MAX: usize = 200;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX).collect();
    format!("{}…", truncated)
}

/// What a matched rule decided to answer, with per-condition match evidence
/// for diagnostics (dashboard debug detail): each condition's selector,
/// operator, expected value, and the actual value resolved from the request
/// (truncated).
#[derive(serde::Serialize)]
pub struct MatchedConditionDetail {
    pub field: String,
    pub op: boom_config::BlockOp,
    pub value: String,
    pub actual: Option<String>,
}

/// What a matched rule decided to answer.
pub struct BlockActionMatch {
    pub rule_name: String,
    pub action: BlockAction,
    pub matched: Vec<MatchedConditionDetail>,
}

/// In-memory store of compiled block rules. Survives config reloads; content
/// is rebuilt from YAML + DB on reload and updated incrementally by dashboard
/// CRUD (DB write → in-memory upsert, no reload needed).
///
/// Ordering: `BTreeMap` keyed by rule name gives deterministic first-match
/// semantics under concurrent readers (DashMap iteration order is not).
pub struct BlockRuleStore {
    rules: RwLock<BTreeMap<String, Arc<CompiledRule>>>,
    enabled: AtomicBool,
}

impl Default for BlockRuleStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockRuleStore {
    pub fn new() -> Self {
        Self {
            rules: RwLock::new(BTreeMap::new()),
            enabled: AtomicBool::new(true),
        }
    }

    // ── in-memory ──

    pub fn set_enabled(&self, v: bool) {
        self.enabled.store(v, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        self.rules.write().expect("block rule store lock").clear();
    }

    pub fn len(&self) -> usize {
        self.rules.read().expect("block rule store lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validate and compile a rule. This is the single validation entry point:
    /// dashboard CRUD rejects bad input (400) by calling this first; YAML/DB
    /// loaders call it and skip+warn on failure. Guarantees:
    /// - non-empty name, non-empty conditions (empty conditions would match
    ///   EVERYTHING — rejected as a foot-gun)
    /// - all `regex` conditions compile
    /// - known field selector prefix (`header.*` / `body` / `body.*`)
    pub fn compile_rule(rule: &BlockRule) -> Result<CompiledRule, String> {
        if rule.name.trim().is_empty() {
            return Err("rule name must not be empty".into());
        }
        if rule.conditions.is_empty() {
            return Err(format!(
                "rule '{}' has no conditions — an empty AND-list would match every request",
                rule.name
            ));
        }
        let mut compiled = Vec::with_capacity(rule.conditions.len());
        for cond in &rule.conditions {
            let field = cond.field.trim();
            if !(field == "body" || field.starts_with("body.") || field.starts_with("header.")) {
                return Err(format!(
                    "rule '{}': field '{}' must be 'header.<name>', 'body.<pointer>' or 'body'",
                    rule.name, field
                ));
            }
            if let Some(status) = rule.action.status {
                if !(400..=599).contains(&status) {
                    return Err(format!(
                        "rule '{}': action.status must be 400-599, got {}",
                        rule.name, status
                    ));
                }
            }
            let regex = if matches!(cond.op, boom_config::BlockOp::Regex) {
                Some(regex::Regex::new(&cond.value).map_err(|e| {
                    format!("rule '{}': invalid regex '{}': {}", rule.name, cond.value, e)
                })?)
            } else {
                None
            };
            let mut cond = cond.clone();
            cond.field = field.to_string();
            compiled.push(CompiledCondition { cond, regex });
        }
        // Header conditions first — cheap lookups short-circuit before any
        // body serialization work.
        compiled.sort_by_key(|c| !c.cond.field.starts_with("header."));
        Ok(CompiledRule {
            name: rule.name.clone(),
            enabled: rule.enabled,
            conditions: compiled,
            action: rule.action.clone(),
        })
    }

    /// Insert/replace a pre-compiled rule (compile_rule output or
    /// store_db loading path).
    pub fn insert_compiled(&self, compiled: CompiledRule) {
        self.rules
            .write()
            .expect("block rule store lock")
            .insert(compiled.name.clone(), Arc::new(compiled));
    }

    /// Convenience: compile + insert. Returns the compile error (if any)
    /// without inserting — callers decide whether to warn (loaders) or
    /// reject (dashboard CRUD, which should call compile_rule first).
    pub fn insert_rule(&self, rule: &BlockRule) -> Result<(), String> {
        let compiled = Self::compile_rule(rule)?;
        self.insert_compiled(compiled);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> bool {
        self.rules
            .write()
            .expect("block rule store lock")
            .remove(name)
            .is_some()
    }

    /// Evaluate all rules against a request. `header_lookup` must be
    /// case-insensitive on the header name (axum's `HeaderMap::get` is).
    /// Returns the first match in rule-name order, or None.
    pub fn check<F>(&self, header_lookup: F, body: &Value) -> Option<BlockActionMatch>
    where
        F: Fn(&str) -> Option<String>,
    {
        if !self.is_enabled() || self.is_empty() {
            return None;
        }
        let snapshot: Vec<Arc<CompiledRule>> = {
            let guard = self.rules.read().expect("block rule store lock");
            guard.values().cloned().collect()
        };
        let mut body_text: Option<String> = None;
        for rule in &snapshot {
            if !rule.enabled {
                continue;
            }
            let all = rule.conditions.iter().all(|c| {
                matcher::eval_condition(&c.cond, c.regex.as_ref(), &header_lookup, body, &mut body_text)
            });
            if all {
                let matched = rule
                    .conditions
                    .iter()
                    .map(|c| {
                        let actual = match matcher::resolve_field(
                            &c.cond.field,
                            &header_lookup,
                            body,
                            &mut body_text,
                        ) {
                            matcher::FieldValue::Text(s) => Some(truncate_debug_value(&s)),
                            matcher::FieldValue::Missing => None,
                        };
                        MatchedConditionDetail {
                            field: c.cond.field.clone(),
                            op: c.cond.op,
                            value: c.cond.value.clone(),
                            actual,
                        }
                    })
                    .collect();
                return Some(BlockActionMatch {
                    rule_name: rule.name.clone(),
                    action: rule.action.clone(),
                    matched,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_config::{BlockAction, BlockOp, MatchCondition};
    use serde_json::json;

    fn cond(field: &str, op: BlockOp, value: &str) -> MatchCondition {
        MatchCondition {
            field: field.into(),
            op,
            value: value.into(),
        }
    }

    fn rule(name: &str, conditions: Vec<MatchCondition>, message: &str) -> BlockRule {
        BlockRule {
            name: name.into(),
            enabled: true,
            conditions,
            action: BlockAction {
                status: None,
                message: message.into(),
                code: None,
            },
        }
    }

    fn no_headers(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn first_match_wins_in_name_order() {
        let store = BlockRuleStore::new();
        // Insert out of order; evaluation must be lexicographic ("a-..." first).
        store
            .insert_rule(&rule(
                "z-second",
                vec![cond("body.model", BlockOp::Eq, "gpt-4o")],
                "second",
            ))
            .unwrap();
        store
            .insert_rule(&rule(
                "a-first",
                vec![cond("body.model", BlockOp::Prefix, "gpt-")],
                "first",
            ))
            .unwrap();

        let m = store.check(no_headers, &json!({"model": "gpt-4o"})).unwrap();
        assert_eq!(m.rule_name, "a-first");
        assert_eq!(m.action.message, "first");
    }

    #[test]
    fn conditions_are_anded() {
        let store = BlockRuleStore::new();
        store
            .insert_rule(&rule(
                "and-rule",
                vec![
                    cond("body.model", BlockOp::Eq, "gpt-4o"),
                    cond("header.user-agent", BlockOp::Contains, "Cursor"),
                ],
                "blocked",
            ))
            .unwrap();

        let body = json!({"model": "gpt-4o"});
        // Only body matches → no hit.
        assert!(store.check(no_headers, &body).is_none());
        // Both match → hit.
        let m = store
            .check(|name| {
                (name.eq_ignore_ascii_case("user-agent")).then(|| "Cursor/1.0".to_string())
            }, &body)
            .unwrap();
        assert_eq!(m.rule_name, "and-rule");
        // Match evidence carries every condition with the request-side value.
        assert_eq!(m.matched.len(), 2);
        assert_eq!(m.matched[0].field, "header.user-agent");
        assert_eq!(m.matched[0].actual.as_deref(), Some("Cursor/1.0"));
        assert_eq!(m.matched[1].field, "body.model");
        assert_eq!(m.matched[1].actual.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn global_and_rule_switches_are_respected() {
        let store = BlockRuleStore::new();
        store
            .insert_rule(&rule("off", vec![cond("body", BlockOp::Exists, "")], "blocked"))
            .unwrap();
        store.set_enabled(false);
        assert!(store.check(no_headers, &json!({})).is_none());
        store.set_enabled(true);

        let mut disabled = rule("off", vec![cond("body", BlockOp::Exists, "")], "blocked");
        disabled.enabled = false;
        store.insert_rule(&disabled).unwrap();
        assert!(store.check(no_headers, &json!({})).is_none());
    }

    #[test]
    fn compile_rejects_foot_gun_rules() {
        // Empty conditions would match everything.
        let e = store_compile_error(&rule("empty", vec![], "x"));
        assert!(e.contains("no conditions"));
        // Bad regex.
        let e = store_compile_error(&rule(
            "badre",
            vec![cond("body.model", BlockOp::Regex, "(unclosed"),
        ], "x"));
        assert!(e.contains("invalid regex"));
        // Unknown field selector.
        let e = store_compile_error(&rule(
            "badfield",
            vec![cond("cookie.jar", BlockOp::Eq, "x")],
            "x",
        ));
        assert!(e.contains("must be"));
        // Empty name.
        let e = store_compile_error(&rule(" ", vec![cond("body", BlockOp::Exists, "")], "x"));
        assert!(e.contains("name"));
        // insert_rule with bad rule must NOT insert.
        let store = BlockRuleStore::new();
        assert!(store.insert_rule(&rule("empty", vec![], "x")).is_err());
        assert_eq!(store.len(), 0);
    }

    fn store_compile_error(rule: &BlockRule) -> String {
        BlockRuleStore::compile_rule(rule).err().expect("should fail")
    }
}
