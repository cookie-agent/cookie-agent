//! The error registry, as specified in `docs/site/specs/compatibility-v1.md`
//! section 6: every error the four protocols send carries a stable
//! `data.code` and a `message` a user can understand, both from this one
//! table.
//!
//! An error object is built from its registry entry:
//!
//! ```
//! use cookie_agent_protocol::errors;
//!
//! let error = errors::MEDIA_UNSUPPORTED
//!     .error()
//!     .field("kind", "image")
//!     .field("model", "anthropic/claude-haiku-5-5")
//!     .detail("the model declares input modalities: text")
//!     .try_build()
//!     .unwrap();
//! assert_eq!(error.code, errors::APPLICATION_ERROR);
//! assert_eq!(error.message, "The model anthropic/claude-haiku-5-5 does not accept image input.");
//! assert_eq!(error.data.unwrap()["code"], "media_unsupported");
//! ```
//!
//! Clients branch only on `data.code`, show `message`, then `hint` when
//! present, and `detail` on request ([`ErrorData`] reads them). Every
//! protocol sends only these errors, both ways. `docs/site/reference/errors.md`
//! is generated from this table ([`reference_markdown`]).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::JsonRpcError;

/// JSON-RPC 2.0: the frame is not JSON.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC 2.0: the frame is not a request, notification, or reply.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC 2.0: no such method.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC 2.0: the params do not match the method.
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC 2.0: the receiver failed in a way it cannot describe further.
pub const INTERNAL_ERROR: i32 = -32603;
/// Any application error; `data.code` says which.
pub const APPLICATION_ERROR: i32 = -32000;
/// The connection must complete its handshake first.
pub const HANDSHAKE_REQUIRED: i32 = -32001;
/// The peer speaks another major of the protocol.
pub const UNSUPPORTED_PROTOCOL: i32 = -32002;
/// The connection has not authenticated.
pub const UNAUTHENTICATED: i32 = -32003;

/// The most bytes of `message`.
pub const MAX_MESSAGE_BYTES: usize = 1024;
/// The most bytes of `data.hint`.
pub const MAX_HINT_BYTES: usize = 512;
/// The most bytes of `data.detail`.
pub const MAX_DETAIL_BYTES: usize = 4096;

/// `data` members every error may carry, which a code cannot declare as its
/// own fields.
pub const RESERVED_DATA_MEMBERS: &[&str] = &["code", "hint", "detail"];

/// One structured `data` member an error code defines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorField {
    pub name: &'static str,
    /// A required field is always present, so templates may name it.
    pub required: bool,
    pub description: &'static str,
}

const fn required(name: &'static str, description: &'static str) -> ErrorField {
    ErrorField {
        name,
        required: true,
        description,
    }
}

const fn optional(name: &'static str, description: &'static str) -> ErrorField {
    ErrorField {
        name,
        required: false,
        description,
    }
}

/// One registered error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorCode {
    /// The stable snake-case `data.code`.
    pub code: &'static str,
    /// The JSON-RPC `code` it is sent with.
    pub rpc_code: i32,
    pub fields: &'static [ErrorField],
    /// The `message`, with `{field}` replaced by required fields' values.
    pub message: &'static str,
    /// The `data.hint`, filled like `message`, where a next step helps.
    pub hint: Option<&'static str>,
    /// A one-line description for the reference docs.
    pub description: &'static str,
}

impl ErrorCode {
    /// Starts an error of this code.
    pub fn error(&'static self) -> ErrorBuilder {
        ErrorBuilder {
            code: self,
            fields: Map::new(),
            detail: None,
            problem: None,
        }
    }

    #[must_use]
    pub fn field(&self, name: &str) -> Option<&'static ErrorField> {
        self.fields.iter().find(|field| field.name == name)
    }
}

