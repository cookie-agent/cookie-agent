//! The tool groups and project skills each runtime capability offered the
//! last time it ran (`docs/site/specs/session-tool-groups.md` section 4.4),
//! kept in `<data_root>/runtime_groups.json` so that a runtime that is not
//! running still shows what it brings.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{RuntimeSkill, RuntimeToolGroup};
use serde::{Deserialize, Serialize};

/// The file's name in the data root.
pub(crate) const FILE: &str = "runtime_groups.json";
/// The most workspaces one runtime capability keeps; the oldest go first.
const MAX_WORKSPACES: usize = 16;
/// How often one instance's groups are taken again while it runs.
const RETAKE: std::time::Duration = std::time::Duration::from_secs(60);

/// One running instance: runtime, capability, workspace, and generation.
type InstanceKey = (String, String, Option<String>, u64);

/// What one capability of one runtime offered at one workspace.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Entry {
    pub(crate) runtime: String,
    pub(crate) capability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    pub(crate) seen: jiff::Timestamp,
    pub(crate) groups: Vec<RuntimeToolGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skills: Vec<RuntimeSkill>,
}

#[derive(Default, Deserialize, Serialize)]
struct File {
    version: u32,
    entries: Vec<Entry>,
}

/// The cache, loaded at start and written when what it holds changes.
pub(crate) struct RuntimeGroupsCache {
    path: Option<PathBuf>,
    entries: Mutex<Vec<Entry>>,
    /// When each instance's groups were last taken, by (runtime,
    /// capability, workspace, generation).
    taken: Mutex<HashMap<InstanceKey, std::time::Instant>>,
}

impl RuntimeGroupsCache {
    /// The cache of `data_dir`; an unreadable file starts it empty.
    pub(crate) fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: Mutex::new(entries),
            taken: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the instance of `generation` is due to be taken again.
    pub(crate) fn due(
        &self,
        runtime: &str,
        capability: &str,
        workspace: Option<&str>,
        generation: u64,
    ) -> bool {
        let key = (
            runtime.to_owned(),
            capability.to_owned(),
            workspace.map(str::to_owned),
            generation,
        );
        let mut taken = lock(&self.taken);
        let now = std::time::Instant::now();
        if taken
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < RETAKE)
        {
            return false;
        }
        taken.insert(key, now);
        true
    }

    /// Records what a running instance offers now; writes the file when it
    /// changed.
    pub(crate) fn record(&self, entry: Entry) {
        let mut entries = lock(&self.entries);
        let same = |known: &Entry| {
            known.runtime == entry.runtime
                && known.capability == entry.capability
                && known.workspace == entry.workspace
        };
        if entries
            .iter()
            .find(|known| same(known))
            .is_some_and(|known| known.groups == entry.groups && known.skills == entry.skills)
        {
            return;
        }
        entries.retain(|known| !same(known));
        entries.push(entry.clone());
        // At most MAX_WORKSPACES per runtime capability, the newest kept.
        let mut kept = entries
            .iter()
            .filter(|known| known.runtime == entry.runtime && known.capability == entry.capability)
            .map(|known| known.seen)
            .collect::<Vec<_>>();
        if kept.len() > MAX_WORKSPACES {
            kept.sort_unstable_by(|left, right| right.cmp(left));
            let oldest = kept[MAX_WORKSPACES - 1];
            entries.retain(|known| {
                known.runtime != entry.runtime
                    || known.capability != entry.capability
                    || known.seen >= oldest
            });
        }
        self.write(&entries);
    }

    /// What `runtime`'s `capability` offered at `workspace`, else at the
    /// workspace it ran at last; with whether it was that workspace.
    pub(crate) fn lookup(
        &self,
        runtime: &str,
        capability: &str,
        workspace: Option<&str>,
    ) -> Option<(Entry, bool)> {
        let entries = lock(&self.entries);
        let of = entries
            .iter()
            .filter(|entry| entry.runtime == runtime && entry.capability == capability);
        if let Some(exact) = of
            .clone()
            .find(|entry| entry.workspace.as_deref() == workspace)
        {
            return Some((exact.clone(), true));
        }
        of.max_by_key(|entry| entry.seen)
            .map(|entry| (entry.clone(), false))
    }

    /// Drops what the cache holds of runtimes `known` does not name.
    pub(crate) fn retain(&self, known: impl Fn(&str) -> bool) {
        let mut entries = lock(&self.entries);
        let before = entries.len();
        entries.retain(|entry| known(&entry.runtime));
        if entries.len() != before {
            self.write(&entries);
        }
    }

    fn write(&self, entries: &[Entry]) {
        let Some(path) = &self.path else {
            return;
        };
        let file = File {
            version: 1,
            entries: entries.to_vec(),
        };
        if let Err(error) = crate::native_compaction::write_atomic(path, &file) {
            eprintln!("cookie-agent: could not write {}: {error}", path.display());
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{ToolGroupKind, ToolGroupState};

    use super::*;

    fn entry(workspace: &str, seen: i64, tool: &str) -> Entry {
        Entry {
            runtime: "labpc".into(),
            capability: "coding".into(),
            workspace: Some(workspace.into()),
            seen: jiff::Timestamp::from_second(seen).unwrap(),
            groups: vec![RuntimeToolGroup {
                selector: "coding@labpc".parse().unwrap(),
                kind: ToolGroupKind::Builtin,
                name: "coding".into(),
                summary: None,
                tools: vec![tool.into()],
                token_estimate: 1,
                default: ToolGroupState::On,
            }],
            skills: Vec::new(),
        }
    }

    #[test]
    fn a_workspace_seen_before_wins_else_the_latest_one_answers() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RuntimeGroupsCache::open(directory.path());
        cache.record(entry("/srv/api", 10, "read"));
        cache.record(entry("/srv/web", 20, "bash"));
        let (found, exact) = cache.lookup("labpc", "coding", Some("/srv/api")).unwrap();
        assert!(exact);
        assert_eq!(found.groups[0].tools, ["read"]);
        let (found, exact) = cache.lookup("labpc", "coding", Some("/srv/new")).unwrap();
        assert!(!exact);
        assert_eq!(found.workspace.as_deref(), Some("/srv/web"));
        assert!(cache.lookup("gpu-box", "coding", None).is_none());
        // It survives a restart.
        let reopened = RuntimeGroupsCache::open(directory.path());
        assert!(
            reopened
                .lookup("labpc", "coding", Some("/srv/api"))
                .is_some()
        );
        // And goes with its runtime.
        reopened.retain(|runtime| runtime != "labpc");
        assert!(reopened.lookup("labpc", "coding", None).is_none());
    }

    #[test]
    fn at_most_sixteen_workspaces_are_kept_the_newest_first() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RuntimeGroupsCache::open(directory.path());
        for index in 0..20 {
            cache.record(entry(&format!("/w/{index}"), index, "read"));
        }
        assert!(cache.lookup("labpc", "coding", Some("/w/19")).unwrap().1);
        assert!(!cache.lookup("labpc", "coding", Some("/w/0")).unwrap().1);
    }
}
