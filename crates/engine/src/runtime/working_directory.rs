use cookie_agent_protocol::{AgentSnapshot, Sha256Digest};

use crate::{
    Engine,
    policy::FrozenRunPolicy,
    runtime::prompt_blocks::{PROMPT_BLOCK_SEPARATOR, push_prompt_block},
};

impl Engine {
    pub(crate) fn compose_working_directory_section(&self, policy: &mut FrozenRunPolicy) {
        let cwd = self.inner.store.cwd().display().to_string();
        let block = format!(
            "<working_directory>{}</working_directory>",
            escape_xml_text(&cwd)
        );
        let Some(composed_bytes) = policy
            .agent
            .composed_prompt
            .len()
            .checked_add(block.len() + PROMPT_BLOCK_SEPARATOR.len())
        else {
            eprintln!(
                "cookie-agent: skipping working-directory prompt section: composed prompt byte count overflowed"
            );
            return;
        };
        if composed_bytes > AgentSnapshot::MAX_PROMPT_BYTES {
            eprintln!(
                "cookie-agent: skipping working-directory prompt section: composed prompt exceeds {} bytes",
                AgentSnapshot::MAX_PROMPT_BYTES
            );
            return;
        }

        push_prompt_block(&mut policy.agent.composed_prompt, &block);
        policy.agent.prompt_fingerprint =
            Sha256Digest::of_bytes(policy.agent.composed_prompt.as_bytes());
        let mut document_fingerprint = policy
            .agent
            .document_fingerprint
            .as_str()
            .as_bytes()
            .to_vec();
        document_fingerprint.extend_from_slice(block.as_bytes());
        policy.agent.document_fingerprint = Sha256Digest::of_bytes(&document_fingerprint);
    }
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::escape_xml_text;

    #[test]
    fn escapes_ampersand_before_angle_brackets() {
        assert_eq!(
            escape_xml_text("/tmp/a&b/<x>&"),
            "/tmp/a&amp;b/&lt;x&gt;&amp;"
        );
    }
}
