//! The model menus' search and groups (`docs/site/specs/web-polish.md`,
//! P4, changed 2026-11-16): which models match what is typed, where a
//! match falls in a name, and the models grouped by provider.

/// Whether a model matches `query`: every word of it, ignoring case, is in
/// one of `fields` (its name, provider, ID, description). An empty query
/// matches everything.
pub fn matches(query: &str, fields: &[&str]) -> bool {
    let fields: Vec<String> = fields.iter().map(|field| field.to_lowercase()).collect();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| fields.iter().any(|field| field.contains(&word)))
}

/// Where the first word of `query` falls in `name`, ignoring case, as a
/// byte range of `name` on character boundaries: the part a row marks.
pub fn match_range(name: &str, query: &str) -> Option<(usize, usize)> {
    let word: Vec<char> = query.split_whitespace().next()?.chars().collect();
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    (0..chars.len()).find_map(|start| {
        let fits = word.iter().enumerate().all(|(offset, wanted)| {
            chars
                .get(start + offset)
                .is_some_and(|(_, found)| found.to_lowercase().eq(wanted.to_lowercase()))
        });
        fits.then(|| {
            let from = chars[start].0;
            let to = chars
                .get(start + word.len())
                .map_or(name.len(), |(index, _)| *index);
            (from, to)
        })
    })
}

/// `items` in groups by `provider`, each provider where it first appears;
/// one group without a heading when they all share one provider.
pub fn provider_groups<T>(
    items: Vec<T>,
    provider: impl Fn(&T) -> String,
) -> Vec<(Option<String>, Vec<T>)> {
    let mut groups: Vec<(String, Vec<T>)> = Vec::new();
    for item in items {
        let name = provider(&item);
        match groups.iter_mut().find(|(known, _)| *known == name) {
            Some((_, members)) => members.push(item),
            None => groups.push((name, vec![item])),
        }
    }
    if groups.len() <= 1 {
        return groups
            .into_iter()
            .map(|(_, members)| (None, members))
            .collect();
    }
    groups
        .into_iter()
        .map(|(name, members)| (Some(name), members))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{match_range, matches, provider_groups};

    #[test]
    fn every_word_must_match_some_field_ignoring_case() {
        let fields = ["Claude Opus 5.5", "anthropic", "anthropic/claude-opus-5-5"];
        assert!(matches("", &fields));
        assert!(matches("opus", &fields));
        assert!(matches("OPUS anthropic", &fields));
        assert!(matches("claude-opus", &fields));
        assert!(!matches("opus openai", &fields));
        assert!(!matches("sonnet", &fields));
    }

    #[test]
    fn a_match_is_found_on_character_boundaries() {
        assert_eq!(match_range("Claude Opus 5.5", "opus"), Some((7, 11)));
        assert_eq!(match_range("Claude Opus 5.5", "  OPUS  x"), Some((7, 11)));
        assert_eq!(match_range("Claude Opus", "sonnet"), None);
        assert_eq!(match_range("Claude Opus", ""), None);
        // Multi-byte characters before and in the match.
        assert_eq!(match_range("Ünïcode Öpus", "öpus"), Some((10, 15)));
        assert_eq!(match_range("ab", "abc"), None);
    }

    #[test]
    fn groups_keep_first_appearance_order_and_one_provider_has_no_heading() {
        let items = vec!["anthropic/a", "openai/b", "anthropic/c"];
        let provider = |item: &&str| item.split('/').next().unwrap().to_owned();
        let groups = provider_groups(items, provider);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0.as_deref(), Some("anthropic"));
        assert_eq!(groups[0].1, ["anthropic/a", "anthropic/c"]);
        assert_eq!(groups[1].0.as_deref(), Some("openai"));
        let one = provider_groups(vec!["anthropic/a", "anthropic/b"], provider);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, None);
        assert!(provider_groups(Vec::<&str>::new(), provider).is_empty());
    }
}
