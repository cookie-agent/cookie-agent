//! An ACP v2 tool call's `diff` content as cookie's diff blocks.
//!
//! A v2 diff lists its file `changes` (`operation` `add`, `delete`,
//! `modify`, `move`, or `copy`, with an absolute `path` and, for a move or
//! copy, `oldPath`) and may carry the change as a `patch` whose `format` is
//! `git_patch`. Each file of the patch becomes a block with its hunks; a
//! change the patch does not cover, or a diff without a patch, becomes a
//! block that names the file and how it changed, without lines.

use cookie_agent_protocol::{
    DiffAsOf, DiffBlock, DiffChange, DiffHunk, DiffLine, DiffLineOp, DisplayBlock,
};
use serde_json::Value;

/// The blocks of one `diff` content item, and the paths it changed. A
/// block names a file under the session's `cwd` relative to it, as
/// cookie's own edits do.
pub(super) fn diff_blocks(content: &Value, cwd: &str) -> (Vec<DisplayBlock>, Vec<String>) {
    let (mut blocks, paths) = diff_blocks_absolute(content);
    let base = format!("{}/", cwd.trim_end_matches('/'));
    for block in &mut blocks {
        if let DisplayBlock::Diff(diff) = block {
            for path in std::iter::once(&mut diff.path).chain(diff.old_path.as_mut()) {
                if let Some(relative) = path.strip_prefix(&base).filter(|rest| !rest.is_empty()) {
                    *path = relative.to_owned();
                }
            }
        }
    }
    (blocks, paths)
}

fn diff_blocks_absolute(content: &Value) -> (Vec<DisplayBlock>, Vec<String>) {
    let changes = content
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Change::of)
        .collect::<Vec<_>>();
    let patch = content
        .get("patch")
        .filter(|patch| patch.get("format").and_then(Value::as_str) == Some("git_patch"))
        .and_then(|patch| patch.get("text"))
        .and_then(Value::as_str)
        .map(parse_patch)
        .unwrap_or_default();
    let mut covered = vec![false; changes.len()];
    let mut blocks = Vec::new();
    let mut paths = Vec::new();
    for file in patch {
        let found = changes.iter().position(|change| change.matches(&file.path));
        let (path, old_path, change) = match found {
            Some(index) => {
                covered[index] = true;
                let change = &changes[index];
                (change.path.clone(), change.old_path.clone(), change.kind)
            }
            None => (file.path.clone(), file.old_path.clone(), file.change),
        };
        paths.push(path.clone());
        blocks.push(block(path, old_path, change, file.hunks));
    }
    for (change, covered) in changes.into_iter().zip(covered) {
        if !covered {
            paths.push(change.path.clone());
            blocks.push(block(change.path, change.old_path, change.kind, Vec::new()));
        }
    }
    (blocks, paths)
}

/// One entry of a diff's `changes`.
struct Change {
    path: String,
    old_path: Option<String>,
    kind: DiffChange,
}

impl Change {
    fn of(value: &Value) -> Option<Self> {
        let path = value.get("path").and_then(Value::as_str)?.to_owned();
        let kind = match value.get("operation").and_then(Value::as_str) {
            Some("add" | "copy") => DiffChange::Created,
            Some("delete") => DiffChange::Deleted,
            Some("move") => DiffChange::Renamed,
            _ => DiffChange::Modified,
        };
        Some(Self {
            path,
            old_path: value
                .get("oldPath")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind,
        })
    }

    /// Whether the patch's (relative) path names this change's file.
    fn matches(&self, patch_path: &str) -> bool {
        self.path == patch_path
            || self
                .path
                .strip_suffix(patch_path)
                .is_some_and(|rest| rest.ends_with('/'))
    }
}

/// One file of a git patch.
struct PatchFile {
    path: String,
    old_path: Option<String>,
    change: DiffChange,
    hunks: Vec<DiffHunk>,
}

