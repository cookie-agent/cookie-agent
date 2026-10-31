//! Whether a new memory repeats one that exists
//! (`docs/site/specs/memory.md` section 16.9.5). A memory a model creates
//! is first compared with the memories in the same place: the same
//! project, the same machine, the user categories, or the agent's file.
//! The comparison uses descriptions and keywords, lowercased, with filler
//! words left out, and matches as search does (section 16.9.3): no
//! embeddings and no model call.

use std::collections::BTreeSet;

use super::{file::MemoryDocument, url::MemoryUrl};

/// The most near memories a result lists.
pub const MAX_NEAR: usize = 3;

/// Words that say nothing about what a memory holds.
const FILLER: [&str; 48] = [
    "a", "about", "all", "an", "and", "any", "are", "as", "at", "be", "by", "can", "do", "does",
    "for", "from", "has", "have", "how", "if", "in", "into", "is", "it", "its", "no", "not", "of",
    "on", "or", "so", "than", "that", "the", "their", "them", "then", "there", "these", "this",
    "to", "use", "was", "what", "when", "where", "which", "with",
];

/// A memory near a new one, as a result lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NearMemory {
    pub url: MemoryUrl,
    pub description: String,
}

/// How a new memory compares with the memories around it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Similarity {
    /// Nothing near: save it.
    None,
    /// Some memories share a word or a keyword: save it, and list them.
    Weak(Vec<NearMemory>),
    /// Memories share two or more keywords, or most of the description's
    /// words: refuse unless the writer says it is new.
    Strong(Vec<NearMemory>),
    /// This memory has the same body: always refuse.
    Identical(MemoryUrl),
}

/// The lowercased words of `text` that are not filler.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '-')
        .map(|word| word.trim_matches('-').to_lowercase())
        .filter(|word| !word.is_empty() && !FILLER.contains(&word.as_str()))
        .collect()
}

fn keywords(document: &MemoryDocument) -> BTreeSet<String> {
    document
        .keywords()
        .iter()
        .map(|keyword| keyword.trim().to_lowercase())
        .filter(|keyword| !keyword.is_empty() && !FILLER.contains(&keyword.as_str()))
        .collect()
}

/// How `new` compares with `existing`, the other valid memories in its
/// place.
pub fn compare(new: &MemoryDocument, existing: &[(MemoryUrl, MemoryDocument)]) -> Similarity {
    let body = new.body().trim();
    if !body.is_empty()
        && let Some((url, _)) = existing
            .iter()
            .find(|(_, document)| document.body().trim() == body)
    {
        return Similarity::Identical(url.clone());
    }
    let new_keywords = keywords(new);
    let new_words = words(new.description());
    let mut strong = Vec::new();
    let mut weak = Vec::new();
    for (url, document) in existing {
        let shared_keywords = keywords(document).intersection(&new_keywords).count();
        let their_words = words(document.description());
        let shared_words = new_words.intersection(&their_words).count();
        let keyword_words = new_keywords
            .iter()
            .filter(|keyword| their_words.contains(*keyword))
            .count();
        let near = NearMemory {
            url: url.clone(),
            description: document.description().to_owned(),
        };
        let rank = (shared_keywords, shared_words + keyword_words);
        if shared_keywords >= 2 || (!new_words.is_empty() && shared_words * 2 > new_words.len()) {
            strong.push((rank, near));
        } else if rank != (0, 0) {
            weak.push((rank, near));
        }
    }
    let best = |mut found: Vec<((usize, usize), NearMemory)>| {
        found.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.url.cmp(&right.1.url))
        });
        found
            .into_iter()
            .take(MAX_NEAR)
            .map(|(_, near)| near)
            .collect::<Vec<_>>()
    };
    if !strong.is_empty() {
        Similarity::Strong(best(strong))
    } else if !weak.is_empty() {
        Similarity::Weak(best(weak))
    } else {
        Similarity::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(description: &str, keywords: &[&str], body: &str) -> MemoryDocument {
        MemoryDocument::new(
            description,
            keywords
                .iter()
                .map(|keyword| (*keyword).to_owned())
                .collect(),
            body,
        )
        .unwrap()
    }

    fn existing(entries: &[(&str, MemoryDocument)]) -> Vec<(MemoryUrl, MemoryDocument)> {
        entries
            .iter()
            .map(|(url, document)| (MemoryUrl::parse(url).unwrap(), document.clone()))
            .collect()
    }

    fn urls(near: &[NearMemory]) -> Vec<String> {
        near.iter().map(|near| near.url.to_string()).collect()
    }

    #[test]
    fn two_shared_keywords_or_most_description_words_are_a_strong_match() {
        let around = existing(&[
            (
                "project://web/release",
                memory("How releases are cut", &["release", "cargo-dist"], "- tag"),
            ),
            (
                "project://web/ci",
                memory("The CI workflows and their gates", &["ci"], "- gates"),
            ),
        ]);
        let keywords = compare(
            &memory("Publishing", &["Release", "cargo-dist", "x"], "other"),
            &around,
        );
        assert!(
            matches!(&keywords, Similarity::Strong(near) if urls(near) == ["project://web/release"]),
            "{keywords:?}"
        );
        // Filler words do not count: "the", "and", "their" are left out,
        // so two of the three words left ("workflows", "gates") match.
        let words = compare(
            &memory("The workflows and their gates on main", &[], "other"),
            &around,
        );
        assert!(
            matches!(&words, Similarity::Strong(near) if urls(near) == ["project://web/ci"]),
            "{words:?}"
        );
    }

    #[test]
    fn a_shared_word_or_keyword_is_a_weak_match_and_none_is_none() {
        let around = existing(&[(
            "machine://labpc/gpu",
            memory(
                "CUDA toolkit and driver versions",
                &["cuda", "gpu"],
                "- 12.4",
            ),
        )]);
        let weak = compare(
            &memory("Where scratch builds go", &["cuda", "disk"], "- /scratch"),
            &around,
        );
        assert!(
            matches!(&weak, Similarity::Weak(near) if urls(near) == ["machine://labpc/gpu"]),
            "{weak:?}"
        );
        let none = compare(&memory("Shell aliases", &["zsh"], "- alias"), &around);
        assert_eq!(none, Similarity::None);
    }

    #[test]
    fn an_identical_body_is_found_whatever_else_differs() {
        let around = existing(&[(
            "user://feedback",
            memory("Feedback", &[], "  - never push on Fridays\n"),
        )]);
        assert_eq!(
            compare(
                &memory("Unrelated", &[], "- never push on Fridays"),
                &around
            ),
            Similarity::Identical(MemoryUrl::parse("user://feedback").unwrap())
        );
        // Empty bodies are not compared.
        let empty = existing(&[("user://profile", memory("Profile", &[], ""))]);
        assert_eq!(
            compare(&memory("Other", &[], " "), &empty),
            Similarity::None
        );
    }

    #[test]
    fn at_most_three_near_memories_are_listed_best_first() {
        let around = existing(&[
            ("project://web/a", memory("a", &["one"], "a")),
            ("project://web/b", memory("b", &["one", "two"], "b")),
            (
                "project://web/c",
                memory("c", &["one", "two", "three"], "c"),
            ),
            ("project://web/d", memory("d", &["one", "two"], "d")),
            ("project://web/e", memory("e", &["two", "three"], "e")),
        ]);
        let found = compare(&memory("new", &["one", "two", "three"], "n"), &around);
        assert!(
            matches!(&found, Similarity::Strong(near) if urls(near) == ["project://web/c", "project://web/b", "project://web/d"]),
            "{found:?}"
        );
    }
}
