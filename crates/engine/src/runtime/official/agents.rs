//! The official agents (`docs/site/specs/machines-on-demand.md` section
//! 2.1): fetched from a versionless source, `<source>/index.json` and
//! `<source>/<id>.md`, each document checked against the index's SHA-256
//! and against this build's configuration, and kept in
//! `<data_root>/official_agents/<id>.md`, which only the daemon writes.
//! Agents the index no longer lists, or whose `requires_cookie` this version
//! does not satisfy, are removed; a failed refresh keeps the cached copies
//! and is reported as a configuration diagnostic. A refresh that changes
//! anything reloads the configuration, so new runs see it at once; a user
//! document of the same ID in `agents/` replaces the official one.
//!
//! The index also lists templates (`templates`, kept as `_<name>.md`) and
//! the presets onboarding offers (`acp_presets`, `mcp_presets`, kept in
//! `.presets.json`), in lists of their own that earlier builds ignore
//! (`docs/site/specs/acp-onboarding.md` sections 3, 6.1, and 9.1).

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::skills::{
    fetch, local, lock, sha256, source_client, version_satisfies, write_atomically,
};
use crate::Engine;

/// The official source. Production never changes it; tests serve their own
/// with [`Engine::set_official_agents_url`].
pub const OFFICIAL_AGENTS_URL: &str = "https://cookie-agent.github.io/agents";
/// How often a running daemon refreshes them.
pub const OFFICIAL_AGENTS_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_INDEX_BYTES: usize = 1024 * 1024;
/// An agent document's own limit (`agents.md`, document structure).
const MAX_AGENT_BYTES: usize = 256 * 1024;
const MAX_AGENTS: usize = 256;

/// `index.json` of the source.
#[derive(Clone, Debug, Deserialize)]
struct SourceIndex {
    version: u32,
    agents: Vec<IndexAgent>,
    /// Templates by name without the `_`; kept as `_<name>.md`.
    #[serde(default)]
    templates: Vec<IndexAgent>,
    /// Each read on its own, so one this build cannot use leaves out only
    /// itself.
    #[serde(default)]
    acp_presets: Vec<serde_json::Value>,
    #[serde(default)]
    mcp_presets: Vec<serde_json::Value>,
}

/// The presets onboarding offers, from the official source
/// (`docs/site/specs/acp-onboarding.md` sections 3 and 9.1).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OfficialPresets {
    #[serde(default)]
    pub acp: Vec<cookie_agent_protocol::AcpPreset>,
    #[serde(default)]
    pub mcp: Vec<cookie_agent_protocol::McpPreset>,
}

/// Where the presets are kept, beside the official agents.
const PRESETS_RECORD: &str = ".presets.json";

/// A preset of the index with the cookie versions it needs.
#[derive(Deserialize)]
struct Versioned<T> {
    requires_cookie: String,
    #[serde(flatten)]
    preset: T,
}

/// The presets of `index` this build can use, and why the others were left
/// out: each must read, need a cookie version this one is, and hold what a
/// preset must (a command, a server table the configuration takes).
pub fn usable_presets(
    acp: &[serde_json::Value],
    mcp: &[serde_json::Value],
    version: &str,
) -> (OfficialPresets, Vec<String>) {
    let mut presets = OfficialPresets::default();
    let mut refusals = Vec::new();
    for value in acp.iter().take(MAX_AGENTS) {
        let label = preset_label(value);
        match serde_json::from_value::<Versioned<cookie_agent_protocol::AcpPreset>>(value.clone()) {
            Err(error) => refusals.push(format!("ACP preset `{label}`: {error}")),
            Ok(versioned) => match version_satisfies(&versioned.requires_cookie, version) {
                None => refusals.push(format!(
                    "ACP preset `{label}`: `{}` is not a version range",
                    versioned.requires_cookie
                )),
                Some(false) => {}
                Some(true) => match acp_preset_problem(&versioned.preset) {
                    Some(problem) => refusals.push(format!("ACP preset `{label}`: {problem}")),
                    None => presets.acp.push(versioned.preset),
                },
            },
        }
    }
    for value in mcp.iter().take(MAX_AGENTS) {
        let label = preset_label(value);
        match serde_json::from_value::<Versioned<cookie_agent_protocol::McpPreset>>(value.clone()) {
            Err(error) => refusals.push(format!("MCP preset `{label}`: {error}")),
            Ok(versioned) => match version_satisfies(&versioned.requires_cookie, version) {
                None => refusals.push(format!(
                    "MCP preset `{label}`: `{}` is not a version range",
                    versioned.requires_cookie
                )),
                Some(false) => {}
                Some(true) => match mcp_preset_problem(&versioned.preset) {
                    Some(problem) => refusals.push(format!("MCP preset `{label}`: {problem}")),
                    None => presets.mcp.push(versioned.preset),
                },
            },
        }
    }
    (presets, refusals)
}

