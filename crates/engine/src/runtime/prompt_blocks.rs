use cookie_agent_protocol::{
    AgentSnapshot, AnnouncedPromptBlock, PromptBlockSpan, SessionId, Sha256Digest,
};

use super::{Engine, EngineError};
use crate::policy::FrozenRunPolicy;

/// Boundaries between composed system-prompt blocks are rendered as a markdown
/// horizontal rule so block transitions are visible in the assembled prompt.
pub(crate) const PROMPT_BLOCK_SEPARATOR: &str = "\n\n---\n\n";

pub(crate) fn push_prompt_block(prompt: &mut String, block: &str) {
    if !prompt.is_empty() {
        prompt.push_str(PROMPT_BLOCK_SEPARATOR);
    }
    prompt.push_str(block);
}

/// The prompt block of where the session's runtimes work.
pub(crate) const RUNTIMES_BLOCK: &str = "runtimes";
/// The prompt block of the tool providers' `<tool_instructions>` sections.
pub(crate) const TOOL_INSTRUCTIONS_BLOCK: &str = "tool_instructions";
/// The prompt block of the `<available_skills>` listing.
pub(crate) const SKILLS_BLOCK: &str = "skills";

/// Appends prompt block `key` to `policy`'s composed prompt, updates the
/// prompt and document fingerprints, and records where the block sits
/// (stable prefix spec, section 6.1).
pub(crate) fn append_prompt_block(policy: &mut FrozenRunPolicy, key: &str, block: &str) {
    let prompt = &mut policy.agent.composed_prompt;
    push_prompt_block(prompt, block);
    let start = prompt.len() - block.len();
    if let (Ok(start), Ok(len)) = (u32::try_from(start), u32::try_from(block.len())) {
        policy.prompt_blocks.push(PromptBlockSpan {
            key: key.to_owned(),
            start,
            len,
        });
    }
    policy.agent.prompt_fingerprint = Sha256Digest::of_bytes(prompt.as_bytes());
    let mut document_fingerprint = policy
        .agent
        .document_fingerprint
        .as_str()
        .as_bytes()
        .to_vec();
    document_fingerprint.extend_from_slice(block.as_bytes());
    policy.agent.document_fingerprint = Sha256Digest::of_bytes(&document_fingerprint);
}

/// Whether `block` fits after `policy`'s composed prompt.
pub(crate) fn prompt_block_fits(policy: &FrozenRunPolicy, block: &str) -> bool {
    policy
        .agent
        .composed_prompt
        .len()
        .checked_add(block.len() + PROMPT_BLOCK_SEPARATOR.len())
        .is_some_and(|bytes| bytes <= AgentSnapshot::MAX_PROMPT_BYTES)
}

/// The prompt blocks `spans` locate in `prompt`, in order; `None` when a
/// span does not fit the prompt.
pub(crate) fn prompt_block_texts(
    prompt: &str,
    spans: &[PromptBlockSpan],
) -> Option<Vec<(String, String)>> {
    spans
        .iter()
        .map(|span| {
            let start = usize::try_from(span.start).ok()?;
            let end = start.checked_add(usize::try_from(span.len).ok()?)?;
            Some((span.key.clone(), prompt.get(start..end)?.to_owned()))
        })
        .collect()
}

impl Engine {
    /// The previous run's prompt blocks, which a run on a model that defers
    /// tools reuses unless a checkpoint lies after that run's start (stable
    /// prefix spec, section 6.2).
    pub(crate) fn reusable_prompt_blocks(
        &self,
        session: SessionId,
    ) -> Result<Option<Vec<(String, String)>>, EngineError> {
        let fold = self.inner.store.get(session)?.log.prompt_blocks();
        Ok(if fold.checkpoint_since_run {
            None
        } else {
            fold.run_blocks
        })
    }

    /// The prompt blocks a run of `policy` in `session` composes now, in
    /// prompt order.
    fn live_prompt_blocks(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        composition: Option<&crate::runtimes::Composition>,
    ) -> Result<Vec<(String, String)>, EngineError> {
        let mut blocks = Vec::new();
        if let Some(block) = self.runtimes_block_with(session, None, Some(policy), composition) {
            blocks.push((RUNTIMES_BLOCK.to_owned(), block));
        }
        if let Some(block) = self.live_tool_instructions_block_with(policy, session, composition)? {
            blocks.push((TOOL_INSTRUCTIONS_BLOCK.to_owned(), block));
        }
        if let Some(block) = self.live_skills_block(session, policy, None)? {
            blocks.push((SKILLS_BLOCK.to_owned(), block));
        }
        Ok(blocks)
    }

    /// The announcement of the prompt blocks that changed since the model
    /// last knew them, if any did (stable prefix spec, section 6.3). With
    /// `composition`, the tool set `policy` composes now, the blocks are
    /// worked out from it rather than composing it again.
    pub(crate) fn prompt_blocks_announcement(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        composition: Option<&crate::runtimes::Composition>,
    ) -> Result<Option<cookie_agent_protocol::EventPayload>, EngineError> {
        let Some(known) = self.inner.store.get(session)?.log.prompt_blocks().known else {
            return Ok(None);
        };
        let live = self.live_prompt_blocks(session, policy, composition)?;
        let mut blocks = live
            .iter()
            .filter(|(key, text)| known.get(key) != Some(text))
            .map(|(key, text)| AnnouncedPromptBlock {
                key: key.clone(),
                text: Some(text.clone()),
            })
            .collect::<Vec<_>>();
        blocks.extend(
            known
                .keys()
                .filter(|key| !live.iter().any(|(live, _)| live == *key))
                .map(|key| AnnouncedPromptBlock {
                    key: key.clone(),
                    text: None,
                }),
        );
        Ok((!blocks.is_empty())
            .then_some(cookie_agent_protocol::EventPayload::PromptBlocksAnnounced { blocks }))
    }
}

#[cfg(test)]
mod tests {
    use super::{PROMPT_BLOCK_SEPARATOR, push_prompt_block};

    #[test]
    fn separator_is_a_markdown_horizontal_rule() {
        assert_eq!(PROMPT_BLOCK_SEPARATOR, "\n\n---\n\n");
    }

    #[test]
    fn empty_prompt_gets_no_leading_separator() {
        let mut prompt = String::new();
        push_prompt_block(&mut prompt, "first");
        assert_eq!(prompt, "first");
    }

    #[test]
    fn blocks_are_joined_by_exactly_one_separator() {
        let mut prompt = String::new();
        push_prompt_block(&mut prompt, "first");
        push_prompt_block(&mut prompt, "second");
        assert_eq!(prompt, "first\n\n---\n\nsecond");
        assert_eq!(prompt.matches(PROMPT_BLOCK_SEPARATOR).count(), 1);
    }
}
