//! Tool-name composition across the base layer and a session's attachments
//! (`docs/site/specs/runtimes.md` section 6.3).
//!
//! Names are computed from the current tool set every time it is composed.
//! A runtime tool composes under its *source*: the attachment that offers
//! it, or the plugin that declares it.
//!
//! - a name offered by exactly one source is published bare;
//! - when several `prefix` tools offer one name, each is published as
//!   `<source>_<name>` and the bare name is removed;
//! - an `override` tool takes the bare name; the `prefix` tools of that name
//!   are published as `<source>_<name>` and its `fallback` tools are hidden.
//!   Two `override` tools of one name are both prefixed, with a diagnostic;
//! - a runtime tool with a base-layer name is published as
//!   `<source>_<name>`; the base tool keeps its name;
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

/// The published names a tool round pins: by a runtime tool's composition
/// label and own name (stable prefix spec, section 5).
pub(crate) type RoundPins = HashMap<(String, String), String>;

/// Composes `candidates` (base layer first, then attachments in attach
/// order). `reserved` holds the base layer's names, which a runtime tool never
/// takes even while the base tool is not offered. A runtime tool `pins` names
/// takes that name; any other tool whose name a pin holds takes its
/// `<label>_<tool>` form. Two tools of one source with one name are an error
/// naming it.
pub(crate) fn compose(
    candidates: Vec<(SourcedProvider, ToolSpec)>,
    reserved: &HashSet<String>,
    pins: &RoundPins,
) -> Result<Composition, String> {
    let mut per_source = HashSet::new();
    let mut base_names = reserved.clone();
    // Per name: runtime tools that do not yield, overriding tools, and
    // fallback tools.
    let mut regular = HashMap::<&str, usize>::new();
    let mut overriding = HashMap::<&str, usize>::new();
    let mut fallback = HashMap::<&str, usize>::new();
    let labels = candidates
        .iter()
        .map(|(source, _)| composition_source(source))
        .collect::<Vec<_>>();
    for ((_, spec), label) in candidates.iter().zip(&labels) {
        if !per_source.insert((label.clone(), spec.name.as_str())) {
            return Err(spec.name.clone());
        }
        let counts = match (label, spec.collision) {
            (None, _) => {
                base_names.insert(spec.name.clone());
                continue;
            }
            (Some(_), ToolCollision::Prefix) => &mut regular,
            (Some(_), ToolCollision::Override) => &mut overriding,
            (Some(_), ToolCollision::Fallback) => &mut fallback,
        };
        *counts.entry(&spec.name).or_default() += 1;
    }
    let count = |counts: &HashMap<&str, usize>, name: &str| counts.get(name).copied().unwrap_or(0);
    let mut names = Vec::with_capacity(candidates.len());
    let mut overridden = Vec::new();
    for ((_, spec), label) in candidates.iter().zip(&labels) {
        let name = spec.name.as_str();
        let published = match (label, spec.collision) {
            (None, _) => name.to_owned(),
            (Some(label), ToolCollision::Override) => {
                if base_names.contains(name) || count(&overriding, name) > 1 {
                    if count(&overriding, name) > 1 && !overridden.contains(&name) {
                        overridden.push(name);
                    }
                    format!("{label}_{name}")
                } else {
                    name.to_owned()
                }
            }
            (Some(label), ToolCollision::Prefix) => {
                if base_names.contains(name)
                    || count(&overriding, name) == 1
                    || count(&regular, name) > 1
                {
                    format!("{label}_{name}")
                } else {
                    name.to_owned()
                }
            }
            (Some(label), ToolCollision::Fallback) => {
                if base_names.contains(name)
                    || count(&regular, name) > 0
                    || count(&overriding, name) > 0
                {
                    names.push(None);
                    continue;
                }
                if count(&fallback, name) > 1 {
                    format!("{label}_{name}")
                } else {
                    name.to_owned()
                }
            }
        };
        names.push(Some(published));
    }
    if !pins.is_empty() {
        let pinned = pins.values().map(String::as_str).collect::<HashSet<_>>();
        for (((_, spec), label), name) in candidates.iter().zip(&labels).zip(names.iter_mut()) {
            let Some(label) = label else {
                continue;
            };
            if let Some(pin) = pins.get(&(label.clone(), spec.name.clone())) {
                // Even a fallback another tool now hides keeps its name.
                *name = Some(pin.clone());
            } else if name.as_deref().is_some_and(|name| pinned.contains(name)) {
                *name = Some(format!("{label}_{}", spec.name));
            }
        }
    }
    let mut diagnostics = overridden
        .into_iter()
        .map(|name| {
            format!("several tools override `{name}`; each is published under its source's prefix")
        })
        .collect::<Vec<_>>();
    // Hidden fallback tools have no name; the others move into the set.
    let mut tools = candidates
        .into_iter()
        .zip(names)
        .filter_map(|((source, spec), name)| name.map(|name| ComposedTool { name, source, spec }))
        .collect::<Vec<_>>();
    let mut published = HashSet::new();
    tools.retain(|tool| {
        let label = |reason: &str| match &tool.source.attachment {
            Some(attachment) => format!(
                "tool `{}` of runtime attachment `{attachment}` is not published: {reason}",
                tool.spec.name
            ),
            None => format!("base tool `{}` is not published: {reason}", tool.spec.name),
        };
        if tool.spec.background_default && !tool.spec.background {
            diagnostics.push(label(
                "it declares `background_default` without `background`",
            ));
            return false;
        }
        if tool.spec.background
            && tool
                .spec
                .parameters
                .get("properties")
                .and_then(|properties| properties.get("background"))
                .is_some()
        {
            diagnostics.push(label(
                "it supports the background but declares its own `background` argument",
            ));
            return false;
        }
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

/// The source `source`'s tools compose under; `None` for the base layer.
pub(crate) fn composition_source(source: &SourcedProvider) -> Option<String> {
    let attachment = source.attachment.as_ref()?;
    Some(
        source
            .provider
            .composition_source()
            .unwrap_or_else(|| attachment.to_string()),
    )
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

    use super::{RoundPins, SourcedProvider, compose};

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

    /// A provider that composes under its own source, as a plugin does.
    struct Plugin(&'static str);

    #[async_trait]
    impl ToolProvider for Plugin {
        fn provider_id(&self) -> &'static str {
            "test.plugin"
        }
        fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
            Ok(Vec::new())
        }
        fn composition_source(&self) -> Option<String> {
            Some(self.0.to_owned())
        }
        fn get_permission_name(_: &str) -> Result<Option<&'static str>, ToolError> {
            Ok(None)
        }
        fn get_permission_resource(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
            Ok((None, None))
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

    /// A tool of plugin `name`, which the `global` runtime hosts.
    fn plugin(name: &'static str) -> SourcedProvider {
        SourcedProvider {
            provider: Arc::new(Plugin(name)),
            ..source(Some("global"))
        }
    }

    #[test]
    fn plugin_tools_compose_under_the_plugins_name() {
        assert_eq!(
            published(vec![
                (plugin("audit"), spec("webfetch", ToolCollision::Prefix)),
                (plugin("audit"), spec("lint", ToolCollision::Prefix)),
                (
                    source(Some("local")),
                    spec("webfetch", ToolCollision::Prefix)
                ),
            ]),
            ["audit_webfetch", "lint", "local_webfetch"]
        );
    }

    #[test]
    fn an_override_takes_the_bare_name_and_prefixes_what_it_overrides() {
        assert_eq!(
            published(vec![
                (source(Some("local")), spec("bash", ToolCollision::Prefix)),
                (
                    source(Some("global")),
                    spec("bash", ToolCollision::Fallback)
                ),
                (plugin("sandbox"), spec("bash", ToolCollision::Override)),
            ]),
            ["local_bash", "bash"]
        );
    }

    #[test]
    fn competing_overrides_are_both_prefixed_with_a_diagnostic() {
        let composition = compose(
            vec![
                (plugin("first"), spec("bash", ToolCollision::Override)),
                (plugin("second"), spec("bash", ToolCollision::Override)),
                (source(Some("local")), spec("bash", ToolCollision::Prefix)),
            ],
            &HashSet::new(),
            &RoundPins::new(),
        )
        .expect("composes");
        assert_eq!(
            composition
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["first_bash", "second_bash", "bash"]
        );
        assert_eq!(
            composition.diagnostics.len(),
            1,
            "{:?}",
            composition.diagnostics
        );
    }

    #[test]
    fn an_override_never_takes_a_base_name() {
        assert_eq!(
            published(vec![
                (source(None), spec("skill", ToolCollision::Prefix)),
                (plugin("audit"), spec("skill", ToolCollision::Override)),
            ]),
            ["skill", "audit_skill"]
        );
    }

    fn spec(name: &str, collision: ToolCollision) -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
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
        compose(
            candidates,
            &HashSet::from(["skill".to_owned()]),
            &RoundPins::new(),
        )
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
            &RoundPins::new(),
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
                &RoundPins::new(),
            )
            .err()
            .as_deref(),
            Some("lookup")
        );
    }

    fn pinned(
        candidates: Vec<(SourcedProvider, ToolSpec)>,
        pins: &[(&str, &str, &str)],
    ) -> Vec<String> {
        let pins = pins
            .iter()
            .map(|(label, tool, name)| {
                (
                    ((*label).to_owned(), (*tool).to_owned()),
                    (*name).to_owned(),
                )
            })
            .collect::<RoundPins>();
        compose(candidates, &HashSet::new(), &pins)
            .expect("composes")
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    #[test]
    fn a_pinned_tool_keeps_its_name_and_a_newcomer_takes_its_prefix() {
        let both = || {
            vec![
                (
                    source(Some("local_web")),
                    spec("bash", ToolCollision::Prefix),
                ),
                (
                    source(Some("labpc_api")),
                    spec("bash", ToolCollision::Prefix),
                ),
            ]
        };
        assert_eq!(
            pinned(both(), &[("local_web", "bash", "bash")]),
            ["bash", "labpc_api_bash"]
        );
        // Without pins, as at a new round, both take their prefix.
        assert_eq!(pinned(both(), &[]), ["local_web_bash", "labpc_api_bash"]);
    }

    #[test]
    fn a_pinned_name_stays_reserved_after_its_tool_left() {
        assert_eq!(
            pinned(
                vec![(
                    source(Some("labpc_api")),
                    spec("bash", ToolCollision::Prefix)
                )],
                &[("local_web", "bash", "bash")],
            ),
            ["labpc_api_bash"]
        );
    }

    #[test]
    fn a_pinned_fallback_stays_published_beside_the_tool_that_hides_it() {
        assert_eq!(
            pinned(
                vec![
                    (
                        source(Some("global")),
                        spec("webfetch", ToolCollision::Fallback)
                    ),
                    (
                        source(Some("local")),
                        spec("webfetch", ToolCollision::Prefix)
                    ),
                ],
                &[("global", "webfetch", "webfetch")],
            ),
            ["webfetch", "local_webfetch"]
        );
    }
}