macro_rules! error_codes {
    ($(
        $(#[$meta:meta])*
        $constant:ident {
            code: $code:literal,
            rpc: $rpc:expr,
            fields: [$($field:expr),* $(,)?],
            message: $message:literal,
            hint: $hint:expr,
            description: $description:literal $(,)?
        }
    )+) => {
        $(
            $(#[$meta])*
            pub static $constant: ErrorCode = ErrorCode {
                code: $code,
                rpc_code: $rpc,
                fields: &[$($field),*],
                message: $message,
                hint: $hint,
                description: $description,
            };
        )+

        /// Every registered code.
        pub static REGISTRY: &[&ErrorCode] = &[$(&$constant),+];
    };
}

error_codes! {
    // --- protocol --------------------------------------------------------
    PARSE_FAILED {
        code: "parse_error",
        rpc: PARSE_ERROR,
        fields: [],
        message: "A message could not be read because it is not valid JSON.",
        hint: None,
        description: "A frame that is not JSON; the daemon answers it and reads on, while the shared client closes its connection.",
    }
    INVALID_MESSAGE {
        code: "invalid_request",
        rpc: INVALID_REQUEST,
        fields: [],
        message: "A message is not a valid request.",
        hint: None,
        description: "A frame that is not a JSON-RPC request, notification, or reply.",
    }
    REQUEST_ID_REQUIRED {
        code: "request_id_required",
        rpc: INVALID_REQUEST,
        fields: [required("method", "The method that was sent as a notification.")],
        message: "{method} must be sent as a request with an ID.",
        hint: None,
        description: "A method that only works as a request arrived as a notification.",
    }
    UNKNOWN_METHOD {
        code: "method_not_found",
        rpc: METHOD_NOT_FOUND,
        fields: [required("method", "The method name.")],
        message: "The other side does not know the method {method}.",
        hint: Some("Update the older of the two programs."),
        description: "A method the receiver does not know.",
    }
    BAD_PARAMS {
        code: "invalid_params",
        rpc: INVALID_PARAMS,
        fields: [optional("field", "The JSON path of the first bad member.")],
        message: "The request's parameters are not valid.",
        hint: None,
        description: "Params that do not match the method.",
    }
    UNKNOWN_FIELD {
        code: "unknown_field",
        rpc: INVALID_PARAMS,
        fields: [required("field", "The JSON path of the member.")],
        message: "The request has a member the receiver does not know: {field}.",
        hint: Some("Update the older of the two programs."),
        description: "A command carried a member its receiver does not know.",
    }
    UNKNOWN_VARIANT {
        code: "unknown_variant",
        rpc: INVALID_PARAMS,
        fields: [
            required("field", "The JSON path of the value."),
            optional("value", "The value that is not known."),
        ],
        message: "The request uses a value the receiver does not know at {field}.",
        hint: Some("Update the older of the two programs."),
        description: "A command carried an enum value or tag its receiver does not know.",
    }
    INTERNAL {
        code: "internal_error",
        rpc: INTERNAL_ERROR,
        fields: [],
        message: "Something went wrong while handling the request.",
        hint: Some("Try again; if it keeps failing, check the daemon log."),
        description: "A failure the receiver cannot describe further.",
    }
    HANDSHAKE_FIRST {
        code: "handshake_required",
        rpc: HANDSHAKE_REQUIRED,
        fields: [],
        message: "The connection must complete its handshake first.",
        hint: None,
        description: "A request before the handshake.",
    }
    PROTOCOL_UNSUPPORTED {
        code: "unsupported_protocol",
        rpc: UNSUPPORTED_PROTOCOL,
        fields: [
            required("supported_version", "The version the receiver speaks."),
            required("peer_version", "The version the peer sent."),
        ],
        message: "The two sides speak incompatible protocol versions: {supported_version} here and {peer_version} on the other side.",
        hint: Some("Update cookie or the program connecting to it so that their versions match."),
        description: "The peer speaks another major version of the protocol.",
    }
    NOT_AUTHENTICATED {
        code: "unauthenticated",
        rpc: UNAUTHENTICATED,
        fields: [],
        message: "The connection is not authenticated.",
        hint: Some("Connect with the daemon's token."),
        description: "A request before authentication, or a wrong token.",
    }

    // --- engine and sessions ---------------------------------------------
    ENGINE_FAILED {
        code: "engine_error",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The daemon could not complete the request.",
        hint: Some("See the details for the cause."),
        description: "A failure with no more specific code; `detail` says what failed.",
    }
    NO_RUNNABLE_MODEL {
        code: "no_runnable_model",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "No model is ready to run.",
        hint: Some("Connect a provider with /connect, or choose another model."),
        description: "No configured model can run: no provider is connected or none offers the model.",
    }
    PROVIDER_STORE_RELOAD_FAILED {
        code: "provider_store_reload_failed",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The saved provider connections could not be reloaded.",
        hint: Some("Check the details, then reconnect the provider."),
        description: "Reloading the provider store after a change failed.",
    }
    SESSION_OWNED_ELSEWHERE {
        code: "session_owned_elsewhere",
        rpc: APPLICATION_ERROR,
        fields: [optional("session_id", "The session.")],
        message: "This session is open in another cookie process.",
        hint: Some("Close it there, or open it read-only."),
        description: "Another process holds the session's lock.",
    }
    SESSION_NOT_FOUND {
        code: "session_not_found",
        rpc: APPLICATION_ERROR,
        fields: [optional("session_id", "The session.")],
        message: "The session does not exist.",
        hint: None,
        description: "A request named a session the daemon does not have.",
    }
    SESSION_RUNNING {
        code: "session_running",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The session."),
            optional("client_run_id", "The run request's idempotency key."),
        ],
        message: "Another run is already active in this session.",
        hint: Some("Steer or queue the input instead, or wait for the run to finish."),
        description: "`run.start` on a session that is running.",
    }
    IDEMPOTENCY_CONFLICT {
        code: "idempotency_conflict",
        rpc: APPLICATION_ERROR,
        fields: [
            optional("session_id", "The session the request named."),
            optional("provider_id", "The provider the request named."),
            optional("approval_id", "The approval the request named."),
            optional("client_run_id", "The reused run key."),
            optional("client_rename_id", "The reused rename key."),
            optional("client_response_id", "The reused approval-response key."),
            optional("client_connect_id", "The reused connect key."),
            optional("client_request_id", "The reused request key."),
        ],
        message: "This request reuses the ID of an earlier request that was different.",
        hint: Some("Send the request again with a new ID."),
        description: "An idempotency key was reused with different params.",
    }
    STALE_REVERT {
        code: "stale_revert",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The session."),
            required("current_last_seq", "The session's last event now."),
        ],
        message: "The session changed before the revert could be applied.",
        hint: Some("Reload the session and choose the point to revert to again."),
        description: "A revert named an expected last event that is no longer the last.",
    }
    NOT_A_ROOT_SESSION {
        code: "not_a_root_session",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The session."),
            required("root_session_id", "The root of its tree."),
        ],
        message: "This session is a subagent of another session, and only a whole session tree can be deleted.",
        hint: Some("Delete its root session, which deletes its subagents with it."),
        description: "`session.delete` named a delegated session instead of a root.",
    }
    SESSION_TREE_RUNNING {
        code: "session_tree_running",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The root session."),
            optional("running_session_id", "The session of the tree that is busy."),
        ],
        message: "This session or one of its subagents is running, so it cannot be deleted.",
        hint: Some("Wait for it to finish, or cancel it, then delete the session."),
        description: "`session.delete` on a tree with an active run, a compaction, or a subagent waiting for a slot.",
    }
    SESSION_CHANGED {
        code: "session_changed",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The session."),
            required("current_last_seq", "The session's last event now."),
        ],
        message: "The session has new activity since it was shown, so it was not deleted.",
        hint: Some("Review the new turns, then delete the session again if you still want to."),
        description: "`session.delete` named an expected last event that is no longer the root's last.",
    }
    SESSION_REQUIRES_NEWER_ENGINE {
        code: "session_requires_newer_engine",
        rpc: APPLICATION_ERROR,
        fields: [
            optional("session_id", "The session."),
            optional("event", "The event type this build does not know."),
            optional("history_format", "The session's history format."),
        ],
        message: "This session was written by a newer version of cookie, so it can only be viewed.",
        hint: Some("Update cookie to continue this session."),
        description: "The session's log has an event or history format this build cannot continue.",
    }
    SEQUENCE_BROKEN {
        code: "sequence_broken",
        rpc: APPLICATION_ERROR,
        fields: [
            optional("session_id", "The session."),
            required("seq", "The first record whose sequence does not advance."),
        ],
        message: "This session's log is damaged after event {seq}, so it can only be viewed.",
        hint: Some("Fork the session from an earlier point to continue it."),
        description: "The session's log has a record whose sequence does not advance; the readable part opens render-only.",
    }
    UNREADABLE_MODEL_BINDING {
        code: "unreadable_model_binding",
        rpc: APPLICATION_ERROR,
        fields: [required("model", "The model whose stored binding cannot be read.")],
        message: "The stored setup of the model {model} cannot be read by this version of cookie.",
        hint: Some("Start a new run, which resolves the model again."),
        description: "An operation needed a run's stored model binding, which this build cannot decode.",
    }
    AGENT_UNAVAILABLE {
        code: "agent_unavailable",
        rpc: APPLICATION_ERROR,
        fields: [optional("agent", "The agent.")],
        message: "The selected agent cannot run here.",
        hint: Some("Choose another agent or model; the details say why."),
        description: "An agent that is disabled, not eligible in this session, unknown, or whose model is unavailable.",
    }
    RUN_NOT_FOUND {
        code: "run_not_found",
        rpc: APPLICATION_ERROR,
        fields: [optional("run_id", "The run.")],
        message: "The run does not exist or has already finished.",
        hint: None,
        description: "A request named a run that is not active.",
    }
    INVALID_TITLE {
        code: "invalid_title",
        rpc: APPLICATION_ERROR,
        fields: [required("session_id", "The session.")],
        message: "The title is not valid.",
        hint: Some("Use a shorter title without control characters."),
        description: "A rename with an empty, too long, or unsafe title.",
    }
    TREE_USAGE_SESSION_MISSING {
        code: "tree_usage_session_missing",
        rpc: APPLICATION_ERROR,
        fields: [optional("session_id", "The missing session.")],
        message: "A session of this tree no longer exists, so its usage cannot be added up.",
        hint: None,
        description: "`session.tree_usage` met a session that is gone.",
    }
    TREE_USAGE_DELEGATION_CORRUPT {
        code: "tree_usage_delegation_corrupt",
        rpc: APPLICATION_ERROR,
        fields: [optional("invocation_id", "The damaged delegation.")],
        message: "A subagent record of this tree is damaged, so its usage cannot be added up.",
        hint: None,
        description: "`session.tree_usage` met a delegation record that does not decode.",
    }
    GOAL_REJECTED {
        code: "goal_rejected",
        rpc: APPLICATION_ERROR,
        fields: [required("reason", "Why, in the daemon's words.")],
        message: "The goal could not be changed: {reason}.",
        hint: None,
        description: "A goal operation the session's goal state does not allow.",
    }
    PRODUCER_REJECTED {
        code: "producer_rejected",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The message source's request was refused.",
        hint: Some("See the details for why."),
        description: "A producer operation was refused: messaging is not open, the session is not attached, or the operation failed.",
    }
    PLUGIN_REJECTED {
        code: "plugin_rejected",
        rpc: APPLICATION_ERROR,
        fields: [
            required("reason", "Why, in the plugin's or the daemon's words."),
            optional("plugin", "The plugin."),
        ],
        message: "A plugin refused the request: {reason}.",
        hint: None,
        description: "A plugin hook or request refused the operation.",
    }
    PLUGIN_HOOK_FAILED {
        code: "plugin_hook_failed",
        rpc: APPLICATION_ERROR,
        fields: [
            required("plugin", "The plugin."),
            required("hook", "The hook that failed, such as `user_before_input`."),
        ],
        message: "The plugin {plugin} could not check this request in its {hook} hook, and it refuses what it cannot check.",
        hint: Some("Check the plugin's status in /plugins, restart or fix it, and try again."),
        description: "A hook of a plugin that blocks on failure timed out, was not connected, or answered with an error or a malformed result.",
    }
    RUNTIME_REJECTED {
        code: "runtime_rejected",
        rpc: APPLICATION_ERROR,
        fields: [required("reason", "Why, in the daemon's words.")],
        message: "The runtime request was refused: {reason}.",
        hint: None,
        description: "A runtime could not be attached, detached, started, or reached.",
    }
    MCP_REJECTED {
        code: "mcp_rejected",
        rpc: APPLICATION_ERROR,
        fields: [required("reason", "Why, in the daemon's words.")],
        message: "The MCP server request was refused: {reason}.",
        hint: None,
        description: "An `mcp.server.*` or `mcp.auth.*` request the daemon could not carry out.",
    }
    MACHINE_NOT_FOUND {
        code: "machine_not_found",
        rpc: APPLICATION_ERROR,
        fields: [required("machine", "The machine name.")],
        message: "There is no machine named {machine}.",
        hint: Some("See `machines.list` for the machines."),
        description: "A `machines.*` request named a machine the registry does not have.",
    }
    MACHINE_NAME_TAKEN {
        code: "machine_name_taken",
        rpc: APPLICATION_ERROR,
        fields: [required("name", "The machine name.")],
        message: "The machine name {name} is taken.",
        hint: Some("Choose another name, or merge the two machines."),
        description: "`machines.rename` to the name of another machine, or of a memory folder that belongs to none.",
    }
    MACHINE_IN_USE {
        code: "machine_in_use",
        rpc: APPLICATION_ERROR,
        fields: [required("machine", "The machine name.")],
        message: "The machine {machine} still has connected runtimes or memories.",
        hint: Some("Disconnect its runtimes and move or delete its memories first, or merge it into another machine."),
        description: "`machines.forget` of a machine with connected runtimes or a nonempty memory folder.",
    }
    MACHINE_MEMORY_CONFLICT {
        code: "machine_memory_conflict",
        rpc: APPLICATION_ERROR,
        fields: [
            required("from", "The machine merged."),
            required("into", "The machine merged into."),
            required("name", "A file both memory folders have."),
        ],
        message: "Both {from} and {into} have a memory file named {name}.",
        hint: Some("Rename or delete one of them, then merge again."),
        description: "`machines.merge` of two machines whose memory folders share a file name; nothing changed.",
    }
    MACHINE_MERGE_SELF {
        code: "machine_merge_self",
        rpc: APPLICATION_ERROR,
        fields: [required("machine", "The machine name.")],
        message: "The machine {machine} cannot be merged into itself.",
        hint: None,
        description: "`machines.merge` with the same machine as `from` and `into`.",
    }
    RUNTIME_NOT_PINNED {
        code: "runtime_not_pinned",
        rpc: APPLICATION_ERROR,
        fields: [required("runtime", "The runtime ID.")],
        message: "The runtime {runtime} is not pinned to a machine.",
        hint: None,
        description: "`machines.unassign` of a runtime ID no machine pins.",
    }
    RUNTIME_NOT_REMEMBERED {
        code: "runtime_not_remembered",
        rpc: APPLICATION_ERROR,
        fields: [required("runtime", "The runtime ID.")],
        message: "The daemon remembers no machine for the runtime {runtime}.",
        hint: Some("See `remembered` in `machines.list` for the runtimes the daemon remembers."),
        description: "`machines.forget_runtime` of a runtime the registry keeps no record of.",
    }
    RUNTIME_STILL_CONNECTED {
        code: "runtime_still_connected",
        rpc: APPLICATION_ERROR,
        fields: [required("runtime", "The runtime ID.")],
        message: "The runtime {runtime} is connected; only an offline runtime can be forgotten.",
        hint: Some("Stop the runtime first."),
        description: "`machines.forget_runtime` of a runtime the daemon holds a connection to.",
    }
    MACHINE_STORE_FAILED {
        code: "machine_store_failed",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The machine registry or a machine's memory folder could not be changed.",
        hint: Some("See the details, then check that the cookie data directory is writable."),
        description: "Reading or writing `machines.json`, taking the memory lock, or moving a memory folder failed; nothing changed.",
    }
    MEMORY_URL_INVALID {
        code: "memory_url_invalid",
        rpc: APPLICATION_ERROR,
        fields: [
            required("url", "The URL as sent."),
            required("reason", "Why it is not a memory URL."),
        ],
        message: "That is not a memory URL: {reason}.",
        hint: Some("Memory URLs are user://<category>, agent://<agent>, machine://<machine>/<name>, and project://<project>/<name>, in lowercase."),
        description: "A `memory.*` request named something that is not a memory URL.",
    }
    MEMORY_NOT_FOUND {
        code: "memory_not_found",
        rpc: APPLICATION_ERROR,
        fields: [required("url", "The memory URL.")],
        message: "There is no memory at {url}.",
        hint: Some("See `memory.list` for the memories."),
        description: "`memory.read` or `memory.delete` of a memory that does not exist, or a `memory.write` with a revision for one that does not.",
    }
    MEMORY_EXISTS {
        code: "memory_exists",
        rpc: APPLICATION_ERROR,
        fields: [
            required("url", "The memory URL."),
            required("revision", "The memory's current revision."),
        ],
        message: "The memory {url} already exists.",
        hint: Some("Read it and send its revision to replace it."),
        description: "`memory.write` without a revision for a memory that exists; nothing changed.",
    }
    MEMORY_REVISION_MISMATCH {
        code: "memory_revision_mismatch",
        rpc: APPLICATION_ERROR,
        fields: [
            required("url", "The memory URL."),
            required("revision", "The memory's current revision."),
        ],
        message: "The memory {url} changed since it was read.",
        hint: Some("Read it again, then repeat the change on the current text."),
        description: "`memory.write` or `memory.delete` with a revision other than the file's current one; nothing changed.",
    }
    MEMORY_INVALID {
        code: "memory_invalid",
        rpc: APPLICATION_ERROR,
        fields: [
            required("url", "The memory URL."),
            required("reason", "What is wrong with the text."),
        ],
        message: "The text is not a valid memory: {reason}.",
        hint: Some("A memory starts with frontmatter that holds a one-line description and optional keywords, then its Markdown body."),
        description: "`memory.write` text that does not parse, breaks a rule of the file format, or is larger than any memory may be (57344 bytes); nothing changed.",
    }
    MEMORY_REFUSED {
        code: "memory_refused",
        rpc: APPLICATION_ERROR,
        fields: [
            required("url", "The memory URL."),
            required("reason", "Why, in the daemon's words."),
        ],
        message: "The memory change was refused: {reason}.",
        hint: None,
        description: "A `memory.*` change the memory rules do not allow, such as creating an `agent://` memory for an agent the agent registry does not have, or a write that would leave a memory over its `[memory]` limit.",
    }
    MEMORY_BUSY {
        code: "memory_busy",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "Another cookie process is changing memory.",
        hint: Some("Try again in a moment."),
        description: "The memory lock could not be taken in time; nothing changed.",
    }
    MEMORY_STORE_FAILED {
        code: "memory_store_failed",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The memory folder could not be read or changed.",
        hint: Some("See the details, then check that the cookie data directory is readable and writable."),
        description: "Reading or writing a file in the memory folder failed.",
    }
    PROJECT_NOT_FOUND {
        code: "project_not_found",
        rpc: APPLICATION_ERROR,
        fields: [required("project", "The project ID.")],
        message: "There is no project {project}.",
        hint: Some("See `projects.list` for the projects."),
        description: "A `project.*`, `session.create`, or `session.project.set` request named a project the daemon does not have.",
    }
    PROJECT_NAME_TAKEN {
        code: "project_name_taken",
        rpc: APPLICATION_ERROR,
        fields: [required("name", "The project name.")],
        message: "The project name {name} is taken.",
        hint: Some("Choose another name."),
        description: "`project.create` or a rename by `project.update` to the name of another project.",
    }
    PROJECT_CHANGED {
        code: "project_changed",
        rpc: APPLICATION_ERROR,
        fields: [
            required("project", "The project ID."),
            required("revision", "The project's current revision."),
        ],
        message: "The project changed since it was read.",
        hint: Some("Read the project again, then repeat the change."),
        description: "`project.update` or `project.delete` with a revision that is no longer the project's; nothing changed.",
    }
    PROJECT_INVALID {
        code: "project_invalid",
        rpc: APPLICATION_ERROR,
        fields: [
            required("field", "The member that was refused."),
            required("reason", "Why, in the daemon's words."),
        ],
        message: "The project's {field} was refused: {reason}.",
        hint: None,
        description: "A `project.create` or `project.update` value the daemon does not accept: an emoji that is not exactly one emoji, defaults that repeat a runtime, an agent the preset does not list or that cannot run as root, or an `AGENTS.md` over the size limit.",
    }
    PROJECT_SESSIONS_RUNNING {
        code: "project_sessions_running",
        rpc: APPLICATION_ERROR,
        fields: [
            required("project", "The project ID."),
            required("session_ids", "The project's root sessions with a running tree."),
        ],
        message: "Sessions of this project are running, so it cannot be deleted.",
        hint: Some("Wait for them to finish, or cancel them, then delete the project."),
        description: "`project.delete` while a session tree of the project has an active run, a compaction, or a subagent waiting for a slot.",
    }
    PROJECT_ROOT_SESSION_ONLY {
        code: "project_root_session_only",
        rpc: APPLICATION_ERROR,
        fields: [
            required("session_id", "The session."),
            required("root_session_id", "The root of its tree."),
        ],
        message: "This session is a subagent and belongs to its root session's project.",
        hint: Some("Move its root session instead."),
        description: "`session.project.set` named a delegated session instead of a root.",
    }
    PROJECT_STORE_FAILED {
        code: "project_store_failed",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The project could not be read or changed.",
        hint: Some("See the details, then check that the cookie data directory is readable and writable."),
        description: "Reading or writing a project's folder, taking the memory lock, or moving the project's memory folder failed; nothing changed.",
    }
    CONFIG_APPLY_REFUSED {
        code: "config_apply_refused",
        rpc: APPLICATION_ERROR,
        fields: [required("reason", "`conflict`, `invalid_configuration`, or `invalid_change`.")],
        message: "The configuration was not changed ({reason}).",
        hint: Some("On a conflict, reload the configuration and apply the change again; otherwise fix what the details name."),
        description: "`config.apply` changed nothing; `detail` says why.",
    }

    // --- approvals ----------------------------------------------------------
    APPROVAL_NOT_FOUND {
        code: "approval_not_found",
        rpc: APPLICATION_ERROR,
        fields: [required("approval_id", "The approval.")],
        message: "The approval request does not exist.",
        hint: None,
        description: "A response named an approval the session does not have.",
    }
    APPROVAL_NOT_PENDING {
        code: "approval_not_pending",
        rpc: APPLICATION_ERROR,
        fields: [required("approval_id", "The approval.")],
        message: "The approval request was already answered or cancelled.",
        hint: None,
        description: "A response to an approval that is no longer pending.",
    }
    APPROVAL_REVISION_CONFLICT {
        code: "approval_revision_conflict",
        rpc: APPLICATION_ERROR,
        fields: [
            required("approval_id", "The approval."),
            optional("expected_revision", "The revision the response answered."),
            optional("found_revision", "The approval's revision now."),
        ],
        message: "The approval request changed before the answer arrived.",
        hint: Some("Review the request again and answer it."),
        description: "A response to an outdated revision of an approval.",
    }
    DECISION_NOT_ALLOWED {
        code: "decision_not_allowed",
        rpc: APPLICATION_ERROR,
        fields: [required("approval_id", "The approval.")],
        message: "That answer is not offered for this approval request.",
        hint: None,
        description: "A response with a decision the approval does not offer.",
    }
    OPERATION_FINGERPRINT_MISMATCH {
        code: "operation_fingerprint_mismatch",
        rpc: APPLICATION_ERROR,
        fields: [required("approval_id", "The approval.")],
        message: "The answer is for a different operation than the one awaiting approval.",
        hint: Some("Review the request again and answer it."),
        description: "A response whose operation fingerprint does not match the approval.",
    }
    OPERATION_CHANGED {
        code: "operation_changed",
        rpc: APPLICATION_ERROR,
        fields: [optional("approval_id", "The approval.")],
        message: "The operation changed after it was approved.",
        hint: Some("Review it again."),
        description: "The approved operation no longer matches what would run.",
    }

    // --- providers ------------------------------------------------------------
    UNKNOWN_PROVIDER {
        code: "unknown_provider",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "There is no provider named {provider_id}.",
        hint: Some("Choose a provider from the list."),
        description: "`provider.connect` named a provider the catalog does not have.",
    }
    UNSUPPORTED_PROVIDER {
        code: "unsupported_provider",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} cannot be connected by this build of cookie.",
        hint: None,
        description: "The provider needs a protocol or setup this build does not support.",
    }
    CUSTOM_PROVIDER_NOT_STORE_BACKED {
        code: "custom_provider_not_store_backed",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} is defined in the configuration file, so it cannot be changed here.",
        hint: Some("Edit its entry in config.toml instead."),
        description: "A connect or disconnect of a provider that `config.toml` defines.",
    }
    REMOVED_WITHOUT_RETAINED_RECIPE_MATCH {
        code: "removed_without_retained_recipe_match",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} is no longer offered, and its saved setup cannot be reused.",
        hint: Some("Choose another provider."),
        description: "The catalog removed the provider and no retained recipe matches.",
    }
    CATALOG_REVISION_CONFLICT {
        code: "catalog_revision_conflict",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider list changed while connecting {provider_id}.",
        hint: Some("Start the connection again."),
        description: "The catalog revision the request expected is no longer current.",
    }
    MISSING_SETUP_FIELD {
        code: "missing_setup_field",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "A required setup value for {provider_id} is missing.",
        hint: Some("Fill in every required field."),
        description: "`provider.connect` without a required setup value.",
    }
    INVALID_SETUP_FIELD {
        code: "invalid_setup_field",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "A setup value for {provider_id} is not valid.",
        hint: Some("Check the values you entered."),
        description: "`provider.connect` with a setup value its recipe rejects.",
    }
    MISSING_CREDENTIAL {
        code: "missing_credential",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "A credential for {provider_id} is missing.",
        hint: Some("Enter the provider's API key or sign in."),
        description: "`provider.connect` without a required credential.",
    }
    INVALID_CREDENTIAL {
        code: "invalid_credential",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The credential for {provider_id} is not valid.",
        hint: Some("Check the key and enter it again."),
        description: "`provider.connect` with a credential of the wrong shape.",
    }
    UNSUPPORTED_AUTH_METHOD {
        code: "unsupported_auth_method",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} does not support that way of signing in.",
        hint: Some("Choose one of the provider's sign-in methods."),
        description: "`provider.connect` with an auth method the provider does not offer.",
    }
    PROVIDER_STORE_WRITE_FAILED {
        code: "provider_store_write_failed",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The connection to {provider_id} could not be saved.",
        hint: Some("Check that the cookie data directory is writable."),
        description: "Writing the provider store failed.",
    }
    LOCK_CONTENTION {
        code: "lock_contention",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "Another change to the provider connections is in progress.",
        hint: Some("Try again in a moment."),
        description: "The provider store was locked by a concurrent change.",
    }
    RUNTIME_COMPILE_FAILED {
        code: "runtime_compile_failed",
        rpc: APPLICATION_ERROR,
        fields: [optional("provider_id", "The provider.")],
        message: "The model list could not be rebuilt after the change.",
        hint: Some("See the details, then try again."),
        description: "Compiling the runtime model catalog after a provider change failed.",
    }
    RUNTIME_REVISION_CONFLICT {
        code: "runtime_revision_conflict",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The model list changed while disconnecting {provider_id}.",
        hint: Some("Reload and try again."),
        description: "The runtime revision the disconnect expected is no longer current.",
    }
    PROVIDER_STATE_REVISION_CONFLICT {
        code: "provider_state_revision_conflict",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} changed while it was being disconnected.",
        hint: Some("Reload and try again."),
        description: "The provider state revision the disconnect expected is no longer current.",
    }
    STALE_PROVIDER_CONNECTION_GENERATION {
        code: "stale_provider_connection_generation",
        rpc: APPLICATION_ERROR,
        fields: [required("provider_id", "The provider.")],
        message: "The provider {provider_id} was reconnected since it was read.",
        hint: Some("Reload and try again."),
        description: "The disconnect named an older connection of the provider.",
    }

    // --- media ------------------------------------------------------------------
    MEDIA_TOO_LARGE {
        code: "media_too_large",
        rpc: APPLICATION_ERROR,
        fields: [
            required("kind", "The media kind."),
            required("byte_length", "The file's size."),
            required("max_bytes", "The largest accepted size."),
            optional("width", "The image's width."),
            optional("height", "The image's height."),
            optional("max_dimension", "The longest accepted side."),
            optional("max_pixels", "The most accepted pixels."),
        ],
        message: "The {kind} is too large to attach.",
        hint: Some("Attach a smaller file, or downscale it first."),
        description: "An upload or attachment above the model's or the store's limits.",
    }
    MEDIA_TYPE_UNSUPPORTED {
        code: "media_type_unsupported",
        rpc: APPLICATION_ERROR,
        fields: [required("media_type", "The sniffed media type.")],
        message: "Files of type {media_type} cannot be attached.",
        hint: Some("Attach an image, PDF, audio, or video file the model accepts."),
        description: "An upload whose content is not an accepted media type.",
    }
    MEDIA_UNSUPPORTED {
        code: "media_unsupported",
        rpc: APPLICATION_ERROR,
        fields: [
            required("kind", "The media kind."),
            required("model", "The model of the run."),
        ],
        message: "The model {model} does not accept {kind} input.",
        hint: Some("Choose a model that accepts it, or remove the attachment."),
        description: "An input with a file part the run's model does not accept.",
    }
    MEDIA_LIMIT_EXCEEDED {
        code: "media_limit_exceeded",
        rpc: APPLICATION_ERROR,
        fields: [
            required("kind", "The media kind."),
            required("count", "The parts of that kind in the input."),
            required("max_count", "The most the model accepts."),
        ],
        message: "The input has {count} {kind} attachments, more than the {max_count} the model accepts.",
        hint: Some("Remove some attachments."),
        description: "An input with more file parts of one kind than the model accepts.",
    }
    DIGEST_MISMATCH {
        code: "digest_mismatch",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The uploaded file did not arrive intact.",
        hint: Some("Attach it again."),
        description: "`artifact.upload.finish` with a digest that does not match the bytes.",
    }
    UPLOAD_NOT_FOUND {
        code: "upload_not_found",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The upload does not exist or has expired.",
        hint: Some("Attach the file again."),
        description: "An upload request named an unknown or dropped upload.",
    }
    ARTIFACT_NOT_FOUND {
        code: "artifact_not_found",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The file is not in this session.",
        hint: None,
        description: "A request named an artifact the session tree's store does not have.",
    }
    UPLOAD_OFFSET_MISMATCH {
        code: "upload_offset_mismatch",
        rpc: APPLICATION_ERROR,
        fields: [required("expected_offset", "The bytes received so far: the next chunk's offset.")],
        message: "The upload's chunks arrived out of order.",
        hint: Some("Send the chunk at offset {expected_offset} next."),
        description: "`artifact.upload.chunk` at an offset other than the bytes received so far.",
    }
    UPLOAD_SIZE_MISMATCH {
        code: "upload_size_mismatch",
        rpc: APPLICATION_ERROR,
        fields: [
            required("byte_length", "The size the upload began with."),
            required("received", "The bytes it would hold."),
        ],
        message: "The upload does not match the size it began with.",
        hint: Some("Send exactly {byte_length} bytes before finishing it."),
        description: "A chunk past the upload's size, or a finish before all of it arrived.",
    }
    TOO_MANY_UPLOADS {
        code: "too_many_uploads",
        rpc: APPLICATION_ERROR,
        fields: [required("max_uploads", "The most uploads one connection may have in progress.")],
        message: "Too many uploads are in progress on this connection.",
        hint: Some("Finish or cancel an upload first."),
        description: "`artifact.upload.begin` on a connection already at its upload limit.",
    }

    // --- runtime and plugin protocols ------------------------------------------
    ALREADY_INITIALIZED {
        code: "already_initialized",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The connection was already initialized.",
        hint: None,
        description: "A second `runtime/initialize` or `plugin/initialize` on one connection.",
    }
    INITIALIZE_REJECTED {
        code: "initialize_rejected",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The runtime refused its settings.",
        hint: Some("See the details, then fix the runtime's configuration."),
        description: "A runtime rejected `runtime/initialize`, such as settings it does not support.",
    }
    CAPABILITY_OPEN_FAILED {
        code: "capability_open_failed",
        rpc: APPLICATION_ERROR,
        fields: [
            optional("capability", "The capability the daemon asked for."),
            optional("workspace", "The workspace it asked for."),
        ],
        message: "The runtime could not open the capability for this workspace.",
        hint: Some("See the details, then check the attachment's workspace."),
        description: "`capability/open` for a capability the runtime does not offer or a workspace it cannot open.",
    }
    NOT_INITIALIZED {
        code: "not_initialized",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The connection has not been initialized yet.",
        hint: None,
        description: "A request before `runtime/initialize` or `plugin/initialize`.",
    }
    UNKNOWN_HANDLE {
        code: "unknown_handle",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The tool call or workspace the request names is no longer open.",
        hint: None,
        description: "A request named a prepared call, job, or instance the runtime does not have.",
    }
    CONTEXT_FAILED {
        code: "context_failed",
        rpc: APPLICATION_ERROR,
        fields: [optional("path", "The file that could not be read.")],
        message: "The workspace's instructions could not be read.",
        hint: Some("Check that the AGENTS.md files are readable."),
        description: "Reading a runtime's `AGENTS.md` context failed.",
    }
    REPLY_TOO_LARGE {
        code: "reply_too_large",
        rpc: APPLICATION_ERROR,
        fields: [],
        message: "The answer is too large to send.",
        hint: None,
        description: "A reply would exceed the frame bound.",
    }
}

