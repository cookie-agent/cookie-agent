//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 8): whether the daemon is set up, derived from the live configuration and
//! runtime with nothing stored for it, and the one call that sets it up:
//! the model fields and, optionally, the starter kit.

pub(crate) mod kit;
#[cfg(test)]
mod tests;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use cookie_agent_config::{
    ConfigTree, ConfiguredModel, EngineConfig, load_from_tree, write_beside, write_tree,
};
use cookie_agent_identity::ProviderId;
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};

pub use kit::{Kit, KitError, KitFileAction, KitFilePlan, KitManifest};

use super::configuration::config_error;
use crate::{Engine, EngineError};

/// Where releases are downloaded from: `<base>/download/<tag>/<asset>` and
/// `<base>/latest/download/<asset>`.
pub const RELEASES_URL: &str = "https://github.com/cookie-agent/cookie-agent/releases";
/// How long a downloaded kit is kept for the run that follows its preview.
const KIT_CACHE: Duration = Duration::from_secs(10 * 60);

/// What onboarding still needs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OnboardingRequirement {
    /// No model provider is connected: none offers an available model.
    Provider,
    /// `default_model` is unset or names no available model.
    DefaultModel,
    /// `small_model` is unset or names no available model.
    SmallModel,
}

/// Whether the daemon is set up, from its configuration and runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingState {
    /// Sorted; empty when the daemon is set up.
    pub missing: Vec<OnboardingRequirement>,
    /// The providers that offer at least one available model, sorted.
    pub providers_connected: Vec<ProviderId>,
    pub default_model: Option<ConfiguredModel>,
    pub small_model: Option<ConfiguredModel>,
}

