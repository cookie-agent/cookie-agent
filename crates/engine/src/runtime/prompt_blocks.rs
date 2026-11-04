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
/// The prompt block on attaching files to a reply.
pub(crate) const ATTACHMENTS_BLOCK: &str = "attachments";
/// The prompt block of the tool providers' `<tool_instructions>` sections.
pub(crate) const TOOL_INSTRUCTIONS_BLOCK: &str = "tool_instructions";
/// The prompt block of the `<available_skills>` listing.
pub(crate) const SKILLS_BLOCK: &str = "skills";

/// How to attach files to a reply
/// (`docs/site/specs/message-attachments.md` section 9), composed after
/// where the session's runtimes work. `machine`: the session has a runtime
/// besides `global`, so a tag can name a file on a machine; without one,
/// the block names only artifacts and assets.
pub(crate) fn attachments_block(machine: bool) -> String {
    let file = if machine {
        "`<attach src=\"runs/loss.png\"/>` for a file on a machine (add `machine=\"name\"` \
         for one on another machine than the default), "
    } else {
        ""
    };
    format!(
        "To give the user a file in your reply, write a tag where it should show: {file}\
         `<attach artifact=\"5f2c81d0\"/>` for an artifact of this conversation, such as a file \
         the user sent, by its digest or its first 8 or more digits, or \
         `<attach asset=\"asset_3f9a1c2e\"/>` to show an asset. Add `title=\"…\"` to name it. \
         Pictures, video, and audio play, an HTML page runs live, Markdown renders, and \
         anything else downloads. Tags written next to each other show as one row. A file is \
         read once, when your message ends, at most {} MiB, and at most {} attachments go in \
         one message. A tag inside code is only text. If an attachment fails, you are told \
         before your next request.",
        cookie_agent_protocol::ASSET_MAX_BYTES / (1024 * 1024),
        cookie_agent_protocol::ATTACH_MAX_PER_MESSAGE,
    )
}

impl Engine {
    /// [`attachments_block`] for `session`: with the file form when it has
    /// a runtime besides `global`.
    pub(crate) fn session_attachments_block(&self, session: SessionId) -> String {
        attachments_block(!self.attachment_facts(session).is_empty())
    }

    /// Appends the attachments block to `policy`'s composed prompt, skipped
    /// rather than truncated at the prompt's byte limit.
    pub(crate) fn compose_attachments_prompt(
        &self,
        policy: &mut FrozenRunPolicy,
        session: SessionId,
    ) {
        let block = self.session_attachments_block(session);
        if !prompt_block_fits(policy, &block) {
            eprintln!(
                "cookie-agent: skipping the attachments prompt: the composed prompt would exceed {} bytes",
                AgentSnapshot::MAX_PROMPT_BYTES
            );
            return;
        }
        append_prompt_block(policy, ATTACHMENTS_BLOCK, &block);
    }
}

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

impl Engine {
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
        blocks.push((
            ATTACHMENTS_BLOCK.to_owned(),
            self.session_attachments_block(session),
        ));
        if let Some(block) = self.live_tool_instructions_block_with(policy, session, composition)? {
            blocks.push((TOOL_INSTRUCTIONS_BLOCK.to_owned(), block));
        }
        if let Some(block) = self.live_skills_block(session, policy)? {
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