fn preset_label(value: &serde_json::Value) -> String {
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?")
        .to_owned()
}

fn acp_preset_problem(preset: &cookie_agent_protocol::AcpPreset) -> Option<String> {
    if preset.name.trim().is_empty() {
        return Some("has no name".into());
    }
    if preset
        .command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        return Some("has no command".into());
    }
    None
}

fn mcp_preset_problem(preset: &cookie_agent_protocol::McpPreset) -> Option<String> {
    if preset.name.trim().is_empty() {
        return Some("has no name".into());
    }
    if preset.place == cookie_agent_protocol::McpPresetPlace::Unknown {
        return Some("`where` is neither `global` nor `machine`".into());
    }
    mcp_server_problem(
        &preset.id,
        &preset.server,
        preset.place == cookie_agent_protocol::McpPresetPlace::Global,
    )
}

/// Why `server` is not a table the configuration takes for an MCP server
/// `name`, at the top level (`global`) or a runtime's.
pub(crate) fn mcp_server_problem(
    name: &str,
    server: &serde_json::Map<String, serde_json::Value>,
    global: bool,
) -> Option<String> {
    let value = match toml::Value::try_from(serde_json::Value::Object(server.clone())) {
        Ok(value) => value,
        Err(error) => return Some(format!("its server is not a TOML table: {error}")),
    };
    let config = match cookie_agent_config::McpServerConfig::deserialize(value) {
        Ok(config) => config,
        Err(error) => return Some(format!("its server does not read: {error}")),
    };
    let checked = if global {
        config.validate_top_level(name)
    } else {
        config.validate(name)
    };
    checked.err().map(|error| error.to_string())
}

