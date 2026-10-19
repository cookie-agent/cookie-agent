use std::path::Path;

use cookie_agent_protocol::{DiffAsOf, DiffChange, DiffHunk, DiffLineOp, DisplayBlock};

use super::{
    diff_block, fit_block, hunks, language_for_path, line_diff_changes, raw_lines,
    replacement_changes,
};

/// `count` lines `line 1` to `line <count>`, each ending in a newline.
fn numbered(count: usize) -> String {
    (1..=count).map(|line| format!("line {line}\n")).collect()
}

/// A hunk as `(old_start, old_lines, new_start, new_lines)` and its lines
/// in unified-diff spelling.
type Rendered = ((u64, u64, u64, u64), Vec<String>);

fn rendered(hunk: &DiffHunk) -> Rendered {
    (
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines,
        ),
        hunk.lines
            .iter()
            .map(|line| {
                let marker = match line.op {
                    DiffLineOp::Context => ' ',
                    DiffLineOp::Add => '+',
                    DiffLineOp::Remove => '-',
                };
                format!("{marker}{}", line.text)
            })
            .collect(),
    )
}

/// The hunks of replacing every occurrence of `old` in `text` with `new`
/// (only the first when `all` is false).
fn edit_hunks(text: &str, old: &str, new: &str, all: bool) -> Vec<Rendered> {
    let mut ranges = text
        .match_indices(old)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect::<Vec<_>>();
    if !all {
        ranges.truncate(1);
    }
    let changes = replacement_changes(text, &ranges, new);
    hunks(&raw_lines(text), &changes)
        .iter()
        .map(rendered)
        .collect()
}

fn context(lines: std::ops::RangeInclusive<usize>) -> Vec<String> {
    lines.map(|line| format!(" line {line}")).collect()
}

fn lines(parts: &[Vec<String>]) -> Vec<String> {
    parts.concat()
}

#[test]
fn a_change_in_the_middle_has_five_lines_of_context_each_side() {
    let hunks = edit_hunks(&numbered(20), "line 10\n", "LINE 10\n", false);
    assert_eq!(
        hunks,
        [(
            (5, 11, 5, 11),
            lines(&[
                context(5..=9),
                vec!["-line 10".into(), "+LINE 10".into()],
                context(11..=15),
            ])
        )]
    );
}

#[test]
fn changes_at_the_start_and_end_of_a_file_have_context_on_one_side() {
    assert_eq!(
        edit_hunks(&numbered(20), "line 1\n", "first\n", false),
        [(
            (1, 6, 1, 6),
            lines(&[vec!["-line 1".into(), "+first".into()], context(2..=6)])
        )]
    );
    assert_eq!(
        edit_hunks(&numbered(20), "line 20\n", "last\n", false),
        [(
            (15, 6, 15, 6),
            lines(&[context(15..=19), vec!["-line 20".into(), "+last".into()]])
        )]
    );
    // Without a final newline the last line's change still ends the file.
    let text = numbered(20).trim_end().to_owned();
    assert_eq!(
        edit_hunks(&text, "line 20", "last", false),
        [(
            (15, 6, 15, 6),
            lines(&[context(15..=19), vec!["-line 20".into(), "+last".into()]])
        )]
    );
}

#[test]
fn replace_all_gives_one_hunk_per_replacement_merging_overlapping_context() {
    let mut text = numbered(30);
    for line in [3, 14, 30] {
        text = text.replace(&format!("line {line}\n"), &format!("line {line} foo\n"));
    }
    // Lines 3 and 14 are 10 lines apart, so their context touches and they
    // share a hunk; line 30 is 15 lines further and gets its own.
    assert_eq!(
        edit_hunks(&text, " foo", " bar", true),
        [
            (
                (1, 19, 1, 19),
                lines(&[
                    context(1..=2),
                    vec!["-line 3 foo".into(), "+line 3 bar".into()],
                    context(4..=13),
                    vec!["-line 14 foo".into(), "+line 14 bar".into()],
                    context(15..=19),
                ])
            ),
            (
                (25, 6, 25, 6),
                lines(&[
                    context(25..=29),
                    vec!["-line 30 foo".into(), "+line 30 bar".into()]
                ])
            ),
        ]
    );
    // Eleven unchanged lines between two changes separate their hunks.
    let mut text = numbered(30);
    for line in [3, 15] {
        text = text.replace(&format!("line {line}\n"), &format!("line {line} foo\n"));
    }
    let hunks = edit_hunks(&text, " foo", " bar", true);
    assert_eq!(
        hunks
            .iter()
            .map(|(numbers, _)| *numbers)
            .collect::<Vec<_>>(),
        [(1, 8, 1, 8), (10, 11, 10, 11)]
    );
}

#[test]
fn replacements_on_one_line_are_one_change() {
    let text = format!("{}a a a\n{}", numbered(2), numbered(2));
    assert_eq!(
        edit_hunks(&text, "a", "b", true),
        [(
            (1, 5, 1, 5),
            vec![
                " line 1".into(),
                " line 2".into(),
                "-a a a".into(),
                "+b b b".into(),
                " line 1".into(),
                " line 2".into(),
            ]
        )]
    );
}

