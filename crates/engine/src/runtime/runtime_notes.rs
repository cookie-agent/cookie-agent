//! What the model is told about where its tools run
//! (`docs/site/specs/memory.md` section 3.4): its attachments, nested in the
//! runtime that serves them and the machine that runtime runs on. `global`
//! is everywhere and is never mentioned. With at most one other
//! attachment, the system prompt says only where it works (its working
//! directory and machine). With several, the system prompt lists them by
//! machine and runtime, and each of their tools' descriptions names the
//! attachment it belongs to. A session in a project also names its project
//! (section 16.9.2): a `Project:` line, or a `project` attribute on the
//! `<machines>` block. An ACP agent's first prompt carries the same block
//! (`docs/site/specs/acp-agents.md` section 15, 2026-10-25); with several
//! attachments it also marks the one its harness's built-in tools run on.

use std::collections::HashMap;

use cookie_agent_protocol::{
    AgentSnapshot, AttachmentName, MachineName, RuntimeAttachment, SessionId,
};

use crate::{Engine, policy::FrozenRunPolicy};

pub(super) const SEVERAL_ATTACHMENTS: &str = concat!(
    "This session has several attachments. An attachment is a set of tools bound to a ",
    "workspace, served by a runtime running on a machine. Each tool's description names the ",
    "attachment it belongs to, and file tools and bash resolve relative paths against that ",
    "attachment's workspace. Attachments on the same machine share its files; a path on one ",
    "machine means nothing on another. A tool several attachments offer is named ",
    "`<attachment>_<tool>`.",
);

