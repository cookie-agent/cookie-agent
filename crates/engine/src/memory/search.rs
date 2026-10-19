//! Memory search (`docs/site/specs/memory.md` sections 8.1 and 9.3).
//!
//! The query is split into terms on whitespace. A memory scores, in this
//! order of weight: the keywords that occur in the query; the terms that
//! occur in its name, its project or machine name, or its description; the
//! terms that occur in its body. All matching is case-insensitive substring
//! matching. Scores compare by the first count, then the second, then the
//! third; memories that score zero are left out, and ties go to the more
//! recently modified file. Files are read on every search; invalid ones are
//! skipped.

use std::{cmp::Reverse, collections::BTreeSet};

use super::{
    MemoryError, MemoryRoot,
    file::MemoryDocument,
    store::{ListScope, MemoryEntry},
    url::{MemoryLayer, MemoryUrl},
};

/// Body lines a hit shows at most.
pub(crate) const MAX_HIT_LINES: usize = 3;

/// How much of a match the caller may see, decided per URL by its `memory`
/// permission rules (section 9.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Visibility {
    /// URL, description, keywords, and matching body lines.
    Full,
    /// URL and description only.
    Summary,
    /// Left out, and not counted against the limit.
    Hidden,
}

/// One search result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchHit {
    pub(crate) url: MemoryUrl,
    pub(crate) description: String,
    /// Present for [`Visibility::Full`] hits.
    pub(crate) detail: Option<HitDetail>,
}

/// What a fully visible hit adds to its URL and description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HitDetail {
    pub(crate) keywords: Vec<String>,
    /// Up to [`MAX_HIT_LINES`] body lines holding a query term, in order.
    pub(crate) lines: Vec<String>,
}

/// A memory's score, compared field by field.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Score {
    keywords: usize,
    names: usize,
    body: usize,
}

struct Query {
    text: String,
    terms: Vec<String>,
}

impl Query {
    fn new(query: &str) -> Self {
        let text = query.to_lowercase();
        let terms = text
            .split_whitespace()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(str::to_owned)
            .collect();
        Self { text, terms }
    }

    fn score(&self, url: &MemoryUrl, document: &MemoryDocument) -> Score {
        let keywords = document
            .keywords()
            .iter()
            .map(|keyword| keyword.to_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|keyword| self.text.contains(keyword.as_str()))
            .count();
        let names = [
            url.name().to_owned(),
            url.group().unwrap_or_default().to_owned(),
            document.description().to_lowercase(),
        ];
        let body = document.body().to_lowercase();
        Score {
            keywords,
            names: self.count(|term| names.iter().any(|name| name.contains(term))),
            body: self.count(|term| body.contains(term)),
        }
    }

    fn count(&self, matches: impl Fn(&str) -> bool) -> usize {
        self.terms.iter().filter(|term| matches(term)).count()
    }

    fn lines(&self, body: &str) -> Vec<String> {
        body.lines()
            .filter(|line| {
                let line = line.to_lowercase();
                self.terms.iter().any(|term| line.contains(term.as_str()))
            })
            .take(MAX_HIT_LINES)
            .map(|line| line.trim_end().to_owned())
            .collect()
    }
}

impl MemoryRoot {
    /// The best `limit` memories for `query`, in `layer` or in all layers,
    /// that `visibility` lets the caller see. `visibility` is asked about
    /// candidates in rank order until `limit` hits are found.
    pub(crate) fn search(
        &self,
        query: &str,
        layer: Option<MemoryLayer>,
        limit: usize,
        max_file_bytes: usize,
        mut visibility: impl FnMut(&MemoryUrl) -> Visibility,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let query = Query::new(query);
        if query.terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let scope = layer.map_or(ListScope::All, ListScope::Layer);
        let mut ranked = self
            .list(&scope, max_file_bytes)?
            .entries
            .into_iter()
            .filter_map(|entry| {
                let MemoryEntry {
                    url,
                    modified,
                    document,
                    ..
                } = entry;
                let document = document.ok()?;
                let score = query.score(&url, &document);
                (score != Score::default()).then_some((score, modified, url, document))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            (Reverse(left.0), Reverse(left.1), &left.2).cmp(&(
                Reverse(right.0),
                Reverse(right.1),
                &right.2,
            ))
        });
        let mut hits = Vec::new();
        for (_, _, url, document) in ranked {
            let detail = match visibility(&url) {
                Visibility::Hidden => continue,
                Visibility::Summary => None,
                Visibility::Full => Some(HitDetail {
                    keywords: document.keywords().to_vec(),
                    lines: query.lines(document.body()),
                }),
            };
            hits.push(SearchHit {
                url,
                description: document.description().to_owned(),
                detail,
            });
            if hits.len() == limit {
                break;
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{Duration, SystemTime},
    };

    use super::*;

    const LIMIT: usize = 4096;

    fn put(root: &Path, path: &str, text: &str, age_seconds: u64) {
        let path = root.join("memory").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 - age_seconds),
        )
        .unwrap();
    }

    fn memory(description: &str, keywords: &str, body: &str) -> String {
        format!("---\ndescription: {description}\nkeywords: [{keywords}]\n---\n{body}")
    }

