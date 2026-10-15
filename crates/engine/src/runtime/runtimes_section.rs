use std::collections::HashSet;

use cookie_agent_protocol::{AgentSnapshot, SessionId, Sha256Digest};

use crate::{
    Engine,
    policy::FrozenRunPolicy,
    runtime::prompt_blocks::{PROMPT_BLOCK_SEPARATOR, push_prompt_block},
};

const RUNTIMES_GUIDANCE: &str = concat!(
    "File tools and bash of a coding runtime resolve relative paths against its workspace. ",
    "Use workspace-relative paths for files inside it. ",
    "When several runtimes offer a tool with one name, each is published as ",
    "`<runtime name>_<tool>`; `prefixed=\"true\"` marks a runtime whose tools carry that prefix.",
);

impl Engine {
    /// The `<runtimes>` block lists the session's attachments: name, kind,
    /// workspace and platform for a coding runtime, and whether its tools
    /// are published with the attachment prefix at admission. It is frozen
    /// with the composed prompt.
    pub(crate) fn compose_runtimes_section(
        &self,
        policy: &mut FrozenRunPolicy,
        session: SessionId,
    ) {
        let attachments = match self.session_runtimes(session) {
            Ok(attachments) => attachments,
            Err(error) => {
                eprintln!("cookie-agent: skipping runtimes prompt section: {error}");
                return;
            }
        };
        let prefixed = self
            .compose_for_policy(session, policy)
            .map(|composition| {
                composition
                    .tools
                    .iter()
                    .filter(|tool| tool.name != tool.spec.name)
                    .filter_map(|tool| tool.source.attachment.clone())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let mut block = String::from("<runtimes>\n");
        for attachment in &attachments {
            block.push_str(&format!(
                "<runtime name=\"{}\" kind=\"{}\"",
                escape_xml_attribute(attachment.name.as_str()),
                attachment.kind.as_str()
            ));
            if let Some(workspace) = &attachment.workspace {
                block.push_str(&format!(
                    " workspace=\"{}\"",
                    escape_xml_attribute(workspace)
                ));
            }
            if let Some(platform) = self
                .inner
                .runtimes
                .instance(attachment)
                .and_then(|instance| {
                    instance
                        .info()
                        .workspace()
                        .map(|workspace| workspace.platform().to_owned())
                })
            {
                block.push_str(&format!(
                    " platform=\"{}\"",
                    escape_xml_attribute(&platform)
                ));
            }
            block.push_str(&format!(
                " prefixed=\"{}\"/>\n",
                prefixed.contains(&attachment.name)
            ));
        }
        block.push_str("</runtimes>\n");
        block.push_str(RUNTIMES_GUIDANCE);
        let Some(composed_bytes) = policy
            .agent
            .composed_prompt
            .len()
            .checked_add(block.len() + PROMPT_BLOCK_SEPARATOR.len())
        else {
            eprintln!(
                "cookie-agent: skipping runtimes prompt section: composed prompt byte count overflowed"
            );
            return;
        };
        if composed_bytes > AgentSnapshot::MAX_PROMPT_BYTES {
            eprintln!(
                "cookie-agent: skipping runtimes prompt section: composed prompt exceeds {} bytes",
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

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::escape_xml_attribute;

    #[test]
    fn escapes_ampersand_before_angle_brackets_and_quotes() {
        assert_eq!(
            escape_xml_attribute("/tmp/a&b/<x>&\"q\""),
            "/tmp/a&amp;b/&lt;x&gt;&amp;&quot;q&quot;"
        );
    }
}
