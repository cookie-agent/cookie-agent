//! The runtime protocol's wire types, which live in
//! [`cookie_agent_protocol::runtime_protocol`] so that plugins and runtimes
//! outside this crate can speak them, and their conversions to and from the
//! types tools see here.

pub use cookie_agent_protocol::runtime_protocol::*;

use crate::{PromptSection, ToolCompletion, ToolError, ToolProgress, ToolSpec, TurnAgentContext};

/// Conversions between a [`ToolDeclaration`] and the [`ToolSpec`] a provider
/// publishes.
pub trait ToolDeclarationExt: Sized {
    /// The declaration of `spec`, whose permission name, when it has one,
    /// must be a permission action.
    fn from_spec(spec: &ToolSpec) -> Result<Self, String>;

    /// The spec a declaration publishes.
    fn into_spec(self) -> ToolSpec;
}

impl ToolDeclarationExt for ToolDeclaration {
    fn from_spec(spec: &ToolSpec) -> Result<Self, String> {
        Ok(Self {
            name: spec.name.clone(),
            collision: spec.collision,
            permission: spec
                .permission_name
                .as_deref()
                .map(|name| {
                    permission_action(name).ok_or_else(|| {
                        format!(
                            "tool `{}` has permission `{name}`, which is not a permission action",
                            spec.name
                        )
                    })
                })
                .transpose()?,
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            concurrency: spec.concurrency,
            result_truncation: spec.result_truncation,
            output: spec.output.clone(),
            background: spec.background,
            background_default: spec.background_default,
        })
    }

    fn into_spec(self) -> ToolSpec {
        ToolSpec {
            name: self.name,
            collision: self.collision,
            permission_name: self
                .permission
                .map(|permission| permission.as_str().to_owned()),
            description: self.description,
            parameters: self.parameters,
            concurrency: self.concurrency,
            result_truncation: self.result_truncation,
            output: self.output,
            background: self.background,
            background_default: self.background_default,
        }
    }
}

impl From<PromptSection> for PromptSectionDeclaration {
    fn from(section: PromptSection) -> Self {
        Self {
            title: section.title,
            body: section.body,
        }
    }
}

impl From<PromptSectionDeclaration> for PromptSection {
    fn from(section: PromptSectionDeclaration) -> Self {
        Self {
            title: section.title,
            body: section.body,
        }
    }
}

impl From<&TurnAgentContext> for ToolTurn {
    fn from(context: &TurnAgentContext) -> Self {
        Self {
            agent: context.agent.clone(),
            model: ToolTurnModel {
                model: context.model.clone(),
                adapter: context.adapter.clone(),
                input_media: context.input_media.clone(),
            },
        }
    }
}

impl From<ToolTurn> for TurnAgentContext {
    fn from(turn: ToolTurn) -> Self {
        Self {
            agent: turn.agent,
            model: turn.model.model,
            adapter: turn.model.adapter,
            input_media: turn.model.input_media,
        }
    }
}

/// Conversions between a wire [`ToolResult`] and a [`ToolCompletion`].
pub trait ToolResultExt: Sized {
    /// The wire result of `completion`, as a runtime returns it. The
    /// background state and retained output are the daemon's to record and
    /// are left out.
    fn of(completion: ToolCompletion) -> Self;

    /// The completion the daemon runs a peer's result as: a result of tool
    /// `tool` without a title is titled with its name, and a `single` output
    /// without display shows its text.
    fn into_completion(self, tool: &str) -> ToolCompletion;
}

impl ToolResultExt for ToolResult {
    fn of(completion: ToolCompletion) -> Self {
        let ToolCompletion {
            output,
            result,
            failed,
        } = completion;
        Self {
            output,
            title: Some(result.title),
            display: result.display,
            metadata: (!result.metadata.is_null()).then_some(result.metadata),
            attachments: result.attachments,
            additional_messages: result.additional_messages,
            failed,
        }
    }

