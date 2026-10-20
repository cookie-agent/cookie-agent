//! `/skills`: which skills a search shows, ranked as the TUI ranks them.

use cookie_agent_protocol::SkillDescriptor;

/// The skills a user can run that match `query`: by name (exact, prefix,
/// then anywhere), then by description, best first.
pub fn matching_skills<'a>(skills: &'a [SkillDescriptor], query: &str) -> Vec<&'a SkillDescriptor> {
    let query = query.trim().to_lowercase();
    let mut ranked = skills
        .iter()
        .filter(|skill| skill.precedence_winner && skill.user_invocable)
        .filter_map(|skill| {
            let name = skill.name.as_str();
            let rank = if query.is_empty() || name == query {
                Some(0)
            } else if name.starts_with(&query) {
                Some(1)
            } else if name.contains(&query) {
                Some(2)
            } else {
                skill
                    .description
                    .to_lowercase()
                    .contains(&query)
                    .then_some(3)
            };
            rank.map(|rank| (rank, skill))
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, skill)| skill).collect()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::SkillDescriptor;

    use super::matching_skills;

    fn skill(name: &str, description: &str, user_invocable: bool) -> SkillDescriptor {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": description,
            "when_to_use": null,
            "location": "/skills",
            "source": "project",
            "precedence_winner": true,
            "permission_effect": "allow",
            "visible": true,
            "user_invocable": user_invocable,
            "argument_hint": null,
        }))
        .expect("skill")
    }

    #[test]
    fn names_rank_before_descriptions_and_hidden_skills_never_show() {
        let skills = [
            skill("summarize", "write a review summary", true),
            skill("review", "review the diff", true),
            skill("preview", "render a page", true),
            skill("reviewer", "internal", false),
        ];
        let names = |query| {
            matching_skills(&skills, query)
                .into_iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("review"), ["review", "preview", "summarize"]);
        assert_eq!(names(""), ["summarize", "review", "preview"]);
        assert!(names("nothing").is_empty());
    }
}
