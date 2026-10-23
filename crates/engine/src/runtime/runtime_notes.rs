//! What the model is told about where its tools run
//! (`docs/site/specs/memory.md` section 3.4): its attachments, nested in the
//! runtime that serves them and the machine that runtime runs on. `global`
//! is everywhere and is never mentioned. With at most one other
//! attachment, the system prompt says only where it works (its working
//! directory and machine). With several, the system prompt lists them by
//! machine and runtime, and each of their tools' descriptions names the
//! attachment it belongs to. A session in a project also names its project
//! (section 16.9.2): a `Project:` line, or a `project` attribute on the
//! `<machines>` block.

use std::collections::HashMap;

use cookie_agent_protocol::{
    AgentSnapshot, AttachmentName, MachineName, RuntimeAttachment, SessionId, Sha256Digest,
};

use crate::{
    Engine,
    policy::FrozenRunPolicy,
    runtime::prompt_blocks::{PROMPT_BLOCK_SEPARATOR, push_prompt_block},
};

const SEVERAL_ATTACHMENTS: &str = concat!(
    "This session has several attachments. An attachment is a set of tools bound to a ",
    "workspace, served by a runtime running on a machine. Each tool's description names the ",
    "attachment it belongs to, and file tools and bash resolve relative paths against that ",
    "attachment's workspace. Attachments on the same machine share its files; a path on one ",
    "machine means nothing on another. A tool several attachments offer is named ",
    "`<attachment>_<tool>`.",
);

/// One attachment the model is told about.
pub(super) struct AttachmentFacts {
    pub(super) name: AttachmentName,
    /// The runtime ID of the runtime serving it.
    runtime: String,
    kind: String,
    workspace: Option<String>,
    platform: Option<String>,
    /// The machine the runtime runs on, when the daemon can name it.
    pub(super) machine: Option<MachineName>,
}

