//! The tool provider trait and the tool declarations it publishes.

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentId, BoundedDisplayText, SafeDisplayText, SessionId, ToolCallId, ToolCallPresentation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{PreparedTool, ToolError, ToolPreparationContext};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionToolContext {
    pub session: SessionId,
    #[serde(skip)]
    prompt_delegate_targets: Option<Vec<(AgentId, String)>>,
}

impl SessionToolContext {
    #[must_use]
    pub const fn new(session: SessionId) -> Self {
        Self {
            session,
            prompt_delegate_targets: None,
        }
    }

    /// Context for composing a run's prompt, carrying the delegate targets
    /// frozen for that run.
    #[must_use]
    pub fn for_prompt_composition(
        session: SessionId,
        delegate_targets: Vec<(AgentId, String)>,
    ) -> Self {
        Self {
            session,
            prompt_delegate_targets: Some(delegate_targets),
        }
    }

    /// Delegate targets frozen for the run currently composing its prompt.
    pub fn prompt_delegate_targets(
        &self,
    ) -> Option<impl ExactSizeIterator<Item = (&AgentId, &str)>> {
        self.prompt_delegate_targets.as_ref().map(|targets| {
            targets
                .iter()
                .map(|(id, description)| (id, description.as_str()))
        })
    }
}

/// One labeled system-prompt section contributed by a tool provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSection {
    /// Short human-meaningful title used to identify validation failures.
    pub title: String,
    /// Markdown-like body rendered inside the provider provenance wrapper.
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    /// What happens when another source publishes the same name.
    #[serde(default)]
    pub collision: ToolCollision,
    /// The permission name the tool's calls are checked under; `None` for a
    /// tool that needs no permission: always published when its source offers
    /// it, and every call allowed without evaluating rules or asking.
    #[serde(default)]
    pub permission_name: Option<String>,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub concurrency: ToolConcurrency,
    #[serde(default)]
    pub result_truncation: ToolResultTruncationPolicy,
    #[serde(default)]
    pub output: cookie_agent_protocol::ToolOutputDeclaration,
}

/// How a runtime tool behaves when another source offers the same name.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCollision {
    /// Stays published. Two such tools with one name are a composition error
    /// until collision prefixing lands.
    #[default]
    Prefix,
    /// Hidden whenever a non-fallback tool with the same name is published.
    Fallback,
}

/// Declares whether calls to a tool may overlap with sibling calls from one model turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    #[default]
    Exclusive,
    Parallel,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultTruncationPolicy {
    #[default]
    Bounded,
    OptOut,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Value,
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Stable engine-facing identity used for system-prompt provenance labels.
    fn provider_id(&self) -> &'static str;

    /// Sections resolved once at run admission and frozen into the agent snapshot.
    fn prompt_sections(&self, _ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        Ok(Vec::new())
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError>;
    /// Claims a currently undiscovered dynamic tool, allowing preparation to make it available.
    fn permission_for_unlisted_tool(
        &self,
        _tool_name: &str,
    ) -> Result<Option<&'static str>, ToolError> {
        Ok(None)
    }
    /// The permission name of `tool_name`; `None` when it needs no
    /// permission.
    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError>
    where
        Self: Sized;
    /// The permission name and resource of `arguments`. The host asks only
    /// for a prepared call without a [`crate::PreparedDeclaration`]; a
    /// provider that cannot answer synchronously, such as a runtime behind
    /// the runtime protocol, reports both at preparation instead.
    fn get_permission_resource(
        &self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError>;
    fn get_display_argument(&self, name: &str, arguments: &Value) -> Result<String, ToolError>;

    /// How clients show `call`. The host uses a prepared call's
    /// [`crate::PreparedDeclaration`] when it has one, and asks here
    /// otherwise, including for a call that failed to prepare.
    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        match self.get_display_argument(&call.name, &call.arguments) {
            Ok(display) => tool_presentation(&call.name, &display),
            Err(_) => tool_title_only(&call.name),
        }
    }
    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError>;

    /// Prepare one turn's parallel call batch. Providers whose calls may target
    /// the same mutable resource can chain preparation against successive states.
    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        let mut prepared = Vec::with_capacity(calls.len());
        for call in calls {
            prepared.push(self.prepare(ctx.clone(), call).await);
        }
        prepared
    }
}

/// A call's presentation with only its tool name as the title.
#[must_use]
pub fn tool_title_only(name: &str) -> ToolCallPresentation {
    ToolCallPresentation {
        title: safe_display(name),
        primary_argument: None,
    }
}

/// A call's presentation: the tool name as the title and `primary` as its
/// byte-bounded display argument.
#[must_use]
pub fn tool_presentation(name: &str, primary: &str) -> ToolCallPresentation {
    let argument = truncate_utf8(primary.trim(), BoundedDisplayText::MAX_BYTES);
    ToolCallPresentation {
        title: safe_display(name),
        primary_argument: Some(
            BoundedDisplayText::new(if argument.is_empty() {
                "unavailable".into()
            } else {
                argument
            })
            .expect("byte-bounded display argument"),
        ),
    }
}

fn safe_display(value: &str) -> SafeDisplayText {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > SafeDisplayText::MAX_BYTES {
            break;
        }
        output.push(character);
    }
    if output.is_empty() {
        output.push_str("unavailable");
    }
    SafeDisplayText::new(output).expect("sanitized display text")
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::BoundedDisplayText;

    use super::{ToolConcurrency, ToolSpec, tool_presentation};

    #[test]
    fn tool_concurrency_defaults_to_exclusive_when_omitted() {
        let spec: ToolSpec = serde_json::from_value(serde_json::json!({
            "name":"external_tool",
            "permission_name":"external",
            "description":"External tool",
            "parameters":{"type":"object"}
        }))
        .expect("tool spec without concurrency");
        assert_eq!(spec.concurrency, ToolConcurrency::Exclusive);
    }

    #[test]
    fn presentation_byte_caps_arguments_and_preserves_controls() {
        let presentation = tool_presentation("bash", "cat secret.txt; TOKEN=visible\u{1b}");
        assert_eq!(presentation.title.as_str(), "bash");
        let argument = presentation.primary_argument.unwrap();
        assert_eq!(argument.as_str(), "cat secret.txt; TOKEN=visible\u{1b}");

        let oversized = "x".repeat(BoundedDisplayText::MAX_BYTES + 8);
        let argument = tool_presentation("bash", &oversized)
            .primary_argument
            .unwrap();
        assert_eq!(argument.as_str().len(), BoundedDisplayText::MAX_BYTES);

        // Three-byte graphemes do not divide the cap evenly: the cut stays on a
        // character boundary rather than yielding an invalid UTF-8 prefix.
        let multibyte = "→".repeat(BoundedDisplayText::MAX_BYTES);
        let argument = tool_presentation("bash", &multibyte)
            .primary_argument
            .unwrap();
        assert_eq!(
            argument.as_str(),
            "→".repeat(BoundedDisplayText::MAX_BYTES / 3)
        );

        assert_eq!(
            tool_presentation("bash", "  \n\t ")
                .primary_argument
                .unwrap()
                .as_str(),
            "unavailable"
        );
    }
}
