//! `list_agents`, a tool of the `global` runtime (`docs/site/specs/cron.md`
//! section 8.1), provider `builtin.agents`: the agents a model may pick for
//! a delegation or a new-session cron job, with what each is and the
//! permissions a run of it starts with.
//!
//! It has no permission action of its own: composition offers it exactly
//! when the run is offered `delegate_subagent` or a cron tool, and it
//! reveals nothing the caller could not use. Like the memory tools, the
//! provider is built with the `global` runtime, before the engine exists,
//! and holds the engine through a handle bound once the engine is made.

use std::{
    fmt::Write as _,
    sync::{Arc, OnceLock, Weak},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, PermissionAction, PermissionRule,
    PersistedToolResult as ToolResult, PreparedBindingLifetime, RunId, RuntimeId, SessionId,
    WildcardPattern,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title},
};
use serde::{Deserialize, Serialize};

use crate::{Engine, runtime::Inner};

/// The provider's ID.
pub(crate) const PROVIDER_ID: &str = "builtin.agents";
pub(crate) const LIST_AGENTS: &str = "list_agents";

const DESCRIPTION: &str = "List the agents you can pick: for `delegate` the subagents delegate_subagent can start from here, for `cron` the agents a new-session cron job can run. Each shows its mode, description, presets, models, and the permission rules its document declares, with the actions left to the daemon's defaults on one `defaults:` line; the defaults themselves are listed once at the end. An ACP agent (an external coding agent such as Claude Code, marked `acp`) shows the runtimes that offer it and whether it runs here: it runs on the one attached runtime that offers it, and not with none or several. Leave `for` out to list both.";

/// What an agent is listed for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Use {
    Delegate,
    Cron,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    use_for: Option<Use>,
}

/// The `list_agents` tool, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct AgentsTool {
    engine: OnceLock<Weak<Inner>>,
}

impl AgentsTool {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Gives the provider its engine. Later calls are ignored.
    pub(crate) fn bind(&self, engine: &Engine) {
        let _ = self.engine.set(Arc::downgrade(&engine.inner));
    }

    fn engine(&self) -> Result<Engine, ToolError> {
        self.engine
            .get()
            .and_then(Weak::upgrade)
            .map(|inner| Engine { inner })
            .ok_or_else(|| ToolError::execution("the engine is gone"))
    }

    pub(crate) fn spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: ToolConcurrency::Parallel,
            result_truncation: ToolResultTruncationPolicy::OptOut,
            name: LIST_AGENTS.into(),
            permission_name: None,
            description: DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "for": {"type": "string", "enum": ["delegate", "cron"],
                        "description": "List only the agents usable for this"}
                }
            }),
        }
    }
}

/// `list_agents` as the `global` runtime publishes it, for the tool
/// reference.
#[must_use]
pub fn agents_tool_spec() -> ToolSpec {
    AgentsTool::spec()
}

fn check_name(tool: &str) -> Result<(), ToolError> {
    if tool == LIST_AGENTS {
        Ok(())
    } else {
        Err(ToolError::execution(format!(
            "the agents provider received another tool, `{tool}`"
        )))
    }
}

const fn effect_name(effect: cookie_agent_protocol::PermissionEffect) -> &'static str {
    match effect {
        cookie_agent_protocol::PermissionEffect::Allow => "allow",
        cookie_agent_protocol::PermissionEffect::Ask => "ask",
        cookie_agent_protocol::PermissionEffect::Deny => "deny",
    }
}

