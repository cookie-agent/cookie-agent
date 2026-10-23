//! The Memory view's project folders: the shared rows
//! (`cookie_agent_client_state::memory`) with each project a folder that
//! folds, folded until this browser opened it, and what deleting a
//! project deletes: its memories, never the other files in its folder.

use cookie_agent_client_state::memory::{MemoryRow, memory_rows};
use cookie_agent_protocol::{MemoryLayer, MemoryListResult};

/// The `localStorage` key of whether `project`'s folder is open.
pub fn folder_key(project: &str) -> String {
    format!("cookie.memory_project:{project}")
}

/// Whether the value stored under [`folder_key`] opens the folder: only
/// `open` does, so a folder starts folded.
pub fn open_from_storage(stored: Option<&str>) -> bool {
    stored == Some("open")
}

/// One line of the view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryLine {
    /// A shared row, shown as it is; boxed, as a row holds a whole listing
    /// entry.
    Row(Box<MemoryRow>),
    /// A project's folder: its name, how many memories it holds, and
    /// whether it is open, which shows them under it.
    Project {
        name: String,
        memories: usize,
        open: bool,
    },
}

/// The lines of `listing`: the shared rows, with each project's group a
/// folder whose memories show only while `open(project)` says so.
pub fn memory_lines(listing: &MemoryListResult, open: impl Fn(&str) -> bool) -> Vec<MemoryLine> {
    let mut lines = Vec::new();
    let mut folded = false;
    for row in memory_rows(listing) {
        match row {
            MemoryRow::Group {
                layer: MemoryLayer::Project,
                name,
            } => {
                let shown = open(&name);
                folded = !shown;
                lines.push(MemoryLine::Project {
                    memories: project_memories(listing, &name).len(),
                    name,
                    open: shown,
                });
            }
            MemoryRow::Memory(memory) if folded && memory.layer == MemoryLayer::Project => {}
            row => {
                if !matches!(row, MemoryRow::Memory(_)) {
                    folded = false;
                }
                lines.push(MemoryLine::Row(Box::new(row)));
            }
        }
    }
    lines
}

/// Whether `project`'s folder holds `url`, the memory the view was opened
/// at, which shows it open whatever this browser keeps.
pub fn holds(project: &str, url: &str) -> bool {
    url.strip_prefix("project://")
        .and_then(|rest| rest.strip_prefix(project))
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The memories of `project` a delete of the project sends `memory.delete`
/// for: each URL with the revision it was listed at.
pub fn project_memories(listing: &MemoryListResult, project: &str) -> Vec<(String, String)> {
    listing
        .memories
        .iter()
        .filter(|memory| {
            memory.layer == MemoryLayer::Project && memory.group.as_deref() == Some(project)
        })
        .map(|memory| (memory.url.clone(), memory.revision.clone()))
        .collect()
}

/// How many files of `project`'s folder are not memories: they stay when
/// the project's memories are deleted, and so does the folder.
pub fn project_strays(listing: &MemoryListResult, project: &str) -> usize {
    let folder = format!("projects/{project}/");
    listing
        .ignored
        .iter()
        .filter(|entry| entry.path.starts_with(&folder))
        .count()
}

/// What the status line says once a project's delete is done: `deleted`
/// of `total` memories went, and `failure` says why the first that did not
/// was kept.
pub fn project_delete_status(
    project: &str,
    deleted: usize,
    total: usize,
    failure: Option<&str>,
) -> String {
    let memories = |count: usize| {
        if count == 1 {
            "1 memory".to_owned()
        } else {
            format!("{count} memories")
        }
    };
    match failure {
        None => format!("Deleted project {project}: {}.", memories(deleted)),
        Some(failure) => format!(
            "Deleted {deleted} of {} of project {project}; {failure}",
            memories(total)
        ),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_client_state::memory::MemoryRow;
    use cookie_agent_protocol::MemoryListResult;

    use super::*;

    fn listing() -> MemoryListResult {
        serde_json::from_value(serde_json::json!({
            "memories": [
                {"url": "user://style", "layer": "user", "revision": "u1", "size": 1, "description": "style"},
                {"url": "project://shop/build", "layer": "project", "group": "shop",
                 "revision": "r1", "size": 10, "description": "how to build"},
                {"url": "project://shop/test", "layer": "project", "group": "shop",
                 "revision": "r2", "size": 10, "description": "how to test"},
                {"url": "project://web/deploy", "layer": "project", "group": "web",
                 "revision": "r3", "size": 10, "description": "how to deploy"}
            ],
            "ignored": [
                {"path": "projects/shop/notes.txt", "reason": "not a .md file"},
                {"path": "projects/shopping/x.txt", "reason": "not a .md file"}
            ]
        }))
        .expect("listing")
    }

    fn shown(lines: &[MemoryLine]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|line| match line {
                MemoryLine::Row(row) => match &**row {
                    MemoryRow::Memory(memory) => Some(memory.url.clone()),
                    _ => None,
                },
                MemoryLine::Project { name, open, .. } => {
                    Some(format!("{name} {}", if *open { "open" } else { "folded" }))
                }
            })
            .collect()
    }

    #[test]
    fn projects_start_folded_and_open_one_at_a_time() {
        let listing = listing();
        assert_eq!(
            shown(&memory_lines(&listing, |_| false)),
            ["user://style", "shop folded", "web folded"]
        );
        assert_eq!(
            shown(&memory_lines(&listing, |name| name == "shop")),
            [
                "user://style",
                "shop open",
                "project://shop/build",
                "project://shop/test",
                "web folded"
            ]
        );
        assert!(
            memory_lines(&listing, |_| false).contains(&MemoryLine::Project {
                name: "shop".into(),
                memories: 2,
                open: false,
            })
        );
    }

    #[test]
    fn a_folder_holds_only_its_own_projects_memories() {
        assert!(holds("shop", "project://shop/build"));
        assert!(!holds("shop", "project://shopping/build"));
        assert!(!holds("shop", "user://shop/build"));
        assert!(!holds("shop", "project://shop"));
    }

    #[test]
    fn a_folder_opens_only_once_open_is_stored() {
        assert!(open_from_storage(Some("open")));
        assert!(!open_from_storage(Some("folded")));
        assert!(!open_from_storage(None));
        assert_eq!(folder_key("shop"), "cookie.memory_project:shop");
    }

    #[test]
    fn deleting_a_project_deletes_its_memories_and_keeps_its_other_files() {
        let listing = listing();
        assert_eq!(
            project_memories(&listing, "shop"),
            [
                ("project://shop/build".to_owned(), "r1".to_owned()),
                ("project://shop/test".to_owned(), "r2".to_owned())
            ]
        );
        assert_eq!(
            project_strays(&listing, "shop"),
            1,
            "shopping/ is another folder"
        );
        assert_eq!(project_strays(&listing, "web"), 0);
        assert_eq!(
            project_delete_status("shop", 2, 2, None),
            "Deleted project shop: 2 memories."
        );
        assert_eq!(
            project_delete_status("shop", 1, 2, Some("project://shop/test changed")),
            "Deleted 1 of 2 memories of project shop; project://shop/test changed"
        );
    }
}
