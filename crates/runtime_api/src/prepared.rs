//! Prepared tool calls: what a provider hands back from preparation, and the
//! completion an executor returns.

use std::sync::Arc;

use async_trait::async_trait;
use cookie_agent_protocol::{
    DisplayBlock, Open, PersistedToolResult as ToolResult, PreparedOperationIdentity, Sha256Digest,
    ToolCallPresentation,
};

use crate::{ToolError, ToolExecutionContext};

/// Completion supplies output once, or explicitly finalizes already accepted deltas.
#[derive(Clone, Debug)]
pub struct ToolCompletion {
    pub output: cookie_agent_protocol::ToolCompletionOutput,
    pub result: ToolResult,
    pub failed: bool,
}

impl ToolCompletion {
    #[cfg(any(test, feature = "test-support"))]
    pub fn into_result_for_test(self) -> Result<ToolResult, ToolError> {
        let mut result = self.result;
        match self.output {
            cookie_agent_protocol::ToolCompletionOutput::Single { text } => result.output = text,
            _ => {
                return Err(ToolError::execution(
                    "streamed tests must use runtime capture",
                ));
            }
        }
        Ok(result)
    }

    /// A completion of `result`'s output. A result without display shows
    /// its output.
    pub fn single(mut result: ToolResult) -> Self {
        let text = std::mem::take(&mut result.output);
        if result.display.is_empty() {
            result.display = bounded_tool_display(&text);
        }
        Self {
            output: cookie_agent_protocol::ToolCompletionOutput::Single { text },
            result,
            failed: false,
        }
    }

    pub fn streamed(result: ToolResult) -> Self {
        Self {
            output: cookie_agent_protocol::ToolCompletionOutput::Streamed,
            result,
            failed: false,
        }
    }
}

/// `text` as a result's display: one `text` block, sanitized for display and
/// cut to whole lines within the display budget. An empty text shows
/// nothing.
#[must_use]
pub fn bounded_tool_display(text: &str) -> Vec<Open<DisplayBlock>> {
    cookie_agent_protocol::fit_result_display(cookie_agent_protocol::text_display(
        sanitize_tool_display(text, usize::MAX),
    ))
}

