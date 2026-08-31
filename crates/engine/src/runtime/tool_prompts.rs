use cookie_agent_protocol::{AgentSnapshot, SessionId, Sha256Digest};

use crate::{
    Engine, EngineError, PromptSection, SessionToolContext, policy::FrozenRunPolicy,
    runtime::helpers::session_depth,
};

const MAX_SECTION_BODY_BYTES: usize = 8 * 1024;
const MAX_PROVIDER_BODY_BYTES: usize = 16 * 1024;
const MAX_TOTAL_BODY_BYTES: usize = 32 * 1024;

impl Engine {
    pub(crate) fn compose_tool_prompt_sections(
        &self,
        policy: &mut FrozenRunPolicy,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        let depth = session_depth(&projection.meta.origin);
        let delegate_targets = policy
            .delegate_targets(depth)
            .into_iter()
            .filter_map(|target| {
                policy
                    .registry
                    .descriptors()
                    .iter()
                    .find(|descriptor| descriptor.id == target)
                    .map(|descriptor| (target, descriptor.description.clone()))
            })
            .collect();
        let context = SessionToolContext::for_prompt_composition(session, delegate_targets);
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut rendered = Vec::new();
        let mut total_body_bytes = 0usize;

        for provider in providers {
            let provider_id = provider.provider_id();
            validate_provider_id(provider_id)?;
            let sections = provider.prompt_sections(&context).map_err(|error| {
                EngineError::ToolPrompt(format!("provider `{provider_id}`: {error}"))
            })?;
            let mut provider_body_bytes = 0usize;
            for section in sections {
                let section = normalize_section(provider_id, section)?;
                let body_bytes = section.body.len();
                if body_bytes > MAX_SECTION_BODY_BYTES {
                    return Err(section_error(
                        provider_id,
                        &section.title,
                        format!("body exceeds {MAX_SECTION_BODY_BYTES} bytes"),
                    ));
                }
                provider_body_bytes =
                    provider_body_bytes.checked_add(body_bytes).ok_or_else(|| {
                        section_error(
                            provider_id,
                            &section.title,
                            "provider byte count overflowed",
                        )
                    })?;
                if provider_body_bytes > MAX_PROVIDER_BODY_BYTES {
                    return Err(section_error(
                        provider_id,
                        &section.title,
                        format!("provider bodies exceed {MAX_PROVIDER_BODY_BYTES} bytes"),
                    ));
                }
                total_body_bytes = total_body_bytes.checked_add(body_bytes).ok_or_else(|| {
                    section_error(provider_id, &section.title, "total byte count overflowed")
                })?;
                if total_body_bytes > MAX_TOTAL_BODY_BYTES {
                    return Err(section_error(
                        provider_id,
                        &section.title,
                        format!("all provider bodies exceed {MAX_TOTAL_BODY_BYTES} bytes"),
                    ));
                }
                rendered.push(render_section(provider_id, &section.body));
            }
        }

        if rendered.is_empty() {
            return Ok(());
        }
        let block = rendered.join("\n\n");
        let composed_bytes = policy
            .agent
            .composed_prompt
            .len()
            .checked_add(block.len() + 2)
            .ok_or_else(|| {
                EngineError::ToolPrompt("composed prompt byte count overflowed".into())
            })?;
        if composed_bytes > AgentSnapshot::MAX_PROMPT_BYTES {
            return Err(EngineError::ToolPrompt(format!(
                "composed prompt exceeds {} bytes",
                AgentSnapshot::MAX_PROMPT_BYTES
            )));
        }

        policy.agent.composed_prompt.push('\n');
        policy.agent.composed_prompt.push_str(&block);
        policy.agent.composed_prompt.push('\n');
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
        Ok(())
    }
}

fn normalize_section(
    provider_id: &str,
    mut section: PromptSection,
) -> Result<PromptSection, EngineError> {
    if section.title.trim().is_empty() || section.title.chars().any(char::is_control) {
        return Err(section_error(
            provider_id,
            &section.title,
            "title must be nonblank and control-free",
        ));
    }
    section.body = section.body.replace("\r\n", "\n").replace('\r', "\n");
    if section.body.trim().is_empty() {
        return Err(section_error(
            provider_id,
            &section.title,
            "body must not be blank",
        ));
    }
    if section
        .body
        .chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        return Err(section_error(
            provider_id,
            &section.title,
            "body contains a disallowed control character",
        ));
    }
    Ok(section)
}

fn validate_provider_id(provider_id: &str) -> Result<(), EngineError> {
    if provider_id.trim().is_empty() || provider_id.chars().any(char::is_control) {
        return Err(EngineError::ToolPrompt(
            "provider ID must be nonblank and control-free".into(),
        ));
    }
    Ok(())
}

fn render_section(provider_id: &str, body: &str) -> String {
    format!(
        "<tool_instructions provider=\"{}\">\n{body}\n</tool_instructions>",
        escape_attribute(provider_id)
    )
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn section_error(provider_id: &str, title: &str, detail: impl std::fmt::Display) -> EngineError {
    EngineError::ToolPrompt(format!(
        "provider `{provider_id}` section `{title}`: {detail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::{normalize_section, render_section};
    use crate::PromptSection;

    #[test]
    fn section_rendering_escapes_provider_and_has_exact_shape() {
        assert_eq!(
            render_section("plugin:\"local&review\"", "First\n\nSecond"),
            "<tool_instructions provider=\"plugin:&quot;local&amp;review&quot;\">\nFirst\n\nSecond\n</tool_instructions>"
        );
    }

    #[test]
    fn section_validation_normalizes_newlines_and_rejects_invalid_content() {
        let normalized = normalize_section(
            "test",
            PromptSection {
                title: "Valid".into(),
                body: "one\r\ntwo\rthree\tend".into(),
            },
        )
        .expect("valid section");
        assert_eq!(normalized.body, "one\ntwo\nthree\tend");

        for section in [
            PromptSection {
                title: "Bad\ntitle".into(),
                body: "body".into(),
            },
            PromptSection {
                title: "Blank".into(),
                body: " \n\t ".into(),
            },
            PromptSection {
                title: "Control".into(),
                body: "bad\u{0}body".into(),
            },
        ] {
            assert!(normalize_section("test", section).is_err());
        }
    }
}
