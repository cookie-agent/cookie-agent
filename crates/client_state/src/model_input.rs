//! What the engine adds to a model's input beyond the conversation, as the
//! transcript shows it, so nothing reaches the model unseen: the memory
//! snapshot turn, a deferred tools announcement, the context a subagent
//! inherits from its parent, and the context an ACP agent's first prompt
//! carries, each a row whose body is the text the model reads; and the
//! notes a tool row gets for what its result sent beyond the tool's own
//! output (a loop warning, tool-emitted messages, an elision marker).

use cookie_agent_protocol::{
    AnnouncedToolGroup, DelegatedContextRole, DelegatedContextTurn, DisplayBlock, Open,
    ToolEmittedContent, ToolEmittedMessage, ToolEmittedMessageRole, model_text,
};

/// Which added input a [`crate::TranscriptItem::ModelInput`] row is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelInputKind {
    /// The memory snapshot turn, shown when it differs from the last one.
    Memory,
    /// A deferred tools announcement.
    DeferredTools,
    /// The parent's conversation a subagent starts from.
    InheritedContext,
    /// What an ACP agent's first prompt carries before the input.
    AgentContext,
}

impl ModelInputKind {
    /// A stable label for keys and test IDs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::DeferredTools => "deferred-tools",
            Self::InheritedContext => "inherited-context",
            Self::AgentContext => "agent-context",
        }
    }
}

/// The memory row's heading: `memory · 42 lines`, or that the model has
/// none from here on.
pub fn memory_header(text: &str) -> String {
    if text.is_empty() {
        "memory · none in the model's context from here on".to_owned()
    } else {
        format!("memory · {}", lines(text))
    }
}

/// The deferred tools row's heading and body: `deferred tools · 3 added,
/// 1 removed`, and the announcement the model reads.
pub fn deferred_tools(
    added: &[AnnouncedToolGroup],
    removed: &[AnnouncedToolGroup],
) -> (String, String) {
    let count = |groups: &[AnnouncedToolGroup]| -> usize {
        groups.iter().map(|group| group.tools.len()).sum()
    };
    let mut changes = Vec::new();
    if !added.is_empty() {
        changes.push(format!("{} added", count(added)));
    }
    if !removed.is_empty() {
        changes.push(format!("{} removed", count(removed)));
    }
    (
        format!("deferred tools · {}", changes.join(", ")),
        model_text::deferred_tools_announcement(added, removed),
    )
}

