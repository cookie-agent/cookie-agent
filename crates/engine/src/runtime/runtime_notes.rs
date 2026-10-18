//! What the model is told about the runtimes its tools run on. `global` is
//! everywhere and is never mentioned. With at most one other runtime, the
//! system prompt says only where it works (its working directory and
//! platform). With several, the system prompt lists them and each of their
//! tools' descriptions names the runtime it runs on.

use std::collections::HashMap;

use cookie_agent_protocol::{
    AgentSnapshot, AttachmentName, RuntimeAttachment, SessionId, Sha256Digest,
};

use crate::{
    Engine,
    policy::FrozenRunPolicy,
    runtime::prompt_blocks::{PROMPT_BLOCK_SEPARATOR, push_prompt_block},
};

const SEVERAL_RUNTIMES: &str = concat!(
    "This session has several runtimes attached. Each tool's description names the runtime ",
    "it runs on, and file tools and bash resolve relative paths against that runtime's ",
    "workspace. A tool several runtimes offer is named `<runtime>_<tool>`.",
);

/// One runtime the model is told about.
struct RuntimeFacts {
    name: AttachmentName,
    kind: String,
    workspace: Option<String>,
    platform: Option<String>,
}

impl Engine {
    /// The session's runtimes besides `global`, with their platforms.
    fn runtime_facts(&self, session: SessionId) -> Vec<RuntimeFacts> {
        let Ok(attachments) = self.session_runtimes(session) else {
            return Vec::new();
        };
        attachments
            .into_iter()
            .filter(|attachment| {
                attachment.runtime_id.as_str() != cookie_agent_protocol::RuntimeId::GLOBAL
            })
            .map(|attachment| RuntimeFacts {
                platform: self.platform_of(&attachment),
                kind: attachment.kind.as_str().to_owned(),
                workspace: attachment.workspace,
                name: attachment.name,
            })
            .collect()
    }

    fn platform_of(&self, attachment: &RuntimeAttachment) -> Option<String> {
        self.inner
            .runtimes
            .instance(attachment)
            .and_then(|instance| {
                instance
                    .info()
                    .workspace()
                    .and_then(|workspace| workspace.platform().map(str::to_owned))
            })
    }

    /// Appends where the session's runtimes work to the run's prompt, frozen
    /// with it: the working directory of the one runtime besides `global`,
    /// or the list of several. Nothing when there is nothing to say, and
    /// skipped rather than truncated at the prompt's byte limit.
    pub(crate) fn compose_runtimes_prompt(&self, policy: &mut FrozenRunPolicy, session: SessionId) {
        let Some(block) = runtimes_prompt(&self.runtime_facts(session)) else {
            return;
        };
        let fits = policy
            .agent
            .composed_prompt
            .len()
            .checked_add(block.len() + PROMPT_BLOCK_SEPARATOR.len())
            .is_some_and(|bytes| bytes <= AgentSnapshot::MAX_PROMPT_BYTES);
        if !fits {
            eprintln!(
                "cookie-agent: skipping the runtimes prompt: the composed prompt would exceed {} bytes",
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

    /// The note each runtime's tools carry in their descriptions; empty
    /// unless the session attaches several runtimes besides `global`.
    pub(crate) fn runtime_tool_notes(&self, session: SessionId) -> HashMap<AttachmentName, String> {
        let facts = self.runtime_facts(session);
        if facts.len() < 2 {
            return HashMap::new();
        }
        facts
            .into_iter()
            .map(|runtime| {
                let note = format!("Runs on runtime `{}`.", runtime.name);
                (runtime.name, note)
            })
            .collect()
    }
}

/// The system prompt's block for `runtimes` (those besides `global`).
fn runtimes_prompt(runtimes: &[RuntimeFacts]) -> Option<String> {
    match runtimes {
        [] => None,
        [only] => {
            let workspace = only.workspace.as_deref()?;
            let mut block = format!("Working directory: {workspace}");
            if let Some(platform) = &only.platform {
                block.push_str(&format!("\nPlatform: {platform}"));
            }
            Some(block)
        }
        several => {
            let mut block = format!("{SEVERAL_RUNTIMES}\n<runtimes>\n");
            for runtime in several {
                block.push_str(&format!(
                    "<runtime name=\"{}\" kind=\"{}\"",
                    escape_xml_attribute(runtime.name.as_str()),
                    escape_xml_attribute(&runtime.kind)
                ));
                if let Some(workspace) = &runtime.workspace {
                    block.push_str(&format!(
                        " workspace=\"{}\"",
                        escape_xml_attribute(workspace)
                    ));
                }
                if let Some(platform) = &runtime.platform {
                    block.push_str(&format!(" platform=\"{}\"", escape_xml_attribute(platform)));
                }
                block.push_str("/>\n");
            }
            block.push_str("</runtimes>");
            Some(block)
        }
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
    use super::{RuntimeFacts, SEVERAL_RUNTIMES, escape_xml_attribute, runtimes_prompt};

    fn runtime(name: &str, workspace: Option<&str>, platform: Option<&str>) -> RuntimeFacts {
        RuntimeFacts {
            name: cookie_agent_protocol::AttachmentName::new(name).unwrap(),
            kind: "coding".into(),
            workspace: workspace.map(str::to_owned),
            platform: platform.map(str::to_owned),
        }
    }

    #[test]
    fn one_runtime_gives_its_working_directory_and_several_are_listed() {
        assert_eq!(runtimes_prompt(&[]), None);
        assert_eq!(
            runtimes_prompt(&[runtime("local_web", Some("/home/me/web"), Some("linux"))]),
            Some("Working directory: /home/me/web\nPlatform: linux".into())
        );
        assert_eq!(
            runtimes_prompt(&[runtime("desktop", None, Some("linux"))]),
            None
        );
        assert_eq!(
            runtimes_prompt(&[
                runtime("local_web", Some("/home/me/web"), Some("linux")),
                runtime("labpc_api", Some("/srv/a&\"b\""), None),
            ]),
            Some(format!(
                "{SEVERAL_RUNTIMES}\n<runtimes>\n<runtime name=\"local_web\" kind=\"coding\" workspace=\"/home/me/web\" platform=\"linux\"/>\n<runtime name=\"labpc_api\" kind=\"coding\" workspace=\"/srv/a&amp;&quot;b&quot;\"/>\n</runtimes>"
            ))
        );
    }

    #[test]
    fn escapes_ampersand_before_angle_brackets_and_quotes() {
        assert_eq!(
            escape_xml_attribute("/tmp/a&b/<x>&\"q\""),
            "/tmp/a&amp;b/&lt;x&gt;&amp;&quot;q&quot;"
        );
    }
}