/// The registry entry of `code`.
#[must_use]
pub fn lookup(code: &str) -> Option<&'static ErrorCode> {
    REGISTRY.iter().copied().find(|entry| entry.code == code)
}

/// Builds one error object from its registry entry.
#[derive(Clone, Debug)]
#[must_use]
pub struct ErrorBuilder {
    code: &'static ErrorCode,
    fields: Map<String, Value>,
    detail: Option<String>,
    problem: Option<ErrorBuildError>,
}

impl ErrorBuilder {
    /// Sets a declared field.
    pub fn field(mut self, name: &str, value: impl Serialize) -> Self {
        if self.code.field(name).is_none() {
            self.problem
                .get_or_insert(ErrorBuildError::UndeclaredField {
                    code: self.code.code,
                    field: name.to_owned(),
                });
            return self;
        }
        match serde_json::to_value(value) {
            Ok(Value::Null) => {
                self.fields.remove(name);
            }
            Ok(value) => {
                self.fields.insert(name.to_owned(), value);
            }
            Err(_) => {
                self.problem.get_or_insert(ErrorBuildError::Unserializable {
                    code: self.code.code,
                    field: name.to_owned(),
                });
            }
        }
        self
    }

    /// Sets the technical detail shown on request.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The error object, or why it cannot be built: an undeclared or
    /// missing field.
    pub fn try_build(self) -> Result<JsonRpcError, ErrorBuildError> {
        if let Some(problem) = self.problem {
            return Err(problem);
        }
        if let Some(missing) = self
            .code
            .fields
            .iter()
            .find(|field| field.required && !self.fields.contains_key(field.name))
        {
            return Err(ErrorBuildError::MissingField {
                code: self.code.code,
                field: missing.name,
            });
        }
        let message = bounded(
            &fill(self.code.message, &self.fields),
            MAX_MESSAGE_BYTES,
            false,
        );
        let mut data = Map::new();
        data.insert("code".into(), Value::String(self.code.code.into()));
        if let Some(hint) = self.code.hint {
            data.insert(
                "hint".into(),
                Value::String(bounded(&fill(hint, &self.fields), MAX_HINT_BYTES, false)),
            );
        }
        if let Some(detail) = self.detail.filter(|detail| !detail.is_empty()) {
            data.insert(
                "detail".into(),
                Value::String(bounded(&detail, MAX_DETAIL_BYTES, true)),
            );
        }
        data.extend(self.fields);
        Ok(JsonRpcError {
            code: self.code.rpc_code,
            message,
            data: Some(Value::Object(data)),
        })
    }

