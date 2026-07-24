use async_trait::async_trait;
use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse, ChatStream};
use boom_core::GatewayError;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRole {
    Panel,
    Aggregator,
}

impl WorkflowRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Aggregator => "aggregator",
        }
    }
}

pub struct ModelInvocation {
    pub response: ChatCompletionResponse,
}

pub struct ModelStreamInvocation {
    pub stream: ChatStream,
}

#[async_trait]
pub trait ModelInvoker: Send + Sync {
    async fn invoke(
        &self,
        workflow_id: &str,
        role: WorkflowRole,
        request: ChatCompletionRequest,
    ) -> Result<ModelInvocation, GatewayError>;

    async fn invoke_stream(
        &self,
        _workflow_id: &str,
        role: WorkflowRole,
        _request: ChatCompletionRequest,
    ) -> Result<ModelStreamInvocation, GatewayError> {
        Err(GatewayError::UnsupportedMode(format!(
            "{} model invocation does not support streaming",
            role.as_str()
        )))
    }
}

pub struct WorkflowContext {
    pub request: ChatCompletionRequest,
    pub invoker: Arc<dyn ModelInvoker>,
}

pub struct WorkflowExecution {
    pub response: ChatCompletionResponse,
}

pub struct WorkflowStreamExecution {
    pub stream: ChatStream,
}

#[derive(Debug)]
pub struct WorkflowFailure {
    pub error: GatewayError,
}

impl fmt::Display for WorkflowFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for WorkflowFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[async_trait]
pub trait Workflow: Send + Sync {
    fn id(&self) -> &str;

    async fn execute(&self, context: WorkflowContext)
        -> Result<WorkflowExecution, WorkflowFailure>;

    async fn execute_stream(
        &self,
        _context: WorkflowContext,
    ) -> Result<WorkflowStreamExecution, WorkflowFailure> {
        Err(WorkflowFailure {
            error: GatewayError::UnsupportedMode(format!(
                "streaming is not supported for workflow '{}'",
                self.id()
            )),
        })
    }
}