/// Where an ACP agent's harness runs its built-in tools: the runtime that
/// hosts the agent's process, and the working directory cookie gave the
/// agent in `session/new`.
pub(crate) struct BuiltinTools<'a> {
    pub(crate) runtime: &'a str,
    pub(crate) cwd: &'a str,
}

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
        let Some(block) = self.runtimes_block(session, None, Some(policy)) else {
            return;
        };
        if !crate::runtime::prompt_blocks::prompt_block_fits(policy, &block) {
            eprintln!(
                "cookie-agent: skipping the runtimes prompt: the composed prompt would exceed {} bytes",
                AgentSnapshot::MAX_PROMPT_BYTES
            );
            return;
        }
        crate::runtime::prompt_blocks::append_prompt_block(
            policy,
            crate::runtime::prompt_blocks::RUNTIMES_BLOCK,
            &block,
        );
    }

    /// What a run of a session with only `global` is told about reaching a
    /// machine (`docs/site/specs/machines-on-demand.md` section 2.4), naming
    /// only the tools the run is offered; `None` for a session with any
    /// other attachment, or without `global`.
    fn machine_reach(&self, policy: &FrozenRunPolicy, session: SessionId) -> Option<String> {
        let projection = self.inner.store.get(session).ok()?;
        let attachments = self.attachments_of(&projection);
        let global_only = matches!(
            attachments.as_slice(),
            [only] if only.runtime_id.as_str() == cookie_agent_protocol::RuntimeId::GLOBAL
        );
        if !global_only {
            return None;
        }
        let reach = self.reach_of(policy, &projection);
        Some(machine_reach_text(
            reach.lists,
            reach.attaches,
            reach.delegates,
        ))
    }

    /// Which of the machine tools a run of `policy` is offered.
    fn reach_of(
        &self,
        policy: &FrozenRunPolicy,
        projection: &crate::session::SessionProjection,
    ) -> Reach {
        let merged = crate::permissions::MergedRules::of(
            &policy.agent,
            Some(&projection.permission_overlay),
            None,
        );
        let lists = merged.offers(
            &cookie_agent_protocol::PermissionAction::LIST_RUNTIMES,
            Some(cookie_agent_protocol::RuntimeId::GLOBAL),
        );
        // `attach@<runtime>` rules count: some machine may be attached.
        let attaches = merged.rules().iter().any(|merged| {
            merged.rule.action == cookie_agent_protocol::PermissionAction::ATTACH
                && matches!(
                    merged.rule.effect,
                    cookie_agent_protocol::PermissionEffect::Allow
                        | cookie_agent_protocol::PermissionEffect::Ask
                )
        });
        let delegates = matches!(
            projection.meta.origin,
            cookie_agent_protocol::SessionOrigin::Root
        ) && !policy.delegate_targets(0).is_empty();
        Reach {
            lists,
            attaches,
            delegates,
        }
    }

    /// The block that tells a run of `session` where its tools work, if
    /// there is anything to say; for an ACP agent, `builtin` says where its
    /// harness's own tools run.
    ///
    /// With `policy`, a run's block: a session with only `global` hears how
    /// to reach a machine, and the tools listed per machine are the ones the
    /// run is offered. The system prompt and the live block a deferring
    /// model's announcements compare against are both this.
    pub(crate) fn runtimes_block(
        &self,
        session: SessionId,
        builtin: Option<&BuiltinTools<'_>>,
        policy: Option<&FrozenRunPolicy>,
    ) -> Option<String> {
        self.runtimes_block_with(session, builtin, policy, None)
    }

    /// [`Self::runtimes_block`] from `composition`, the tool set `policy`
    /// composes now, when the caller has it.
    pub(crate) fn runtimes_block_with(
        &self,
        session: SessionId,
        builtin: Option<&BuiltinTools<'_>>,
        policy: Option<&FrozenRunPolicy>,
        composition: Option<&crate::runtimes::Composition>,
    ) -> Option<String> {
        let project = self.session_project_name(session);
        let project = project.as_ref().map(|name| name.as_str());
        let facts = self.attachment_facts(session);
        if facts.is_empty()
            && let Some(policy) = policy
            && let Some(reach) = self.machine_reach(policy, session)
        {
            return Some(match project {
                Some(project) => format!("{reach}\nProject: {project}"),
                None => reach,
            });
        }
        // An ACP agent's bridge offers cookie's tools under their
        // attachments' names, so its prompt keeps naming attachments.
        if builtin.is_none()
            && let Ok(Some(machines)) = self.routing_machines(session)
            && !facts.is_empty()
        {
            let tools = self.machine_tools(session, policy, &machines, composition);
            let reach = policy
                .and_then(|policy| {
                    self.inner
                        .store
                        .get(session)
                        .ok()
                        .map(|projection| self.reach_of(policy, &projection))
                })
                .unwrap_or_default();
            let homes = self
                .runtime_templates()
                .into_iter()
                .map(|template| (template.runtime_id, template.listing.home))
                .collect::<HashMap<_, _>>();
            return Some(routed_runtimes_prompt(
                &facts, project, builtin, &tools, &reach, &homes,
            ));
        }
        runtimes_prompt(&facts, project, builtin)
    }

    /// The tools each attachment offers, routed by machine, as groups in
    /// composition order: the run's offered tools with `policy`, every
    /// tool without.
    fn machine_tools(
        &self,
        session: SessionId,
        policy: Option<&FrozenRunPolicy>,
        machines: &HashMap<AttachmentName, String>,
        composed: Option<&crate::runtimes::Composition>,
    ) -> HashMap<AttachmentName, Vec<(String, Vec<String>)>> {
        let mut tools = HashMap::<AttachmentName, Vec<(String, Vec<String>)>>::new();
        let owned;
        let composition = match (composed.filter(|_| policy.is_some()), policy) {
            (Some(composition), _) => composition,
            (None, Some(policy)) => match self.compose_for_policy(session, policy) {
                Ok(composition) => {
                    owned = composition;
                    &owned
                }
                Err(_) => return tools,
            },
            (None, None) => match self.compose_session_tools(session, |_, _| true) {
                Ok(composition) => {
                    owned = composition;
                    &owned
                }
                Err(_) => return tools,
            },
        };
        // Only an attachment's tools are listed: the loader tools a
        // composition may hold are the base layer's.
        for tool in &composition.tools {
            let Some(attachment) = tool
                .source
                .attachment
                .clone()
                .filter(|attachment| machines.contains_key(attachment))
            else {
                continue;
            };
            let group = crate::deferred_tools::group_of(&tool.source, &tool.spec.name).name;
            let groups = tools.entry(attachment).or_default();
            match groups.iter_mut().find(|(name, _)| *name == group) {
                Some((_, members)) => members.push(tool.spec.name.clone()),
                None => groups.push((group, vec![tool.spec.name.clone()])),
            }
        }
        tools
    }

    /// The machine of each of `session`'s attachments besides `global`,
    /// when its runtime tools are routed by `machine`
    /// (`docs/site/specs/machines-on-demand.md` section 5.1): the session
    /// has `global`, so it can attach machines, or attachments on two or
    /// more machines. `None` otherwise: a session created without `global`
    /// on one machine publishes its tools as their runtime declares them.
    /// A runtime the daemon cannot place is named by its runtime ID.
    pub(crate) fn routing_machines(
        &self,
        session: SessionId,
    ) -> Result<Option<HashMap<AttachmentName, String>>, crate::EngineError> {
        let attachments = self.session_runtimes(session)?;
        let has_global = attachments.iter().any(|attachment| {
            attachment.runtime_id.as_str() == cookie_agent_protocol::RuntimeId::GLOBAL
        });
        let machines = attachments
            .into_iter()
            .filter(|attachment| {
                attachment.runtime_id.as_str() != cookie_agent_protocol::RuntimeId::GLOBAL
            })
            .map(|attachment| {
                let machine = self
                    .runtime_machine(attachment.runtime_id.as_str())
                    .map_or_else(
                        || attachment.runtime_id.to_string(),
                        |machine| machine.to_string(),
                    );
                (attachment.name, machine)
            })
            .collect::<HashMap<_, _>>();
        let distinct = machines
            .values()
            .collect::<std::collections::HashSet<_>>()
            .len();
        Ok((has_global || distinct > 1).then_some(machines))
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

/// Which of the machine tools a run is offered.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Reach {
    lists: bool,
    attaches: bool,
    delegates: bool,
}

