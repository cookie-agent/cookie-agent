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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
}

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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
