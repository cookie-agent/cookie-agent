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

const DESCRIPTION: &str = "List the agents you can pick: for `delegate` the subagents delegate_subagent can start from here, for `cron` the agents a new-session cron job can run. Each shows its mode, description, presets, models, and the permission rules a run of it starts with, the daemon's defaults marked (default). Leave `for` out to list both.";

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
/// order, each line marked `(default)` when the daemon supplied its rules.
fn permission_lines(rules: &[(PermissionRule, bool)]) -> Vec<String> {
    let mut keys = Vec::<(String, Vec<&PermissionRule>, bool)>::new();
    for (rule, default) in rules {
        let key = match &rule.runtime {
            Some(runtime) => format!("{}@{runtime}", rule.action),
            None => rule.action.to_string(),
        };
        match keys.iter_mut().find(|(existing, ..)| *existing == key) {
            Some((_, group, _)) => group.push(rule),
            None => keys.push((key, vec![rule], *default)),
        }
    }
    keys.into_iter()
        .map(|(key, group, default)| {
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
            let mark = if default { " (default)" } else { "" };
            format!("{key}: {text}{mark}")
        })
        .collect()
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
        Ok(render_agent_list(
            &policy,
            &projection.permission_overlay,
            depth,
            filter,
            &self.current_runtime(),
        ))
    }
}

/// What `list_agents` shows a run of `policy` at `depth`, with `overlay`
/// as its session's overlay: every enabled non-internal agent of the run's
/// registry it can use, for delegation (its delegation targets at that
/// depth) and for a new-session cron job (any, when the run is offered the
/// cron tools), as `filter` narrows it.
pub(crate) fn render_agent_list(
    policy: &crate::policy::FrozenRunPolicy,
    overlay: &cookie_agent_protocol::SessionPermissionOverlay,
    depth: u32,
    filter: Option<Use>,
    runtime: &crate::runtime_snapshot::PublishedRuntime,
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
    for (id, agent) in policy.registry.agents() {
        let frontmatter = &agent.document.frontmatter;
        if !frontmatter.enabled || frontmatter.mode == cookie_agent_config::AgentMode::Internal {
            continue;
        }
        let mut uses = Vec::new();
        if wants(Use::Delegate) && delegate_targets.contains(id) {
            uses.push("delegate");
        }
        if wants(Use::Cron) && cron_offered {
            uses.push("cron");
        }
        if uses.is_empty() {
            continue;
        }
        let mode = serde_json::to_value(frontmatter.mode)
            .ok()
            .and_then(|mode| mode.as_str().map(str::to_owned))
            .unwrap_or_default();
        let _ = writeln!(output, "{id} ({mode}) — {}", frontmatter.description);
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
        if !models.is_empty() {
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
        for (title, rules) in blocks {
            let _ = writeln!(output, "  {title}:");
            for line in permission_lines(&rules) {
                let _ = writeln!(output, "    {line}");
            }
        }
    }
    if output.is_empty() {
        output.push_str("No agents are usable from here.");
    }
    output.trim_end().to_owned()
}

#[async_trait]
impl ToolProvider for AgentsTool {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
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
        }))
    }
}
