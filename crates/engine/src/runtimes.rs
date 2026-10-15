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
//! 2. The **global runtime** (ID [`GLOBAL_RUNTIME_ID`], kind
//!    [`RuntimeKind::Global`]): top-level MCP servers, plugins, and a
//!    `webfetch` declared [`ToolCollision::Fallback`]. No workspace.
//! 3. The **local coding runtime** (ID [`LOCAL_RUNTIME_ID`], kind
//!    [`RuntimeKind::Coding`]): `read`, `write`, `edit`, `bash`, and
//!    `webfetch`, working in the daemon's working directory.
//!
//! Every tool is traced to its [`ToolOrigin`]. Preparation, permission
//! decisions, and visibility read the workspace of that origin, never the
//! daemon's working directory, which only locates the session store.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::tool_api::{SessionToolContext, ToolCollision, ToolProvider, ToolSpec};

/// Runtime ID of the global runtime; also its attachment name.
pub const GLOBAL_RUNTIME_ID: &str = "global";
/// Runtime ID of the built-in coding runtime for the daemon's working
/// directory.
pub const LOCAL_RUNTIME_ID: &str = "local";
/// Upper bound of an attachment name in bytes.
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 32;

/// What a runtime offers. A runtime reports its own kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeKind {
    /// The daemon's own runtime: MCP servers and plugins, no workspace.
    Global,
    /// A runtime that works in one workspace with file and shell tools.
    Coding,
}

impl RuntimeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Coding => "coding",
        }
    }
}

/// The workspace a coding runtime works in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceContext {
    root: PathBuf,
    platform: String,
}

impl WorkspaceContext {
    /// Resolves `path` to its canonical form on the current platform.
    ///
    /// # Errors
    ///
    /// Fails when the path cannot be canonicalized, for example because it
    /// does not exist.
    pub fn new(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: path.canonicalize()?,
            platform: std::env::consts::OS.to_owned(),
        })
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Operating system the workspace lives on, as `std::env::consts::OS`
    /// names it.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }
}

/// Identity and context of one runtime instance.
#[derive(Debug, Eq, PartialEq)]
pub struct RuntimeInfo {
    id: String,
    kind: RuntimeKind,
    attachment_name: String,
    workspace: Option<WorkspaceContext>,
}

impl RuntimeInfo {
    /// The global runtime: ID, kind, and attachment name `global`, no
    /// workspace.
    #[must_use]
    pub fn global() -> Self {
        Self {
            id: GLOBAL_RUNTIME_ID.to_owned(),
            kind: RuntimeKind::Global,
            attachment_name: GLOBAL_RUNTIME_ID.to_owned(),
            workspace: None,
        }
    }

    /// A coding runtime for `workspace`, named per [`coding_attachment_name`].
    #[must_use]
    pub fn coding(id: &str, workspace: WorkspaceContext) -> Self {
        Self {
            id: id.to_owned(),
            kind: RuntimeKind::Coding,
            attachment_name: coding_attachment_name(id, workspace.root()),
            workspace: Some(workspace),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn kind(&self) -> RuntimeKind {
        self.kind
    }

    /// The name a session would attach this instance under. Tool-name
    /// prefixing on collisions uses it (not yet applied).
    #[must_use]
    pub fn attachment_name(&self) -> &str {
        &self.attachment_name
    }

    #[must_use]
    pub const fn workspace(&self) -> Option<&WorkspaceContext> {
        self.workspace.as_ref()
    }
}

/// `<runtime ID>_<workspace basename>` with the basename lowercased, every
/// character outside `[a-z0-9_]` replaced by `_`, runs of `_` collapsed, the
/// result capped at [`MAX_ATTACHMENT_NAME_BYTES`], and trailing `_` trimmed so
/// a prefixed tool name never contains `__`. A workspace without a basename
/// (a filesystem root) yields the runtime ID alone.
#[must_use]
pub fn coding_attachment_name(runtime_id: &str, workspace: &Path) -> String {
    let basename = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name = String::with_capacity(MAX_ATTACHMENT_NAME_BYTES);
    let source = format!("{runtime_id}_{basename}");
    for character in source.chars() {
        let character = character.to_ascii_lowercase();
        let character = if character.is_ascii_lowercase() || character.is_ascii_digit() {
            character
        } else {
            '_'
        };
        if character == '_' && name.ends_with('_') {
            continue;
        }
        if name.len() == MAX_ATTACHMENT_NAME_BYTES {
            break;
        }
        name.push(character);
    }
    name.truncate(name.trim_end_matches('_').len());
    name
}

/// The layer or runtime a tool belongs to.
#[derive(Clone, Debug)]
pub enum ToolOrigin {
    /// The daemon's base layer: no runtime, no workspace.
    Base,
    /// A runtime instance.
    Runtime(Arc<RuntimeInfo>),
}

impl ToolOrigin {
    #[must_use]
    pub fn runtime(&self) -> Option<&RuntimeInfo> {
        match self {
            Self::Base => None,
            Self::Runtime(runtime) => Some(runtime),
        }
    }

    #[must_use]
    pub fn workspace(&self) -> Option<&WorkspaceContext> {
        self.runtime().and_then(RuntimeInfo::workspace)
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace().map(WorkspaceContext::root)
    }

    /// Stable bytes that identify where a prepared operation runs, for the
    /// operation's execution-context digest: the canonical workspace root, or
    /// the runtime ID (`base` for the base layer) when there is no workspace.
    #[must_use]
    pub fn context_bytes(&self) -> Vec<u8> {
        match self {
            Self::Base => b"base".to_vec(),
            Self::Runtime(runtime) => runtime.workspace().map_or_else(
                || format!("runtime:{}", runtime.id()).into_bytes(),
                |workspace| workspace.root().as_os_str().as_encoded_bytes().to_vec(),
            ),
        }
    }
}

/// Where [`crate::Engine::try_register_tool_provider`] adds a provider.
/// Variants are in composition order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolLayer {
    /// The daemon's base layer.
    Base,
    /// The global runtime.
    Global,
    /// The local coding runtime.
    Local,
}

impl ToolLayer {
    const fn index(self) -> usize {
        match self {
            Self::Base => 0,
            Self::Global => 1,
            Self::Local => 2,
        }
    }
}

/// Builds a coding runtime's tool providers for its workspace, so the
/// runtime is the single source of the workspace its tools work in.
pub type CodingToolsFactory =
    Arc<dyn Fn(&WorkspaceContext) -> Vec<Arc<dyn ToolProvider>> + Send + Sync>;

/// What the engine needs to build the global and local runtimes. MCP servers
/// and plugins come from the configuration; the base layer is registered
/// after open because its providers need the engine.
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Global-runtime providers besides MCP servers and plugins.
    pub global_tools: Vec<Arc<dyn ToolProvider>>,
    /// Workspace of the local coding runtime.
    pub local_workspace: PathBuf,
    /// Builds the local coding runtime's providers.
    pub local_tools: CodingToolsFactory,
}

