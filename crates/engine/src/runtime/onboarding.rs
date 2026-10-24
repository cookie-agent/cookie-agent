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
/// How long a release list that could not be read is not asked for again.
const RELEASE_LIST_FAILURE_CACHE: Duration = Duration::from_secs(60);
/// The largest release list read.
const MAX_RELEASE_LIST_BYTES: usize = 4 * 1024 * 1024;

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
    /// Shared with the timer that drops it once it expires.
    cached: Arc<Mutex<Option<CachedKit>>>,
    /// The last release list that could not be read, so a retry within
    /// [`RELEASE_LIST_FAILURE_CACHE`] does not ask again.
    list_failure: Mutex<Option<ListFailure>>,
    /// The onboarding state as of the runtime published last, computed once
    /// per publication for every connection to compare.
    state: tokio::sync::watch::Sender<OnboardingState>,
}

impl OnboardingRuntimeState {
    /// The state of an engine that opens in onboarding state `state`.
    pub(crate) fn new(state: OnboardingState) -> Self {
        Self {
            releases_url: Mutex::new(RELEASES_URL.to_owned()),
            cached: Arc::default(),
            list_failure: Mutex::new(None),
            state: tokio::sync::watch::Sender::new(state),
        }
    }
}

struct ListFailure {
    at: Instant,
    api: String,
    error: String,
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

    /// The onboarding state, updated with each runtime publication when it
    /// changes: a configuration or runtime change publishes one.
    #[must_use]
    pub fn subscribe_onboarding(&self) -> tokio::sync::watch::Receiver<OnboardingState> {
        self.inner.onboarding.state.subscribe()
    }

    /// Recomputes the onboarding state against `models`, about to be
    /// published, telling subscribers when it changed.
    pub(crate) fn publish_onboarding_state(&self, models: &CompiledModelRuntime) {
        let state = onboarding_state(&self.inner.config.load().runtime, models);
        self.inner.onboarding.state.send_if_modified(|current| {
            let changed = *current != state;
            if changed {
                *current = state;
            }
            changed
        });
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
        {
            let mut cached = self
                .inner
                .onboarding
                .cached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cached
                .as_ref()
                .is_some_and(|cached| cached.fetched.elapsed() >= KIT_CACHE)
            {
                *cached = None;
            }
            if let Some(cached) = cached
                .as_ref()
                .filter(|cached| cached.releases_url == releases_url)
            {
                return Ok((cached.origin.clone(), Arc::clone(&cached.kit)));
            }
        }
        let (origin, bytes) =
            download_release_kit(&releases_url, &self.inner.onboarding.list_failure).await?;
        let kit = super::blocking_io::run(move || Kit::from_archive(&bytes))
            .await
            .map_err(|error| EngineError::Runtime(error.to_string()))??;
        let kit = Arc::new(kit);
        let fetched = Instant::now();
        *self
            .inner
            .onboarding
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedKit {
            fetched,
            releases_url,
            origin: origin.clone(),
            kit: Arc::clone(&kit),
        });
        // The kit is large: it goes once it expires rather than staying until
        // the next download replaces it.
        let cached = Arc::downgrade(&self.inner.onboarding.cached);
        tokio::spawn(async move {
            tokio::time::sleep(KIT_CACHE).await;
            if let Some(cached) = cached.upgrade() {
                let mut cached = cached
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if cached.as_ref().is_some_and(|kit| kit.fetched == fetched) {
                    *cached = None;
                }
            }
        });
        Ok((origin, kit))
    }
}

