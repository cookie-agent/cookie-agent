//! The skills a client lists (`skills.list`): grouped by where they come
//! from, searched by name and description, and what a refresh of the
//! official skills (`skills.update`) did, in words.

use cookie_agent_protocol::{SkillDescriptor, SkillSource, SkillsUpdateResult};

/// The order the groups show in: the user's own skills, the workspace's,
/// then the official ones, which either of the others overrides by name.
pub const SOURCES: [SkillSource; 3] = [
    SkillSource::User,
    SkillSource::Project,
    SkillSource::Official,
];

/// A group's heading.
#[must_use]
pub const fn source_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::User => "Your skills",
        SkillSource::Project => "This workspace's skills",
        SkillSource::Official => "Official skills",
    }
}

/// Whether `skill`'s name or description holds `query` (any case); every
/// skill for a blank query.
#[must_use]
pub fn skill_matches(skill: &SkillDescriptor, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || skill.name.to_lowercase().contains(&query)
        || skill.description.to_lowercase().contains(&query)
}

/// The skills matching `query`, grouped in [`SOURCES`] order, each group by
/// name; groups with none are left out.
#[must_use]
pub fn skill_groups<'a>(
    skills: &'a [SkillDescriptor],
    query: &str,
) -> Vec<(SkillSource, Vec<&'a SkillDescriptor>)> {
    SOURCES
        .into_iter()
        .filter_map(|source| {
            let mut group = skills
                .iter()
                .filter(|skill| skill.source == source && skill_matches(skill, query))
                .collect::<Vec<_>>();
            group.sort_by(|left, right| left.name.cmp(&right.name));
            (!group.is_empty()).then_some((source, group))
        })
        .collect()
}

/// What a refresh of the official skills did, for a status line, and
/// whether it failed.
#[must_use]
pub fn update_summary(result: &SkillsUpdateResult) -> (String, bool) {
    if !result.enabled {
        return (
            "Official skills are off ([skills] official = false).".to_owned(),
            false,
        );
    }
    let mut parts = Vec::new();
    if !result.updated.is_empty() {
        parts.push(format!("updated {}", result.updated.join(", ")));
    }
    if !result.removed.is_empty() {
        parts.push(format!("removed {}", result.removed.join(", ")));
    }
    let done = if parts.is_empty() {
        "The official skills are up to date.".to_owned()
    } else {
        let mut text = parts.join("; ");
        if let Some(first) = text.get(..1) {
            text = first.to_uppercase() + &text[1..];
        }
        format!("{text}.")
    };
    match &result.error {
        Some(error) => (format!("{done} The refresh failed: {error}"), true),
        None => (done, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str, source: SkillSource) -> SkillDescriptor {
        SkillDescriptor {
            name: name.to_owned(),
            description: description.to_owned(),
            when_to_use: None,
            location: format!("/skills/{name}/SKILL.md"),
            source,
            precedence_winner: true,
            permission_effect: cookie_agent_protocol::PermissionEffect::Allow,
            visible: true,
            user_invocable: true,
            argument_hint: None,
            version: None,
        }
    }

    fn names(groups: &[(SkillSource, Vec<&SkillDescriptor>)]) -> Vec<(SkillSource, Vec<String>)> {
        groups
            .iter()
            .map(|(source, skills)| {
                (
                    *source,
                    skills.iter().map(|skill| skill.name.clone()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn skills_group_by_source_in_order_and_by_name() {
        let skills = [
            skill("zeta", "", SkillSource::Official),
            skill("deploy", "Ship it", SkillSource::User),
            skill("alpha", "", SkillSource::Official),
            skill("review", "Read a diff", SkillSource::Project),
        ];
        assert_eq!(
            names(&skill_groups(&skills, "")),
            [
                (SkillSource::User, vec!["deploy".to_owned()]),
                (SkillSource::Project, vec!["review".to_owned()]),
                (
                    SkillSource::Official,
                    vec!["alpha".to_owned(), "zeta".to_owned()]
                ),
            ]
        );
        // The search looks in descriptions too, in any case, and leaves
        // out the groups it empties.
        assert_eq!(
            names(&skill_groups(&skills, "  DIFF ")),
            [(SkillSource::Project, vec!["review".to_owned()])]
        );
        assert!(skill_groups(&skills, "nothing").is_empty());
    }

    fn result(updated: &[&str], removed: &[&str], error: Option<&str>) -> SkillsUpdateResult {
        SkillsUpdateResult {
            enabled: true,
            skills: Vec::new(),
            updated: updated.iter().map(|name| (*name).to_owned()).collect(),
            removed: removed.iter().map(|name| (*name).to_owned()).collect(),
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn an_update_says_what_changed_and_whether_it_failed() {
        let (quiet, failed) = update_summary(&result(&[], &[], None));
        assert!(!failed);
        let (changed, _) = update_summary(&result(&["a", "b"], &["c"], None));
        assert!(
            changed.contains("a, b") && changed.contains('c'),
            "{changed}"
        );
        assert_ne!(quiet, changed);
        let (text, failed) = update_summary(&result(&[], &[], Some("offline")));
        assert!(failed && text.contains("offline"), "{text}");
        // With official skills off, nothing was refreshed, whatever it lists.
        let mut off = result(&["listed"], &[], None);
        off.enabled = false;
        let (text, failed) = update_summary(&off);
        assert!(
            !failed && !text.contains("listed") && text != quiet,
            "{text}"
        );
    }
}
