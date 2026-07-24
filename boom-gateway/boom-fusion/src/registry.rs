use crate::Workflow;
use std::collections::HashMap;
use std::sync::Arc;

pub struct WorkflowRegistry {
    workflows: HashMap<String, Arc<dyn Workflow>>,
    model_routes: HashMap<String, String>,
}

impl WorkflowRegistry {
    pub fn empty() -> Self {
        Self {
            workflows: HashMap::new(),
            model_routes: HashMap::new(),
        }
    }

    pub fn new(
        workflows: HashMap<String, Arc<dyn Workflow>>,
        model_routes: HashMap<String, String>,
    ) -> Result<Self, String> {
        for (model, workflow_id) in &model_routes {
            if model.is_empty() {
                return Err("workflow model name must not be empty".to_string());
            }
            if !workflows.contains_key(workflow_id) {
                return Err(format!(
                    "workflow model '{}' references unknown workflow '{}'",
                    model, workflow_id
                ));
            }
        }
        Ok(Self {
            workflows,
            model_routes,
        })
    }

    pub fn workflow_for_model(&self, model: &str) -> Option<Arc<dyn Workflow>> {
        let workflow_id = self.model_routes.get(model)?;
        self.workflows.get(workflow_id).cloned()
    }

    pub fn contains_model(&self, model: &str) -> bool {
        self.model_routes.contains_key(model)
    }

    pub fn model_names(&self) -> Vec<String> {
        let mut names = self.model_routes.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }
}

impl Default for WorkflowRegistry {
    fn default() -> Self {
        Self::empty()
    }
}
