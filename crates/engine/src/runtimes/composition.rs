//! Tool-name composition across the base layer and a session's attachments
//! (`docs/site/specs/runtimes.md` section 6.3).
//!
//! Names are computed from the current tool set every time it is composed:
//!
//! - a name offered by exactly one source is published bare;
//! - when several `prefix` runtime tools offer one name, each is published as
//!   `<attachment>_<name>` and the bare name is removed;
//! - a runtime tool with a base-layer name is published as
//!   `<attachment>_<name>`; the base tool keeps its name;
//! - a `fallback` tool is hidden whenever a non-fallback tool offers its name;
//! - a published name longer than [`MAX_TOOL_NAME_BYTES`] is dropped with a
//!   diagnostic.

use std::collections::{HashMap, HashSet};

use cookie_agent_runtime_api::{ToolCollision, ToolSpec};

use super::SourcedProvider;

/// Longest tool name model providers accept.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 64;

/// One tool as the model sees it.
#[derive(Clone)]
pub(crate) struct ComposedTool {
    /// The name published to the model.
    pub(crate) name: String,
    pub(crate) source: SourcedProvider,
    /// The tool as its source declares it; `spec.name` is the name the
    /// source knows it by.
    pub(crate) spec: ToolSpec,
}

/// The composed tool set and what composition had to drop.
pub(crate) struct Composition {
    pub(crate) tools: Vec<ComposedTool>,
    pub(crate) diagnostics: Vec<String>,
}