impl Engine {
    /// The session's attachments besides `global`, in attach order, with
    /// their platforms and machines.
    pub(super) fn attachment_facts(&self, session: SessionId) -> Vec<AttachmentFacts> {
        let Ok(attachments) = self.session_runtimes(session) else {
            return Vec::new();
        };
        attachments
            .into_iter()
            .filter(|attachment| {
                attachment.runtime_id.as_str() != cookie_agent_protocol::RuntimeId::GLOBAL
            })
            .map(|attachment| AttachmentFacts {
                platform: self.platform_of(&attachment),
                machine: self.runtime_machine(attachment.runtime_id.as_str()),
                runtime: attachment.runtime_id.into_string(),
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
        let project = self.session_project(session).map(|project| project.name);
        let Some(block) = runtimes_prompt(
            &self.attachment_facts(session),
            project.as_ref().map(|name| name.as_str()),
        ) else {
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

    /// The note each attachment's tools carry in their descriptions; empty
    /// unless the session has several attachments besides `global`.
    pub(crate) fn runtime_tool_notes(&self, session: SessionId) -> HashMap<AttachmentName, String> {
        let facts = self.attachment_facts(session);
        if facts.len() < 2 {
            return HashMap::new();
        }
        facts
            .into_iter()
            .map(|attachment| (attachment.name.clone(), tool_note(&attachment)))
            .collect()
    }
}

/// The note that ends the description of each of `attachment`'s tools.
fn tool_note(attachment: &AttachmentFacts) -> String {
    match &attachment.machine {
        Some(machine) => format!(
            "Belongs to attachment `{}` (runtime `{}` on machine `{machine}`).",
            attachment.name, attachment.runtime
        ),
        None => format!(
            "Belongs to attachment `{}` (runtime `{}`).",
            attachment.name, attachment.runtime
        ),
    }
}

/// The system prompt's block for `attachments` (those besides `global`) of
/// a session in `project`, if any.
fn runtimes_prompt(attachments: &[AttachmentFacts], project: Option<&str>) -> Option<String> {
    let project_line = project.map(|project| format!("Project: {project}"));
    match attachments {
        [] => project_line,
        [only] => {
            let mut lines = Vec::new();
            if let Some(workspace) = only.workspace.as_deref() {
                let mut block = format!("Working directory: {workspace}");
                if let Some(machine) = &only.machine {
                    block.push_str(&format!("\nMachine: {machine}"));
                    if let Some(platform) = &only.platform {
                        block.push_str(&format!(" ({platform})"));
                    }
                }
                lines.push(block);
            }
            lines.extend(project_line);
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        several => {
            let open = project.map_or_else(
                || "<machines>".to_owned(),
                |project| format!("<machines project=\"{}\">", escape_xml_attribute(project)),
            );
            let mut block = format!("{SEVERAL_ATTACHMENTS}\n{open}\n");
            for machine in group_by_machine(several) {
                push_machine(&mut block, &machine);
            }
            block.push_str("</machines>");
            Some(block)
        }
    }
}

/// The attachments of one machine, by runtime, in first-named order.
struct MachineGroup<'a> {
    name: Option<&'a MachineName>,
    runtimes: Vec<(&'a str, Vec<&'a AttachmentFacts>)>,
}

/// `attachments` grouped by machine, machines in the order the attachments
/// first name them and runtimes within a machine likewise. Attachments
/// whose machine is unknown each get a machine of their own, since nothing
/// says they share files.
fn group_by_machine(attachments: &[AttachmentFacts]) -> Vec<MachineGroup<'_>> {
    let mut machines: Vec<MachineGroup<'_>> = Vec::new();
    for attachment in attachments {
        let known = attachment.machine.as_ref().and_then(|machine| {
            machines
                .iter_mut()
                .find(|group| group.name == Some(machine))
        });
        let group = match known {
            Some(group) => group,
            None => {
                machines.push(MachineGroup {
                    name: attachment.machine.as_ref(),
                    runtimes: Vec::new(),
                });
                machines.last_mut().expect("just pushed")
            }
        };
        match group
            .runtimes
            .iter_mut()
            .find(|(runtime, _)| *runtime == attachment.runtime)
        {
            Some((_, members)) => members.push(attachment),
            None => group
                .runtimes
                .push((attachment.runtime.as_str(), vec![attachment])),
        }
    }
    machines
}

/// One `<machine>` element. The platform is the machine's when every
/// attachment on it reports the same one; otherwise each runtime carries
/// the platform it reports.
fn push_machine(block: &mut String, machine: &MachineGroup<'_>) {
    let platforms = machine
        .runtimes
        .iter()
        .flat_map(|(_, members)| members.iter().map(|member| member.platform.as_deref()))
        .collect::<Vec<_>>();
    let shared = platforms
        .first()
        .copied()
        .flatten()
        .filter(|first| platforms.iter().all(|platform| *platform == Some(*first)));
    block.push_str("<machine");
    if let Some(name) = machine.name {
        block.push_str(&format!(
            " name=\"{}\"",
            escape_xml_attribute(name.as_str())
        ));
    }
    if let Some(platform) = shared {
        block.push_str(&format!(" platform=\"{}\"", escape_xml_attribute(platform)));
    }
    block.push_str(">\n");
    for (runtime, members) in &machine.runtimes {
        block.push_str(&format!(
            "<runtime name=\"{}\"",
            escape_xml_attribute(runtime)
        ));
        if shared.is_none()
            && let Some(platform) = members.iter().find_map(|member| member.platform.as_deref())
        {
            block.push_str(&format!(" platform=\"{}\"", escape_xml_attribute(platform)));
        }
        block.push_str(">\n");
        for attachment in members {
            block.push_str(&format!(
                "<attachment name=\"{}\" kind=\"{}\"",
                escape_xml_attribute(attachment.name.as_str()),
                escape_xml_attribute(&attachment.kind)
            ));
            if let Some(workspace) = &attachment.workspace {
                block.push_str(&format!(
                    " workspace=\"{}\"",
                    escape_xml_attribute(workspace)
                ));
            }
            block.push_str("/>\n");
        }
        block.push_str("</runtime>\n");
    }
    block.push_str("</machine>\n");
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
    use super::{
        AttachmentFacts, SEVERAL_ATTACHMENTS, escape_xml_attribute, runtimes_prompt, tool_note,
    };

    fn attachment(
        name: &str,
        runtime: &str,
        workspace: Option<&str>,
        platform: Option<&str>,
        machine: Option<&str>,
    ) -> AttachmentFacts {
        AttachmentFacts {
            name: cookie_agent_protocol::AttachmentName::new(name).unwrap(),
            runtime: runtime.to_owned(),
            kind: "coding".into(),
            workspace: workspace.map(str::to_owned),
            platform: platform.map(str::to_owned),
            machine: machine
                .map(|machine| cookie_agent_protocol::MachineName::new(machine).unwrap()),
        }
    }

    #[test]
    fn one_attachment_gives_its_working_directory_and_machine() {
        assert_eq!(runtimes_prompt(&[], None), None);
        assert_eq!(
            runtimes_prompt(
                &[attachment(
                    "local_web",
                    "local",
                    Some("/home/me/web"),
                    Some("linux"),
                    Some("laptop")
                )],
                None
            ),
            Some("Working directory: /home/me/web\nMachine: laptop (linux)".into())
        );
        // No platform: the machine alone; no machine: no machine line.
        assert_eq!(
            runtimes_prompt(
                &[attachment("w", "local", Some("/w"), None, Some("laptop"))],
                None
            ),
            Some("Working directory: /w\nMachine: laptop".into())
        );
        assert_eq!(
            runtimes_prompt(
                &[attachment("w", "local", Some("/w"), Some("linux"), None)],
                None
            ),
            Some("Working directory: /w".into())
        );
        // Without a workspace, one attachment adds nothing.
        assert_eq!(
            runtimes_prompt(
                &[attachment(
                    "desktop",
                    "desk",
                    None,
                    Some("linux"),
                    Some("m")
                )],
                None
            ),
            None
        );
    }

    #[test]
    fn a_project_adds_a_line_or_an_attribute() {
        let one = attachment("w", "local", Some("/w"), None, Some("laptop"));
        assert_eq!(
            runtimes_prompt(std::slice::from_ref(&one), Some("web")),
            Some("Working directory: /w\nMachine: laptop\nProject: web".into())
        );
        assert_eq!(
            runtimes_prompt(&[], Some("web")),
            Some("Project: web".into())
        );
        let several = runtimes_prompt(
            &[
                one,
                attachment("v", "labpc", Some("/v"), None, Some("labpc")),
            ],
            Some("web"),
        )
        .unwrap();
        assert!(
            several.contains("\n<machines project=\"web\">\n<machine name=\"laptop\">"),
            "{several}"
        );
        assert!(!several.contains("Project:"));
    }

    #[test]
    fn several_attachments_nest_in_their_runtimes_and_machines() {
        let block = runtimes_prompt(
            &[
                attachment(
                    "local_web",
                    "local",
                    Some("/home/me/web"),
                    Some("linux"),
                    Some("laptop"),
                ),
                attachment(
                    "labpc_api",
                    "labpc",
                    Some("/srv/api"),
                    Some("linux"),
                    Some("labpc"),
                ),
                attachment(
                    "local_api",
                    "local",
                    Some("/home/me/api"),
                    Some("linux"),
                    Some("laptop"),
                ),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            block,
            format!(
                "{SEVERAL_ATTACHMENTS}\n<machines>\n\
                 <machine name=\"laptop\" platform=\"linux\">\n\
                 <runtime name=\"local\">\n\
                 <attachment name=\"local_web\" kind=\"coding\" workspace=\"/home/me/web\"/>\n\
                 <attachment name=\"local_api\" kind=\"coding\" workspace=\"/home/me/api\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 <machine name=\"labpc\" platform=\"linux\">\n\
                 <runtime name=\"labpc\">\n\
                 <attachment name=\"labpc_api\" kind=\"coding\" workspace=\"/srv/api\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 </machines>"
            )
        );
    }

    #[test]
    fn runtimes_of_one_machine_with_different_platforms_carry_their_own() {
        let block = runtimes_prompt(
            &[
                attachment("host_a", "host", Some("/a"), Some("linux"), Some("pc")),
                attachment(
                    "box_b",
                    "box",
                    Some("/b&\"c\""),
                    Some("windows"),
                    Some("pc"),
                ),
                attachment("lost", "lost", None, None, None),
            ],
            None,
        )
        .unwrap();
        assert!(
            block.ends_with(
                "<machines>\n\
                 <machine name=\"pc\">\n\
                 <runtime name=\"host\" platform=\"linux\">\n\
                 <attachment name=\"host_a\" kind=\"coding\" workspace=\"/a\"/>\n\
                 </runtime>\n\
                 <runtime name=\"box\" platform=\"windows\">\n\
                 <attachment name=\"box_b\" kind=\"coding\" workspace=\"/b&amp;&quot;c&quot;\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 <machine>\n\
                 <runtime name=\"lost\">\n\
                 <attachment name=\"lost\" kind=\"coding\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 </machines>"
            ),
            "{block}"
        );
    }

    #[test]
    fn a_tool_note_names_the_attachment_its_runtime_and_its_machine() {
        assert_eq!(
            tool_note(&attachment("labpc_api", "labpc", None, None, Some("labpc"))),
            "Belongs to attachment `labpc_api` (runtime `labpc` on machine `labpc`)."
        );
        assert_eq!(
            tool_note(&attachment("box_b", "box", None, None, None)),
            "Belongs to attachment `box_b` (runtime `box`)."
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
