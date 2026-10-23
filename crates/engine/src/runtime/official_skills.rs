//! The official skills (`docs/site/specs/default-models-and-onboarding.md`
//! section 13): fetched from a versionless source, `<source>/index.json`
//! and `<source>/<name>/<file>`, every file checked against the index's
//! SHA-256, and kept in `<data_root>/official_skills/<name>/`, which only
//! the daemon writes. Skills the index no longer lists, or whose
//! `requires_cookie` this version does not satisfy, are removed; a failed
//! refresh keeps the cached copies and is reported as a configuration
//! diagnostic.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::Engine;

/// The official source. Production never changes it; tests serve their own
/// with [`Engine::set_official_skills_url`].
pub const OFFICIAL_SKILLS_URL: &str = "https://cookie-agent.github.io/skills";
/// How often a running daemon refreshes them.
pub const OFFICIAL_SKILLS_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);
/// The daemon's record of what it keeps, in the official skills folder; its
/// leading `.` keeps it out of skill discovery.
const KEPT_INDEX: &str = ".index.json";
const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_FILES_PER_SKILL: usize = 64;

/// `index.json` of the source.
#[derive(Clone, Debug, Deserialize)]
struct SourceIndex {
    version: u32,
    skills: Vec<IndexSkill>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct IndexSkill {
    name: String,
    version: String,
    requires_cookie: String,
    files: Vec<IndexFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct IndexFile {
    path: String,
    sha256: String,
}

/// What a refresh did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OfficialSkillsReport {
    /// `false` when `[skills] official` is off: nothing was refreshed.
    pub enabled: bool,
    /// The skills kept afterwards, by name, with their versions.
    pub skills: BTreeMap<String, String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// Why the refresh failed, or what it refused, if anything.
    pub error: Option<String>,
}

/// Where the user skills live, for reloading the daemon's skills after a
/// refresh changed the official ones.
#[derive(Clone, Debug, Default)]
pub struct UserSkillRoots {
    /// `~/.agents/skills`.
    pub shared: Option<PathBuf>,
    /// `~/.cookie-agent/skills`.
    pub user: Option<PathBuf>,
}

pub(crate) struct OfficialSkillsState {
    root: PathBuf,
    source_url: Mutex<String>,
    last_error: Mutex<Option<String>>,
    user_roots: Mutex<Option<UserSkillRoots>>,
    refresh: tokio::sync::Mutex<()>,
}

impl OfficialSkillsState {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            root: cookie_agent_config::official_skills_root(data_dir),
            source_url: Mutex::new(OFFICIAL_SKILLS_URL.to_owned()),
            last_error: Mutex::new(None),
            user_roots: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Engine {
    /// Fetches the official skills from `url` instead of
    /// [`OFFICIAL_SKILLS_URL`], for tests that serve their own.
    pub fn set_official_skills_url(&self, url: impl Into<String>) {
        *lock(&self.inner.official_skills.source_url) = url.into();
    }

    /// The user skill roots the daemon's skills were loaded from; once set,
    /// a refresh that changes the official skills reloads the daemon's
    /// skills from them and the official folder, so new sessions see the
    /// change without a restart.
    pub fn set_user_skill_roots(&self, roots: UserSkillRoots) {
        *lock(&self.inner.official_skills.user_roots) = Some(roots);
    }

    /// The official skills kept, by name, with their versions.
    #[must_use]
    pub fn official_skills(&self) -> BTreeMap<String, String> {
        read_kept(&self.inner.official_skills.root)
            .into_iter()
            .map(|skill| (skill.name, skill.version))
            .collect()
    }

    /// Why the last refresh failed, as a configuration diagnostic.
    pub(crate) fn official_skills_diagnostic(&self) -> Option<String> {
        lock(&self.inner.official_skills.last_error)
            .as_ref()
            .map(|error| format!("official skills were not refreshed: {error}"))
    }

    /// Refreshes the official skills now; refreshes do not overlap.
    pub async fn refresh_official_skills(&self) -> OfficialSkillsReport {
        let state = &self.inner.official_skills;
        let _refresh = state.refresh.lock().await;
        if !self.inner.config.load().runtime.skills.official {
            *lock(&state.last_error) = None;
            return OfficialSkillsReport {
                enabled: false,
                ..OfficialSkillsReport::default()
            };
        }
        let url = lock(&state.source_url).clone();
        let mut report = refresh(&url, &state.root).await;
        report.enabled = true;
        *lock(&state.last_error) = report.error.clone();
        if !report.updated.is_empty() || !report.removed.is_empty() {
            self.reload_daemon_skills();
        }
        report
    }

    /// Reloads the global runtime's skills from the user roots and the
    /// official folder, when the roots are known.
    fn reload_daemon_skills(&self) {
        let Some(roots) = lock(&self.inner.official_skills.user_roots).clone() else {
            return;
        };
        match cookie_agent_config::load_skill_roots_with_official(
            Some(&self.inner.official_skills.root),
            roots.shared.as_deref(),
            roots.user.as_deref(),
            &[],
        ) {
            Ok(skills) => self.inner.runtimes.global().replace_skills(skills),
            Err(error) => {
                *lock(&self.inner.official_skills.last_error) =
                    Some(format!("the skills did not load: {error}"));
            }
        }
    }
}

fn read_kept(root: &Path) -> Vec<IndexSkill> {
    std::fs::read(root.join(KEPT_INDEX))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// One refresh against the source at `url` into `root`.
async fn refresh(url: &str, root: &Path) -> OfficialSkillsReport {
    let mut report = OfficialSkillsReport::default();
    let kept = read_kept(root);
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            report.error = Some(error.to_string());
            report.skills = kept.into_iter().map(|s| (s.name, s.version)).collect();
            return report;
        }
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
        Err(error) => {
            report.error = Some(error);
            report.skills = kept.into_iter().map(|s| (s.name, s.version)).collect();
            return report;
        }
    };
    let mut refusals = Vec::new();
    let mut wanted = BTreeMap::new();
    for skill in index.skills {
        if let Err(reason) = valid_entry(&skill) {
            refusals.push(format!("`{}`: {reason}", skill.name));
            continue;
        }
        match version_satisfies(&skill.requires_cookie, env!("CARGO_PKG_VERSION")) {
            Some(true) => {
                wanted.insert(skill.name.clone(), skill);
            }
            Some(false) => {}
            None => refusals.push(format!(
                "`{}`: `{}` is not a version range",
                skill.name, skill.requires_cookie
            )),
        }
    }
    let kept_by_name = kept
        .into_iter()
        .map(|skill| (skill.name.clone(), skill))
        .collect::<BTreeMap<_, _>>();
    let mut now_kept = BTreeMap::new();
    for (name, skill) in wanted {
        let directory = root.join(&name);
        match update_skill(&client, base, &directory, &skill).await {
            Ok(changed) => {
                if changed || !kept_by_name.contains_key(&name) {
                    report.updated.push(name.clone());
                }
                now_kept.insert(name, skill);
            }
            Err(reason) => {
                refusals.push(format!("`{name}`: {reason}"));
                // The cached copy, if any, stays as it was.
                if let Some(previous) = kept_by_name.get(&name) {
                    now_kept.insert(name, previous.clone());
                }
            }
        }
    }
    // Everything else in the folder goes: unlisted skills, skills this
    // version does not satisfy, and leftovers.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || now_kept.contains_key(&name) {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if removed.is_ok() && kept_by_name.contains_key(&name) {
                report.removed.push(name);
            }
        }
    }
    let record = now_kept.values().cloned().collect::<Vec<_>>();
    if let Err(error) = write_atomically(
        &root.join(KEPT_INDEX),
        &serde_json::to_vec_pretty(&record).unwrap_or_default(),
    ) {
        refusals.push(format!(
            "the record of kept skills was not written: {error}"
        ));
    }
    report.skills = now_kept
        .into_values()
        .map(|skill| (skill.name, skill.version))
        .collect();
    if !refusals.is_empty() {
        report.error = Some(refusals.join("; "));
    }
    report
}

