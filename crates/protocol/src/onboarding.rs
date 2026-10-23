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
    SmallModel,
    #[serde(other)]
    Unknown,
}

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
}

/// `onboarding.get`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingGetParams {}

/// Where the starter kit comes from.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OnboardingKitSource {
    /// The release of the daemon's version, else the latest release.
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