    fn urls(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|hit| hit.url.to_string()).collect()
    }

    fn all(_: &MemoryUrl) -> Visibility {
        Visibility::Full
    }

    #[test]
    fn keywords_outrank_names_and_descriptions_which_outrank_bodies() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(
            root,
            "projects/web/body.md",
            &memory("Notes", "x", "uses cargo and clippy\n"),
            0,
        );
        put(
            root,
            "projects/web/named.md",
            &memory("cargo setup", "x", ""),
            0,
        );
        put(
            root,
            "projects/cargo/other.md",
            &memory("Other", "x", ""),
            0,
        );
        put(
            root,
            "projects/web/keyword.md",
            &memory("Build", "Cargo", ""),
            0,
        );
        put(
            root,
            "user/profile.md",
            &memory("Who", "z", "nothing here\n"),
            0,
        );
        let memory = MemoryRoot::new(root);
        let hits = memory.search("CARGO clippy", None, 10, LIMIT, all).unwrap();
        // The keyword hit first; one name hit each (project name, then
        // description) tie on score and go by URL at equal times; the body
        // hit matches two terms but only in the body.
        assert_eq!(
            urls(&hits),
            [
                "project://web/keyword",
                "project://cargo/other",
                "project://web/named",
                "project://web/body",
            ]
        );
    }

    #[test]
    fn more_terms_in_the_same_tier_rank_higher_and_ties_go_to_newer_files() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(
            root,
            "machines/labpc/old.md",
            &memory("gpu driver", "x", ""),
            300,
        );
        put(
            root,
            "machines/labpc/new.md",
            &memory("gpu notes", "x", ""),
            10,
        );
        put(
            root,
            "machines/labpc/both.md",
            &memory("gpu driver notes", "x", ""),
            900,
        );
        let memory = MemoryRoot::new(root);
        let hits = memory
            .search("gpu driver notes", None, 10, LIMIT, all)
            .unwrap();
        assert_eq!(
            urls(&hits),
            [
                "machine://labpc/both",
                "machine://labpc/new",
                "machine://labpc/old"
            ]
        );
    }

    #[test]
    fn an_agent_file_matches_by_its_agent_id_and_the_layer_narrows() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(root, "agents/reviewer.md", &memory("Lessons", "x", ""), 0);
        put(
            root,
            "projects/reviewer/a.md",
            &memory("Lessons", "x", ""),
            0,
        );
        let memory = MemoryRoot::new(root);
        let hits = memory.search("reviewer", None, 10, LIMIT, all).unwrap();
        assert_eq!(hits.len(), 2);
        let hits = memory
            .search("reviewer", Some(MemoryLayer::Agent), 10, LIMIT, all)
            .unwrap();
        assert_eq!(urls(&hits), ["agent://reviewer"]);
    }

    #[test]
    fn zero_scores_and_invalid_files_are_left_out() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(
            root,
            "projects/web/a.md",
            &memory("Alpha", "one", "two\n"),
            0,
        );
        put(root, "projects/web/broken.md", "no frontmatter alpha\n", 0);
        put(
            root,
            "projects/web/oversized.md",
            &memory("alpha", "x", &"y".repeat(LIMIT)),
            0,
        );
        let memory = MemoryRoot::new(root);
        assert!(
            memory
                .search("zeta", None, 10, LIMIT, all)
                .unwrap()
                .is_empty()
        );
        assert!(
            memory
                .search("   ", None, 10, LIMIT, all)
                .unwrap()
                .is_empty()
        );
        let hits = memory.search("alpha", None, 10, LIMIT, all).unwrap();
        assert_eq!(urls(&hits), ["project://web/a"]);
    }

    #[test]
    fn full_hits_show_keywords_and_up_to_three_matching_lines() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(
            root,
            "projects/web/a.md",
            &memory(
                "Deploy",
                "deploy, ship",
                "intro\nDeploy with make  \nno\nship it\ndeploy again\ndeploy fourth\n",
            ),
            0,
        );
        let memory = MemoryRoot::new(root);
        let hits = memory.search("deploy ship", None, 10, LIMIT, all).unwrap();
        assert_eq!(
            hits[0].detail,
            Some(HitDetail {
                keywords: vec!["deploy".into(), "ship".into()],
                lines: vec![
                    "Deploy with make".into(),
                    "ship it".into(),
                    "deploy again".into()
                ],
            })
        );
    }

    #[test]
    fn visibility_hides_or_trims_hits_before_the_limit_applies() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(root, "agents/reviewer.md", &memory("build", "build", ""), 0);
        put(root, "user/profile.md", &memory("build", "build", ""), 1);
        put(root, "projects/web/a.md", &memory("build", "build", ""), 2);
        put(root, "projects/web/b.md", &memory("build", "build", ""), 3);
        let memory = MemoryRoot::new(root);
        let mut asked = Vec::new();
        let hits = memory
            .search("build", None, 2, LIMIT, |url| {
                asked.push(url.to_string());
                match url.layer() {
                    MemoryLayer::Agent => Visibility::Hidden,
                    MemoryLayer::User => Visibility::Summary,
                    _ => Visibility::Full,
                }
            })
            .unwrap();
        assert_eq!(urls(&hits), ["user://profile", "project://web/a"]);
        assert_eq!(hits[0].detail, None);
        assert_eq!(hits[0].description, "build");
        assert!(hits[1].detail.is_some());
        // Asked in rank order, and no further than the limit needs.
        assert_eq!(
            asked,
            ["agent://reviewer", "user://profile", "project://web/a"]
        );
    }
}