fn valid_entry(skill: &IndexSkill) -> Result<(), String> {
    let name_ok = !skill.name.is_empty()
        && skill.name.len() <= 64
        && !skill.name.starts_with('-')
        && !skill.name.ends_with('-')
        && !skill.name.contains("--")
        && skill
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !name_ok {
        return Err("not a skill name".into());
    }
    if skill.files.is_empty() || skill.files.len() > MAX_FILES_PER_SKILL {
        return Err(format!("lists 1 to {MAX_FILES_PER_SKILL} files"));
    }
    if !skill.files.iter().any(|file| file.path == "SKILL.md") {
        return Err("lists no SKILL.md".into());
    }
    let mut seen = BTreeSet::new();
    for file in &skill.files {
        let safe = !file.path.is_empty()
            && !file.path.starts_with('/')
            && !file.path.contains('\\')
            && file.path.split('/').all(|part| {
                !part.is_empty() && part != "." && part != ".." && !part.starts_with('.')
            });
        if !safe || !seen.insert(file.path.as_str()) {
            return Err(format!("`{}` is not a file path it may have", file.path));
        }
        if file.sha256.len() != 64 || !file.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("`{}` has no SHA-256", file.path));
        }
    }
    Ok(())
}

/// Brings `directory` in line with `skill`: downloads the files whose hash
/// changed, checks every one, and only then writes them and removes files
/// the index no longer lists. Whether anything changed.
async fn update_skill(
    client: &reqwest::Client,
    base: &str,
    directory: &Path,
    skill: &IndexSkill,
) -> Result<bool, String> {
    let mut downloads = Vec::new();
    for file in &skill.files {
        let local = directory.join(&file.path);
        let current = std::fs::read(&local).ok();
        if current
            .as_deref()
            .is_some_and(|bytes| sha256(bytes).eq_ignore_ascii_case(&file.sha256))
        {
            continue;
        }
        let bytes = fetch(
            client,
            &format!("{base}/{}/{}", skill.name, file.path),
            MAX_FILE_BYTES,
        )
        .await?;
        let actual = sha256(&bytes);
        if !actual.eq_ignore_ascii_case(&file.sha256) {
            return Err(format!(
                "`{}` does not match the index: its SHA-256 is {actual}",
                file.path
            ));
        }
        downloads.push((local, bytes));
    }
    if let Some((_, skill_md)) = downloads
        .iter()
        .find(|(path, _)| path.ends_with("SKILL.md") && path.parent() == Some(directory))
    {
        let text = std::str::from_utf8(skill_md).map_err(|_| "SKILL.md is not UTF-8")?;
        cookie_agent_config::parse_skill_document(
            text,
            cookie_agent_protocol::SkillSource::Official,
            &directory.join("SKILL.md"),
            directory,
            Vec::new(),
        )
        .map_err(|error| format!("SKILL.md does not load: {error}"))?;
    }
    let changed = !downloads.is_empty();
    for (path, bytes) in downloads {
        write_atomically(&path, &bytes).map_err(|error| error.to_string())?;
    }
    let listed = skill
        .files
        .iter()
        .map(|file| directory.join(&file.path))
        .collect::<BTreeSet<_>>();
    let mut removed_any = false;
    remove_unlisted(directory, &listed, &mut removed_any);
    Ok(changed || removed_any)
}

