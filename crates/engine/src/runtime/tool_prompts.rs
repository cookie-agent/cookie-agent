use cookie_agent_protocol::{AgentSnapshot, SessionId};

use crate::{
    Engine, EngineError, PromptSection, SessionToolContext, policy::FrozenRunPolicy,
    runtime::helpers::session_depth, runtime::prompt_blocks::PROMPT_BLOCK_SEPARATOR,
};

const MAX_SECTION_BODY_BYTES: usize = 8 * 1024;
const MAX_PROVIDER_BODY_BYTES: usize = 16 * 1024;
const MAX_TOTAL_BODY_BYTES: usize = 32 * 1024;

/// The tools an ACP agent is offered over the MCP bridge, by the provider
/// that publishes them: which providers' instruction sections its first
/// prompt carries (`docs/site/specs/acp-agents.md` section 15, 2026-10-23).
#[derive(Debug, Default)]
pub(crate) struct OfferedTools {
    /// `(provider ID, attachment)` of each provider with an offered tool.
    pub(crate) providers: std::collections::HashSet<(String, Option<String>)>,
    /// A memory tool is offered, and one that writes.
    pub(crate) memory: bool,
    pub(crate) memory_write: bool,
    /// The `skill` tool is offered.
    pub(crate) skill: bool,
}

impl OfferedTools {
    fn offers(&self, provider: &str, attachment: Option<&str>) -> bool {
        self.providers
            .contains(&(provider.to_owned(), attachment.map(str::to_owned)))
    }
}

impl Engine {
    /// The `tool_instructions` prompt block a run of `policy` composes now.
    pub(crate) fn live_tool_instructions_block(
        &self,
        policy: &FrozenRunPolicy,
        session: SessionId,
    ) -> Result<Option<String>, EngineError> {
        // A provider whose tools are all deferred gives its sections with
        // its first load instead (deferred tools in cookie, section 7.3).
        let deferred = self.deferred_prompt_providers(session, policy)?;
        self.tool_prompt_block(policy, session, None, &|id, attachment| {
            !deferred.contains(&(id.to_owned(), attachment.map(str::to_owned)))
        })
    }

    pub(crate) fn compose_tool_prompt_sections(
        &self,
        policy: &mut FrozenRunPolicy,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let Some(block) = self.live_tool_instructions_block(policy, session)? else {
            return Ok(());
        };
        let composed_bytes = policy
            .agent
            .composed_prompt
            .len()
            .checked_add(block.len() + PROMPT_BLOCK_SEPARATOR.len())
            .ok_or_else(|| {
                EngineError::ToolPrompt("composed prompt byte count overflowed".into())
            })?;
        if composed_bytes > AgentSnapshot::MAX_PROMPT_BYTES {
            return Err(EngineError::ToolPrompt(format!(
                "composed prompt exceeds {} bytes",
                AgentSnapshot::MAX_PROMPT_BYTES
            )));
        }

        crate::runtime::prompt_blocks::append_prompt_block(
            policy,
            crate::runtime::prompt_blocks::TOOL_INSTRUCTIONS_BLOCK,
            &block,
        );
        Ok(())
    }

    /// The `<tool_instructions>` sections of `session`'s tool providers
    /// for a run of `policy`, joined; `None` when there are none. With
    /// `offered`, only the providers that publish an offered tool give
    /// sections, and the memory section follows the offered memory tools.
    pub(crate) fn tool_prompt_block(
        &self,
        policy: &FrozenRunPolicy,
        session: SessionId,
        offered: Option<&OfferedTools>,
        keep: &dyn Fn(&str, Option<&str>) -> bool,
    ) -> Result<Option<String>, EngineError> {
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
        let providers = self.session_tool_sources(session)?;
        // The memory provider decides from the run's frozen policy, with the
        // visibility test the tool list uses (memory spec section 16.9.1).
        let mut memory_access = super::memory_runs::memory_access(
            true,
            &policy.agent,
            Some(&projection.permission_overlay),
        );
        if let Some(offered) = offered {
            memory_access.available &= offered.memory;
            memory_access.writable &= offered.memory_write;
        }
        let delegated = matches!(
            projection.meta.origin,
            cookie_agent_protocol::SessionOrigin::Delegated { .. }
        );
        let mut rendered = Vec::new();
        let mut total_body_bytes = 0usize;

        for crate::runtimes::SourcedProvider {
            provider,
            attachment,
            ..
        } in providers
        {
            let provider_id = provider.provider_id();
            validate_provider_id(provider_id)?;
            if !keep(provider_id, attachment.as_ref().map(|name| name.as_str())) {
                continue;
            }
            if let Some(offered) = offered
                && provider_id != crate::memory_tools::PROVIDER_ID
                && !offered.offers(provider_id, attachment.as_ref().map(|name| name.as_str()))
            {
                continue;
            }
            let sections = if provider_id == crate::memory_tools::PROVIDER_ID {
                crate::memory_tools::prompt_section(memory_access, delegated)
                    .map(|body| PromptSection {
                        title: "Memory".to_owned(),
                        body,
                    })
                    .into_iter()
                    .collect()
            } else {
                provider.prompt_sections(&context).map_err(|error| {
                    EngineError::ToolPrompt(format!("provider `{provider_id}`: {error}"))
                })?
            };
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

        Ok((!rendered.is_empty()).then(|| rendered.join("\n\n")))
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