/// The inherited context row's heading and body: each turn under the
/// role it has in the model's history.
pub fn inherited_context(turns: &[DelegatedContextTurn]) -> (String, String) {
    let body = turns
        .iter()
        .map(|turn| {
            let role = match turn.role {
                DelegatedContextRole::User => "user",
                DelegatedContextRole::Assistant => "assistant",
            };
            format!("── {role}\n{}", turn.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let count = if turns.len() == 1 {
        "1 turn".to_owned()
    } else {
        format!("{} turns", turns.len())
    };
    (format!("context from the parent · {count}"), body)
}

/// The ACP context row's heading: `context sent to claude · 120 lines`.
pub fn agent_context_header(agent: &str, text: &str) -> String {
    format!("context sent to {agent} · {}", lines(text))
}

fn lines(text: &str) -> String {
    match crate::rows::display_line_count(text) {
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    }
}

/// The note a tool row shows for the loop warning its output carries.
pub fn loop_warning_note(output: &str) -> Option<Open<DisplayBlock>> {
    model_text::loop_warning(output)
        .map(|warning| note(format!("Sent to the model with this result:\n{warning}")))
}

/// The notes a tool row shows for the messages its result emitted, each a
/// user turn after the tool's result in the model's history.
pub fn emitted_message_notes(messages: &[ToolEmittedMessage]) -> Vec<Open<DisplayBlock>> {
    messages
        .iter()
        .map(|message| {
            let mut text =
                String::from("Sent to the model after this result, as a user message:\n");
            if message.role == ToolEmittedMessageRole::System {
                text.push_str(model_text::TOOL_EMITTED_SYSTEM_USER_MARKER);
                text.push('\n');
            }
            let parts = message
                .content
                .iter()
                .map(|part| match part {
                    ToolEmittedContent::Text(text) => text.clone(),
                    ToolEmittedContent::File(file) => format!(
                        "[file: {} ({})]",
                        file.filename.as_deref().unwrap_or("unnamed"),
                        file.mime_type.as_str()
                    ),
                })
                .collect::<Vec<_>>();
            text.push_str(&parts.join("\n"));
            note(text)
        })
        .collect()
}

/// The note a tool row shows once its output left the model's context:
/// the marker the model reads in its place.
pub fn elision_note(
    retained_uri: &str,
    original_bytes: u64,
    emitted_messages: usize,
) -> Open<DisplayBlock> {
    let artifact = retained_uri
        .strip_prefix("artifact://sha256/")
        .unwrap_or(retained_uri);
    note(format!(
        "Left the model's context; the model reads instead:\n{}",
        model_text::tool_output_elision_marker(artifact, original_bytes, emitted_messages)
    ))
}

fn note(text: String) -> Open<DisplayBlock> {
    Open::Known(DisplayBlock::Text {
        text,
        omitted_lines: None,
    })
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        DelegatedContextRole, DelegatedContextTurn, DisplayBlock, Open, ToolEmittedContent,
        ToolEmittedMessage, ToolEmittedMessageRole, model_text::LOOP_WARNING_MARKER,
    };

    use super::{
        elision_note, emitted_message_notes, inherited_context, loop_warning_note, memory_header,
    };

    fn text(block: &Open<DisplayBlock>) -> &str {
        match block {
            Open::Known(DisplayBlock::Text { text, .. }) => text,
            other => panic!("not a text block: {other:?}"),
        }
    }

    #[test]
    fn a_tool_rows_notes_quote_what_the_model_reads() {
        let output = format!("ls{LOOP_WARNING_MARKER}: you have made …\n</system-reminder>");
        let warning = loop_warning_note(&output).expect("a warning");
        assert!(text(&warning).ends_with(
            "<system-reminder>\nRepeated tool calls detected: you have made …\n</system-reminder>"
        ));
        assert!(loop_warning_note("ls").is_none());

        let system = ToolEmittedMessage {
            role: ToolEmittedMessageRole::System,
            content: vec![ToolEmittedContent::Text("Mind the cwd.".into())],
        };
        let notes = emitted_message_notes(&[system]);
        assert_eq!(
            text(&notes[0]),
            "Sent to the model after this result, as a user message:\n\
             [tool-emitted system message; materialized as user history]\nMind the cwd."
        );

        let elided = elision_note("artifact://sha256/abc", 12, 1);
        assert_eq!(
            text(&elided),
            "Left the model's context; the model reads instead:\n[tool output elided; 12 bytes \
             retained; read it with read_artifact(artifact=\"abc\")] 1 tool-emitted message(s) \
             were elided with this result and are not recoverable."
        );
    }

    #[test]
    fn inherited_context_lists_each_turn_under_its_role() {
        let turns = [
            DelegatedContextTurn {
                role: DelegatedContextRole::User,
                text: "Fix the build.".into(),
            },
            DelegatedContextTurn {
                role: DelegatedContextRole::Assistant,
                text: "On it.".into(),
            },
        ];
        let (header, body) = inherited_context(&turns);
        assert_eq!(header, "context from the parent · 2 turns");
        assert_eq!(body, "── user\nFix the build.\n── assistant\nOn it.");
        assert_eq!(
            memory_header(""),
            "memory · none in the model's context from here on"
        );
        assert_eq!(memory_header("a\nb"), "memory · 2 lines");
    }
}