/// `text` with control characters other than newline and tab replaced by
/// spaces, cut on a character boundary to at most `maximum` bytes.
#[must_use]
pub fn sanitize_tool_display(text: &str, maximum: usize) -> String {
    let mut display = String::new();
    for character in text.chars() {
        let character = if character.is_control() && !matches!(character, '\n' | '\t') {
            ' '
        } else {
            character
        };
        if display.len() + character.len_utf8() > maximum {
            break;
        }
        display.push(character);
    }
    display
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PreparedSerializationKey(Vec<u8>);

impl PreparedSerializationKey {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[async_trait]
pub trait PreparedExecutor: Send + Sync {
    /// Checks that the prepared operation can still run as approved. Runs
    /// when an approval is answered and again, under the per-target
    /// serialization lock, immediately before execution.
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError>;
}

/// The executor of a prepared call, taken exactly once by whoever runs it.
pub type PreparedExecutorCell = Arc<tokio::sync::Mutex<Option<Box<dyn PreparedExecutor>>>>;

/// What a provider that cannot answer the sync [`crate::ToolProvider`]
/// metadata methods, such as a runtime behind the runtime protocol, reports
/// about one prepared call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDeclaration {
    /// The permission name the call is checked under; `None` when the tool
    /// needs no permission.
    pub permission_name: Option<String>,
    /// The permission resource of the call's normalized arguments.
    pub permission_resource: Option<String>,
    /// How clients show the call.
    pub presentation: ToolCallPresentation,
}

pub struct PreparedTool {
    operation: PreparedOperationIdentity,
    policy_labels: Vec<Option<String>>,
    normalized_arguments: serde_json::Value,
    serialization_key: Option<PreparedSerializationKey>,
    executor: PreparedExecutorCell,
    declaration: Option<PreparedDeclaration>,
}

impl PreparedTool {
    pub fn new(
        operation: PreparedOperationIdentity,
        normalized_arguments: serde_json::Value,
        serialization_key: Option<PreparedSerializationKey>,
        executor: Box<dyn PreparedExecutor>,
    ) -> Result<Self, ToolError> {
        if operation.resources().is_empty() {
            return Err(ToolError::execution(
                "prepared tool requires at least one permission resource",
            ));
        }
        if normalized_arguments.is_null() {
            return Err(ToolError::execution(
                "prepared normalized arguments must not be null",
            ));
        }
        let policy_labels = operation
            .resources()
            .iter()
            .map(|resource| Some(resource.canonical.as_str().to_owned()))
            .collect();
        Ok(Self {
            operation,
            policy_labels,
            normalized_arguments,
            serialization_key,
            executor: Arc::new(tokio::sync::Mutex::new(Some(executor))),
            declaration: None,
        })
    }

    /// Attaches the permission and presentation the provider reported while
    /// preparing; the host then uses them instead of asking the provider.
    #[must_use]
    pub fn with_declaration(mut self, declaration: PreparedDeclaration) -> Self {
        self.declaration = Some(declaration);
        self
    }

    /// The permission and presentation reported at preparation, if any.
    #[must_use]
    pub const fn declaration(&self) -> Option<&PreparedDeclaration> {
        self.declaration.as_ref()
    }

    /// Restores policy labels a provider set while preparing, as another
    /// process reported them: a label is either the resource's canonical
    /// identity, a label matching its digest, or absent (the loose marker).
    pub fn with_reported_policy_labels(
        mut self,
        labels: Vec<Option<String>>,
    ) -> Result<Self, ToolError> {
        if labels.len() != self.operation.resources().len() {
            return Err(ToolError::execution(
                "prepared policy labels must cover every resource",
            ));
        }
        for (resource, label) in self.operation.resources().iter().zip(&labels) {
            let Some(label) = label else { continue };
            if label.is_empty() {
                return Err(ToolError::execution(
                    "permission resource must not be empty",
                ));
            }
            let canonical = resource.canonical.as_str();
            let expected = Sha256Digest::of_bytes(label.as_bytes());
            if label != canonical
                && canonical
                    .rsplit_once(':')
                    .is_none_or(|(_, digest)| digest != expected.as_str())
            {
                return Err(ToolError::execution(
                    "prepared policy label does not match its immutable resource identity",
                ));
            }
        }
        self.policy_labels = labels;
        Ok(self)
    }

    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }

    pub fn with_policy_labels(mut self, labels: Vec<String>) -> Result<Self, ToolError> {
        if labels.is_empty() || labels.len() != self.operation.resources().len() {
            return Err(ToolError::execution(
                "prepared policy labels must cover every resource",
            ));
        }
        for (resource, label) in self.operation.resources().iter().zip(&labels) {
            let expected = Sha256Digest::of_bytes(label.as_bytes());
            if resource
                .canonical
                .as_str()
                .rsplit_once(':')
                .is_none_or(|(_, digest)| digest != expected.as_str())
            {
                return Err(ToolError::execution(
                    "prepared policy label does not match its immutable resource identity",
                ));
            }
        }
        self.policy_labels = labels.into_iter().map(Some).collect();
        Ok(self)
    }

    pub fn with_permission_resource(mut self, resource: Option<String>) -> Result<Self, ToolError> {
        if resource.as_ref().is_some_and(String::is_empty) {
            return Err(ToolError::execution(
                "permission resource must not be empty",
            ));
        }
        self.policy_labels.fill(resource);
        Ok(self)
    }

    #[must_use]
    pub const fn normalized_arguments(&self) -> &serde_json::Value {
        &self.normalized_arguments
    }

    #[must_use]
    pub fn policy_labels(&self) -> &[Option<String>] {
        &self.policy_labels
    }

    /// Calls sharing a key run one at a time.
    #[must_use]
    pub const fn serialization_key(&self) -> Option<&PreparedSerializationKey> {
        self.serialization_key.as_ref()
    }

    /// The executor, for the host that runs the call.
    #[must_use]
    pub const fn executor(&self) -> &PreparedExecutorCell {
        &self.executor
    }

    #[cfg(feature = "test-support")]
    pub async fn execute_for_test(
        self,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let executor = self
            .executor
            .lock()
            .await
            .take()
            .ok_or_else(|| ToolError::execution("prepared executor was already consumed"))?;
        executor.execute(context).await?.into_result_for_test()
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        PersistedToolResult as ToolResult, PreparedOperationIdentity, Sha256Digest,
    };

    use super::{PreparedExecutor, PreparedTool, ToolCompletion, async_trait};
    use crate::{ToolError, ToolExecutionContext};

    struct NoopExecutor;

    #[async_trait]
    impl PreparedExecutor for NoopExecutor {
        async fn revalidate(&self) -> Result<(), ToolError> {
            Ok(())
        }

        async fn execute(
            self: Box<Self>,
            _context: ToolExecutionContext,
        ) -> Result<ToolCompletion, ToolError> {
            let result: Result<ToolResult, ToolError> =
                async move { unreachable!("constructor validation test never executes") }.await;
            result.map(ToolCompletion::single)
        }
    }

    fn operation() -> PreparedOperationIdentity {
        let label = "command:test";
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"arguments"),
            vec![cookie_agent_protocol::ApprovalCapability {
                action: cookie_agent_protocol::PermissionAction::BASH,
                operation: cookie_agent_protocol::PreparedCapabilityOperation::new("bash:execute")
                    .expect("capability operation"),
            }],
            vec![cookie_agent_protocol::PreparedApprovalResource {
                capability: cookie_agent_protocol::PermissionAction::BASH,
                canonical: cookie_agent_protocol::PreparedResourceIdentity::new(format!(
                    "command:{}",
                    Sha256Digest::of_bytes(label.as_bytes())
                ))
                .expect("resource identity"),
                binding_digest:
                    cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                        label.as_bytes(),
                    ),
                binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
                boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
                source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation")
    }

    #[test]
    fn prepared_tool_rejects_null_normalized_arguments() {
        let error = match PreparedTool::new(
            operation(),
            serde_json::Value::Null,
            None,
            Box::new(NoopExecutor),
        ) {
            Ok(_) => panic!("null normalized arguments must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must not be null"));
    }

    #[test]
    fn absent_permission_resource_sets_the_loose_policy_marker() {
        let prepared = PreparedTool::new(
            operation(),
            serde_json::json!({}),
            None,
            Box::new(NoopExecutor),
        )
        .expect("prepared tool")
        .with_permission_resource(None)
        .expect("loose permission resource");
        assert_eq!(prepared.policy_labels(), [None]);
    }
}
