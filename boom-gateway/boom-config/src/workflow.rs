use crate::Config;
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

impl WorkflowSettings {
    pub fn validate(&self, config: &Config) -> Result<(), GatewayError> {
        let workflow_model_names = self.models.keys().map(String::as_str).collect::<HashSet<_>>();
        let mut available_models = config
            .model_list
            .iter()
            .map(|entry| entry.model_name.as_str())
            .collect::<HashSet<_>>();
        available_models.extend(
            config
                .router_settings
                .model_group_alias
                .keys()
                .map(String::as_str),
        );

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
                        if config
                            .router_settings
                            .model_group_alias
                            .get(instance.model.as_str())
                            .is_some_and(|alias| {
                                workflow_model_names.contains(alias.target_model())
                            })
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
    use crate::Config;

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
        assert!(config.validate().is_ok());
        assert_eq!(
            config.workflow_settings.models.get("fusion"),
            Some(&"direct_synthesis".to_string())
        );
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
        assert!(config.validate().is_err());
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
        let error = config.validate().unwrap_err().to_string();
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
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("panel_timeout_secs"));
    }
}
