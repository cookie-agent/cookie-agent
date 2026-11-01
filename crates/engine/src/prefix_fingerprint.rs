//! The request prefix each assistant turn was made in, for models that bind
//! replayed reasoning to it (`docs/site/specs/frozen-context.md`, section 4).
//!
//! A turn's fingerprint covers what a request sent before it: the system
//! prompt and every earlier message (all in the history, reasoning included as
//! sent) and the eager tools. Deferred tools are left out: Anthropic does not
//! bind reasoning to them. Cache markers are not in the history yet when it is
//! taken.

use cookie_agent_protocol::Sha256Digest;
use oven_sdk::{AssistantPart, HistoryTurn, ToolDefinition};
use sha2::{Digest as _, Sha256};

/// The fingerprints of one request.
pub(crate) struct PrefixFingerprints {
    /// One per assistant turn of the history, in order: the prefix before
    /// it.
    pub(crate) assistant: Vec<Sha256Digest>,
    /// The whole request: the prefix of the turn it makes.
    pub(crate) whole: Sha256Digest,
}

pub(crate) fn prefix_fingerprints(
    history: &[HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<PrefixFingerprints, serde_json::Error> {
    let mut hasher = Sha256::new();
    hasher.update(b"cookie.prefix.v1\0");
    for tool in tools.iter().filter(|tool| !tool.is_deferred()) {
        serde_json::to_writer(HashWriter(&mut hasher), tool)?;
        hasher.update([0]);
    }
    hasher.update([1]);
    let mut assistant = Vec::new();
    for turn in history {
        if matches!(turn, HistoryTurn::Assistant(_)) {
            assistant.push(digest(hasher.clone()));
        }
        serde_json::to_writer(HashWriter(&mut hasher), turn)?;
        hasher.update([0]);
    }
    Ok(PrefixFingerprints {
        assistant,
        whole: digest(hasher),
    })
}

/// Whether an assistant turn replays reasoning a provider may bind.
pub(crate) fn carries_reasoning(turn: &HistoryTurn) -> bool {
    match turn {
        HistoryTurn::Assistant(turn) => {
            turn.finish.native_replay.is_some()
                || turn
                    .message
                    .content
                    .iter()
                    .any(|part| matches!(part, AssistantPart::Reasoning(_)))
        }
        _ => false,
    }
}

fn digest(hasher: Sha256) -> Sha256Digest {
    let hex = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Sha256Digest::new(hex).expect("SHA-256 hex digest is well-formed")
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oven_sdk::{
        AssistantMessage, CompletedTurn, Finish, FinishReason, InputPart, JsonSchema,
        ReasoningPart, SystemMessage, SystemPart, TextPart, UserMessage,
    };

    fn system(text: &str) -> HistoryTurn {
        HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            text,
        ))]))
    }

    fn user(text: &str) -> HistoryTurn {
        HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(text))]))
    }

    fn assistant(text: &str) -> HistoryTurn {
        HistoryTurn::assistant(CompletedTurn::new(
            AssistantMessage::new(vec![AssistantPart::Text(TextPart::new(text))]),
            Finish::new(oven_sdk::Usage::default(), FinishReason::Stop),
        ))
    }

    fn tool(name: &str, deferred: bool) -> ToolDefinition {
        let tool = ToolDefinition::new(
            name,
            "a tool",
            JsonSchema::new(serde_json::json!({"type": "object"})).unwrap(),
        );
        if deferred { tool.deferred() } else { tool }
    }

    #[test]
    fn a_turn_keeps_its_fingerprint_while_what_came_before_it_is_unchanged() {
        let tools = [tool("read", false)];
        let first = [system("s"), user("u1"), assistant("a1")];
        let made = prefix_fingerprints(&first[..2], &tools).unwrap().whole;
        let later = [system("s"), user("u1"), assistant("a1"), user("u2")];
        let fingerprints = prefix_fingerprints(&later, &tools).unwrap();
        assert_eq!(fingerprints.assistant, vec![made.clone()]);
        assert_ne!(fingerprints.whole, made);
    }

    #[test]
    fn a_changed_system_prompt_earlier_message_or_eager_tool_changes_it() {
        let tools = [tool("read", false)];
        let base = prefix_fingerprints(&[system("s"), user("u1")], &tools)
            .unwrap()
            .whole;
        for (history, tools) in [
            (vec![system("t"), user("u1")], vec![tool("read", false)]),
            (vec![system("s"), user("u0")], vec![tool("read", false)]),
            (
                vec![system("s"), user("u1")],
                vec![tool("read", false), tool("write", false)],
            ),
        ] {
            assert_ne!(prefix_fingerprints(&history, &tools).unwrap().whole, base);
        }
    }

    #[test]
    fn deferred_tools_do_not_count() {
        let history = [system("s"), user("u1")];
        assert_eq!(
            prefix_fingerprints(&history, &[tool("read", false)])
                .unwrap()
                .whole,
            prefix_fingerprints(&history, &[tool("read", false), tool("later", true)])
                .unwrap()
                .whole
        );
    }

    #[test]
    fn only_turns_with_reasoning_or_native_replay_carry_reasoning() {
        assert!(!carries_reasoning(&assistant("a")));
        assert!(!carries_reasoning(&user("u")));
        let thought = HistoryTurn::assistant(CompletedTurn::new(
            AssistantMessage::new(vec![AssistantPart::Reasoning(ReasoningPart::new("think"))]),
            Finish::new(oven_sdk::Usage::default(), FinishReason::Stop),
        ));
        assert!(carries_reasoning(&thought));
    }
}
