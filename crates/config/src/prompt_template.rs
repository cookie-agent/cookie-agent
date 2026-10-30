//! `{{name}}` placeholders in agent prompts.
//!
//! A placeholder is `{{`, an ASCII identifier with optional surrounding
//! spaces, and `}}`. Any other `{{` is literal text.
//!
//! `{{default_prompt}}` in a document of the user's stands for the official
//! agent's prompt of the same ID, which the loader puts in its place, else
//! for the prompt this build carries for it (only `compaction` has one),
//! which the engine puts in. Official documents never use it.
//!
//! Each internal agent accepts its own set of identifiers; an unknown one is
//! rejected when the document loads, so a typo never reaches the model. Other
//! placeholders in the bodies of non-internal agents are literal text.

use crate::agents::{
    BUILT_IN_APPROVAL_AGENT_ID, BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_TITLE_AGENT_ID,
};

/// Replaced by the official agent's prompt, else the engine's own.
pub const DEFAULT_PROMPT_PLACEHOLDER: &str = "default_prompt";
/// Replaced by the user's or plugins' compaction focus, or by nothing.
/// Only the `compaction` agent accepts it.
pub const FOCUS_PLACEHOLDER: &str = "focus";

/// The placeholders an agent's body may use, or `None` when the agent's body
/// is sent as written.
pub(crate) fn internal_prompt_placeholders(agent: &str) -> Option<&'static [&'static str]> {
    match agent {
        BUILT_IN_COMPACTION_AGENT_ID => Some(&[DEFAULT_PROMPT_PLACEHOLDER, FOCUS_PLACEHOLDER]),
        // Their prompts are the official documents' bodies, which the
        // loader substitutes: nothing is compiled in.
        BUILT_IN_APPROVAL_AGENT_ID | BUILT_IN_TITLE_AGENT_ID => Some(&[]),
        _ => None,
    }
}

/// Values substituted into an internal agent prompt template.
#[derive(Clone, Copy, Debug)]
pub struct InternalPromptValues<'a> {
    pub default_prompt: &'a str,
    pub focus: Option<&'a str>,
}

enum Segment<'a> {
    Text(&'a str),
    Placeholder(&'a str),
}

fn scan<'a>(template: &'a str, mut segment: impl FnMut(Segment<'a>)) {
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let name = after[..close].trim_matches(' ');
        if !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            segment(Segment::Text(&rest[..open]));
            segment(Segment::Placeholder(name));
            rest = &after[close + 2..];
        } else {
            segment(Segment::Text(&rest[..open + 2]));
            rest = after;
        }
    }
    segment(Segment::Text(rest));
}

/// Returns the first placeholder in `template` outside `allowed`.
pub(crate) fn unknown_placeholder(template: &str, allowed: &[&str]) -> Option<String> {
    let mut unknown = None;
    scan(template, |segment| {
        if let Segment::Placeholder(name) = segment
            && unknown.is_none()
            && !allowed.contains(&name)
        {
            unknown = Some(name.to_owned());
        }
    });
    unknown
}

/// Whether `template` places `{{default_prompt}}`.
pub(crate) fn uses_default_prompt(template: &str) -> bool {
    let mut found = false;
    scan(template, |segment| {
        found |= matches!(segment, Segment::Placeholder(DEFAULT_PROMPT_PLACEHOLDER));
    });
    found
}

/// `template` with each `{{default_prompt}}` replaced by `prompt`; every
/// other placeholder stays, for the engine to expand.
pub(crate) fn replace_default_prompt(template: &str, prompt: &str) -> String {
    let mut replaced = String::with_capacity(template.len() + prompt.len());
    scan(template, |segment| match segment {
        Segment::Text(text) => replaced.push_str(text),
        Segment::Placeholder(DEFAULT_PROMPT_PLACEHOLDER) => replaced.push_str(prompt),
        Segment::Placeholder(name) => {
            replaced.push_str("{{");
            replaced.push_str(name);
            replaced.push_str("}}");
        }
    });
    replaced
}