/// The largest tool group the machines block lists by name.
const LISTED_GROUP: usize = 8;

/// The machines block of a session whose tools are routed by `machine`
/// (`docs/site/specs/machines-on-demand.md` section 5.4): each machine with
/// its directory and the tools it offers by group, and which machine is
/// each tool's default.
fn routed_runtimes_prompt(
    facts: &[AttachmentFacts],
    project: Option<&str>,
    builtin: Option<&BuiltinTools<'_>>,
    tools: &HashMap<AttachmentName, Vec<(String, Vec<String>)>>,
    reach: &Reach,
    homes: &HashMap<String, Option<String>>,
) -> String {
    let mut block = String::from(
        "This session is attached to the following machines. Tools that take a `machine` \
         argument run on the machine you name in it; when you leave it out, they run on the \
         tool's default machine, which each tool's definition names and which is marked \
         default below. Relative paths resolve against the machine's directory, and a path on \
         one machine means nothing on another.",
    );
    match (reach.lists, reach.attaches) {
        (true, true) => block.push_str(
            " To see every machine and runtime you could attach, call list_runtimes; attach \
             one with attach_machine.",
        ),
        (true, false) => {
            block.push_str(" To see every machine and runtime there is, call list_runtimes.")
        }
        (false, true) => block.push_str(" Attach another machine with attach_machine."),
        (false, false) => {}
    }
    block.push('\n');
    block.push_str(&project.map_or_else(
        || "<machines>".to_owned(),
        |project| format!("<machines project=\"{}\">", escape_xml_attribute(project)),
    ));
    block.push('\n');
    let host = builtin.and_then(|builtin| builtin_host(facts, builtin));
    // A tool's default machine is the first, in attach order, to offer it.
    let mut defaulted = std::collections::HashSet::<&str>::new();
    let mut first = true;
    for machine in group_by_machine(facts) {
        let members = machine
            .runtimes
            .iter()
            .flat_map(|(_, members)| members.iter().copied())
            .collect::<Vec<_>>();
        block.push_str("<machine");
        if let Some(name) = machine.name {
            block.push_str(&format!(
                " name=\"{}\"",
                escape_xml_attribute(name.as_str())
            ));
        } else if let Some(member) = members.first() {
            block.push_str(&format!(
                " name=\"{}\"",
                escape_xml_attribute(&member.runtime)
            ));
        }
        if let Some(platform) = members.iter().find_map(|member| member.platform.as_deref()) {
            block.push_str(&format!(" platform=\"{}\"", escape_xml_attribute(platform)));
        }
        if let Some(home) = members
            .iter()
            .find_map(|member| homes.get(&member.runtime).cloned().flatten())
        {
            block.push_str(&format!(" home=\"{}\"", escape_xml_attribute(&home)));
        }
        if let Some(workspace) = members
            .iter()
            .find_map(|member| member.workspace.as_deref())
        {
            block.push_str(&format!(
                " directory=\"{}\"",
                escape_xml_attribute(workspace)
            ));
        }
        if first {
            block.push_str(" default=\"true\"");
        }
        if members
            .iter()
            .any(|member| host.is_some_and(|host| std::ptr::eq(host, *member)))
        {
            block.push_str(" builtin_tools=\"true\"");
        }
        block.push_str(">\n");
        for member in &members {
            for (group, names) in tools.get(&member.name).into_iter().flatten() {
                let new_defaults = names
                    .iter()
                    .filter(|name| defaulted.insert(name.as_str()))
                    .count();
                let marker = if !first && new_defaults == names.len() {
                    " (default)"
                } else {
                    ""
                };
                block.push_str(&format!("{group}{marker}: {}\n", group_listing(names)));
            }
        }
        block.push_str("</machine>\n");
        first = false;
    }
    block.push_str("</machines>");
    if let Some(builtin) = builtin {
        block.push('\n');
        block.push_str(&builtin_note(host, builtin));
    }
    block
}