/// The presets kept in `root`, or none.
fn read_presets(root: &Path) -> OfficialPresets {
    std::fs::read(root.join(PRESETS_RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// One agent of the index, and of the daemon's record of what it keeps
/// (`.index.json` in the official agents folder, which the configuration
/// reads the versions from).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct IndexAgent {
    id: String,
    version: String,
    requires_cookie: String,
    sha256: String,
}

/// What a refresh did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OfficialAgentsReport {
    /// `false` when `[agents] official` is off: nothing was refreshed.
    pub enabled: bool,
    /// The agents kept afterwards, by ID, with their versions.
    pub agents: BTreeMap<String, String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// Why the refresh failed, or what it refused, if anything.
    pub error: Option<String>,
    /// The presets kept afterwards: the index's, or the cached ones when
    /// it was not read.
    pub presets: OfficialPresets,
}

pub(crate) struct OfficialAgentsState {
    /// Where the engine keeps them without a user configuration root; with
    /// one, they live below it, where the configuration reads them.
    root: PathBuf,
    versions: Mutex<Arc<BTreeMap<String, String>>>,
    /// The presets as of the last refresh; `None` before one, when they
    /// are read from where the configuration keeps them.
    presets: Mutex<Option<Arc<OfficialPresets>>>,
    source_url: Mutex<String>,
    last_error: Mutex<Option<String>>,
    refresh: tokio::sync::Mutex<()>,
}

impl OfficialAgentsState {
    pub(crate) fn new(data_dir: &Path) -> Self {
        let root = cookie_agent_config::official_agents_root(data_dir);
        Self {
            // The configuration reads the versions it shows from the same
            // record; this is only the engine's own view until a refresh.
            versions: Mutex::new(Arc::new(versions(read_kept(&root)))),
            root,
            presets: Mutex::new(None),
            source_url: Mutex::new(OFFICIAL_AGENTS_URL.to_owned()),
            last_error: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }
}

impl Engine {
    /// Fetches the official agents from `url` instead of
    /// [`OFFICIAL_AGENTS_URL`], for tests that serve their own.
    pub fn set_official_agents_url(&self, url: impl Into<String>) {
        *lock(&self.inner.official_agents.source_url) = url.into();
    }

    /// The official agents kept, by ID, with their versions.
    #[must_use]
    pub fn official_agents(&self) -> Arc<BTreeMap<String, String>> {
        Arc::clone(&lock(&self.inner.official_agents.versions))
    }

    /// The presets onboarding offers: the last refresh's, else the kept
    /// ones.
    #[must_use]
    pub fn official_presets(&self) -> Arc<OfficialPresets> {
        let state = &self.inner.official_agents;
        if let Some(presets) = lock(&state.presets).as_ref() {
            return Arc::clone(presets);
        }
        let root = self.configuration_root().ok().map_or_else(
            || state.root.clone(),
            |root| cookie_agent_config::official_agents_root(&root),
        );
        let presets = Arc::new(read_presets(&root));
        *lock(&state.presets) = Some(Arc::clone(&presets));
        presets
    }

    /// Why the last refresh failed, and why kept official agents were left
    /// out of the configuration, as configuration diagnostics.
    pub(crate) fn official_agents_diagnostics(&self) -> Vec<String> {
        let mut diagnostics = lock(&self.inner.official_agents.last_error)
            .as_ref()
            .map(|error| format!("official agents were not refreshed: {error}"))
            .into_iter()
            .collect::<Vec<_>>();
        diagnostics.extend(
            self.inner
                .config
                .load()
                .official_agent_diagnostics
                .iter()
                .cloned(),
        );
        diagnostics
    }

    /// Refreshes the official agents now and, when anything changed,
    /// reloads the configuration; refreshes do not overlap.
    pub async fn refresh_official_agents(&self) -> OfficialAgentsReport {
        let state = &self.inner.official_agents;
        let _refresh = state.refresh.lock().await;
        if !self.inner.config.load().runtime.agents.official {
            *lock(&state.last_error) = None;
            return OfficialAgentsReport {
                enabled: false,
                ..OfficialAgentsReport::default()
            };
        }
        let url = lock(&state.source_url).clone();
        let config_root = self.configuration_root().ok();
        let root = config_root.as_deref().map_or_else(
            || state.root.clone(),
            cookie_agent_config::official_agents_root,
        );
        let mut report = refresh(&url, &root, config_root).await;
        report.enabled = true;
        *lock(&state.versions) = Arc::new(report.agents.clone());
        let presets_changed = lock(&state.presets)
            .as_deref()
            .is_none_or(|presets| *presets != report.presets);
        *lock(&state.presets) = Some(Arc::new(report.presets.clone()));
        if (!report.updated.is_empty() || !report.removed.is_empty() || presets_changed)
            && let Err(error) = self.reload_configuration().await
        {
            let reload = format!("the configuration did not reload: {error}");
            report.error = Some(match report.error.take() {
                Some(error) => format!("{error}; {reload}"),
                None => reload,
            });
        }
        *lock(&state.last_error) = report.error.clone();
        report
    }
}

fn read_kept(root: &Path) -> Vec<IndexAgent> {
    std::fs::read(root.join(cookie_agent_config::OFFICIAL_AGENTS_RECORD))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn versions(agents: impl IntoIterator<Item = IndexAgent>) -> BTreeMap<String, String> {
    agents
        .into_iter()
        .map(|agent| (agent.id, agent.version))
        .collect()
}

/// One refresh against the source at `url` into `root`. `config_root` is
/// the user configuration each downloaded document is validated with.
async fn refresh(url: &str, root: &Path, config_root: Option<PathBuf>) -> OfficialAgentsReport {
    let mut report = OfficialAgentsReport::default();
    let kept = {
        let root = root.to_owned();
        local(move || read_kept(&root)).await.unwrap_or_default()
    };
    let cached_presets = {
        let root = root.to_owned();
        local(move || read_presets(&root)).await.unwrap_or_default()
    };
    let fail = |mut report: OfficialAgentsReport, error: String, kept: Vec<IndexAgent>| {
        report.error = Some(error);
        report.agents = versions(kept);
        report.presets = cached_presets.clone();
        report
    };
    let client = match source_client() {
        Ok(client) => client,
        Err(error) => return fail(report, error.to_string(), kept),
    };
    let base = url.trim_end_matches('/');
    let index = match fetch(&client, &format!("{base}/index.json"), MAX_INDEX_BYTES)
        .await
        .and_then(|bytes| {
            serde_json::from_slice::<SourceIndex>(&bytes)
                .map_err(|error| format!("index.json does not read: {error}"))
        })
        .and_then(|index| {
            (index.version == 1)
                .then_some(index)
                .ok_or_else(|| "index.json is of a version this build does not read".to_owned())
        }) {
        Ok(index) => index,
        Err(error) => return fail(report, error, kept),
    };
    let mut refusals = Vec::new();
    let (presets, preset_refusals) = usable_presets(
        &index.acp_presets,
        &index.mcp_presets,
        env!("CARGO_PKG_VERSION"),
    );
    refusals.extend(preset_refusals);
    {
        let root = root.to_owned();
        let bytes = serde_json::to_vec_pretty(&presets).unwrap_or_default();
        if let Ok(Err(error)) =
            local(move || write_atomically(&root.join(PRESETS_RECORD), &bytes)).await
        {
            refusals.push(format!("the presets were not kept: {error}"));
        }
    }
    report.presets = presets;
    // A template is kept as `_<name>.md`, under the ID `_<name>`.
    let templates = index.templates.into_iter().map(|template| IndexAgent {
        id: format!("_{}", template.id),
        ..template
    });
    let mut wanted = BTreeMap::new();
    for agent in index
        .agents
        .into_iter()
        .take(MAX_AGENTS)
        .chain(templates.take(MAX_AGENTS))
    {
        if let Err(reason) = valid_entry(&agent) {
            refusals.push(format!("`{}`: {reason}", agent.id));
            continue;
        }
        match version_satisfies(&agent.requires_cookie, env!("CARGO_PKG_VERSION")) {
            Some(true) => {
                wanted.insert(agent.id.clone(), agent);
            }
            Some(false) => {}
            None => refusals.push(format!(
                "`{}`: `{}` is not a version range",
                agent.id, agent.requires_cookie
            )),
        }
    }
    let kept_by_id = kept
        .into_iter()
        .map(|agent| (agent.id.clone(), agent))
        .collect::<BTreeMap<_, _>>();
    // Download what changed, checked against the index, before anything is
    // written.
    let mut downloads = Vec::new();
    let mut now_kept = BTreeMap::new();
    for (id, agent) in wanted {
        let path = root.join(format!("{id}.md"));
        let current = {
            let path = path.clone();
            local(move || std::fs::read(path).ok()).await.ok().flatten()
        };
        if current
            .as_deref()
            .is_some_and(|bytes| sha256(bytes).eq_ignore_ascii_case(&agent.sha256))
        {
            if !kept_by_id.contains_key(&id) {
                report.updated.push(id.clone());
            }
            now_kept.insert(id, agent);
            continue;
        }
        match fetch(&client, &format!("{base}/{id}.md"), MAX_AGENT_BYTES).await {
            Ok(bytes) if sha256(&bytes).eq_ignore_ascii_case(&agent.sha256) => {
                downloads.push((id.clone(), bytes));
                now_kept.insert(id, agent);
            }
            Ok(bytes) => {
                refusals.push(format!(
                    "`{id}`: does not match the index: its SHA-256 is {}",
                    sha256(&bytes)
                ));
                if let Some(previous) = kept_by_id.get(&id) {
                    now_kept.insert(id, previous.clone());
                }
            }
            Err(error) => {
                refusals.push(format!("`{id}`: {error}"));
                if let Some(previous) = kept_by_id.get(&id) {
                    now_kept.insert(id, previous.clone());
                }
            }
        }
    }
    let previously = kept_by_id.keys().cloned().collect::<BTreeSet<_>>();
    let root = root.to_owned();
    let installed = local(move || {
        install(
            &root,
            config_root.as_deref(),
            downloads,
            &mut now_kept,
            &previously,
        )
        .map(|outcome| (outcome, now_kept))
    })
    .await;
    match installed {
        Ok(Ok(((updated, removed, install_refusals), now_kept))) => {
            report.updated.extend(updated);
            report.removed = removed;
            refusals.extend(install_refusals);
            report.agents = versions(now_kept.into_values());
        }
        Ok(Err(error)) | Err(error) => {
            refusals.push(error);
            report.agents = versions(kept_by_id.into_values());
        }
    }
    report.updated.sort();
    report.updated.dedup();
    if !refusals.is_empty() {
        report.error = Some(refusals.join("; "));
    }
    report
}

/// The updated agents, the removed ones, and the documents refused.
type InstallOutcome = (Vec<String>, Vec<String>, Vec<String>);

/// Checks each download loads with this build and the user's configuration
/// at `config_root`, writes the ones that do, removes the agents `now_kept`
/// no longer lists, and records `now_kept`. A refused download keeps the
/// previous copy, if any.
fn install(
    root: &Path,
    config_root: Option<&Path>,
    downloads: Vec<(String, Vec<u8>)>,
    now_kept: &mut BTreeMap<String, IndexAgent>,
    previously: &BTreeSet<String>,
) -> Result<InstallOutcome, String> {
    let mut updated = Vec::new();
    let mut refusals = Vec::new();
    let base_tree = config_root
        .map(|config_root| cookie_agent_config::ConfigTree::read(Some(config_root)))
        .transpose()
        .map_err(|error| format!("the configuration does not read: {error}"))?;
    for (id, bytes) in downloads {
        if let Err(reason) = loads(base_tree.as_ref(), &id, &bytes) {
            refusals.push(format!("`{id}` does not load: {reason}"));
            now_kept.remove(&id);
            continue;
        }
        write_atomically(&root.join(format!("{id}.md")), &bytes)
            .map_err(|error| format!("`{id}` was not written: {error}"))?;
        updated.push(id);
    }
    // A refused agent that was kept before stays as it was on disk.
    let kept_before = read_kept(root)
        .into_iter()
        .map(|agent| (agent.id.clone(), agent))
        .collect::<BTreeMap<_, _>>();
    for refused in refusals.iter().filter_map(|refusal| {
        refusal
            .strip_prefix('`')
            .and_then(|rest| rest.split('`').next())
    }) {
        if let Some(previous) = kept_before.get(refused) {
            now_kept.insert(refused.to_owned(), previous.clone());
        }
    }
    let mut removed = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                // The record, and temporary files an interrupted write left.
                if name.ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            }
            let id = name.strip_suffix(".md").unwrap_or(&name);
            if now_kept.contains_key(id) {
                continue;
            }
            if std::fs::remove_file(entry.path()).is_ok() && previously.contains(id) {
                removed.push(id.to_owned());
            }
        }
    }
    let record = now_kept.values().cloned().collect::<Vec<_>>();
    write_atomically(
        &root.join(cookie_agent_config::OFFICIAL_AGENTS_RECORD),
        &serde_json::to_vec_pretty(&record).unwrap_or_default(),
    )
    .map_err(|error| format!("the record of kept agents was not written: {error}"))?;
    Ok((updated, removed, refusals))
}

