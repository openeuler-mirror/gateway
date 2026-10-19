use boom_core::GatewayError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct WorkflowSettings {
    #[serde(default)]
    pub models: HashMap<String, String>,
    #[serde(default)]
    pub workflows: HashMap<String, WorkflowDefinitionConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowDefinitionConfig {
    DirectSynthesis {
        roles: DirectSynthesisRolesConfig,
        #[serde(default)]
        panel_timeout_secs: Option<u64>,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DirectSynthesisRolesConfig {
    pub panel: Vec<WorkflowModelInstanceConfig>,
    pub aggregator: WorkflowModelInstanceConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkflowModelInstanceConfig {
    pub model: String,
    #[serde(default)]
    pub temperature: Option<f64>,
}

/// Namespaces a workflow configuration is validated against: the routable
/// model universe (deployment names + alias names) and the alias → target
/// mapping (a panel model that is an alias must not resolve back into a
/// workflow model).
#[derive(Debug, Default)]
pub struct WorkflowNamespaces {
    /// Deployment model names usable as workflow children. Callers must
    /// exclude workflow-owned exclusive models (their own previous
    /// registrations) or re-validation would self-collide.
    pub deployment_names: Vec<String>,
    /// alias_name → target_model.
    pub aliases: HashMap<String, String>,
}

impl WorkflowNamespaces {
    fn available_models(&self) -> HashSet<&str> {
        let mut set: HashSet<&str> = self.deployment_names.iter().map(String::as_str).collect();
        set.extend(self.aliases.keys().map(String::as_str));
        set
    }
}

impl WorkflowSettings {
    /// Validate structural and namespace constraints against the routable
    /// model universe. Pure: takes the deployment names and alias map as
    /// inputs so the same check runs at YAML load (via the caller's own
    /// namespace assembly), at fusion registration (live stores), and in
    /// workflow CRUD handlers (scratch copy + store namespaces).
    pub fn validate_with_namespaces(
        &self,
        namespaces: &WorkflowNamespaces,
    ) -> Result<(), GatewayError> {
        let workflow_model_names = self.models.keys().map(String::as_str).collect::<HashSet<_>>();
        let available_models = namespaces.available_models();

        for (model, workflow_id) in &self.models {
            if model.trim().is_empty() {
                return Err(GatewayError::ConfigError(
                    "workflow_settings.models contains an empty model name".to_string(),
                ));
            }
            if available_models.contains(model.as_str()) {
                return Err(GatewayError::ConfigError(format!(
                    "workflow model '{}' conflicts with a deployment or alias",
                    model
                )));
            }
            if !self.workflows.contains_key(workflow_id) {
                return Err(GatewayError::ConfigError(format!(
                    "workflow model '{}' references unknown workflow '{}'",
                    model, workflow_id
                )));
            }
        }

        for (workflow_id, workflow) in &self.workflows {
            if workflow_id.trim().is_empty() {
                return Err(GatewayError::ConfigError(
                    "workflow_settings.workflows contains an empty workflow id".to_string(),
                ));
            }
            match workflow {
                WorkflowDefinitionConfig::DirectSynthesis {
                    roles,
                    panel_timeout_secs,
                } => {
                    if roles.panel.len() < 2 {
                        return Err(GatewayError::ConfigError(format!(
                            "workflow '{}' direct_synthesis requires at least two panel instances",
                            workflow_id
                        )));
                    }
                    if panel_timeout_secs.is_some_and(|seconds| seconds == 0) {
                        return Err(GatewayError::ConfigError(format!(
                            "workflow '{}' panel_timeout_secs must be greater than zero",
                            workflow_id
                        )));
                    }
                    for (role, instance) in roles
                        .panel
                        .iter()
                        .map(|instance| ("panel", instance))
                        .chain(std::iter::once(("aggregator", &roles.aggregator)))
                    {
                        if instance.model.trim().is_empty() {
                            return Err(GatewayError::ConfigError(format!(
                                "workflow '{}' {} model must not be empty",
                                workflow_id, role
                            )));
                        }
                        if workflow_model_names.contains(instance.model.as_str()) {
                            return Err(GatewayError::ConfigError(format!(
                                "workflow '{}' {} model '{}' references a workflow model",
                                workflow_id, role, instance.model
                            )));
                        }
                        if namespaces
                            .aliases
                            .get(instance.model.as_str())
                            .is_some_and(|alias| workflow_model_names.contains(alias.as_str()))
                        {
                            return Err(GatewayError::ConfigError(format!(
                                "workflow '{}' {} model alias '{}' resolves to a workflow model",
                                workflow_id, role, instance.model
                            )));
                        }
                        if !available_models.contains(instance.model.as_str()) {
                            return Err(GatewayError::ConfigError(format!(
                                "workflow '{}' {} model '{}' is not configured",
                                workflow_id, role, instance.model
                            )));
                        }
                        if instance.temperature.is_some_and(|value| !value.is_finite()) {
                            return Err(GatewayError::ConfigError(format!(
                                "workflow '{}' {} temperature must be finite",
                                workflow_id, role
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    /// Assemble namespaces the way the live gateway does (deployment names +
    /// alias map), from a parsed Config — mirrors what state.rs feeds the
    /// validator at registration time.
    fn namespaces_from_config(config: &Config) -> WorkflowNamespaces {
        WorkflowNamespaces {
            deployment_names: config
                .model_list
                .iter()
                .map(|e| e.model_name.clone())
                .collect(),
            aliases: config
                .router_settings
                .model_group_alias
                .iter()
                .map(|(alias, cfg)| (alias.clone(), cfg.target_model().to_string()))
                .collect(),
        }
    }

    fn validate(config: &Config) -> Result<(), GatewayError> {
        config
            .workflow_settings
            .validate_with_namespaces(&namespaces_from_config(config))
    }

    #[test]
    fn direct_synthesis_config_is_valid() {
        let yaml = r#"
model_list:
  - model_name: glm-5.2
    litellm_params:
      model: openai/glm-5.2
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: glm-5.2
            temperature: 0.3
          - model: glm-5.2
            temperature: 0.5
        aggregator:
          model: glm-5.2
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        validate(&config).expect("valid");
        assert_eq!(
            config.workflow_settings.models.get("fusion"),
            Some(&"direct_synthesis".to_string())
        );
    }

    #[test]
    fn workflow_model_conflicting_with_deployment_is_rejected() {
        let yaml = r#"
model_list:
  - model_name: fusion
    litellm_params:
      model: openai/fusion
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: glm-5.2
          - model: glm-5.2
        aggregator:
          model: glm-5.2
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("conflicts with a deployment or alias"));
    }

    #[test]
    fn workflow_role_cannot_reference_workflow_model() {
        let yaml = r#"
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: fusion
          - model: fusion
        aggregator:
          model: fusion
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn workflow_role_alias_cannot_resolve_to_workflow_model() {
        let yaml = r#"
router_settings:
  model_group_alias:
    panel-alias: fusion
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-alias
          - model: panel-alias
        aggregator:
          model: panel-alias
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("resolves to a workflow model"));
    }

    #[test]
    fn panel_timeout_must_be_positive() {
        let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      panel_timeout_secs: 0
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("panel_timeout_secs"));
    }

    #[test]
    fn alias_names_count_as_available_models() {
        // A workflow model may not shadow an alias, and a panel may use one.
        let yaml = r#"
model_list:
  - model_name: glm-5.2
    litellm_params:
      model: openai/glm-5.2
router_settings:
  model_group_alias:
    glm-fast: glm-5.2
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: glm-fast
          - model: glm-5.2
        aggregator:
          model: glm-5.2
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        validate(&config).expect("alias is a usable panel model");
    }
}