/// A group's tools: listed by name up to [`LISTED_GROUP`], else as the
/// prefix their names share with how many there are.
fn group_listing(names: &[String]) -> String {
    if names.len() <= LISTED_GROUP {
        return names.join(", ");
    }
    let mut prefix = names[0].clone();
    for name in &names[1..] {
        while !name.starts_with(&prefix) {
            prefix.pop();
        }
    }
    match prefix.rfind('_') {
        Some(end) if end > 0 => format!("{}* ({} tools)", &prefix[..=end], names.len()),
        _ => names.join(", "),
    }
}

/// The global-only paragraph: `lists` when the run is offered
/// `list_runtimes`, `attaches` when it may attach a machine, `delegates`
/// when it can delegate with `capabilities`.
pub(super) fn machine_reach_text(lists: bool, attaches: bool, delegates: bool) -> String {
    let mut text =
        String::from("No machine is attached: you have no files or shell of your own yet.");
    if lists {
        text.push_str(" list_runtimes shows the user's machines.");
    }
    match (attaches, delegates) {
        (true, true) => text.push_str(
            " To work on one yourself, call attach_machine; to hand a task to a subagent \
             there, delegate with `capabilities`.",
        ),
        (true, false) => text.push_str(" To work on one, call attach_machine."),
        (false, true) => text.push_str(
            " Work that needs files or commands goes to a subagent there: delegate with \
             `capabilities` naming the machine's runtime and the directory to work in.",
        ),
        (false, false) => {}
    }
    text
}

/// The note that ends the description of each of `attachment`'s tools.
fn tool_note(attachment: &AttachmentFacts) -> String {
    format!("Belongs to {}.", placement(attachment))
}

/// `attachment `<name>` (runtime `<id>` on machine `<machine>`)`, without
/// the machine when the daemon cannot name it.
fn placement(attachment: &AttachmentFacts) -> String {
    match &attachment.machine {
        Some(machine) => format!(
            "attachment `{}` (runtime `{}` on machine `{machine}`)",
            attachment.name, attachment.runtime
        ),
        None => format!(
            "attachment `{}` (runtime `{}`)",
            attachment.name, attachment.runtime
        ),
    }
}

/// The attachment of `attachments` whose runtime hosts an ACP agent placed
/// by `builtin`: the one whose workspace is the agent's working directory,
/// else (the runtime's home) the runtime's first.
fn builtin_host<'a>(
    attachments: &'a [AttachmentFacts],
    builtin: &BuiltinTools<'_>,
) -> Option<&'a AttachmentFacts> {
    let mut on_runtime = attachments
        .iter()
        .filter(|attachment| attachment.runtime == builtin.runtime);
    on_runtime
        .clone()
        .find(|attachment| attachment.workspace.as_deref() == Some(builtin.cwd))
        .or_else(|| on_runtime.next())
}