#[test]
fn later_hunks_number_new_lines_after_earlier_changes_moved_them() {
    let mut text = numbered(30);
    for line in [3, 25] {
        text = text.replace(&format!("line {line}\n"), &format!("line {line} foo\n"));
    }
    let hunks = edit_hunks(&text, " foo\n", "\nadded one\nadded two\n", true);
    assert_eq!(
        hunks
            .iter()
            .map(|(numbers, _)| *numbers)
            .collect::<Vec<_>>(),
        [(1, 8, 1, 10), (20, 11, 22, 13)]
    );
    assert_eq!(
        hunks[1].1[5..9],
        [
            "-line 25 foo".to_owned(),
            "+line 25".into(),
            "+added one".into(),
            "+added two".into(),
        ]
    );
}

#[test]
fn lines_the_old_and_new_strings_share_are_context() {
    let hunks = edit_hunks(
        &numbered(20),
        "line 9\nline 10\nline 11\n",
        "line 9\nten\nline 11\n",
        false,
    );
    assert_eq!(
        hunks,
        [(
            (5, 11, 5, 11),
            lines(&[
                context(5..=9),
                vec!["-line 10".into(), "+ten".into()],
                context(11..=15),
            ])
        )]
    );
}

#[test]
fn removing_a_line_break_joins_the_lines_around_it() {
    assert_eq!(
        edit_hunks("a\nb\nc\n", "a\n", "a", false),
        [(
            (1, 3, 1, 2),
            vec!["-a".into(), "-b".into(), "+ab".into(), " c".into()]
        )]
    );
    // Deleting whole lines adds nothing.
    assert_eq!(
        edit_hunks(&numbered(8), "line 4\nline 5\n", "", false),
        [(
            (1, 8, 1, 6),
            lines(&[
                context(1..=3),
                vec!["-line 4".into(), "-line 5".into()],
                context(6..=8),
            ])
        )]
    );
}

#[test]
fn a_line_diff_of_a_whole_file_numbers_and_merges_like_an_edit() {
    let old = numbered(30);
    let new = old
        .replace("line 2\n", "two\n")
        .replace("line 28\n", "line 28\ninserted\n");
    let changes = line_diff_changes(&old, &new);
    let modified = hunks(&raw_lines(&old), &changes)
        .iter()
        .map(rendered)
        .collect::<Vec<_>>();
    assert_eq!(
        modified,
        [
            (
                (1, 7, 1, 7),
                lines(&[
                    context(1..=1),
                    vec!["-line 2".into(), "+two".into()],
                    context(3..=7),
                ])
            ),
            (
                (24, 7, 24, 8),
                lines(&[context(24..=28), vec!["+inserted".into()], context(29..=30)])
            ),
        ]
    );
    // A created file is one hunk of added lines from the top.
    let created = line_diff_changes("", "a\nb\n");
    let created = hunks(&[], &created)
        .iter()
        .map(rendered)
        .collect::<Vec<_>>();
    assert_eq!(created, [((0, 0, 1, 2), vec!["+a".into(), "+b".into()])]);
}

#[test]
fn diff_counts_every_change_and_labels_the_language() {
    let old = numbered(4);
    let new = "line 1\nx\ny\nline 4\n";
    let changes = line_diff_changes(&old, new);
    let diff = diff_block(
        Path::new("src/main.rs"),
        DiffChange::Modified,
        &raw_lines(&old),
        &changes,
        DiffAsOf::Prepare,
    );
    assert_eq!((diff.added, diff.removed), (2, 2));
    assert_eq!(diff.language.as_deref(), Some("rust"));
    assert_eq!(diff.as_of, DiffAsOf::Prepare);
    DisplayBlock::Diff(diff).validate().expect("valid diff");
}

#[test]
fn languages_come_from_the_extension() {
    for (path, language) in [
        ("src/lib.rs", Some("rust")),
        ("tool.PY", Some("python")),
        ("Makefile", Some("makefile")),
        ("notes.weird", Some("weird")),
        ("archive.tar.gz", Some("gz")),
        (".bashrc", None),
        ("README", None),
    ] {
        assert_eq!(
            language_for_path(Path::new(path)).as_deref(),
            language,
            "{path}"
        );
    }
}

#[test]
fn a_cut_preview_counts_what_it_left_out() {
    let old = numbered(5_000);
    let ranges = old
        .match_indices("line")
        .map(|(start, matched)| (start, start + matched.len()))
        .collect::<Vec<_>>();
    let changes = replacement_changes(&old, &ranges, "LINE");
    let diff = diff_block(
        Path::new("big.txt"),
        DiffChange::Modified,
        &raw_lines(&old),
        &changes,
        DiffAsOf::Prepare,
    );
    let DisplayBlock::Diff(cut) = fit_block(DisplayBlock::Diff(diff), None).expect("fit") else {
        panic!("a diff stays a diff");
    };
    assert_eq!((cut.added, cut.removed), (5_000, 5_000));
    assert!(cut.hunks.is_empty());
    assert_eq!(cut.omitted.map(|omitted| omitted.hunks), Some(1));
    assert!(cut.full.is_none());
}