    /// The error object. A builder misused by its caller (a bug) yields an
    /// `internal_error` naming the problem in its detail instead.
    pub fn build(self) -> JsonRpcError {
        self.try_build().unwrap_or_else(|problem| {
            debug_assert!(false, "{problem}");
            INTERNAL
                .error()
                .detail(problem.to_string())
                .try_build()
                .expect("internal_error declares no fields")
        })
    }
}

/// A caller misused [`ErrorBuilder`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ErrorBuildError {
    #[error("error code {code} declares no field {field}")]
    UndeclaredField { code: &'static str, field: String },
    #[error("error code {code} requires field {field}")]
    MissingField {
        code: &'static str,
        field: &'static str,
    },
    #[error("field {field} of error code {code} does not serialize")]
    Unserializable { code: &'static str, field: String },
}

/// The `{name}` placeholders of a template.
pub fn placeholders(template: &str) -> impl Iterator<Item = &str> {
    template
        .split('{')
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
}

fn fill(template: &str, fields: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match fields.get(name) {
            Some(Value::String(text)) => out.push_str(text),
            Some(value) => out.push_str(&value.to_string()),
            None => out.push_str(&rest[start..=start + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `text` made terminal-safe and cut to `limit` bytes at a character
/// boundary, ending in `…` when cut. Newlines and tabs stay in a detail and
/// become spaces elsewhere.
fn bounded(text: &str, limit: usize, multiline: bool) -> String {
    let mut safe = crate::sanitize_display_text(text).into_owned();
    if !multiline {
        safe = safe.replace(['\n', '\t'], " ");
    }
    if safe.len() <= limit {
        return safe;
    }
    let ellipsis = '…';
    let mut end = limit - ellipsis.len_utf8();
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    safe.truncate(end);
    safe.push(ellipsis);
    safe
}

/// What a reader takes from an error object: its stable `data.code`, the
/// hint and detail, and the fields its code defines. Data is read per code:
/// a code this build knows keeps only the fields the registry declares for
/// it; a code it does not know reads as a generic error with every member
/// kept, since its `message` and `hint` are already readable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ErrorData {
    /// `data.code`; `None` for an error without registry data.
    pub code: Option<String>,
    pub hint: Option<String>,
    pub detail: Option<String>,
    pub fields: Map<String, Value>,
}

impl ErrorData {
    /// Reads `error.data`.
    #[must_use]
    pub fn of(error: &JsonRpcError) -> Self {
        let Some(Value::Object(data)) = &error.data else {
            return Self::default();
        };
        let text = |name: &str| data.get(name).and_then(Value::as_str).map(str::to_owned);
        let code = text("code");
        let entry = code.as_deref().and_then(lookup);
        let fields = data
            .iter()
            .filter(|(name, _)| !RESERVED_DATA_MEMBERS.contains(&name.as_str()))
            .filter(|(name, _)| entry.is_none_or(|entry| entry.field(name).is_some()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Self {
            code,
            hint: text("hint"),
            detail: text("detail"),
            fields,
        }
    }

    /// The registry entry of the code, when this build knows it.
    #[must_use]
    pub fn entry(&self) -> Option<&'static ErrorCode> {
        self.code.as_deref().and_then(lookup)
    }

    /// Whether the error is of `code`.
    #[must_use]
    pub fn is(&self, code: &ErrorCode) -> bool {
        self.code.as_deref() == Some(code.code)
    }

    /// A string field of the code.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    /// An integer field of the code.
    #[must_use]
    pub fn integer(&self, name: &str) -> Option<u64> {
        self.fields.get(name).and_then(Value::as_u64)
    }
}

/// The error reference page, `docs/site/reference/errors.md`: every
/// registered code with its JSON-RPC code, description, message, hint, and
/// fields. A test keeps the committed page equal to it.
#[must_use]
pub fn reference_markdown() -> String {
    let cell = |text: &str| text.replace('|', "\\|").replace('\n', " ");
    let mut out = String::from(
        "<!-- Generated from the registry in crates/protocol/src/errors.rs. Do not edit by hand; regenerate with\n\
         COOKIE_UPDATE_ERROR_REFERENCE=1 cargo test -p cookie_agent_protocol --lib errors -->\n\n\
         # Errors\n\n\
         Every error the client, plugin, and runtime protocols send has a stable\n\
         `data.code` and a `message` a user can understand; `data.hint` says what to\n\
         do next where that helps, and `data.detail` carries technical detail for a\n\
         details view. Clients branch only on `data.code`, show `message` and then\n\
         `hint`, and show `detail` on request. A code a client does not know is\n\
         still readable: its `message` and `hint` are written for people. See the\n\
         [compatibility spec](../specs/compatibility-v1.md) section 6.\n\n\
         The JSON-RPC `code` is `-32000` for every application error; the protocol\n\
         codes are the standard `-32700`, `-32600`, `-32601`, `-32602`, and\n\
         `-32603`, and `-32001` (handshake required), `-32002`\n\
         (`unsupported_protocol`), and `-32003` (unauthenticated).\n\n\
         | `data.code` | JSON-RPC | Meaning | Message | Hint | Fields |\n\
         |---|---|---|---|---|---|\n",
    );
    for entry in REGISTRY {
        let fields = entry
            .fields
            .iter()
            .map(|field| {
                format!(
                    "`{}`{}: {}",
                    field.name,
                    if field.required { "" } else { " (optional)" },
                    cell(field.description)
                )
            })
            .collect::<Vec<_>>()
            .join("<br>");
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} | {} |\n",
            entry.code,
            entry.rpc_code,
            cell(entry.description),
            cell(entry.message),
            entry.hint.map_or_else(|| "–".to_owned(), cell),
            if fields.is_empty() {
                "–".to_owned()
            } else {
                fields
            },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn the_error_reference_is_generated_from_the_registry() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/site/reference/errors.md"
        );
        let generated = reference_markdown();
        if std::env::var_os("COOKIE_UPDATE_ERROR_REFERENCE").is_some() {
            std::fs::write(path, &generated).expect("write the error reference");
            return;
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap_or_default(),
            generated,
            "docs/site/reference/errors.md is stale; regenerate it with \
             COOKIE_UPDATE_ERROR_REFERENCE=1 cargo test -p cookie_agent_protocol --lib errors"
        );
        for entry in REGISTRY {
            assert!(generated.contains(&format!("| `{}` |", entry.code)));
        }
    }

    #[test]
    fn error_data_is_read_per_code() {
        let known = MEDIA_UNSUPPORTED
            .error()
            .field("kind", "image")
            .field("model", "m")
            .detail("why")
            .build();
        let mut sent = known.clone();
        sent.data.as_mut().unwrap()["added_later"] = json!(1);
        let data = ErrorData::of(&sent);
        assert!(data.is(&MEDIA_UNSUPPORTED));
        assert_eq!(data.entry(), Some(&MEDIA_UNSUPPORTED));
        assert_eq!(data.text("kind"), Some("image"));
        assert_eq!(data.detail.as_deref(), Some("why"));
        assert!(data.hint.is_some());
        // A known code keeps only the fields it declares.
        assert!(!data.fields.contains_key("added_later"));

        let unknown = JsonRpcError {
            code: APPLICATION_ERROR,
            message: "Something new went wrong.".into(),
            data: Some(json!({"code": "from_the_future", "hint": "Try later.", "count": 3})),
        };
        let data = ErrorData::of(&unknown);
        assert_eq!(data.code.as_deref(), Some("from_the_future"));
        assert_eq!(data.entry(), None);
        assert_eq!(data.hint.as_deref(), Some("Try later."));
        assert_eq!(data.integer("count"), Some(3));
        assert_eq!(
            ErrorData::of(&JsonRpcError {
                code: INTERNAL_ERROR,
                message: "x".into(),
                data: None,
            }),
            ErrorData::default()
        );
    }

    #[test]
    fn every_code_is_unique_snake_case_and_documented() {
        let mut codes = BTreeSet::new();
        for entry in REGISTRY {
            assert!(codes.insert(entry.code), "duplicate code {}", entry.code);
            assert!(
                entry
                    .code
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| byte.is_ascii_lowercase()
                        || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))),
                "{}",
                entry.code
            );
            assert!(!entry.message.trim().is_empty(), "{}", entry.code);
            assert!(!entry.description.trim().is_empty(), "{}", entry.code);
            assert!(entry.message.len() <= MAX_MESSAGE_BYTES, "{}", entry.code);
            assert!(entry.message.ends_with('.'), "{}", entry.code);
            if let Some(hint) = entry.hint {
                assert!(
                    !hint.trim().is_empty() && hint.len() <= MAX_HINT_BYTES,
                    "{}",
                    entry.code
                );
                assert!(hint.ends_with('.'), "{}", entry.code);
            }
            assert!(
                [
                    PARSE_ERROR,
                    INVALID_REQUEST,
                    METHOD_NOT_FOUND,
                    INVALID_PARAMS,
                    INTERNAL_ERROR,
                    APPLICATION_ERROR,
                    HANDSHAKE_REQUIRED,
                    UNSUPPORTED_PROTOCOL,
                    UNAUTHENTICATED
                ]
                .contains(&entry.rpc_code),
                "{}",
                entry.code
            );
            assert_eq!(lookup(entry.code), Some(*entry));
        }
        assert_eq!(lookup("no_such_code"), None);
    }

    #[test]
    fn fields_are_unique_and_templates_name_only_required_fields() {
        for entry in REGISTRY {
            let mut names = BTreeSet::new();
            for field in entry.fields {
                assert!(names.insert(field.name), "{}: {}", entry.code, field.name);
                assert!(
                    !RESERVED_DATA_MEMBERS.contains(&field.name),
                    "{}: {}",
                    entry.code,
                    field.name
                );
                assert!(
                    !field.description.is_empty(),
                    "{}: {}",
                    entry.code,
                    field.name
                );
            }
            for template in std::iter::once(entry.message).chain(entry.hint) {
                for name in placeholders(template) {
                    let field = entry
                        .field(name)
                        .unwrap_or_else(|| panic!("{}: undeclared {{{name}}}", entry.code));
                    assert!(field.required, "{}: optional {{{name}}}", entry.code);
                }
            }
        }
    }

    #[test]
    fn the_spec_names_these_codes() {
        for code in [
            "no_runnable_model",
            "session_owned_elsewhere",
            "not_a_root_session",
            "session_tree_running",
            "session_changed",
            "tree_usage_session_missing",
            "tree_usage_delegation_corrupt",
            "config_apply_refused",
            "already_initialized",
            "not_initialized",
            "unsupported_protocol",
            "unknown_field",
            "unknown_variant",
            "session_requires_newer_engine",
            "media_too_large",
            "media_type_unsupported",
            "media_unsupported",
            "media_limit_exceeded",
            "digest_mismatch",
            "upload_not_found",
            "artifact_not_found",
        ] {
            assert!(lookup(code).is_some(), "{code}");
        }
    }

    #[test]
    fn approval_refusals_are_registered_codes() {
        use crate::ApprovalRespondErrorCode as Code;
        for code in [
            Code::ApprovalNotFound,
            Code::ApprovalNotPending,
            Code::ApprovalRevisionConflict,
            Code::DecisionNotAllowed,
            Code::OperationFingerprintMismatch,
            Code::OperationChanged,
            Code::IdempotencyConflict,
        ] {
            let entry = code.error_code();
            assert_eq!(lookup(entry.code), Some(entry));
            assert_eq!(Code::from_error_code(entry.code), Some(code));
        }
        assert_eq!(Code::from_error_code("session_not_found"), None);
    }

    #[test]
    fn an_error_has_the_registered_shape() {
        let error = MEDIA_TOO_LARGE
            .error()
            .field("kind", "image")
            .field("byte_length", 30_000_000_u64)
            .field("max_bytes", 3_932_160_u64)
            .field("width", Option::<u32>::None)
            .detail("upload of 30000000 bytes")
            .try_build()
            .unwrap();
        assert_eq!(error.code, APPLICATION_ERROR);
        assert_eq!(error.message, "The image is too large to attach.");
        assert_eq!(
            error.data,
            Some(json!({
                "code": "media_too_large",
                "hint": "Attach a smaller file, or downscale it first.",
                "detail": "upload of 30000000 bytes",
                "kind": "image",
                "byte_length": 30_000_000_u64,
                "max_bytes": 3_932_160_u64
            }))
        );
        let numbered = MEDIA_LIMIT_EXCEEDED
            .error()
            .field("kind", "image")
            .field("count", 21)
            .field("max_count", 20)
            .build();
        assert_eq!(
            numbered.message,
            "The input has 21 image attachments, more than the 20 the model accepts."
        );
        let protocol = PROTOCOL_UNSUPPORTED
            .error()
            .field("supported_version", crate::SemVer::new(1, 0, 0))
            .field("peer_version", "2.3.0")
            .build();
        assert_eq!(protocol.code, UNSUPPORTED_PROTOCOL);
        assert!(
            protocol.message.contains("1.0.0 here and 2.3.0"),
            "{}",
            protocol.message
        );
    }

    #[test]
    fn misuse_is_reported() {
        assert_eq!(
            MEDIA_UNSUPPORTED.error().field("kind", "image").try_build(),
            Err(ErrorBuildError::MissingField {
                code: "media_unsupported",
                field: "model"
            })
        );
        assert_eq!(
            DIGEST_MISMATCH.error().field("sha256", "x").try_build(),
            Err(ErrorBuildError::UndeclaredField {
                code: "digest_mismatch",
                field: "sha256".into()
            })
        );
    }

    #[test]
    fn text_is_terminal_safe_and_bounded() {
        let error = UNKNOWN_PROVIDER
            .error()
            .field("provider_id", "evil\u{1b}[2J\nname")
            .detail(format!(
                "line one\nline\ttwo\u{7}{}",
                "é".repeat(MAX_DETAIL_BYTES)
            ))
            .build();
        assert_eq!(error.message, "There is no provider named evil name.");
        let detail = error.data.as_ref().unwrap()["detail"].as_str().unwrap();
        assert!(detail.starts_with("line one\nline\ttwo"), "{detail:?}");
        assert!(detail.len() <= MAX_DETAIL_BYTES && detail.ends_with('…'));
        let long = UNKNOWN_PROVIDER
            .error()
            .field("provider_id", "x".repeat(2 * MAX_MESSAGE_BYTES))
            .build();
        assert!(long.message.len() <= MAX_MESSAGE_BYTES && long.message.ends_with('…'));
        // A structured field keeps its value; only the texts are cleaned.
        assert_eq!(
            long.data.unwrap()["provider_id"].as_str().map(str::len),
            Some(2 * MAX_MESSAGE_BYTES)
        );
    }

    #[test]
    fn templates_fill_only_their_placeholders() {
        let mut fields = Map::new();
        fields.insert("a".into(), json!("x"));
        fields.insert("n".into(), json!(3));
        assert_eq!(
            fill("{a} and {n} but {b} and {", &fields),
            "x and 3 but {b} and {"
        );
        assert_eq!(placeholders("{a} {b}c {").collect::<Vec<_>>(), ["a", "b"]);
    }
}