/// Downloads the kit and says where it came from. In order:
///
/// 1. `<releases>/download/v<version>/<asset>`, this build's own release;
/// 2. the newest release in the release list, prereleases included, whose
///    tag's major.minor.patch is this build's version and which carries the
///    kit (nightlies are tagged `v<version>-alpha.<n>`);
/// 3. `<releases>/latest/download/<asset>`, the latest stable release.
///
/// The list is one unauthenticated page of the GitHub API; a rate limit or
/// any other failure there moves on to the next step.
async fn download_release_kit(
    releases_url: &str,
    list_failure: &Mutex<Option<ListFailure>>,
) -> Result<(String, Vec<u8>), KitError> {
    let unavailable = |reason: String| KitError::Unavailable(reason);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| unavailable(error.to_string()))?;
    let base = releases_url.trim_end_matches('/');
    let own = format!(
        "{base}/download/v{}/{}",
        env!("CARGO_PKG_VERSION"),
        kit::KIT_ASSET
    );
    let mut failures = Vec::new();
    match download(&client, &own).await {
        Ok(Some(bytes)) => return Ok((own, bytes)),
        Ok(None) => failures.push(format!("{own}: not found")),
        Err(KitError::TooLarge) => return Err(KitError::TooLarge),
        Err(error) => failures.push(error.to_string()),
    }
    match cached_prerelease_kit_url(&client, &releases_api_url(base), list_failure).await {
        Ok(Some(url)) => match download(&client, &url).await {
            Ok(Some(bytes)) => return Ok((url, bytes)),
            Ok(None) => failures.push(format!("{url}: not found")),
            Err(KitError::TooLarge) => return Err(KitError::TooLarge),
            Err(error) => failures.push(error.to_string()),
        },
        Ok(None) => failures.push(format!(
            "no release of {} carries a starter kit",
            env!("CARGO_PKG_VERSION")
        )),
        Err(error) => failures.push(error),
    }
    let latest = format!("{base}/latest/download/{}", kit::KIT_ASSET);
    match download(&client, &latest).await {
        Ok(Some(bytes)) => return Ok((latest, bytes)),
        Ok(None) => failures.push(format!("{latest}: not found")),
        Err(KitError::TooLarge) => return Err(KitError::TooLarge),
        Err(error) => failures.push(error.to_string()),
    }
    Err(unavailable(format!(
        "no release has a starter kit for this version ({})",
        failures.join("; ")
    )))
}

/// The GitHub API's release list for a `https://github.com/<owner>/<repo>/releases`
/// page, one page of 30; for any other base (a test's server),
/// `<base>/api`.
fn releases_api_url(base: &str) -> String {
    match base
        .strip_prefix("https://github.com/")
        .and_then(|rest| rest.strip_suffix("/releases"))
    {
        Some(repository) => {
            format!("https://api.github.com/repos/{repository}/releases?per_page=30")
        }
        None => format!("{base}/api"),
    }
}

/// [`prerelease_kit_url`] for this build's version, unless the list at `api`
/// could not be read within the last [`RELEASE_LIST_FAILURE_CACHE`]: then
/// that failure again, without asking.
async fn cached_prerelease_kit_url(
    client: &reqwest::Client,
    api: &str,
    list_failure: &Mutex<Option<ListFailure>>,
) -> Result<Option<String>, String> {
    let lock = || {
        list_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    if let Some(failure) = lock()
        .as_ref()
        .filter(|failure| failure.api == api && failure.at.elapsed() < RELEASE_LIST_FAILURE_CACHE)
    {
        return Err(failure.error.clone());
    }
    let found = prerelease_kit_url(client, api, env!("CARGO_PKG_VERSION")).await;
    *lock() = found.as_ref().err().map(|error| ListFailure {
        at: Instant::now(),
        api: api.to_owned(),
        error: error.clone(),
    });
    found
}

/// The download URL of the kit of the newest release in the list at `api`
/// whose tag is `version`, or a pre-release of it, and which carries the
/// kit; `None` when no release does.
async fn prerelease_kit_url(
    client: &reqwest::Client,
    api: &str,
    version: &str,
) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        published_at: Option<String>,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    let mut response = client
        .get(api)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("{api}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{api}: HTTP {}", response.status()));
    }
    let too_large =
        || format!("{api}: the release list is larger than {MAX_RELEASE_LIST_BYTES} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_LIST_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("{api}: {error}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_RELEASE_LIST_BYTES {
            return Err(too_large());
        }
    }
    let mut releases = serde_json::from_slice::<Vec<Release>>(&body)
        .map_err(|error| format!("{api}: the release list does not read: {error}"))?;
    // Newest first, as the API lists them; published_at settles it when the
    // list says otherwise. RFC 3339 times in UTC sort as text.
    releases.sort_by(|left, right| right.published_at.cmp(&left.published_at));
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter(|release| release_core(&release.tag_name) == Some(release_core_of(version)))
        .find_map(|release| {
            release
                .assets
                .into_iter()
                .find(|asset| asset.name == kit::KIT_ASSET)
                .map(|asset| asset.browser_download_url)
        }))
}

/// The major.minor.patch of a release tag: `v0.4.0-alpha.7` is `0.4.0`.
fn release_core(tag: &str) -> Option<&str> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let core = release_core_of(version);
    (core.split('.').count() == 3 && core.split('.').all(|part| part.parse::<u64>().is_ok()))
        .then_some(core)
}

fn release_core_of(version: &str) -> &str {
    version.split(['-', '+']).next().unwrap_or(version)
}

/// The body at `url`: `None` for a 404, an error for any other failure.
async fn download(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>, KitError> {
    let unavailable = |reason: String| KitError::Unavailable(reason);
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| unavailable(format!("{url}: {error}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
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
    Ok(Some(bytes))
}
