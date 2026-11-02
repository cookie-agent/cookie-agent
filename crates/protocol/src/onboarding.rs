//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 8, client protocol 1.22.0): whether the daemon is set up, what installing
//! the starter kit would do, and the one call that sets it up. Every client
//! onboards with these calls and `provider.connect`; the daemon does the
//! work and never branches on the kind of client asking.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ModelKey, ProviderId};
use cookie_agent_identity::ConfiguredVariantRef;

pub const ONBOARDING_GET_METHOD: &str = "onboarding.get";
pub const ONBOARDING_PREVIEW_METHOD: &str = "onboarding.preview";
pub const ONBOARDING_RUN_METHOD: &str = "onboarding.run";
pub const ONBOARDING_CHANGED_METHOD: &str = "onboarding.changed";

/// The largest kit archive `onboarding.preview` and `onboarding.run` take,
/// before base64.
pub const MAX_ONBOARDING_ARCHIVE_BYTES: usize = 1024 * 1024;

/// What onboarding still needs. A value this build does not know reads as
/// `unknown`: something the daemon needs that a client cannot name, which
/// keeps Finish disabled.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum OnboardingRequirement {
    /// No model provider is connected.
    Provider,
    /// `default_model` is unset or names no model a connected provider
    /// offers.
    DefaultModel,
    /// `small_model` is unset or names no model a connected provider offers.
    /// Not sent since 1.56.0: `small_model` is optional.
    SmallModel,
    /// `[agents] default` names no agent that can run as a root, such as
    /// before the first refresh of the official agents (client protocol
    /// 1.56.0).
    DefaultAgent,
    #[serde(other)]
    Unknown,
}

/// One of the model fields, as an entry of another field names it (client
/// protocol 1.69.0). A value this build does not know reads as `unknown`;
/// `onboarding.run` refuses it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum OnboardingModelField {
    DefaultModel,
    SmallModel,
    UtilityModel,
    #[serde(other)]
    Unknown,
}

/// One entry of a model field's fallback list (client protocol 1.69.0):
/// exactly one of `model`, a model key, and `field`, another model field
/// whose models the entry stands for. `variant` absent means the model's
/// default variant; beside `field`, it applies to each of that field's
/// models that has no variant of its own and has a variant of that name
/// (`docs/site/specs/model-fallback-lists.md`).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingModelEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<OnboardingModelField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ConfiguredVariantRef>,
}

/// The most entries a model field's list holds.
pub const MAX_ONBOARDING_MODEL_ENTRIES: usize = 32;

/// A model and its variant as the configuration holds them: `variant`
/// absent means the model's default variant, `base` its base variant.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingModel {
    pub model: ModelKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ConfiguredVariantRef>,
}

/// Whether the daemon is set up: the result of `onboarding.get` and the
/// payload of `onboarding.changed`. Computed from the live configuration and
/// runtime; nothing is stored for it.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingStatus {
    /// `missing` is empty.
    pub ready: bool,
    #[schemars(length(max = 16))]
    pub missing: Vec<OnboardingRequirement>,
    /// The providers that offer at least one available model, sorted.
    #[schemars(length(max = 4096))]
    pub providers_connected: Vec<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<OnboardingModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<OnboardingModel>,
    /// `utility_model` (client protocol 1.56.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utility_model: Option<OnboardingModel>,
    /// `default_model` as configured, a list of one for a single model, its
    /// references to other fields kept; absent when unset (client protocol
    /// 1.69.0). `default_model` above is the first model it reaches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub default_model_list: Option<Vec<OnboardingModelEntry>>,
    /// `small_model` as configured, like `default_model_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub small_model_list: Option<Vec<OnboardingModelEntry>>,
    /// `utility_model` as configured, like `default_model_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub utility_model_list: Option<Vec<OnboardingModelEntry>>,
    /// The two ways to set the daemon up, each with whether it is done
    /// (client protocol 1.74.0, `docs/site/specs/acp-onboarding.md`
    /// section 2). While the ACP path is complete, `missing` no longer lists
    /// what only the models path needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<OnboardingPaths>,
    /// The ACP agents onboarding can link (client protocol 1.74.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub acp_presets: Vec<AcpPreset>,
    /// The MCP servers the Tools step offers (client protocol 1.74.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub mcp_presets: Vec<McpPreset>,
    /// The MCP servers configured now, by name and place (client protocol
    /// 1.74.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4096))]
    pub mcp_servers: Vec<OnboardingMcpServer>,
    /// `[agents] default` as configured; absent when unset, and new
    /// sessions then start on `primary`, else the first agent that can run
    /// as a root (client protocol 1.74.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<crate::AgentId>,
}

/// The two ways to set a daemon up (client protocol 1.74.0).
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingPaths {
    pub models: OnboardingModelsPath,
    pub acp: OnboardingAcpPath,
}

