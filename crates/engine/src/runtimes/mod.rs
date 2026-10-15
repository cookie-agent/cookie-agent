//! Tool layers and runtimes: where a tool comes from and which workspace it
//! sees.
//!
//! Not to be confused with `crate::runtime` (the engine internals: sessions,
//! runs, the model loop) or [`crate::PublishedRuntime`] (the compiled model
//! runtime published to clients). Here a *runtime* is a source of tools and
//! workspace context, as specified in `docs/site/specs/runtimes.md`.
//!
//! The engine composes the model's tool list from three layers, always in this
//! order:
//!
//! 1. The **base layer** ([`ToolLayer::Base`]): tools the daemon implements
//!    for its own orchestration (delegation, messaging, skills, goals,
//!    `read_artifact`). It is not a runtime and has no workspace.
//! 2. The **global runtime** (ID
//!    [`GLOBAL_RUNTIME_ID`](cookie_agent_runtime_api::GLOBAL_RUNTIME_ID), kind
//!    [`RuntimeKind::Global`](cookie_agent_runtime_api::RuntimeKind::Global)): top-level MCP servers, plugins, and a
//!    `webfetch` declared [`ToolCollision::Fallback`]. No workspace.
//! 3. The **local coding runtime** (ID [`LOCAL_RUNTIME_ID`], kind
//!    [`RuntimeKind::Coding`](cookie_agent_runtime_api::RuntimeKind::Coding)): `read`, `write`, `edit`, `bash`, and
//!    `webfetch`, working in the daemon's working directory.
//!
//! Every tool is traced to its [`ToolOrigin`]. Preparation, permission
//! decisions, and visibility read the workspace of that origin, never the
//! daemon's working directory, which only locates the session store.
//!
//! The runtime identity types ([`ToolOrigin`], [`RuntimeInfo`],
//! [`WorkspaceContext`]) live in `cookie_agent_runtime_api`, which tool
//! providers depend on; this module composes them.

use std::sync::Arc;

use cookie_agent_runtime_api::{
    SessionToolContext, ToolCollision, ToolOrigin, ToolProvider, ToolSpec,
};

mod builtin;
mod registry;

pub(crate) use builtin::{GlobalTemplate, LocalTemplate};
pub(crate) use registry::{RuntimeInstance, RuntimeRegistry};
pub use registry::{
    RuntimeInstanceStatus, RuntimeInstanceSummary, RuntimeTemplate, RuntimeTemplateSummary,
    StartedRuntime,
};

/// One-line description of where a tool runs, for the approval reviewer.
pub(crate) fn describe_origin(origin: &ToolOrigin) -> String {
    match origin {
        ToolOrigin::Base => "the daemon's base layer (no workspace)".to_owned(),
        ToolOrigin::Runtime(runtime) => match runtime.workspace() {
            Some(workspace) => format!(
                "the `{}` {} runtime with workspace {}",
                runtime.id(),
                runtime.kind().as_str(),
                workspace.root().display()
            ),
            None => format!(
                "the `{}` {} runtime (no workspace)",
                runtime.id(),
                runtime.kind().as_str()
            ),
        },
    }
}

/// Where [`crate::Engine::try_register_tool_provider`] adds a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolLayer {
    /// The daemon's base layer.
    Base,
    /// The global runtime instance.
    Global,
    /// The local coding runtime instance.
    Local,
}

/// A provider together with the layer or runtime attachment that owns it.
#[derive(Clone)]
pub(crate) struct SourcedProvider {
    /// The session's attachment name; `None` for the base layer.
    pub(crate) attachment: Option<cookie_agent_protocol::AttachmentName>,
    /// The instance generation; `0` for the base layer.
    pub(crate) generation: u64,
    pub(crate) origin: ToolOrigin,
    pub(crate) provider: Arc<dyn ToolProvider>,
}

/// The provider and spec that currently answer to `name` in `session`: the
/// non-fallback tool when one exists, else a fallback.
pub(crate) fn current_tool(
    providers: &[SourcedProvider],
    session: &SessionToolContext,
    name: &str,
) -> Option<(SourcedProvider, ToolSpec)> {
    let mut fallback = None;
    for source in providers {
        let Ok(tools) = source.provider.tools_for_session(session) else {
            continue;
        };
        let Some(spec) = tools.into_iter().find(|tool| tool.name == name) else {
            continue;
        };
        match spec.collision {
            ToolCollision::Prefix => return Some((source.clone(), spec)),
            ToolCollision::Fallback => {
                fallback.get_or_insert((source.clone(), spec));
            }
        }
    }
    fallback
}

/// Applies the collision rules to the tools about to be published, keeping
/// their order: a fallback tool is hidden when a non-fallback tool with the
/// same name is published. Any other duplicate name is an error naming it.
pub(crate) fn resolve_collisions<T>(
    candidates: Vec<(T, ToolSpec)>,
) -> Result<Vec<(T, ToolSpec)>, String> {
    let mut seen = std::collections::HashMap::<&str, (usize, usize)>::new();
    for (_, spec) in &candidates {
        let counts = seen.entry(spec.name.as_str()).or_default();
        match spec.collision {
            ToolCollision::Prefix => counts.0 += 1,
            ToolCollision::Fallback => counts.1 += 1,
        }
    }
    if let Some((name, _)) = seen
        .iter()
        .find(|(_, (prefix, fallback))| *prefix > 1 || (*prefix == 0 && *fallback > 1))
    {
        return Err((*name).to_owned());
    }
    let shadowed = seen
        .into_iter()
        .filter(|(_, (prefix, _))| *prefix > 0)
        .map(|(name, _)| name.to_owned())
        .collect::<std::collections::HashSet<_>>();
    Ok(candidates
        .into_iter()
        .filter(|(_, spec)| {
            spec.collision == ToolCollision::Prefix || !shadowed.contains(&spec.name)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{ToolCollision, ToolSpec, resolve_collisions};

    fn spec(name: &str, collision: ToolCollision) -> ToolSpec {
        ToolSpec {
            collision,
            name: name.into(),
            permission_name: "webfetch".into(),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }
    }

    #[test]
    fn fallback_tools_hide_behind_a_published_name() {
        let composed = resolve_collisions(vec![
            ("global", spec("webfetch", ToolCollision::Fallback)),
            ("global", spec("lookup", ToolCollision::Fallback)),
            ("local", spec("webfetch", ToolCollision::Prefix)),
        ])
        .expect("no hard collision");
        assert_eq!(
            composed
                .iter()
                .map(|(source, spec)| (*source, spec.name.as_str()))
                .collect::<Vec<_>>(),
            [("global", "lookup"), ("local", "webfetch")]
        );
    }

    #[test]
    fn duplicate_names_without_one_winner_are_errors() {
        assert_eq!(
            resolve_collisions(vec![
                ("a", spec("bash", ToolCollision::Prefix)),
                ("b", spec("bash", ToolCollision::Prefix)),
            ])
            .err()
            .as_deref(),
            Some("bash")
        );
        assert_eq!(
            resolve_collisions(vec![
                ("a", spec("webfetch", ToolCollision::Fallback)),
                ("b", spec("webfetch", ToolCollision::Fallback)),
            ])
            .err()
            .as_deref(),
            Some("webfetch")
        );
    }
}
