//! Prepared tool calls: what a provider hands back from preparation, and the
//! completion an executor returns.

use std::sync::Arc;

use async_trait::async_trait;
use cookie_agent_protocol::{
    DisplayBlock, Open, PersistedToolResult as ToolResult, PreparedOperationIdentity,
    PreparedPresentation, Sha256Digest,
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

/// The head of `text` a display block shows: its first lines within
/// `max_bytes` (the start of the first line when even that is longer), and
/// the count of the lines it leaves out. A display then fits the head
/// without measuring all of a long text.
#[must_use]
pub fn text_head(text: &str, max_bytes: usize) -> (&str, Option<u64>) {
    if text.len() <= max_bytes {
        return (text, None);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').map_or(end, |newline| newline + 1);
    let rest = &text[end..];
    let omitted = rest.bytes().filter(|byte| *byte == b'\n').count() as u64
        + u64::from(!rest.ends_with('\n'));
    (&text[..end], Some(omitted))
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
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedDeclaration {
    /// The permission name the call is checked under; `None` when the tool
    /// needs no permission.
    pub permission_name: Option<String>,
    /// The permission resource of the call's normalized arguments.
    pub permission_resource: Option<String>,
    /// How clients show the call.
    pub presentation: PreparedPresentation,
}

/// A prepared call: its approval identity, normalized arguments, and
/// executor.
///
/// A provider the host owns states the call's approval identity itself
/// ([`Self::new`]). Every other provider, such as a runtime's tools or a
/// plugin's, only declares the call ([`Self::declared`]): the host builds its
/// identity from the tool's permission and resource and from the tool it
/// knows the call goes to, and sets it with [`Self::identify`] before
/// anything reads it.
pub struct PreparedTool {
    /// `None` until the host identifies a declared call.
    operation: Option<PreparedOperationIdentity>,
    policy_labels: Vec<Option<String>>,
    normalized_arguments: serde_json::Value,
    serialization_key: Option<PreparedSerializationKey>,
    executor: PreparedExecutorCell,
    declaration: Option<PreparedDeclaration>,
    presentation: Option<PreparedPresentation>,
    /// Runtime IDs the call's permission is checked under, each as
    /// `action@runtime`, in place of its origin's; empty for its origin's.
    permission_runtimes: Vec<String>,
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
            operation: Some(operation),
            policy_labels,
            normalized_arguments,
            serialization_key,
            executor: Arc::new(tokio::sync::Mutex::new(Some(executor))),
            declaration: None,
            presentation: None,
            permission_runtimes: Vec::new(),
        })
    }

    /// A call whose approval identity the host builds: the provider states
    /// only its normalized arguments, and its permission resource as
    /// [`crate::ToolProvider::get_permission_resource`] or a
    /// [`PreparedDeclaration`] reports it.
    pub fn declared(
        normalized_arguments: serde_json::Value,
        serialization_key: Option<PreparedSerializationKey>,
        executor: Box<dyn PreparedExecutor>,
    ) -> Result<Self, ToolError> {
        if normalized_arguments.is_null() {
            return Err(ToolError::execution(
                "prepared normalized arguments must not be null",
            ));
        }
        Ok(Self {
            operation: None,
            policy_labels: Vec::new(),
            normalized_arguments,
            serialization_key,
            executor: Arc::new(tokio::sync::Mutex::new(Some(executor))),
            declaration: None,
            presentation: None,
            permission_runtimes: Vec::new(),
        })
    }

    /// Whether the host has yet to build the call's identity.
    #[must_use]
    pub const fn is_declared(&self) -> bool {
        self.operation.is_none()
    }

    /// Sets the identity the host built for a declared call: one resource,
    /// whose policy label is `resource` (`None` for the loose marker).
    pub fn identify(
        mut self,
        operation: PreparedOperationIdentity,
        resource: Option<String>,
    ) -> Result<Self, ToolError> {
        if self.operation.is_some() {
            return Err(ToolError::execution(
                "a prepared call that states its identity is not identified again",
            ));
        }
        if operation.resources().len() != 1 {
            return Err(ToolError::execution(
                "a declared call's identity has exactly one resource",
            ));
        }
        if resource.as_ref().is_some_and(String::is_empty) {
            return Err(ToolError::execution(
                "permission resource must not be empty",
            ));
        }
        self.operation = Some(operation);
        self.policy_labels = vec![resource];
        Ok(self)
    }

    /// Attaches the permission and presentation the provider reported while
    /// preparing; the host then uses them instead of asking the provider.
    #[must_use]
    pub fn with_declaration(mut self, declaration: PreparedDeclaration) -> Self {
        self.declaration = Some(declaration);
        self
    }

    /// Attaches how clients show the call, as the provider knows it only
    /// once it prepared the call (such as a diff of the file it read). The
    /// host uses it instead of [`crate::ToolProvider::presentation`].
    #[must_use]
    pub fn with_presentation(mut self, presentation: PreparedPresentation) -> Self {
        self.presentation = Some(presentation);
        self
    }

    /// Checks the call's permission under each of `runtimes`, as
    /// `action@runtime`, instead of under the runtime that offers the tool:
    /// the call is denied when any runtime denies it and asks when any asks.
    /// A base tool that acts on runtimes, such as `delegate_subagent` with
    /// `capabilities`, uses it.
    #[must_use]
    pub fn with_permission_runtimes(mut self, runtimes: Vec<String>) -> Self {
        self.permission_runtimes = runtimes;
        self
    }

    /// The runtime IDs the call's permission is checked under; empty for
    /// the runtime that offers the tool.
    #[must_use]
    pub fn permission_runtimes(&self) -> &[String] {
        &self.permission_runtimes
    }

    /// How clients show the call, as the provider prepared it. A
    /// [`PreparedDeclaration`]'s presentation takes precedence.
    #[must_use]
    pub fn presentation(&self) -> Option<&PreparedPresentation> {
        self.declaration
            .as_ref()
            .map(|declaration| &declaration.presentation)
            .or(self.presentation.as_ref())
    }

    /// The permission and presentation reported at preparation, if any.
    #[must_use]
    pub const fn declaration(&self) -> Option<&PreparedDeclaration> {
        self.declaration.as_ref()
    }

    /// The call's approval identity.
    ///
    /// # Panics
    ///
    /// For a declared call the host has not identified yet.
    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        match &self.operation {
            Some(operation) => operation,
            None => panic!("the host identifies a declared call before reading its identity"),
        }
    }

    pub fn with_policy_labels(mut self, labels: Vec<String>) -> Result<Self, ToolError> {
        let Some(operation) = &self.operation else {
            return Err(ToolError::execution(
                "a declared call's policy label is its permission resource",
            ));
        };
        if labels.is_empty() || labels.len() != operation.resources().len() {
            return Err(ToolError::execution(
                "prepared policy labels must cover every resource",
            ));
        }
        for (resource, label) in operation.resources().iter().zip(&labels) {
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
