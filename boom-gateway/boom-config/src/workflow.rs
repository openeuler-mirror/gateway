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
                        validate_openai_compatible_model(
                            config,
                            workflow_id,
                            role,
                            &instance.model,
                        )?;
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

fn validate_openai_compatible_model(
    config: &Config,
    workflow_id: &str,
    role: &str,
    configured_model: &str,
) -> Result<(), GatewayError> {
    let model = config
        .router_settings
        .model_group_alias
        .get(configured_model)
        .map(|alias| alias.target_model())
        .unwrap_or(configured_model);
    let mut deployment_found = false;

    for entry in config
        .model_list
        .iter()
        .filter(|entry| entry.model_name == model)
    {
        deployment_found = true;
        let (provider_type, _) = entry.litellm_params.resolve_provider_and_model();
        if !is_openai_compatible_provider(&provider_type) {
            return Err(GatewayError::ConfigError(format!(
                "workflow '{}' {} model '{}' uses provider '{}' \
                 (litellm_params.model='{}'); Fusion child models must use an \
                 OpenAI-compatible provider",
                workflow_id,
                role,
                configured_model,
                provider_type,
                entry.litellm_params.model
            )));
        }
    }

    if !deployment_found {
        return Err(GatewayError::ConfigError(format!(
            "workflow '{}' {} model '{}' resolves to model '{}', which has no configured deployment",
            workflow_id, role, configured_model, model
        )));
    }

    Ok(())
}

fn is_openai_compatible_provider(provider_type: &str) -> bool {
    matches!(
        provider_type,
        "openai"
            | "azure"
            | "hosted_vllm"
            | "vllm"
            | "ollama"
            | "ollama_chat"
            | "deepseek"
            | "groq"
            | "together_ai"
            | "fireworks_ai"
            | "perplexity"
            | "anyscale"
            | "deepinfra"
            | "lm_studio"
            | "llamafile"
            | "xinference"
            | "sambanova"
            | "cerebras"
            | "nvidia_nim"
            | "codestral"
            | "volcengine"
            | "dashscope"
            | "moonshot"
            | "xai"
            | "ai21"
            | "ai21_chat"
    )
}

#[cfg(test)]
mod tests {
    use crate::Config;

    fn config_with_providers(panel_provider: &str, aggregator_provider: &str) -> Config {
        serde_yaml::from_str(&format!(
            r#"
model_list:
  - model_name: panel
    litellm_params:
      model: {panel_provider}/panel
  - model_name: aggregator
    litellm_params:
      model: {aggregator_provider}/aggregator
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel
          - model: panel
        aggregator:
          model: aggregator
"#
        ))
        .unwrap()
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

    #[test]
    fn openai_panel_and_azure_aggregator_are_allowed() {
        let config = config_with_providers("openai", "azure");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn anthropic_panel_is_rejected_without_tools() {
        let config = config_with_providers("anthropic", "openai");
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("panel model 'panel' uses provider 'anthropic'"));
        assert!(error.contains("OpenAI-compatible provider"));
    }

    #[test]
    fn gemini_aggregator_is_rejected() {
        let config = config_with_providers("openai", "gemini");
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("aggregator model 'aggregator' uses provider 'gemini'"));
    }

    #[test]
    fn all_deployments_of_a_fusion_child_model_must_be_compatible() {
        let yaml = r#"
model_list:
  - model_name: panel
    litellm_params:
      model: openai/panel
  - model_name: panel
    litellm_params:
      model: anthropic/panel
  - model_name: aggregator
    litellm_params:
      model: openai/aggregator
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel
          - model: panel
        aggregator:
          model: aggregator
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("panel model 'panel' uses provider 'anthropic'"));
    }

    #[test]
    fn fusion_child_alias_is_checked_against_its_target_deployments() {
        let yaml = r#"
model_list:
  - model_name: panel-target
    litellm_params:
      model: anthropic/panel
  - model_name: aggregator
    litellm_params:
      model: openai/aggregator
router_settings:
  model_group_alias:
    panel-alias: panel-target
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
          model: aggregator
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("panel model 'panel-alias' uses provider 'anthropic'"));
    }
}