/// `rules` grouped by key (`action` or `action@runtime`) in first-seen
/// order, each with its rules' text: `allow` for a lone `*` rule, else
/// `pattern effect` per rule, joined by `; `.
fn key_lines<'a>(rules: impl IntoIterator<Item = &'a PermissionRule>) -> Vec<(String, String)> {
    let mut keys = Vec::<(String, Vec<&PermissionRule>)>::new();
    for rule in rules {
        let key = match &rule.runtime {
            Some(runtime) => format!("{}@{runtime}", rule.action),
            None => rule.action.to_string(),
        };
        match keys.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, group)) => group.push(rule),
            None => keys.push((key, vec![rule])),
        }
    }
    keys.into_iter()
        .map(|(key, group)| {
            let text = if let [rule] = group[..]
                && rule.resource.as_str() == "*"
            {
                effect_name(rule.effect).to_owned()
            } else {
                group
                    .iter()
                    .map(|rule| format!("{} {}", rule.resource.as_str(), effect_name(rule.effect)))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            (key, text)
        })
        .collect()
}

/// The permission lines of a run of `agent` with `rules`, each marked
/// whether the daemon supplied it: one line per key the document declares,
/// with `${agent_id}` expanded to `agent`, then one `defaults:` line naming
/// the actions whose rules are all the daemon's defaults.
fn permission_lines(agent: &AgentId, rules: &[(PermissionRule, bool)]) -> Vec<String> {
    let [agent_id, ..] = WildcardPattern::MEMORY_VARIABLES;
    let mut lines = key_lines(
        rules
            .iter()
            .filter(|(_, default)| !default)
            .map(|(rule, _)| rule),
    )
    .into_iter()
    .map(|(key, text)| format!("{key}: {}", text.replace(agent_id, agent.as_str())))
    .collect::<Vec<_>>();
    let mut defaults = Vec::<&str>::new();
    for (rule, _) in rules.iter().filter(|(_, default)| *default) {
        if !defaults.contains(&rule.action.as_str()) {
            defaults.push(rule.action.as_str());
        }
    }
    if !defaults.is_empty() {
        lines.push(format!("defaults: {}", defaults.join(", ")));
    }
    lines
}

/// The defaults section of `list_agents`: the daemon's default rules for a
/// root run (of a `primary` or `all` agent) when `root`, and for a subagent
/// (of a `subagent` or `all` agent) when `subagent`, each line noting the
/// modes it is limited to, from the same functions that supply them to runs
/// ([`crate::permissions::run_default_rules`]). `${agent_id}` reads as
/// `<agent id>`.
fn defaults_section(root: bool, subagent: bool) -> Vec<String> {
    use cookie_agent_config::AgentMode;
    let [agent_id, ..] = WildcardPattern::MEMORY_VARIABLES;
    let mut lines = vec!["Defaults (for actions an agent document names no key for):".to_owned()];
    let kinds = [
        (
            root,
            "as a root run",
            false,
            [AgentMode::All, AgentMode::Primary],
        ),
        (
            subagent,
            "as a subagent",
            true,
            [AgentMode::All, AgentMode::Subagent],
        ),
    ];
    for (_, title, delegated, modes) in kinds.into_iter().filter(|(shown, ..)| *shown) {
        // Each key's text, with the modes that get it, in first-seen order;
        // `all` agents come first, as they have every default either kind
        // has.
        let mut entries = Vec::<(String, String, Vec<AgentMode>)>::new();
        for mode in modes {
            let rules = crate::permissions::run_default_rules(mode, delegated);
            for (key, text) in key_lines(&rules) {
                match entries
                    .iter_mut()
                    .find(|(existing, existing_text, _)| *existing == key && *existing_text == text)
                {
                    Some((.., with)) => with.push(mode),
                    None => entries.push((key, text, vec![mode])),
                }
            }
        }
        lines.push(format!("  {title}:"));
        for (key, text, with) in entries {
            let text = text.replace(agent_id, "<agent id>");
            if with.len() == modes.len() {
                lines.push(format!("    {key}: {text}"));
            } else {
                let names = with.iter().map(|mode| mode_name(*mode)).collect::<Vec<_>>();
                lines.push(format!(
                    "    {key}: {text} (only agents of mode {})",
                    names.join(" or ")
                ));
            }
        }
    }
    if crate::permissions::run_default_rules(AgentMode::Internal, false).is_empty()
        && crate::permissions::run_default_rules(AgentMode::Internal, true).is_empty()
    {
        lines.push("  Internal agents get none.".to_owned());
    }
    lines
}