    fn into_completion(self, tool: &str) -> ToolCompletion {
        let display = match (&self.output, self.display.is_empty()) {
            (cookie_agent_protocol::ToolCompletionOutput::Single { text }, true) => {
                crate::bounded_tool_display(text)
            }
            _ => self.display,
        };
        ToolCompletion {
            output: self.output,
            result: cookie_agent_protocol::PersistedToolResult {
                title: self
                    .title
                    .unwrap_or_else(|| crate::authoring::safe_title(tool)),
                output: String::new(),
                display,
                background: None,
                retained_output: None,
                metadata: self.metadata.unwrap_or(serde_json::Value::Null),
                attachments: self.attachments,
                additional_messages: self.additional_messages,
            },
            failed: self.failed,
        }
    }
}

/// Conversions between an [`Outcome`] and a tool result.
pub trait OutcomeExt {
    /// The outcome of `result`.
    fn of(result: Result<(), ToolError>) -> Self;

    /// The result an outcome carries.
    fn into_result(self) -> Result<(), ToolError>;
}

impl OutcomeExt for Outcome {
    fn of(result: Result<(), ToolError>) -> Self {
        Self {
            error: result.err().as_ref().map(WireToolError::from),
        }
    }

    fn into_result(self) -> Result<(), ToolError> {
        self.error.map_or(Ok(()), |error| Err(error.into()))
    }
}

impl From<ToolProgress> for WireProgress {
    fn from(progress: ToolProgress) -> Self {
        Self {
            tool_call_id: progress.tool_call_id,
            message: progress.message,
            display: progress.display,
            output: progress.output,
        }
    }
}

impl From<WireProgress> for ToolProgress {
    fn from(progress: WireProgress) -> Self {
        Self {
            tool_call_id: progress.tool_call_id,
            message: progress.message,
            display: progress.display,
            output: progress.output,
        }
    }
}

impl From<&ToolError> for WireToolError {
    fn from(error: &ToolError) -> Self {
        let kind = match error {
            ToolError::ProgressSinkClosed => ToolErrorKind::ProgressSinkClosed,
            ToolError::Failed(_) => ToolErrorKind::Failed,
            ToolError::OperationChanged(_) => ToolErrorKind::OperationChanged,
            ToolError::UnsupportedSecurity(_) => ToolErrorKind::UnsupportedSecurity,
            ToolError::UnsupportedPlatform(_) => ToolErrorKind::UnsupportedPlatform,
            ToolError::ResourceLimit(_) => ToolErrorKind::ResourceLimit,
            ToolError::InvalidUrl(_) => ToolErrorKind::InvalidUrl,
            ToolError::PermissionDenied(_) => ToolErrorKind::PermissionDenied,
            ToolError::RedirectError(_) => ToolErrorKind::RedirectError,
            ToolError::Timeout(_) => ToolErrorKind::Timeout,
            ToolError::TransportError(_) => ToolErrorKind::TransportError,
            ToolError::UnsupportedContentType(_) => ToolErrorKind::UnsupportedContentType,
            ToolError::RuntimeUnavailable(_) => ToolErrorKind::RuntimeUnavailable,
        };
        Self {
            kind,
            message: error.message(),
        }
    }
}

impl From<WireToolError> for ToolError {
    fn from(error: WireToolError) -> Self {
        let message = error.message;
        match error.kind {
            ToolErrorKind::ProgressSinkClosed => Self::ProgressSinkClosed,
            ToolErrorKind::Failed => Self::Failed(message),
            ToolErrorKind::OperationChanged => Self::OperationChanged(message),
            ToolErrorKind::UnsupportedSecurity => Self::UnsupportedSecurity(message),
            ToolErrorKind::UnsupportedPlatform => Self::UnsupportedPlatform(message),
            ToolErrorKind::ResourceLimit => Self::ResourceLimit(message),
            ToolErrorKind::InvalidUrl => Self::InvalidUrl(message),
            ToolErrorKind::PermissionDenied => Self::PermissionDenied(message),
            ToolErrorKind::RedirectError => Self::RedirectError(message),
            ToolErrorKind::Timeout => Self::Timeout(message),
            ToolErrorKind::TransportError => Self::TransportError(message),
            ToolErrorKind::UnsupportedContentType => Self::UnsupportedContentType(message),
            ToolErrorKind::RuntimeUnavailable => Self::RuntimeUnavailable(message),
        }
    }
}