/// The models path: a connected provider and `default_model`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingModelsPath {
    pub complete: bool,
    /// What it still needs: `provider`, `default_model`.
    #[schemars(length(max = 16))]
    pub missing: Vec<OnboardingRequirement>,
}

/// The ACP path: an enabled ACP agent a runtime that can run it offers.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingAcpPath {
    pub complete: bool,
    /// Every ACP agent declaration, in ID order.
    #[schemars(length(max = 4096))]
    pub agents: Vec<OnboardingAcpAgent>,
}

/// An ACP agent declaration as onboarding shows it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingAcpAgent {
    pub id: crate::AgentId,
    /// The runtime whose table declares it; absent for `[acp.agents]`,
    /// which every runtime but `global` offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::RuntimeId>,
    #[schemars(length(max = 64))]
    pub command: Vec<String>,
    /// The names of its environment variables; their values are not sent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub env_keys: Vec<String>,
    #[schemars(length(max = 512))]
    pub description: String,
    pub enabled: bool,
    /// The ACP preset whose command and environment it has, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<crate::AgentId>,
    /// The runtimes that offer it, and whether each can run it now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub runtimes: Vec<OnboardingAcpRuntime>,
}

/// A runtime that offers an ACP agent onboarding shows.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingAcpRuntime {
    pub runtime_id: crate::RuntimeId,
    /// Connected, or started on demand.
    pub available: bool,
}

/// A configured MCP server, by name and place.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingMcpServer {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// The runtime whose table holds it; absent for `[mcp.servers]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::RuntimeId>,
}

/// An ACP agent `onboarding.run` declares (client protocol 1.74.0).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingAcpAgentInput {
    pub id: crate::AgentId,
    /// The runtime whose table declares it; `[acp.agents]`, every runtime,
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::RuntimeId>,
    #[schemars(length(min = 1, max = 64))]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 512))]
    pub description: Option<String>,
    /// A value of the agent's model option the agent starts its sessions
    /// with, written as the `models` of an `agents/<id>.md` that inherits
    /// the `_acp` template; skipped when that document exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub model: Option<String>,
    /// A value of the agent's effort option, beside `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub effort: Option<String>,
}

/// An MCP server `onboarding.run` adds (client protocol 1.74.0).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingMcpServerInput {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// The runtime whose table holds it; `[mcp.servers]` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::RuntimeId>,
    /// The server's table, as `config.toml` takes it.
    pub server: serde_json::Map<String, serde_json::Value>,
}

/// `acp.check` (client protocol 1.74.0): starts a declared ACP agent on a
/// runtime, opens a session in the runtime's home, records what it offers,
/// and closes it (`docs/site/specs/acp-onboarding.md` section 5).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcpCheckParams {
    pub runtime_id: crate::RuntimeId,
    /// The declaration's key on the runtime.
    pub agent: crate::AgentId,
}

/// How far an `acp.check` got. A value this build does not know reads as
/// `unknown`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum AcpCheckStage {
    /// The runtime does not declare the agent, or is away.
    Declared,
    /// Starting the agent and its `initialize`.
    Start,
    /// `session/new`, where an agent that is not logged in fails.
    Session,
    /// Everything worked.
    Ready,
    #[serde(other)]
    Unknown,
}

/// What an `acp.check` found.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpCheckResult {
    pub ok: bool,
    /// The stage it reached: `ready` when `ok`, else the one that failed.
    pub stage: AcpCheckStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub error: Option<String>,
    /// The agent's name and version, as its `initialize` gave them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub agent_version: Option<String>,
    /// The options a new session of it starts with, such as its models.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub options: Vec<crate::AcpConfigOption>,
}

pub const ACP_CHECK_METHOD: &str = "acp.check";

/// `onboarding.get`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingGetParams {}

/// Where the starter kit comes from.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OnboardingKitSource {
    /// The release of the daemon's version, else the newest release of that
    /// version in the release list (a nightly's), else the latest release.
    #[default]
    Release,
    /// A downloaded `cookie-agent-defaults.tar.gz`, sent whole.
    Archive {
        /// The archive's bytes in standard base64, at most
        /// [`MAX_ONBOARDING_ARCHIVE_BYTES`] decoded.
        #[schemars(length(min = 1, max = 1_398_104))]
        data: String,
    },
}

/// `onboarding.preview`: downloads and checks the kit, writes nothing.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingPreviewParams {
    /// `release` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<OnboardingKitSource>,
}

/// What installing a kit file would do. A value this build does not know
/// reads as `unknown`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum OnboardingFileAction {
    /// The file does not exist: it is written.
    Write,
    /// The file is identical to the kit's: nothing is written.
    Skip,
    /// The file differs: it stays, and the kit's version is written beside
    /// it as `<path>.new`.
    WriteNew,
    #[serde(other)]
    Unknown,
}

