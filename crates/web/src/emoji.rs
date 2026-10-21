//! The project editor's emoji picker list (`docs/site/specs/projects.md`
//! section 10.3): a few hundred common emoji by category, each with its
//! Unicode name for the search box. Names and code points only: the
//! emoji draw in the system font.

mod data;

/// A category of the picker.
#[derive(Debug)]
pub struct EmojiGroup {
    pub name: &'static str,
    /// Each emoji and its lowercase Unicode name.
    pub emoji: &'static [(&'static str, &'static str)],
}

/// Every category, in the picker's order.
pub fn groups() -> &'static [EmojiGroup] {
    data::GROUPS
}

/// The picker's categories filtered by `query`: each emoji whose name has
/// every word of the query, or that is the query itself. Categories left
/// empty go; an empty query keeps them all.
pub fn search(query: &str) -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let query = query.trim().to_lowercase();
    let words = query.split_whitespace().collect::<Vec<_>>();
    groups()
        .iter()
        .map(|group| {
            let emoji = group
                .emoji
                .iter()
                .filter(|(emoji, name)| {
                    *emoji == query || words.iter().all(|word| name.contains(word))
                })
                .copied()
                .collect::<Vec<_>>();
            (group.name, emoji)
        })
        .filter(|(_, emoji)| !emoji.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_emoji_is_one_the_daemon_takes() {
        let mut count = 0;
        for group in groups() {
            assert!(!group.emoji.is_empty(), "{}", group.name);
            for (emoji, name) in group.emoji {
                assert_eq!(
                    cookie_agent_client_state::projects::validate_emoji(emoji),
                    Ok(()),
                    "{emoji} {name}"
                );
                assert_eq!(*name, name.to_lowercase());
                count += 1;
            }
        }
        // A few hundred, as the spec asks.
        assert!((300..=600).contains(&count), "{count}");
    }

    #[test]
    fn the_search_matches_every_word_of_the_name() {
        let found = search("Cookie");
        assert_eq!(found.len(), 1);
        assert!(found[0].1.contains(&("🍪", "cookie")));
        // Every word, in any order.
        let hearts = search("heart red");
        assert!(
            hearts
                .iter()
                .flat_map(|(_, emoji)| emoji)
                .any(|(emoji, _)| *emoji == "❤️")
        );
        assert!(search("no such emoji name").is_empty());
        assert_eq!(search("  ").len(), groups().len());
        // The emoji itself finds it too.
        assert!(
            search("🍪")
                .iter()
                .any(|(_, emoji)| emoji.contains(&("🍪", "cookie")))
        );
    }
}