fn remove_unlisted(directory: &Path, listed: &BTreeSet<PathBuf>, removed: &mut bool) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_unlisted(&path, listed, removed);
            if std::fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none()) {
                let _ = std::fs::remove_dir(&path);
            }
        } else if !listed.contains(&path) && std::fs::remove_file(&path).is_ok() {
            *removed = true;
        }
    }
}

async fn fetch(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("{url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("{url}: larger than {limit} bytes"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("{url}: {error}"))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > limit {
            return Err(format!("{url}: larger than {limit} bytes"));
        }
    }
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Writes `bytes` to `path` through a temporary file beside it and a
/// rename, creating the directories on the way.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// A semver version as ranges compare it: major, minor, patch, and the
/// pre-release identifiers (build metadata ignored).
#[derive(Clone, Debug, Eq, PartialEq)]
struct SemVersion {
    core: (u64, u64, u64),
    pre: Vec<String>,
}

impl SemVersion {
    fn parse(text: &str) -> Option<Self> {
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => {
                let identifiers = pre.split('.').map(str::to_owned).collect::<Vec<_>>();
                if identifiers.iter().any(|identifier| {
                    identifier.is_empty()
                        || !identifier
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-')
                }) {
                    return None;
                }
                identifiers
            }
        };
        Some(Self {
            core: (major, minor, patch),
            pre,
        })
    }
}

impl Ord for SemVersion {
    /// Semver precedence: the core first; then a version without a
    /// pre-release above one with it; then the identifiers in order,
    /// numeric ones numerically and below alphanumeric ones, a longer list
    /// above its prefix.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                (false, false) => {}
            }
            for (left, right) in self.pre.iter().zip(&other.pre) {
                let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                    (Ok(left), Ok(right)) => left.cmp(&right),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => left.cmp(right),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            self.pre.len().cmp(&other.pre.len())
        })
    }
}

impl PartialOrd for SemVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `version` satisfies the semver range `range`: comparators
/// (`>=`, `>`, `<=`, `<`, `=`, or a bare version for `=`) separated by
/// commas or spaces, all of which must hold; `*` matches every version.
/// Versions compare by plain semver precedence, pre-releases included:
/// `0.4.0-alpha.7` satisfies `>=0.4.0-alpha.0` and `>=0.3.0-alpha.0` but not
/// `>=0.4.0` (unlike Cargo's rule, a pre-release may match a comparator of
/// another version). `None` when `range` or `version` does not read.
pub fn version_satisfies(range: &str, version: &str) -> Option<bool> {
    let version = SemVersion::parse(version)?;
    let mut comparators = 0;
    for comparator in range
        .split([',', ' '])
        .filter(|comparator| !comparator.is_empty())
    {
        comparators += 1;
        if comparator == "*" {
            continue;
        }
        let (operator, bound) = ["<=", ">=", "<", ">", "="]
            .iter()
            .find_map(|operator| {
                comparator
                    .strip_prefix(operator)
                    .map(|bound| (*operator, bound))
            })
            .unwrap_or(("=", comparator));
        let bound = SemVersion::parse(bound.trim())?;
        let holds = match operator {
            "<=" => version <= bound,
            ">=" => version >= bound,
            "<" => version < bound,
            ">" => version > bound,
            _ => version == bound,
        };
        if !holds {
            return Some(false);
        }
    }
    (comparators > 0).then_some(true)
}

#[cfg(test)]
mod tests;