/// Whether the official document `id` loads with this build, beside the
/// user's configuration `tree` when there is one.
fn loads(
    tree: Option<&cookie_agent_config::ConfigTree>,
    id: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let put = |tree: &mut cookie_agent_config::ConfigTree| -> Result<(), String> {
        match id.strip_prefix('_') {
            Some(name) => {
                cookie_agent_identity::AgentId::new(name).map_err(|error| error.to_string())?;
                tree.put_official_template(name, bytes.to_vec());
            }
            None => {
                let id =
                    cookie_agent_identity::AgentId::new(id).map_err(|error| error.to_string())?;
                tree.put_official_agent(id, bytes.to_vec());
            }
        }
        Ok(())
    };
    let mut tree = tree.cloned().unwrap_or_default();
    tree.clear_official_agents();
    put(&mut tree)?;
    match cookie_agent_config::load_from_tree(&tree) {
        // The user's own configuration may not load; that is not the
        // official document's fault, so check it alone.
        Err(_) => {
            let mut alone = cookie_agent_config::ConfigTree::default();
            put(&mut alone)?;
            first_diagnostic(cookie_agent_config::load_from_tree(&alone))
        }
        loaded => first_diagnostic(loaded),
    }
}

fn first_diagnostic(
    loaded: Result<cookie_agent_config::LoadedConfiguration, cookie_agent_config::ConfigError>,
) -> Result<(), String> {
    let loaded = loaded.map_err(|error| error.to_string())?;
    match loaded.official_agent_diagnostics.into_iter().next() {
        Some(diagnostic) => Err(diagnostic),
        None => Ok(()),
    }
}

fn valid_entry(agent: &IndexAgent) -> Result<(), String> {
    // A template's entry is `_<name>`, its file `_<name>.md`.
    cookie_agent_identity::AgentId::new(agent.id.strip_prefix('_').unwrap_or(&agent.id))
        .map_err(|_| "not an agent ID".to_owned())?;
    if agent.sha256.len() != 64 || !agent.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("has no SHA-256".into());
    }
    if agent.version.is_empty() || agent.version.len() > 64 {
        return Err("has no version".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