/// What each rule variable that `text` uses stands for, as one line.
fn variables_line(text: &str) -> Option<String> {
    let [_, session_machine, session_project] = WildcardPattern::MEMORY_VARIABLES;
    let meanings = [
        (
            session_machine,
            "each machine the session's attachments run on",
        ),
        (session_project, "the session's project"),
        (
            WildcardPattern::WORKSPACE_DIR_EXPRESSION,
            "the session's workspace",
        ),
    ]
    .into_iter()
    .filter(|(variable, _)| text.contains(variable))
    .map(|(variable, meaning)| format!("{variable} is {meaning}"))
    .collect::<Vec<_>>();
    (!meanings.is_empty()).then(|| format!("  Variables: {}.", meanings.join("; ")))
}

/// Whether an ACP agent runs with the runtimes `attached`: on the one
/// that offers it, or why not.
fn acp_placement_line(
    offers: &crate::runtime_snapshot::AcpOffers,
    attached: &[RuntimeId],
) -> String {
    let offering = offers
        .offers
        .iter()
        .filter(|offer| attached.contains(&offer.runtime))
        .collect::<Vec<_>>();
    match offering[..] {
        [] => format!(
            "no — needs one of: {} attached",
            crate::runtime_snapshot::join_runtimes(&offers.runtimes())
        ),
        [offer] if !offer.enabled => format!("no — disabled on runtime {}", offer.runtime),
        [offer] if !offer.connected => {
            format!("no — runtime {} is not connected", offer.runtime)
        }
        [offer] => format!("yes, on runtime {}", offer.runtime),
        _ => format!(
            "no — {} attached runtimes offer it: {}",
            if offering.len() == 2 {
                "two"
            } else {
                "several"
            },
            offering
                .iter()
                .map(|offer| offer.runtime.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn mode_name(mode: cookie_agent_config::AgentMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|mode| mode.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl Engine {
    /// The text of `list_agents` for the active run `run` of `session`.
    fn list_agents_text(
        &self,
        session: SessionId,
        run: RunId,
        filter: Option<Use>,
    ) -> Result<String, ToolError> {
        let policy = self
            .active_run_policy(run)
            .ok_or_else(|| ToolError::execution("the calling run is not active"))?;
        let projection = self
            .inner
            .store
            .get(session)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let depth = match projection.meta.origin {
            cookie_agent_protocol::SessionOrigin::Delegated { depth, .. } => depth,
            cookie_agent_protocol::SessionOrigin::Root => 0,
        };
        let attached = self
            .session_runtime_ids(session)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok(render_agent_list(
            &policy,
            &projection.permission_overlay,
            depth,
            filter,
            &self.current_runtime(),
            &attached,
        ))
    }
}

/// What `list_agents` shows a run of `policy` at `depth`, with `overlay`
/// as its session's overlay: every enabled non-internal agent of the run's
/// registry it can use, for delegation (its delegation targets at that
/// depth) and for a new-session cron job (any, when the run is offered the
/// cron tools), as `filter` narrows it. An ACP agent says which runtimes
/// offer it and whether it can run with `attached`, the session's runtimes
/// (a delegated child inherits them).
pub(crate) fn render_agent_list(
    policy: &crate::policy::FrozenRunPolicy,
    overlay: &cookie_agent_protocol::SessionPermissionOverlay,
    depth: u32,
    filter: Option<Use>,
    runtime: &crate::runtime_snapshot::PublishedRuntime,
    attached: &[RuntimeId],
) -> String {
    let delegate_targets = policy.delegate_targets(depth);
    let cron_offered = crate::permissions::tool_visible(
        &policy.agent.permissions,
        Some(overlay),
        &PermissionAction::CRON,
        Some(RuntimeId::GLOBAL),
    );
    let wants = |use_for: Use| filter.is_none_or(|filter| filter == use_for);
    let presets_of = |agent: &AgentId| {
        runtime
            .agent_presets
            .iter()
            .filter(|(_, registry)| registry.get(agent).is_some())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };
    let mut output = String::new();
    let (mut shows_root, mut shows_subagent) = (false, false);
    for (id, agent) in policy.registry.agents() {
        let frontmatter = &agent.document.frontmatter;
        if !frontmatter.enabled || frontmatter.mode == cookie_agent_config::AgentMode::Internal {
            continue;
        }
        let mut uses = Vec::new();
        if wants(Use::Delegate) && delegate_targets.contains(id) {
            uses.push("delegate");
        }
        // A new-session job runs its agent as a root, which only `primary`
        // and `all` agents can be (cron_create refuses the others).
        let runs_as_root = matches!(
            frontmatter.mode,
            cookie_agent_config::AgentMode::Primary | cookie_agent_config::AgentMode::All
        );
        if wants(Use::Cron) && cron_offered && runs_as_root {
            uses.push("cron");
        }
        if uses.is_empty() {
            continue;
        }
        let mode = mode_name(frontmatter.mode);
        let tag = if agent.acp.is_some() { ", acp" } else { "" };
        let _ = writeln!(output, "{id} ({mode}{tag}) — {}", frontmatter.description);
        if let Some(offers) = &agent.acp {
            let runtimes = crate::runtime_snapshot::join_runtimes(&offers.runtimes());
            if offers.declaration() != id {
                let _ = writeln!(output, "  acp agent: {}", offers.declaration());
            }
            let _ = writeln!(output, "  offered by runtimes: {runtimes}");
            let _ = writeln!(
                output,
                "  runs here: {}",
                acp_placement_line(offers, attached)
            );
        }
        let presets = presets_of(id);
        if !presets.is_empty() {
            let _ = writeln!(output, "  presets: {}", presets.join(", "));
        }
        let models = agent
            .resolved_fallback
            .iter()
            .map(|fallback| match fallback {
                crate::runtime_snapshot::ResolvedAgentFallback::Selection { selection, .. } => {
                    match &selection.variant {
                        Some(variant) => format!("{} ({variant})", selection.model),
                        None => selection.model.to_string(),
                    }
                }
                crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { .. } => {
                    "the parent's model".to_owned()
                }
            })
            .collect::<Vec<_>>();
        // An ACP agent chooses its own model.
        if !models.is_empty() && agent.acp.is_none() {
            let _ = writeln!(output, "  models: {}", models.join(", "));
        }
        let _ = writeln!(output, "  usable for: {}", uses.join(", "));
        // A new-session job runs the agent as a root; a delegation as a
        // delegated run, whose defaults differ.
        let root = crate::permissions::agent_rules_with_sources(&agent.document, false);
        let delegated = crate::permissions::agent_rules_with_sources(&agent.document, true);
        let blocks = match (uses.contains(&"cron"), uses.contains(&"delegate")) {
            (true, true) if root != delegated => vec![
                ("permissions as a root run", root),
                ("permissions as a subagent", delegated),
            ],
            (true, _) => vec![("permissions", root)],
            (false, _) => vec![("permissions", delegated)],
        };
        shows_root |= uses.contains(&"cron");
        shows_subagent |= uses.contains(&"delegate");
        for (title, rules) in blocks {
            let _ = writeln!(output, "  {title}:");
            for line in permission_lines(id, &rules) {
                let _ = writeln!(output, "    {line}");
            }
        }
    }
    if output.is_empty() {
        return "No agents are usable from here.".to_owned();
    }
    output.push('\n');
    for line in defaults_section(shows_root, shows_subagent) {
        let _ = writeln!(output, "{line}");
    }
    if let Some(line) = variables_line(&output) {
        let _ = writeln!(output, "{line}");
    }
    output.push_str("\nActions not listed are denied.");
    output
}

#[async_trait]
impl ToolProvider for AgentsTool {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tool_group(&self) -> Option<cookie_agent_runtime_api::ToolGroup> {
        Some(cookie_agent_runtime_api::ToolGroup {
            name: "agents",
            summary: "The agents this session can delegate to or schedule, and what each is for.",
        })
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        check_name(tool_name)?;
        Ok(None)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        check_name(name)?;
        Ok((None, None))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        check_name(name)?;
        Ok(arguments
            .get("for")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        if call.name != LIST_AGENTS {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        }
        let primary = call
            .arguments
            .get("for")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        crate::base_tools::base_presentation(
            "List agents",
            cookie_agent_protocol::ToolHint::SEARCH,
            primary,
            Vec::new(),
        )
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        check_name(&call.name)?;
        let engine = self.engine()?;
        let args: ListAgentsArgs = parse_args(LIST_AGENTS, call.arguments)?;
        let resource = prepared_resource(
            PermissionAction::DELEGATE,
            "agents",
            b"agents",
            b"agents",
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            LIST_AGENTS,
            &args,
            vec![(PermissionAction::DELEGATE, "list_agents")],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        let normalized =
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(AgentsExecutor {
                engine,
                filter: args.use_for,
            }),
        )
    }
}

struct AgentsExecutor {
    engine: Engine,
    filter: Option<Use>,
}

#[async_trait]
impl PreparedExecutor for AgentsExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        let filter = self.filter;
        let (session, run) = (context.session, context.run);
        let output =
            tokio::task::spawn_blocking(move || engine.list_agents_text(session, run, filter))
                .await
                .map_err(|error| ToolError::execution(format!("list_agents failed: {error}")))??;
        Ok(ToolCompletion::single(ToolResult {
            title: safe_title("Listed agents"),
            display: crate::base_tools::markdown_display(&format!("```text\n{output}\n```")),
            output,
            background: None,
            retained_output: None,
            metadata: serde_json::json!({}),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{AgentId, RuntimeId};

    use super::acp_placement_line;
    use crate::runtime_snapshot::{AcpOffers, RuntimeAgent};

    fn offers(runtimes: &[(&str, bool, bool)]) -> AcpOffers {
        let id = AgentId::new("claude").unwrap();
        AcpOffers {
            offers: runtimes
                .iter()
                .map(|(runtime, enabled, connected)| RuntimeAgent {
                    id: id.clone(),
                    runtime: RuntimeId::new(*runtime).unwrap(),
                    agent: id.clone(),
                    description: String::new(),
                    enabled: *enabled,
                    connected: *connected,
                    known: Default::default(),
                })
                .collect(),
            id,
        }
    }

    #[test]
    fn an_acp_agent_says_whether_it_runs_with_the_sessions_runtimes() {
        let attached = |ids: &[&str]| {
            ids.iter()
                .map(|id| RuntimeId::new(*id).unwrap())
                .collect::<Vec<_>>()
        };
        let both = offers(&[("labpc", true, true), ("local", true, true)]);
        assert_eq!(
            acp_placement_line(&both, &attached(&["global", "local"])),
            "yes, on runtime local"
        );
        assert_eq!(
            acp_placement_line(&both, &attached(&["global"])),
            "no — needs one of: labpc, local attached"
        );
        assert_eq!(
            acp_placement_line(&both, &attached(&["labpc", "local"])),
            "no — two attached runtimes offer it: labpc, local"
        );
        let away = offers(&[("labpc", true, false), ("local", false, true)]);
        assert_eq!(
            acp_placement_line(&away, &attached(&["labpc"])),
            "no — runtime labpc is not connected"
        );
        assert_eq!(
            acp_placement_line(&away, &attached(&["local"])),
            "no — disabled on runtime local"
        );
    }
}
