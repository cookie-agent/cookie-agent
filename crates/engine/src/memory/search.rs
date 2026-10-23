//! Memory search (`docs/site/specs/memory.md` sections 16.9.3 and 9.3).
//!
//! The query is split into terms on whitespace. A memory is matched
//! against its keywords, its description, and the segments of its URL,
//! split on `://` and `/`; bodies are never searched. A memory scores, in
//! this order of weight: the keywords that occur in the query, then the
//! terms that occur in its description or one of its URL segments. All
//! matching is case-insensitive substring matching. Scores compare by the
//! first count, then the second; memories that score zero are left out, and
//! ties go to the more recently modified memory. Files are read on every
//! search; invalid ones are skipped.

use std::{cmp::Reverse, collections::BTreeSet};

use jiff::Timestamp;

use super::{
    MemoryError, MemoryRoot,
    file::MemoryDocument,
    store::{ListScope, MemoryEntry},
    url::{MemoryLayer, MemoryUrl},
};

/// How much of a match the caller may see, decided per URL by its `memory`
/// permission rules (section 9.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Visibility {
    /// URL, description, keywords, and what the caller may do with it.
    Full,
    /// URL, description, and what the caller may do with it; reading it
    /// asks first.
    Summary,
    /// Left out, and not counted against the limit.
    Hidden,
}

/// One search result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchHit {
    pub(crate) url: MemoryUrl,
    pub(crate) description: String,
    /// The memory's keywords; empty for a [`Visibility::Summary`] hit.
    pub(crate) keywords: Vec<String>,
    /// When the memory was last modified.
    pub(crate) modified: Timestamp,
    /// Whether the caller may read it without asking.
    pub(crate) full: bool,
}

/// A memory's score, compared field by field.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Score {
    keywords: usize,
    names: usize,
}

struct Query {
    text: String,
    terms: Vec<String>,
}

/// The segments of `url`, split on `://` and `/`: its scheme, then its
/// group and name (section 16.9.3).
pub(crate) fn url_segments(url: &MemoryUrl) -> Vec<String> {
    let text = url.to_string();
    let (scheme, rest) = text.split_once("://").unwrap_or((text.as_str(), ""));
    std::iter::once(scheme)
        .chain(rest.split('/'))
        .map(str::to_owned)
        .collect()
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
        let mut names = url_segments(url);
        names.push(document.description().to_lowercase());
        Score {
            keywords,
            names: self
                .terms
                .iter()
                .filter(|term| names.iter().any(|name| name.contains(term.as_str())))
                .count(),
        }
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
        mut visibility: impl FnMut(&MemoryUrl) -> Visibility,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let query = Query::new(query);
        if query.terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let scope = layer.map_or(ListScope::All, ListScope::Layer);
        let mut ranked = self
            .list(&scope)?
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
        for (_, modified, url, document) in ranked {
            let full = match visibility(&url) {
                Visibility::Hidden => continue,
                Visibility::Summary => false,
                Visibility::Full => true,
            };
            hits.push(SearchHit {
                url,
                description: document.description().to_owned(),
                keywords: if full {
                    document.keywords().to_vec()
                } else {
                    Vec::new()
                },
                modified,
                full,
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
    fn urls_split_into_their_segments() {
        for (url, segments) in [
            ("user://preferences", vec!["user", "preferences"]),
            ("agent://reviewer", vec!["agent", "reviewer"]),
            (
                "project://cookie-agent/release",
                vec!["project", "cookie-agent", "release"],
            ),
            ("machine://labpc/cuda", vec!["machine", "labpc", "cuda"]),
        ] {
            assert_eq!(url_segments(&MemoryUrl::parse(url).unwrap()), segments);
        }
    }

    #[test]
    fn keywords_outrank_descriptions_and_url_segments_and_bodies_never_match() {
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
        let memory = MemoryRoot::new(root);
        let hits = memory.search("CARGO clippy", None, 10, all).unwrap();
        // The keyword hit first; one segment or description hit each tie
        // on score and go by URL at equal times; the body is not searched.
        assert_eq!(
            urls(&hits),
            [
                "project://web/keyword",
                "project://cargo/other",
                "project://web/named",
            ]
        );
    }

    #[test]
    fn more_terms_rank_higher_and_ties_go_to_newer_memories() {
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
        // A stamp wins over the file's time.
        put(
            root,
            "machines/labpc/stamped.md",
            "---\ndescription: gpu notes\nmodified: 2001-09-09T01:46:40Z\n---\n",
            5_000,
        );
        let memory = MemoryRoot::new(root);
        let hits = memory.search("gpu driver notes", None, 10, all).unwrap();
        assert_eq!(
            urls(&hits),
            [
                "machine://labpc/both",
                "machine://labpc/stamped",
                "machine://labpc/new",
                "machine://labpc/old"
            ]
        );
    }

    #[test]
    fn a_memory_matches_by_its_url_segments_and_the_layer_narrows() {
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
        let hits = memory.search("reviewer", None, 10, all).unwrap();
        assert_eq!(hits.len(), 2);
        let hits = memory
            .search("reviewer", Some(MemoryLayer::Agent), 10, all)
            .unwrap();
        assert_eq!(urls(&hits), ["agent://reviewer"]);
        let hits = memory.search("machine", None, 10, all).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn zero_scores_and_invalid_files_are_left_out() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        put(
            root,
            "projects/web/a.md",
            &memory("Alpha", "one", "zeta\n"),
            0,
        );
        put(root, "projects/web/broken.md", "no frontmatter alpha\n", 0);
        let memory = MemoryRoot::new(root);
        assert!(memory.search("zeta", None, 10, all).unwrap().is_empty());
        assert!(memory.search("   ", None, 10, all).unwrap().is_empty());
        let hits = memory.search("alpha", None, 10, all).unwrap();
        assert_eq!(urls(&hits), ["project://web/a"]);
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
            .search("build", None, 2, |url| {
                asked.push(url.to_string());
                match url.layer() {
                    MemoryLayer::Agent => Visibility::Hidden,
                    MemoryLayer::User => Visibility::Summary,
                    _ => Visibility::Full,
                }
            })
            .unwrap();
        assert_eq!(urls(&hits), ["user://profile", "project://web/a"]);
        assert!(!hits[0].full && hits[0].keywords.is_empty());
        assert_eq!(hits[0].description, "build");
        assert!(hits[1].full);
        assert_eq!(hits[1].keywords, ["build"]);
        assert_eq!(
            hits[1].modified,
            Timestamp::from_second(1_000_000_000 - 2).unwrap()
        );
        // Asked in rank order, and no further than the limit needs.
        assert_eq!(
            asked,
            ["agent://reviewer", "user://profile", "project://web/a"]
        );
    }
}
