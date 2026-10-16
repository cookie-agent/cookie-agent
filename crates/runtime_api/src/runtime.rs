//! Runtime identity: which runtime a tool belongs to and the workspace it
//! works in, as specified in `docs/site/specs/runtimes.md`.
//!
//! A *runtime* is a source of tools and workspace context. The engine
//! composes the model's tool list from the daemon's base layer, the global
//! runtime, and coding runtimes; every tool is traced to its [`ToolOrigin`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::ToolProvider;

/// Runtime ID of the global runtime; also its attachment name.
pub const GLOBAL_RUNTIME_ID: &str = "global";
/// Runtime ID of the built-in coding runtime for the daemon's working
/// directory.
pub const LOCAL_RUNTIME_ID: &str = "local";
/// The kind the coding runtimes cookie agent ships declare. A label only: the
/// daemon never branches on a runtime's kind.
pub const CODING_RUNTIME_KIND: &str = "coding";
/// Upper bound of an attachment name in bytes.
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 32;

pub use cookie_agent_protocol::RuntimeKind;

/// The workspace a coding runtime works in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceContext {
    root: PathBuf,
    platform: Option<String>,
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
            platform: Some(std::env::consts::OS.to_owned()),
        })
    }

    /// A workspace a runtime reported: `root` is already canonical on the
    /// runtime's machine, which runs `platform` when the runtime said.
    #[must_use]
    pub fn reported(root: PathBuf, platform: Option<String>) -> Self {
        Self { root, platform }
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Operating system the workspace lives on, as `std::env::consts::OS`
    /// names it, when the runtime reported one.
    #[must_use]
    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
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
            kind: RuntimeKind::new(GLOBAL_RUNTIME_ID).expect("`global` is a valid runtime kind"),
            attachment_name: GLOBAL_RUNTIME_ID.to_owned(),
            workspace: None,
        }
    }

    /// A runtime of `kind` that reports `attachment_name` and `workspace`.
    #[must_use]
    pub fn new(
        id: &str,
        kind: RuntimeKind,
        attachment_name: &str,
        workspace: Option<WorkspaceContext>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            kind,
            attachment_name: attachment_name.to_owned(),
            workspace,
        }
    }

    /// A coding runtime for `workspace`, named per [`coding_attachment_name`].
    #[must_use]
    pub fn coding(id: &str, workspace: WorkspaceContext) -> Self {
        Self {
            id: id.to_owned(),
            kind: RuntimeKind::new(CODING_RUNTIME_KIND).expect("`coding` is a valid runtime kind"),
            attachment_name: coding_attachment_name(id, workspace.root()),
            workspace: Some(workspace),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn kind(&self) -> &RuntimeKind {
        &self.kind
    }

    /// The name the instance reports for itself. A session attaches it under
    /// this name, suffixed with `_2`, `_3`, ... when the session already has
    /// an attachment of that name.
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

/// Builds a coding runtime's tool providers for its workspace, so the
/// runtime is the single source of the workspace its tools work in.
pub type CodingToolsFactory =
    Arc<dyn Fn(&WorkspaceContext) -> Vec<Arc<dyn ToolProvider>> + Send + Sync>;

/// Starts runtimes that speak the runtime protocol: each connection is one
/// runtime instance. The in-process coding runtime implements it with an
/// in-memory frame pair; a runtime in a child process implements it by
/// spawning the process and framing its stdio with
/// [`crate::rpc::LineTransport`].
#[async_trait::async_trait]
pub trait RuntimeConnector: Send + Sync {
    /// Starts one runtime and returns the daemon's end of its transport.
    /// `workspace` is the path a session asked for, exactly as given, when
    /// the template takes one; the runtime resolves it on its own machine.
    /// The daemon then sends `runtime/initialize`. The error is a
    /// human-readable reason.
    async fn connect(&self, workspace: Option<&Path>) -> Result<RuntimeConnection, String>;
}

/// A runtime a [`RuntimeConnector`] started.
pub struct RuntimeConnection {
    /// The daemon's end of the runtime's transport.
    pub transport: Box<dyn cookie_agent_protocol::Transport>,
    /// What the runtime writes outside the protocol, such as a child
    /// process's standard error; the daemon adds it to failure reasons.
    pub diagnostics: Option<DiagnosticTail>,
}

impl RuntimeConnection {
    /// A connection with no diagnostics beyond the protocol.
    #[must_use]
    pub fn new(transport: Box<dyn cookie_agent_protocol::Transport>) -> Self {
        Self {
            transport,
            diagnostics: None,
        }
    }
}

/// The last lines a runtime wrote outside the protocol, bounded to
/// [`DiagnosticTail::MAX_BYTES`] in at most [`DiagnosticTail::MAX_LINES`]
/// lines of at most [`DiagnosticTail::MAX_LINE_BYTES`] each. Clones share
/// one buffer.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticTail {
    lines: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl DiagnosticTail {
    pub const MAX_BYTES: usize = 4 * 1024;
    pub const MAX_LINES: usize = 32;
    pub const MAX_LINE_BYTES: usize = 512;

    /// Records one line (without its terminator), cut to
    /// [`Self::MAX_LINE_BYTES`] and stripped of control characters; the
    /// oldest lines go when the bounds are exceeded. Blank lines are skipped.
    pub fn push_line(&self, line: &str) {
        let mut cleaned = String::with_capacity(line.len().min(Self::MAX_LINE_BYTES));
        for character in line.chars() {
            let character = if character == '\t' { ' ' } else { character };
            if character.is_control() {
                continue;
            }
            if cleaned.len() + character.len_utf8() > Self::MAX_LINE_BYTES {
                break;
            }
            cleaned.push(character);
        }
        let cleaned = cleaned.trim_end();
        if cleaned.trim().is_empty() {
            return;
        }
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lines.push_back(cleaned.to_owned());
        while lines.len() > Self::MAX_LINES
            || lines.iter().map(|line| line.len() + 1).sum::<usize>() > Self::MAX_BYTES
        {
            lines.pop_front();
        }
    }

    /// The recorded lines joined by newlines; `None` when there are none.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        let lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (!lines.is_empty()).then(|| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    /// `reason` followed by the recorded lines, for a failure message.
    #[must_use]
    pub fn annotate(&self, reason: &str) -> String {
        match self.text() {
            Some(text) => format!("{reason}; stderr:\n{text}"),
            None => reason.to_owned(),
        }
    }
}

/// How the daemon runs its `local` coding runtime.
#[derive(Clone)]
pub enum LocalRuntime {
    /// Through the runtime protocol, like any other runtime.
    Protocol(Arc<dyn RuntimeConnector>),
    /// With providers called in-process, for tests and embedders that do not
    /// need the protocol.
    InProcess(CodingToolsFactory),
}

/// What the engine needs to build the global and local runtimes. MCP servers
/// and plugins come from the configuration, and the engine provides the base
/// layer itself.
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Global-runtime providers besides MCP servers and plugins.
    pub global_tools: Vec<Arc<dyn ToolProvider>>,
    /// Workspace of the local coding runtime.
    pub local_workspace: PathBuf,
    /// How the local coding runtime runs.
    pub local: LocalRuntime,
}

impl RuntimeOptions {
    /// A local coding runtime at `local_workspace` with no tools, called
    /// in-process, and a global runtime with no tools beyond MCP servers and
    /// plugins.
    #[must_use]
    pub fn new(local_workspace: impl Into<PathBuf>) -> Self {
        Self {
            global_tools: Vec::new(),
            local_workspace: local_workspace.into(),
            local: LocalRuntime::InProcess(Arc::new(|_| Vec::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DiagnosticTail, coding_attachment_name};

    #[test]
    fn a_diagnostic_tail_keeps_the_last_bounded_lines() {
        let tail = DiagnosticTail::default();
        assert_eq!(tail.text(), None);
        assert_eq!(tail.annotate("exited"), "exited");
        tail.push_line("ssh: connect to host labpc port 22: Connection refused\r");
        tail.push_line("   ");
        tail.push_line("bell\u{7} and\ttab");
        assert_eq!(
            tail.text().unwrap(),
            "ssh: connect to host labpc port 22: Connection refused\nbell and tab"
        );
        assert_eq!(
            tail.clone().annotate("exited"),
            "exited; stderr:\nssh: connect to host labpc port 22: Connection refused\nbell and tab"
        );

        let long = "é".repeat(DiagnosticTail::MAX_LINE_BYTES);
        tail.push_line(&long);
        let last = tail.text().unwrap().lines().last().unwrap().to_owned();
        assert_eq!(last.len(), DiagnosticTail::MAX_LINE_BYTES);

        for index in 0..DiagnosticTail::MAX_LINES + 5 {
            tail.push_line(&format!("line {index}"));
        }
        let text = tail.text().unwrap();
        assert_eq!(text.lines().count(), DiagnosticTail::MAX_LINES);
        assert!(text.ends_with(&format!("line {}", DiagnosticTail::MAX_LINES + 4)));

        for _ in 0..DiagnosticTail::MAX_LINES {
            tail.push_line(&"x".repeat(DiagnosticTail::MAX_LINE_BYTES));
        }
        assert!(tail.text().unwrap().len() <= DiagnosticTail::MAX_BYTES);
    }

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
}