impl Composition {
    pub(crate) fn get(&self, name: &str) -> Option<&ComposedTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

/// Composes `candidates` (base layer first, then attachments in attach
/// order). `reserved` holds the base layer's names, which a runtime tool never
/// takes even while the base tool is not offered. Two tools of one source with
/// one name are an error naming it.
pub(crate) fn compose(
    candidates: Vec<(SourcedProvider, ToolSpec)>,
    reserved: &HashSet<String>,
) -> Result<Composition, String> {
    let mut per_source = HashSet::new();
    let mut base_names = reserved.clone();
    // Per name: runtime tools that do not yield, and fallback tools.
    let mut regular = HashMap::<&str, usize>::new();
    let mut fallback = HashMap::<&str, usize>::new();
    for (source, spec) in &candidates {
        if !per_source.insert((source.attachment.clone(), spec.name.as_str())) {
            return Err(spec.name.clone());
        }
        match (&source.attachment, spec.collision) {
            (None, _) => {
                base_names.insert(spec.name.clone());
            }
            (Some(_), ToolCollision::Prefix) => *regular.entry(&spec.name).or_default() += 1,
            (Some(_), ToolCollision::Fallback) => *fallback.entry(&spec.name).or_default() += 1,
        }
    }
    let mut tools = Vec::with_capacity(candidates.len());
    for (source, spec) in &candidates {
        let name = spec.name.as_str();
        let published = match (&source.attachment, spec.collision) {
            (None, _) => name.to_owned(),
            (Some(attachment), ToolCollision::Prefix) => {
                if base_names.contains(name) || regular[name] > 1 {
                    format!("{attachment}_{name}")
                } else {
                    name.to_owned()
                }
            }
            (Some(attachment), ToolCollision::Fallback) => {
                if base_names.contains(name) || regular.contains_key(name) {
                    continue;
                }
                if fallback[name] > 1 {
                    format!("{attachment}_{name}")
                } else {
                    name.to_owned()
                }
            }
        };
        tools.push(ComposedTool {
            name: published,
            source: source.clone(),
            spec: spec.clone(),
        });
    }
    let mut diagnostics = Vec::new();
    let mut published = HashSet::new();
    tools.retain(|tool| {
        let label = |reason: &str| match &tool.source.attachment {
            Some(attachment) => format!(
                "tool `{}` of runtime attachment `{attachment}` is not published: {reason}",
                tool.spec.name
            ),
            None => format!("base tool `{}` is not published: {reason}", tool.spec.name),
        };
        if tool.name.len() > MAX_TOOL_NAME_BYTES {
            diagnostics.push(label(&format!(
                "its published name `{}` is longer than {MAX_TOOL_NAME_BYTES} bytes",
                tool.name
            )));
            return false;
        }
        if !published.insert(tool.name.clone()) {
            diagnostics.push(label(&format!(
                "its published name `{}` is already taken",
                tool.name
            )));
            return false;
        }
        true
    });
    Ok(Composition { tools, diagnostics })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use async_trait::async_trait;
    use cookie_agent_protocol::AttachmentName;
    use cookie_agent_runtime_api::{
        PreparedTool, SessionToolContext, ToolCall, ToolCollision, ToolError, ToolOrigin,
        ToolPreparationContext, ToolProvider, ToolSpec,
    };

    use super::{SourcedProvider, compose};

    struct Nothing;

    #[async_trait]
    impl ToolProvider for Nothing {
        fn provider_id(&self) -> &'static str {
            "test.nothing"
        }
        fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
            Ok(Vec::new())
        }
        fn get_permission_name(_: &str) -> Result<Option<&'static str>, ToolError> {
            Ok(Some("read"))
        }
        fn get_permission_resource(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
            Ok((Some("read"), None))
        }
        fn get_display_argument(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
        async fn prepare(
            &self,
            _: ToolPreparationContext,
            _: ToolCall,
        ) -> Result<PreparedTool, ToolError> {
            Err(ToolError::execution("unused"))
        }
    }

    fn source(attachment: Option<&str>) -> SourcedProvider {
        SourcedProvider {
            attachment: attachment.map(|name| AttachmentName::new(name).unwrap()),
            generation: 0,
            origin: ToolOrigin::Base,
            provider: Arc::new(Nothing),
        }
    }

    fn spec(name: &str, collision: ToolCollision) -> ToolSpec {
        ToolSpec {
            collision,
            name: name.into(),
            permission_name: Some("read".into()),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }
    }

    fn published(candidates: Vec<(SourcedProvider, ToolSpec)>) -> Vec<String> {
        compose(candidates, &HashSet::from(["skill".to_owned()]))
            .expect("composes")
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    #[test]
    fn colliding_runtime_tools_take_their_attachment_prefix() {
        assert_eq!(
            published(vec![
                (
                    source(Some("local_web")),
                    spec("bash", ToolCollision::Prefix)
                ),
                (
                    source(Some("local_web")),
                    spec("read", ToolCollision::Prefix)
                ),
                (
                    source(Some("labpc_api")),
                    spec("bash", ToolCollision::Prefix)
                ),
            ]),
            ["local_web_bash", "read", "labpc_api_bash"]
        );
    }

    #[test]
    fn base_names_stay_bare_and_reserved() {
        assert_eq!(
            published(vec![
                (source(None), spec("read_artifact", ToolCollision::Prefix)),
                (
                    source(Some("global")),
                    spec("read_artifact", ToolCollision::Prefix)
                ),
                (source(Some("global")), spec("skill", ToolCollision::Prefix)),
            ]),
            ["read_artifact", "global_read_artifact", "global_skill"]
        );
    }

    #[test]
    fn fallback_tools_hide_behind_any_regular_tool() {
        assert_eq!(
            published(vec![
                (
                    source(Some("global")),
                    spec("webfetch", ToolCollision::Fallback)
                ),
                (
                    source(Some("local_web")),
                    spec("webfetch", ToolCollision::Prefix)
                ),
            ]),
            ["webfetch"]
        );
        assert_eq!(
            published(vec![(
                source(Some("global")),
                spec("webfetch", ToolCollision::Fallback)
            )]),
            ["webfetch"]
        );
        assert_eq!(
            published(vec![(
                source(Some("global")),
                spec("skill", ToolCollision::Fallback)
            )]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn names_over_64_bytes_are_dropped_with_a_diagnostic() {
        let long_attachment = format!("l{}", "o".repeat(39));
        let tool = "t".repeat(24);
        let composition = compose(
            vec![
                (
                    source(Some(&long_attachment)),
                    spec(&tool, ToolCollision::Prefix),
                ),
                (source(Some("local")), spec(&tool, ToolCollision::Prefix)),
            ],
            &HashSet::new(),
        )
        .expect("composes");
        assert_eq!(
            composition
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [format!("local_{tool}")]
        );
        assert_eq!(composition.diagnostics.len(), 1);
        assert!(
            composition.diagnostics[0].contains("longer than 64 bytes"),
            "{}",
            composition.diagnostics[0]
        );
    }

    #[test]
    fn one_source_offering_a_name_twice_is_an_error() {
        assert_eq!(
            compose(
                vec![
                    (
                        source(Some("global")),
                        spec("lookup", ToolCollision::Prefix)
                    ),
                    (
                        source(Some("global")),
                        spec("lookup", ToolCollision::Prefix)
                    ),
                ],
                &HashSet::new(),
            )
            .err()
            .as_deref(),
            Some("lookup")
        );
    }
}