/// Expands an internal agent prompt template. A focus the template does not
/// place with `{{focus}}` is appended, so it is never dropped.
#[must_use]
pub fn render_internal_prompt(template: &str, values: InternalPromptValues<'_>) -> String {
    let mut rendered = String::with_capacity(template.len() + values.default_prompt.len());
    let mut focus_placed = false;
    scan(template.trim(), |segment| match segment {
        Segment::Text(text) => rendered.push_str(text),
        Segment::Placeholder(DEFAULT_PROMPT_PLACEHOLDER) => {
            rendered.push_str(values.default_prompt);
        }
        Segment::Placeholder(FOCUS_PLACEHOLDER) => {
            focus_placed = true;
            rendered.push_str(values.focus.unwrap_or_default());
        }
        // Validation rejects these at load; keep them visible rather than lose text.
        Segment::Placeholder(name) => {
            rendered.push_str("{{");
            rendered.push_str(name);
            rendered.push_str("}}");
        }
    });
    if !focus_placed && let Some(focus) = values.focus {
        rendered.push_str("\n\nUser-requested focus: ");
        rendered.push_str(focus);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::{
        InternalPromptValues, internal_prompt_placeholders, render_internal_prompt,
        unknown_placeholder,
    };

    fn unknown_compaction_placeholder(template: &str) -> Option<String> {
        unknown_placeholder(
            template,
            internal_prompt_placeholders("compaction").unwrap(),
        )
    }

    const VALUES: InternalPromptValues<'static> = InternalPromptValues {
        default_prompt: "DEFAULT",
        focus: Some("parser"),
    };

    #[test]
    fn placeholders_expand_once_without_rescanning_substituted_text() {
        let values = InternalPromptValues {
            default_prompt: "keeps {{focus}} literal",
            focus: Some("{{default_prompt}}"),
        };
        assert_eq!(
            render_internal_prompt("{{ default_prompt }}\nFocus: {{focus}}", values),
            "keeps {{focus}} literal\nFocus: {{default_prompt}}"
        );
    }

    #[test]
    fn unplaced_focus_is_appended_and_absent_focus_renders_empty() {
        assert_eq!(
            render_internal_prompt("{{default_prompt}}\nAlso keep test names.", VALUES),
            "DEFAULT\nAlso keep test names.\n\nUser-requested focus: parser"
        );
        assert_eq!(
            render_internal_prompt(
                "Focus: [{{focus}}]",
                InternalPromptValues {
                    focus: None,
                    ..VALUES
                }
            ),
            "Focus: []"
        );
    }

    #[test]
    fn non_identifier_braces_stay_literal() {
        for literal in [
            "{{",
            "{{}}",
            "{{a b}}",
            "{{a-b}}",
            "{{\nfocus}}",
            "x }} {{ y",
        ] {
            assert_eq!(unknown_compaction_placeholder(literal), None, "{literal}");
            assert_eq!(
                render_internal_prompt(
                    literal,
                    InternalPromptValues {
                        focus: None,
                        ..VALUES
                    }
                ),
                literal.trim()
            );
        }
    }

    #[test]
    fn unknown_identifiers_are_reported() {
        assert_eq!(
            unknown_compaction_placeholder("{{focus}} {{default_prompt}}"),
            None
        );
        assert_eq!(
            unknown_compaction_placeholder("{{focus}} {{ defaults }} {{other}}").as_deref(),
            Some("defaults")
        );
    }

    #[test]
    fn each_internal_agent_accepts_only_its_placeholders() {
        for agent in ["approval", "title"] {
            let allowed = internal_prompt_placeholders(agent).unwrap();
            assert_eq!(unknown_placeholder("Review calls.", allowed), None);
            assert_eq!(
                unknown_placeholder("{{default_prompt}}", allowed).as_deref(),
                Some("default_prompt")
            );
        }
        assert_eq!(internal_prompt_placeholders("reviewer"), None);
    }
}