impl OnboardingState {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Where a kit comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KitSource {
    /// The release of this build's version, else the latest release.
    Release,
    /// A `.tar.gz` archive the client sent.
    Archive(Vec<u8>),
}

/// What installing a kit would do.
#[derive(Clone, Debug)]
pub struct KitPreview {
    pub manifest: KitManifest,
    /// Where the kit came from: its URL, or `archive`.
    pub origin: String,
    pub files: Vec<KitFilePlan>,
}

/// An `onboarding.run`.
#[derive(Clone, Debug)]
pub struct OnboardingRequest {
    pub install_kit: bool,
    pub source: KitSource,
    /// `default_model` as the configuration takes it: a key string or
    /// `{ model, variant }`.
    pub default_model: Option<serde_json::Value>,
    pub small_model: Option<serde_json::Value>,
}

/// What an `onboarding.run` did.
#[derive(Clone, Debug)]
pub struct OnboardingOutcome {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
    /// The kit files that differed, whose kit version went beside them as
    /// `<path>.new`.
    pub written_new: Vec<String>,
    /// The configuration fields set.
    pub settings: Vec<String>,
    pub state: OnboardingState,
}

/// The kit fetching state of the engine.
pub(crate) struct OnboardingRuntimeState {
    releases_url: Mutex<String>,
    cached: Mutex<Option<CachedKit>>,
}

impl Default for OnboardingRuntimeState {
    fn default() -> Self {
        Self {
            releases_url: Mutex::new(RELEASES_URL.to_owned()),
            cached: Mutex::new(None),
        }
    }
}

struct CachedKit {
    fetched: Instant,
    releases_url: String,
    origin: String,
    kit: Arc<Kit>,
}

/// `missing` as a sentence: "no provider is connected; `small_model` …".
pub(crate) fn describe_missing(missing: &[OnboardingRequirement]) -> String {
    missing
        .iter()
        .map(|requirement| match requirement {
            OnboardingRequirement::Provider => "no model provider is connected",
            OnboardingRequirement::DefaultModel => {
                "`default_model` does not name an available model"
            }
            OnboardingRequirement::SmallModel => "`small_model` does not name an available model",
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The onboarding state of `config` against `models`.
pub(crate) fn onboarding_state(
    config: &EngineConfig,
    models: &CompiledModelRuntime,
) -> OnboardingState {
    let mut providers_connected = models
        .models()
        .values()
        .filter(|model| model.model.status == CompiledModelStatus::Available)
        .map(|model| model.key.provider_id())
        .collect::<Vec<_>>();
    providers_connected.sort();
    providers_connected.dedup();
    let available = |configured: Option<&ConfiguredModel>| {
        configured.is_some_and(|configured| {
            crate::runtime_snapshot::selection_available(
                models,
                &crate::runtime_snapshot::configured_selection(models, configured, None),
            )
        })
    };
    let mut missing = Vec::new();
    if providers_connected.is_empty() {
        missing.push(OnboardingRequirement::Provider);
    }
    if !available(config.default_model.as_ref()) {
        missing.push(OnboardingRequirement::DefaultModel);
    }
    if !available(config.small_model.as_ref()) {
        missing.push(OnboardingRequirement::SmallModel);
    }
    OnboardingState {
        missing,
        providers_connected,
        default_model: config.default_model.clone(),
        small_model: config.small_model.clone(),
    }
}

impl Engine {
    /// Whether the daemon is set up, from the applied configuration and the
    /// current runtime.
    #[must_use]
    pub fn onboarding_state(&self) -> OnboardingState {
        let config = self.inner.config.load();
        onboarding_state(&config.runtime, &self.current_runtime().models)
    }

    /// Where releases are downloaded from instead of [`RELEASES_URL`], for
    /// tests that serve a kit locally.
    pub fn set_releases_url(&self, url: impl Into<String>) {
        *self
            .inner
            .onboarding
            .releases_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = url.into();
        *self
            .inner
            .onboarding
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// What installing the kit of `source` would do; writes nothing. A kit
    /// downloaded from a release is kept briefly for the run that follows.
    pub async fn preview_kit(&self, source: KitSource) -> Result<KitPreview, EngineError> {
        let (origin, kit) = self.kit(source).await?;
        let root = self.configuration_root()?;
        let plan_kit = Arc::clone(&kit);
        let files = super::blocking_io::run(move || {
            let tree = ConfigTree::read(Some(&root)).map_err(config_error)?;
            Ok::<_, EngineError>(plan_kit.plan(&tree))
        })
        .await
        .map_err(|error| EngineError::Runtime(error.to_string()))??;
        Ok(KitPreview {
            manifest: kit.manifest.clone(),
            origin,
            files,
        })
    }

    /// Sets the model fields `request` names and installs the kit when it
    /// asks, as one change; refuses with
    /// [`EngineError::OnboardingIncomplete`], writing nothing, when the
    /// daemon would not be set up afterwards.
    pub async fn run_onboarding(
        &self,
        request: OnboardingRequest,
    ) -> Result<OnboardingOutcome, EngineError> {
        let kit = if request.install_kit {
            Some(self.kit(request.source.clone()).await?.1)
        } else {
            None
        };
        let _mutation = self.inner.config_mutation.lock().await;
        let root = self.configuration_root()?;
        let models = Arc::clone(&self.current_runtime().models);
        let fields = [
            ("default_model", request.default_model),
            ("small_model", request.small_model),
        ];
        let (outcome, loaded) = super::blocking_io::run(move || {
            let tree = ConfigTree::read(Some(&root)).map_err(config_error)?;
            let mut candidate = tree.clone();
            let plan = kit.as_ref().map(|kit| kit.plan(&tree)).unwrap_or_default();
            if let Some(kit) = &kit {
                for (path, bytes) in &kit.files {
                    if candidate.get(path).is_none() {
                        candidate
                            .put(path.clone(), bytes.clone())
                            .map_err(config_error)?;
                    }
                }
            }
            let mut settings = Vec::new();
            for (field, value) in fields {
                if let Some(value) = value {
                    candidate
                        .set_key_json(&[field.to_owned()], &value)
                        .map_err(|error| EngineError::ConfigChangeRefused(error.to_string()))?;
                    settings.push(field.to_owned());
                }
            }
            let loaded = load_from_tree(&candidate).map_err(config_error)?;
            let state = onboarding_state(&loaded.runtime, &models);
            if !state.ready() {
                return Err(EngineError::OnboardingIncomplete(state.missing));
            }
            let changed = tree.revision() != candidate.revision();
            if changed {
                write_tree(&tree, &candidate).map_err(|error| match error {
                    cookie_agent_config::ConfigError::ChangedOnDisk(path) => {
                        EngineError::ConfigConflict(format!(
                            "{} changed while it was written",
                            path.display()
                        ))
                    }
                    error => config_error(error),
                })?;
            }
            let mut outcome = OnboardingOutcome {
                written: Vec::new(),
                skipped: Vec::new(),
                written_new: Vec::new(),
                settings,
                state,
            };
            if let Some(kit) = &kit {
                for file in plan {
                    let path = file.path.as_str().to_owned();
                    match file.action {
                        KitFileAction::Write => outcome.written.push(path),
                        KitFileAction::Skip => outcome.skipped.push(path),
                        KitFileAction::WriteNew => {
                            write_beside(&root, &path, &kit.files[&file.path])
                                .map_err(config_error)?;
                            outcome.written_new.push(path);
                        }
                    }
                }
            }
            Ok::<_, EngineError>((outcome, changed.then_some(loaded)))
        })
        .await
        .map_err(|error| EngineError::Runtime(error.to_string()))??;
        if let Some(loaded) = loaded {
            self.apply_configuration_locked(loaded).await?;
        }
        Ok(OnboardingOutcome {
            state: self.onboarding_state(),
            ..outcome
        })
    }

    /// The kit of `source`, checked, with where it came from.
    async fn kit(&self, source: KitSource) -> Result<(String, Arc<Kit>), EngineError> {
        match source {
            KitSource::Archive(bytes) => {
                let kit = super::blocking_io::run(move || Kit::from_archive(&bytes))
                    .await
                    .map_err(|error| EngineError::Runtime(error.to_string()))??;
                Ok(("archive".to_owned(), Arc::new(kit)))
            }
            KitSource::Release => self.release_kit().await,
        }
    }

    async fn release_kit(&self) -> Result<(String, Arc<Kit>), EngineError> {
        let releases_url = self
            .inner
            .onboarding
            .releases_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(cached) = self
            .inner
            .onboarding
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|cached| {
                cached.releases_url == releases_url && cached.fetched.elapsed() < KIT_CACHE
            })
        {
            return Ok((cached.origin.clone(), Arc::clone(&cached.kit)));
        }
        let (origin, bytes) = download_release_kit(&releases_url).await?;
        let kit = super::blocking_io::run(move || Kit::from_archive(&bytes))
            .await
            .map_err(|error| EngineError::Runtime(error.to_string()))??;
        let kit = Arc::new(kit);
        *self
            .inner
            .onboarding
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedKit {
            fetched: Instant::now(),
            releases_url,
            origin: origin.clone(),
            kit: Arc::clone(&kit),
        });
        Ok((origin, kit))
    }
}

/// Downloads the kit of this build's release, else of the latest release:
/// its URL and bytes.
async fn download_release_kit(releases_url: &str) -> Result<(String, Vec<u8>), KitError> {
    let unavailable = |reason: String| KitError::Unavailable(reason);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| unavailable(error.to_string()))?;
    let base = releases_url.trim_end_matches('/');
    let candidates = [
        format!(
            "{base}/download/v{}/{}",
            env!("CARGO_PKG_VERSION"),
            kit::KIT_ASSET
        ),
        format!("{base}/latest/download/{}", kit::KIT_ASSET),
    ];
    for url in candidates {
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|error| unavailable(format!("{url}: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !response.status().is_success() {
            return Err(unavailable(format!("{url}: HTTP {}", response.status())));
        }
        if response
            .content_length()
            .is_some_and(|length| length > kit::MAX_ARCHIVE_BYTES as u64)
        {
            return Err(KitError::TooLarge);
        }
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| unavailable(format!("{url}: {error}")))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > kit::MAX_ARCHIVE_BYTES {
                return Err(KitError::TooLarge);
            }
        }
        return Ok((url, bytes));
    }
    Err(unavailable(
        "neither this version's release nor the latest release has a starter kit".into(),
    ))
}