/// One kit file against the daemon's user root.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingKitFile {
    /// Below the user root: `config.toml` or `agents/<id>.md`.
    pub path: String,
    pub action: OnboardingFileAction,
    /// For `write_new`, a unified diff from the file to the kit's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingPreviewResult {
    pub kit_version: u32,
    /// The cookie version the kit was packed for, when its manifest says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie_version: Option<String>,
    /// Where the kit came from: its URL, or `archive`.
    pub origin: String,
    #[schemars(length(max = 256))]
    pub files: Vec<OnboardingKitFile>,
}

/// A model value as `config.toml` takes it: a model key, or
/// `{ model, variant }`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OnboardingModelInput {
    Key(ModelKey),
    Table(OnboardingModelTable),
}

/// The table form of [`OnboardingModelInput`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingModelTable {
    pub model: ModelKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ConfiguredVariantRef>,
}

/// `onboarding.run`: sets the model fields given and installs the kit when
/// asked, as one change. Refused with `onboarding_incomplete`, writing
/// nothing, when the daemon would not be set up afterwards.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingRunParams {
    pub install_kit: bool,
    /// `release` when absent; ignored without `install_kit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<OnboardingKitSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<OnboardingModelInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<OnboardingModelInput>,
    /// `utility_model` (client protocol 1.56.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utility_model: Option<OnboardingModelInput>,
    /// `default_model` as a fallback list, in order (client protocol
    /// 1.69.0); refused beside `default_model`. A list of one is written as
    /// the entry alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub default_model_list: Option<Vec<OnboardingModelEntry>>,
    /// `small_model` as a fallback list, like `default_model_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub small_model_list: Option<Vec<OnboardingModelEntry>>,
    /// `utility_model` as a fallback list, like `default_model_list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub utility_model_list: Option<Vec<OnboardingModelEntry>>,
    /// ACP agents to declare (client protocol 1.74.0); a declaration of the
    /// same ID at the same place is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    pub acp_agents: Option<Vec<OnboardingAcpAgentInput>>,
    /// MCP servers to add (client protocol 1.74.0); a name already taken
    /// at that place is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    pub mcp_servers: Option<Vec<OnboardingMcpServerInput>>,
    /// `[agents] default` to set (client protocol 1.74.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<crate::AgentId>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OnboardingRunResult {
    /// Kit files written.
    #[schemars(length(max = 256))]
    pub written: Vec<String>,
    /// Kit files identical to the kit's.
    #[schemars(length(max = 256))]
    pub skipped: Vec<String>,
    /// Kit files that differ, whose kit version was written beside them as
    /// `<path>.new`.
    #[schemars(length(max = 256))]
    pub written_new: Vec<String>,
    /// The configuration fields set.
    #[schemars(length(max = 8))]
    pub settings: Vec<String>,
    pub status: OnboardingStatus,
}

/// An ACP agent onboarding can link (`docs/site/specs/acp-onboarding.md`
/// section 3, client protocol 1.73.0), from the official agents
/// repository: the declaration it fills in.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AcpPreset {
    /// The agent ID it suggests, such as `claude`.
    pub id: crate::AgentId,
    /// Its name, such as "Claude Code".
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// The declaration's `description`.
    #[schemars(length(max = 512))]
    pub description: String,
    /// The program and its arguments.
    #[schemars(length(min = 1, max = 64))]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// What each machine that runs it needs, as a sentence.
    #[schemars(length(max = 1024))]
    pub needs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 2048))]
    pub docs_url: Option<String>,
}

/// Where an MCP preset's server goes. A value this build does not know
/// reads as `unknown`, which onboarding does not offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum McpPresetPlace {
    /// A hosted server: `[mcp.servers.<name>]`, where the daemon runs.
    Global,
    /// A program on a machine the user picks:
    /// `[runtimes.<runtime>.mcp.servers.<name>]`.
    Machine,
    #[serde(other)]
    Unknown,
}

/// An MCP server that needs no login, which onboarding's Tools step offers
/// (`docs/site/specs/acp-onboarding.md` section 9.1, client protocol
/// 1.73.0).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpPreset {
    /// The server name it suggests, such as `exa`.
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// What kind of tool it is, such as "Web search".
    #[schemars(length(max = 128))]
    pub category: String,
    #[schemars(length(max = 512))]
    pub description: String,
    /// The server's table as `config.toml` takes it (`url`, or `command`
    /// and `args`, and the other keys of an MCP server).
    pub server: serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "where")]
    pub place: McpPresetPlace,
    #[schemars(length(max = 1024))]
    pub needs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 2048))]
    pub docs_url: Option<String>,
}
