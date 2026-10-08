//! Human display names for runnable model keys, projected from the runtime
//! snapshot for presentation only.
//!
//! Attribution stays keyed by the frozen `provider/model-id`; the name is a
//! render-time decoration looked up here. A key with no known name (an old
//! session replaying a model that left the catalog) or whose name merely
//! repeats its id renders as the bare id, never blank.

use std::collections::BTreeMap;

use cookie_agent_protocol::{ModelKey, ModelSelection, RuntimeSnapshotV1};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelDisplayNames {
    /// Only names that add information over the id.
    names: BTreeMap<ModelKey, String>,
    /// Bumped whenever `names` changes, so render caches keyed by it
    /// invalidate exactly when a visible label could differ.
    revision: u64,
}

impl ModelDisplayNames {
    /// Replace the names with those of `snapshot`, bumping the revision only
    /// when the resulting map differs.
    pub(super) fn update(&mut self, snapshot: &RuntimeSnapshotV1) {
        let names = snapshot_names(snapshot);
        if names != self.names {
            self.names = names;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The display name for `key`, when one is known and differs from its id.
    pub fn name(&self, key: &ModelKey) -> Option<&str> {
        self.names.get(key).map(String::as_str)
    }

    /// The composer label: the display name, or the id when none is known.
    pub fn short_label(&self, key: &ModelKey) -> String {
        self.name(key)
            .map_or_else(|| key.to_string(), ToOwned::to_owned)
    }

    /// The attribution label: `short_label`, then ` • <variant>` for a named
    /// variant. Exact base behavior shows no variant.
    pub fn selection_label(&self, selection: &ModelSelection) -> String {
        let label = self.short_label(&selection.model);
        match &selection.variant {
            Some(variant) => format!("{label} • {variant}"),
            None => label,
        }
    }
}

/// The variant retained in a selection, rendered as `base` when the
/// selection is exact base behavior.
pub fn variant_label(selection: &ModelSelection) -> String {
    selection
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), ToString::to_string)
}

fn snapshot_names(snapshot: &RuntimeSnapshotV1) -> BTreeMap<ModelKey, String> {
    let mut names = BTreeMap::new();
    // Unavailable models keep their names so a model that dropped out of
    // the runnable set still reads by name; runnable entries win on overlap.
    for provider in &snapshot.providers {
        for model in &provider.unavailable_models {
            let Ok(key) = ModelKey::new(provider.id.clone(), model.id.clone()) else {
                continue;
            };
            if let Some(name) = distinct_name(&key, model.display_name.as_str()) {
                names.insert(key, name);
            }
        }
    }
    for model in &snapshot.models {
        match distinct_name(&model.key, &model.display_name) {
            Some(name) => {
                names.insert(model.key.clone(), name);
            }
            None => {
                names.remove(&model.key);
            }
        }
    }
    names
}

/// `name` trimmed, unless it is empty or just repeats the key or its bare
/// model id.
fn distinct_name(key: &ModelKey, name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name != key.as_str() && name != key.model_id().as_str())
        .then(|| name.to_owned())
}