impl RuntimeOptions {
    /// A local coding runtime at `local_workspace` and a global runtime with
    /// no tools beyond MCP servers and plugins.
    #[must_use]
    pub fn new(local_workspace: impl Into<PathBuf>) -> Self {
        Self {
            global_tools: Vec::new(),
            local_workspace: local_workspace.into(),
            local_tools: Arc::new(|_| Vec::new()),
        }
    }
}

/// A provider together with the layer or runtime that owns it.
#[derive(Clone)]
pub(crate) struct SourcedProvider {
    pub(crate) origin: ToolOrigin,
    pub(crate) provider: Arc<dyn ToolProvider>,
}

/// The engine's tool sources, kept in composition order: base layer, global
/// runtime, local coding runtime, each in registration order.
pub(crate) struct RuntimeSet {
    global: Arc<RuntimeInfo>,
    local: Arc<RuntimeInfo>,
    /// Providers per layer, indexed by [`ToolLayer::index`].
    providers: Mutex<[Vec<SourcedProvider>; 3]>,
}

impl RuntimeSet {
    pub(crate) fn new(local_workspace: WorkspaceContext) -> Self {
        Self {
            global: Arc::new(RuntimeInfo::global()),
            local: Arc::new(RuntimeInfo::coding(LOCAL_RUNTIME_ID, local_workspace)),
            providers: Mutex::default(),
        }
    }

    /// The local coding runtime's workspace root: the workspace of project
    /// skills, `AGENTS.md`, the `<working_directory>` block, and session
    /// overlay effects until sessions attach runtimes.
    pub(crate) fn local_workspace_root(&self) -> &Path {
        self.local
            .workspace()
            .expect("the local coding runtime has a workspace")
            .root()
    }

    fn origin(&self, layer: ToolLayer) -> ToolOrigin {
        match layer {
            ToolLayer::Base => ToolOrigin::Base,
            ToolLayer::Global => ToolOrigin::Runtime(Arc::clone(&self.global)),
            ToolLayer::Local => ToolOrigin::Runtime(Arc::clone(&self.local)),
        }
    }

    pub(crate) fn push(&self, layer: ToolLayer, provider: Arc<dyn ToolProvider>) {
        let origin = self.origin(layer);
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[layer.index()]
        .push(SourcedProvider { origin, provider });
    }

    /// Every provider in composition order.
    pub(crate) fn providers(&self) -> Vec<SourcedProvider> {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .concat()
    }

    pub(crate) fn clear(&self) {
        for layer in self
            .providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter_mut()
        {
            layer.clear();
        }
    }
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
    use std::path::Path;

    use super::{ToolCollision, ToolSpec, coding_attachment_name, resolve_collisions};

    #[test]
    fn attachment_names_follow_the_sanitization_rule() {
        for (id, workspace, expected) in [
            ("labpc", "/home/me/cookie-agent", "labpc_cookie_agent"),
            ("local", "/srv/My Project.v2", "local_my_project_v2"),
            ("local", "/srv/a--b__c..d", "local_a_b_c_d"),
            ("local", "/srv/_leading", "local_leading"),
            ("local", "/srv/trailing-", "local_trailing"),
            ("local", "/srv/Ünïcode", "local_n_code"),
            ("local", "/", "local"),
            // 32 bytes exactly: `local_` plus 26 characters.
            (
                "local",
                "/srv/abcdefghijklmnopqrstuvwxyz",
                "local_abcdefghijklmnopqrstuvwxyz",
            ),
            (
                "local",
                "/srv/abcdefghijklmnopqrstuvwxyz0",
                "local_abcdefghijklmnopqrstuvwxyz",
            ),
            // A cut that lands on `_` drops it instead of ending in one.
            (
                "local",
                "/srv/abcdefghijklmnopqrstuvwxy-z",
                "local_abcdefghijklmnopqrstuvwxy",
            ),
        ] {
            let name = coding_attachment_name(id, Path::new(workspace));
            assert_eq!(name, expected, "{workspace}");
            assert!(name.len() <= 32, "{name}");
        }
    }

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