/// What an ACP agent with several attachments is told about its harness's
/// built-in tools, which `host` serves (`None` when no listed attachment
/// does).
fn builtin_note(host: Option<&AttachmentFacts>, builtin: &BuiltinTools<'_>) -> String {
    let place = host.map_or_else(
        || format!("runtime `{}`", builtin.runtime),
        |host| format!("{}, marked `builtin_tools` above,", placement(host)),
    );
    format!(
        "Your harness's built-in tools run on {place} in working directory {}, and act on that \
         machine and workspace only. Cookie's tools from the `cookie` MCP server act on the \
         attachment their description or `<attachment>_` prefix names.",
        builtin.cwd
    )
}

/// The system prompt's block for `attachments` (those besides `global`) of
/// a session in `project`, if any. With several, an ACP agent's (`builtin`)
/// also says where its harness's built-in tools run.
fn runtimes_prompt(
    attachments: &[AttachmentFacts],
    project: Option<&str>,
    builtin: Option<&BuiltinTools<'_>>,
) -> Option<String> {
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
            let host = builtin.and_then(|builtin| builtin_host(several, builtin));
            let mut block = format!("{SEVERAL_ATTACHMENTS}\n{open}\n");
            for machine in group_by_machine(several) {
                push_machine(&mut block, &machine, host);
            }
            block.push_str("</machines>");
            if let Some(builtin) = builtin {
                block.push('\n');
                block.push_str(&builtin_note(host, builtin));
            }
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
/// the platform it reports. `host`, the attachment an ACP agent's built-in
/// tools run on, is marked `builtin_tools="true"`.
fn push_machine(block: &mut String, machine: &MachineGroup<'_>, host: Option<&AttachmentFacts>) {
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
            if host.is_some_and(|host| std::ptr::eq(host, *attachment)) {
                block.push_str(" builtin_tools=\"true\"");
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
        AttachmentFacts, BuiltinTools, SEVERAL_ATTACHMENTS, escape_xml_attribute, runtimes_prompt,
        tool_note,
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
            kind: cookie_agent_protocol::RuntimeKind::CODING.into(),
            workspace: workspace.map(str::to_owned),
            platform: platform.map(str::to_owned),
            machine: machine
                .map(|machine| cookie_agent_protocol::MachineName::new(machine).unwrap()),
        }
    }

    #[test]
    fn one_attachment_gives_its_working_directory_and_machine() {
        assert_eq!(runtimes_prompt(&[], None, None), None);
        assert_eq!(
            runtimes_prompt(
                &[attachment(
                    "local_web",
                    "local",
                    Some("/home/me/web"),
                    Some("linux"),
                    Some("laptop")
                )],
                None,
                None
            ),
            Some("Working directory: /home/me/web\nMachine: laptop (linux)".into())
        );
        // No platform: the machine alone; no machine: no machine line.
        assert_eq!(
            runtimes_prompt(
                &[attachment("w", "local", Some("/w"), None, Some("laptop"))],
                None,
                None
            ),
            Some("Working directory: /w\nMachine: laptop".into())
        );
        assert_eq!(
            runtimes_prompt(
                &[attachment("w", "local", Some("/w"), Some("linux"), None)],
                None,
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
                None,
                None
            ),
            None
        );
    }

    #[test]
    fn a_project_adds_a_line_or_an_attribute() {
        let one = attachment("w", "local", Some("/w"), None, Some("laptop"));
        assert_eq!(
            runtimes_prompt(std::slice::from_ref(&one), Some("web"), None),
            Some("Working directory: /w\nMachine: laptop\nProject: web".into())
        );
        assert_eq!(
            runtimes_prompt(&[], Some("web"), None),
            Some("Project: web".into())
        );
        let several = runtimes_prompt(
            &[
                one,
                attachment("v", "labpc", Some("/v"), None, Some("labpc")),
            ],
            Some("web"),
            None,
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
    fn an_acp_agent_with_one_attachment_is_told_what_a_model_run_is() {
        let builtin = BuiltinTools {
            runtime: "local",
            cwd: "/home/me/web",
        };
        let one = [attachment(
            "local_web",
            "local",
            Some("/home/me/web"),
            Some("linux"),
            Some("laptop"),
        )];
        assert_eq!(
            runtimes_prompt(&one, Some("web"), Some(&builtin)),
            Some("Working directory: /home/me/web\nMachine: laptop (linux)\nProject: web".into())
        );
        assert_eq!(
            runtimes_prompt(&one, None, Some(&builtin)),
            runtimes_prompt(&one, None, None)
        );
        // The runtime's home as the working directory: still nothing to say.
        let home = BuiltinTools {
            runtime: "desk",
            cwd: "/home/me",
        };
        assert_eq!(
            runtimes_prompt(
                &[attachment("desktop", "desk", None, None, Some("m"))],
                None,
                Some(&home)
            ),
            None
        );
    }

    #[test]
    fn several_attachments_mark_where_an_acp_agents_builtin_tools_run() {
        let attachments = [
            attachment(
                "local_web",
                "local",
                Some("/home/me/web"),
                Some("linux"),
                Some("laptop"),
            ),
            attachment(
                "local_api",
                "local",
                Some("/home/me/api"),
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
        ];
        // The agent's working directory is the second workspace on its
        // runtime: that attachment is marked, not the runtime's first.
        let block = runtimes_prompt(
            &attachments,
            Some("web"),
            Some(&BuiltinTools {
                runtime: "local",
                cwd: "/home/me/api",
            }),
        )
        .unwrap();
        assert_eq!(
            block,
            format!(
                "{SEVERAL_ATTACHMENTS}\n<machines project=\"web\">\n\
                 <machine name=\"laptop\" platform=\"linux\">\n\
                 <runtime name=\"local\">\n\
                 <attachment name=\"local_web\" kind=\"coding\" workspace=\"/home/me/web\"/>\n\
                 <attachment name=\"local_api\" kind=\"coding\" workspace=\"/home/me/api\" builtin_tools=\"true\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 <machine name=\"labpc\" platform=\"linux\">\n\
                 <runtime name=\"labpc\">\n\
                 <attachment name=\"labpc_api\" kind=\"coding\" workspace=\"/srv/api\"/>\n\
                 </runtime>\n\
                 </machine>\n\
                 </machines>\n\
                 Your harness's built-in tools run on attachment `local_api` (runtime `local` on \
                 machine `laptop`), marked `builtin_tools` above, in working directory \
                 /home/me/api, and act on that machine and workspace only. Cookie's tools from \
                 the `cookie` MCP server act on the attachment their description or \
                 `<attachment>_` prefix names."
            )
        );
        // A model run's block has neither.
        let model = runtimes_prompt(&attachments, Some("web"), None).unwrap();
        assert!(!model.contains("builtin_tools"), "{model}");
        assert!(!model.contains("built-in"), "{model}");
    }

    #[test]
    fn an_acp_agent_in_its_runtimes_home_marks_that_runtimes_attachment() {
        let attachments = [
            attachment("labpc_api", "labpc", Some("/srv/api"), None, Some("labpc")),
            attachment("desktop", "desk", None, None, None),
        ];
        let block = runtimes_prompt(
            &attachments,
            None,
            Some(&BuiltinTools {
                runtime: "desk",
                cwd: "/home/me",
            }),
        )
        .unwrap();
        assert!(
            block.contains("<attachment name=\"desktop\" kind=\"coding\" builtin_tools=\"true\"/>"),
            "{block}"
        );
        assert!(
            block.contains(
                "</machines>\nYour harness's built-in tools run on attachment `desktop` (runtime \
                 `desk`), marked `builtin_tools` above, in working directory /home/me, and act"
            ),
            "{block}"
        );
        // A runtime no listed attachment belongs to is named alone.
        let block = runtimes_prompt(
            &attachments,
            None,
            Some(&BuiltinTools {
                runtime: "other",
                cwd: "/x",
            }),
        )
        .unwrap();
        assert!(!block.contains("builtin_tools=\"true\""), "{block}");
        assert!(
            block.contains(
                "</machines>\nYour harness's built-in tools run on runtime `other` in working \
                 directory /x, and act"
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