/// The files of a `git_patch`, with their hunks.
fn parse_patch(text: &str) -> Vec<PatchFile> {
    let mut files: Vec<PatchFile> = Vec::new();
    let strip = |path: &str| {
        let path = path.trim_end_matches('\t').trim();
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_owned()
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/x b/x`, or, as Claude Code's adapter writes it, the same
            // absolute path twice.
            let path = rest.rsplit_once(" b/").map_or_else(
                || {
                    if !rest.is_char_boundary(rest.len() / 2) {
                        return strip(rest);
                    }
                    let (half, other) = rest.split_at(rest.len() / 2);
                    match other.strip_prefix(' ') {
                        Some(other) if rest.len() % 2 == 1 && half == other => half.to_owned(),
                        _ => strip(rest),
                    }
                },
                |(_, new)| new.to_owned(),
            );
            files.push(PatchFile {
                path,
                old_path: None,
                change: DiffChange::Modified,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("new file mode") {
            file.change = DiffChange::Created;
        } else if line.starts_with("deleted file mode") {
            file.change = DiffChange::Deleted;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.old_path = Some(from.to_owned());
            file.change = DiffChange::Renamed;
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_owned();
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            // The header lines; the paths come from `diff --git`.
        } else if let Some(header) = line.strip_prefix("@@ ") {
            if let Some(hunk) = hunk_header(header) {
                file.hunks.push(hunk);
            }
        } else if let Some(hunk) = file.hunks.last_mut() {
            let (op, text) = match line.chars().next() {
                Some(' ') => (DiffLineOp::Context, &line[1..]),
                Some('+') => (DiffLineOp::Add, &line[1..]),
                Some('-') => (DiffLineOp::Remove, &line[1..]),
                // `\ No newline at end of file`, or anything else.
                _ => continue,
            };
            hunk.lines.push(DiffLine {
                op,
                text: text.to_owned(),
            });
        }
    }
    files
}

/// A hunk from `-a,b +c,d @@`, without its lines yet.
fn hunk_header(header: &str) -> Option<DiffHunk> {
    let ranges = header.split(" @@").next()?;
    let mut parts = ranges.split_whitespace();
    let range = |part: Option<&str>, sign: char| -> Option<(u64, u64)> {
        let part = part?.strip_prefix(sign)?;
        let (start, lines) = part.split_once(',').unwrap_or((part, "1"));
        Some((start.parse().ok()?, lines.parse().ok()?))
    };
    let (old_start, old_lines) = range(parts.next(), '-')?;
    let (new_start, new_lines) = range(parts.next(), '+')?;
    Some(DiffHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
    })
}

fn block(
    path: String,
    old_path: Option<String>,
    change: DiffChange,
    hunks: Vec<DiffHunk>,
) -> DisplayBlock {
    let count = |op: DiffLineOp| {
        hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.op == op)
            .count() as u64
    };
    let (added, removed) = (count(DiffLineOp::Add), count(DiffLineOp::Remove));
    let mut block = DisplayBlock::Diff(DiffBlock {
        path,
        old_path,
        language: None,
        change,
        hunks,
        added,
        removed,
        omitted: None,
        as_of: DiffAsOf::Execute,
        full: None,
    });
    block.sanitize();
    block
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::diff_blocks;
    use cookie_agent_protocol::{DiffChange, DiffLineOp, DisplayBlock};

    const PATCH: &str = "diff --git a/src/notes.txt b/src/notes.txt
index 1..2 100644
--- a/src/notes.txt
+++ b/src/notes.txt
@@ -1,2 +1,2 @@
 one
-two
+three
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
\\ No newline at end of file
";

    #[test]
    fn a_patch_becomes_a_block_per_file_with_the_changes_absolute_paths() {
        let (blocks, paths) = diff_blocks(
            &json!({
                "type": "diff",
                "changes": [
                    {"operation": "modify", "path": "/work/src/notes.txt"},
                    {"operation": "add", "path": "/work/new.txt"},
                    {"operation": "delete", "path": "/work/gone.txt"},
                ],
                "patch": {"format": "git_patch", "text": PATCH},
            }),
            "/elsewhere",
        );
        assert_eq!(
            paths,
            ["/work/src/notes.txt", "/work/new.txt", "/work/gone.txt"]
        );
        let diffs = blocks
            .iter()
            .map(|block| match block {
                DisplayBlock::Diff(diff) => diff,
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(diffs[0].change, DiffChange::Modified);
        assert_eq!((diffs[0].added, diffs[0].removed), (1, 1));
        let ops = diffs[0].hunks[0]
            .lines
            .iter()
            .map(|line| line.op)
            .collect::<Vec<_>>();
        assert_eq!(
            ops,
            [DiffLineOp::Context, DiffLineOp::Remove, DiffLineOp::Add]
        );
        assert_eq!(diffs[1].change, DiffChange::Created);
        assert_eq!(
            diffs[1].hunks[0].lines.len(),
            1,
            "the no-newline marker is no line"
        );
        assert_eq!(diffs[2].change, DiffChange::Deleted);
        assert!(diffs[2].hunks.is_empty(), "the patch does not cover it");
    }

    #[test]
    fn a_diff_without_a_patch_names_its_files() {
        let (blocks, paths) = diff_blocks(
            &json!({
                "type": "diff",
                "changes": [{"operation": "move", "path": "/w/b.txt", "oldPath": "/w/a.txt"}],
            }),
            "/elsewhere",
        );
        assert_eq!(paths, ["/w/b.txt"]);
        let [DisplayBlock::Diff(diff)] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert_eq!(diff.change, DiffChange::Renamed);
        assert_eq!(diff.old_path.as_deref(), Some("/w/a.txt"));
    }

    #[test]
    fn claude_codes_absolute_patch_is_one_file_shown_from_the_cwd() {
        let patch = "diff --git /work/src/a.txt /work/src/a.txt\n--- /work/src/a.txt\n+++ /work/src/a.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let (blocks, paths) = diff_blocks(
            &json!({
                "type": "diff",
                "changes": [{"operation": "modify", "path": "/work/src/a.txt"}],
                "patch": {"format": "git_patch", "text": patch},
            }),
            "/work/",
        );
        assert_eq!(paths, ["/work/src/a.txt"]);
        let [DisplayBlock::Diff(diff)] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert_eq!(diff.path, "src/a.txt");
        assert_eq!((diff.added, diff.removed), (1, 1));
    }
}
