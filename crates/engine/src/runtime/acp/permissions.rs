//! An ACP agent's own permission requests
//! (`docs/site/specs/acp-agents.md` section 8).
//!
//! A `session/request_permission` names one of the agent's tool calls. Its
//! ACP kind maps onto one of cookie's actions, and its locations, command,
//! URL, or MCP tool onto that action's resources; the agent's merged rules
//! decide each, through the same pipeline as cookie's tools. A call of
//! cookie's own tools, which reach cookie's MCP server where the rules
//! decide them, and a `think` are allowed at once; `switch_mode`, every
//! kind that maps onto no action, and a file call that names no path go
//! through the approval flow, as does an `ask`. The answer is the agent's
//! allow-once or reject-once option, never an "always" one.

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalEvaluation, ApprovalId, ApprovalRequest, ApprovalResourceSource,
    ApprovalTrigger, DecisionTrace, PermissionAction, PermissionEffect, PreparedBindingLifetime,
    ToolCallId,
};
use cookie_agent_runtime_api::authoring::{prepared_operation, prepared_resource};
use serde_json::Value;

use super::super::{ActiveRun, Engine};
use super::mcp::BRIDGE_SERVER;
use super::wire::{self, ToolCallFields};
use crate::runtime_snapshot::RuntimeAgent;

/// What a permission request asks, by its mapping (spec section 8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MappedRequest {
    /// Allowed at once: a call of cookie's own tools, which cookie's MCP
    /// server decides when it arrives, or a `think`.
    Allow,
    /// Decided by the agent's rules: one action and the resources it
    /// needs, each of which must be allowed.
    Rules {
        action: PermissionAction,
        resources: Vec<String>,
    },
    /// Decided by the approval flow alone: `switch_mode`, every kind that
    /// maps onto no action, and a file call that names no path. `label`
    /// names the kind (an action label no rule decides) or the file call's
    /// action, `resource` what the request shows.
    Ask {
        label: PermissionAction,
        resource: String,
    },
}

/// The ACP tool kinds of ACP v1.
const KNOWN_KINDS: [&str; 10] = [
    "read",
    "edit",
    "delete",
    "move",
    "search",
    "execute",
    "think",
    "fetch",
    "switch_mode",
    "other",
];

/// The first string member of `input` among `names`.
fn input_string<'a>(input: Option<&'a Value>, names: &[&str]) -> Option<&'a str> {
    let input = input?;
    names
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

/// The command an `execute` call runs: its raw input's `command` (a string
/// or an argument vector), else its title.
fn command_of(call: &ToolCallFields) -> Option<String> {
    let input = call.raw_input.as_ref();
    if let Some(command) = input.and_then(|input| input.get("command")) {
        match command {
            Value::String(command) if !command.is_empty() => return Some(command.clone()),
            Value::Array(arguments) => {
                let words = arguments
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if !words.is_empty() {
                    return Some(words.join(" "));
                }
            }
            _ => {}
        }
    }
    input_string(input, &["cmd"])
        .map(str::to_owned)
        .or_else(|| call.title.clone())
}

/// The paths a file call touches, as labels
/// ([`cookie_agent_protocol::paths::path_label`]): its locations, else its
/// raw input's path. None when it names neither: its title is no path.
fn paths_of(call: &ToolCallFields) -> Vec<String> {
    let locations = call
        .locations
        .iter()
        .flatten()
        .map(|location| location.path.as_str())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    let paths = if locations.is_empty() {
        input_string(
            call.raw_input.as_ref(),
            &["path", "file_path", "filePath", "abs_path", "absPath"],
        )
        .into_iter()
        .collect()
    } else {
        locations
    };
    paths
        .into_iter()
        .map(cookie_agent_protocol::paths::path_label)
        .collect()
}

/// A file call of `action`: decided by the rules on the paths it names, or,
/// naming none, through the approval flow.
fn file_request(action: PermissionAction, call: &ToolCallFields) -> MappedRequest {
    let resources = paths_of(call);
    if resources.is_empty() {
        return MappedRequest::Ask {
            resource: call
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(action.as_str())
                .to_owned(),
            label: action,
        };
    }
    MappedRequest::Rules { action, resources }
}

/// The MCP server and tool a call of kind `other` (or of a kind ACP v1
/// does not know) goes to, as the adapters name them: Claude Code titles
/// it `mcp__<server>__<tool>`, Codex `Tool: <server>/<tool>` with
/// `{server, tool}` as its raw input, and Gemini CLI `<tool> (<server> MCP
/// Server)`. The whole title must have the form, so a command or a path
/// that merely contains one never matches.
fn mcp_tool_of(call: &ToolCallFields) -> Option<(String, String)> {
    let input = call.raw_input.as_ref();
    let from_input = input_string(input, &["server", "serverName", "server_name"])
        .zip(input_string(input, &["tool", "toolName", "tool_name"]));
    if let Some((server, tool)) = from_input {
        return Some((server.to_owned(), tool.to_owned()));
    }
    let title = call.title.as_deref()?.trim();
    let named = if let Some(rest) = title.strip_prefix("mcp__") {
        rest.split_once("__")
    } else if let Some(rest) = title.strip_prefix("Tool: ") {
        rest.split_once('/')
    } else if let Some(rest) = title.strip_suffix(" MCP Server)") {
        rest.rsplit_once(" (").map(|(tool, server)| (server, tool))
    } else {
        None
    };
    named
        .filter(|(server, tool)| {
            [server, tool].iter().all(|part| {
                !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c.is_control())
            })
        })
        .map(|(server, tool)| (server.to_owned(), tool.to_owned()))
}

/// The tool of cookie's own MCP bridge that `call` is the agent's call of,
/// by any agent's form that [`mcp_tool_of`] reads, or Claude Code's
/// `claudeCode.toolName` (`mcp__cookie__<tool>`).
pub(crate) fn bridge_tool_of(call: &ToolCallFields) -> Option<String> {
    let from_name = || {
        call.tool_name()?
            .strip_prefix("mcp__")?
            .split_once("__")
            .map(|(server, tool)| (server.to_owned(), tool.to_owned()))
    };
    mcp_tool_of(call)
        .or_else(from_name)
        .filter(|(server, tool)| server == BRIDGE_SERVER && !tool.is_empty())
        .map(|(_, tool)| tool)
}

/// A name as cookie publishes an MCP server's tools: ASCII letters,
/// digits, `_`, and `-` kept, everything else `_`.
fn mcp_name_part(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Maps the tool call a permission request names onto cookie's action
/// and resources: `call` as the request carries it, merged over `known`,
/// what the agent reported of the call before.
pub(crate) fn map_request(call: &ToolCallFields, known: Option<&ToolCallFields>) -> MappedRequest {
    let merged = merge(known, call);
    let kind = merged.kind.as_deref().unwrap_or("other");
    match kind {
        "read" | "search" => file_request(PermissionAction::READ, &merged),
        "edit" | "delete" | "move" => file_request(PermissionAction::WRITE, &merged),
        "execute" => MappedRequest::Rules {
            action: PermissionAction::BASH,
            resources: command_of(&merged).into_iter().collect(),
        },
        "fetch" => MappedRequest::Rules {
            action: PermissionAction::WEBFETCH,
            resources: input_string(merged.raw_input.as_ref(), &["url", "uri"])
                .map(str::to_owned)
                .or_else(|| merged.title.clone())
                .into_iter()
                .collect(),
        },
        "think" => MappedRequest::Allow,
        "switch_mode" => ask(kind, &merged),
        other => match mcp_tool_of(&merged) {
            Some((server, _)) if server == BRIDGE_SERVER => MappedRequest::Allow,
            Some((server, tool)) => MappedRequest::Rules {
                action: PermissionAction::MCP,
                resources: vec![format!(
                    "{}_{}",
                    mcp_name_part(&server),
                    mcp_name_part(&tool)
                )],
            },
            None if KNOWN_KINDS.contains(&other) => ask(other, &merged),
            None => ask("other", &merged),
        },
    }
}

fn ask(kind: &str, call: &ToolCallFields) -> MappedRequest {
    let label = PermissionAction::new(kind)
        .unwrap_or_else(|_| PermissionAction::new("other").expect("`other` is an action label"));
    let title = call.title.as_deref().map(str::trim).unwrap_or_default();
    MappedRequest::Ask {
        label,
        resource: if title.is_empty() {
            kind.to_owned()
        } else {
            title.to_owned()
        },
    }
}

/// `update` over `known`: each field the update carries replaces the known
/// one.
pub(crate) fn merge(known: Option<&ToolCallFields>, update: &ToolCallFields) -> ToolCallFields {
    let mut merged = known.cloned().unwrap_or_default();
    merged.tool_call_id.clone_from(&update.tool_call_id);
    if update.name.is_some() {
        merged.name.clone_from(&update.name);
    }
    if update.title.is_some() {
        merged.title.clone_from(&update.title);
    }
    if update.kind.is_some() {
        merged.kind.clone_from(&update.kind);
    }
    if update.status.is_some() {
        merged.status.clone_from(&update.status);
    }
    if update.content.is_some() {
        merged.content.clone_from(&update.content);
    }
    if update.locations.is_some() {
        merged.locations.clone_from(&update.locations);
    }
    if update.raw_input.is_some() {
        merged.raw_input.clone_from(&update.raw_input);
    }
    if update.raw_output.is_some() {
        merged.raw_output.clone_from(&update.raw_output);
    }
    match (&mut merged.meta, &update.meta) {
        (Some(Value::Object(known)), Some(Value::Object(new))) => {
            for (key, value) in new {
                known.insert(key.clone(), value.clone());
            }
        }
        (_, Some(new)) => merged.meta = Some(new.clone()),
        _ => {}
    }
    merged
}

/// The option of `kind` (`allow_once` or `reject_once`) the agent offered.
fn option_of<'a>(options: &'a [wire::PermissionOption], kind: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|option| option.kind == kind)
        .map(|option| option.option_id.as_str())
}

impl Engine {
    /// Decides ACP permission request `request` of the run `active` of
    /// `agent`, whose session works in `cwd` on the agent's runtime, home
    /// `home`: the answer to send the agent. An approval it asks for names
    /// `tool_call_id`, the cookie call the agent's call is shown as.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime) async fn decide_acp_permission(
        &self,
        active: &ActiveRun,
        run: cookie_agent_protocol::RunId,
        agent: &RuntimeAgent,
        cwd: &str,
        home: Option<&str>,
        request: &wire::PermissionRequest,
        known: Option<&ToolCallFields>,
        tool_call_id: Option<ToolCallId>,
    ) -> Value {
        let allowed = self
            .acp_permission_allowed(active, run, agent, cwd, home, request, known, tool_call_id)
            .await;
        let option = if allowed {
            option_of(&request.options, "allow_once")
        } else {
            option_of(&request.options, "reject_once")
        };
        wire::permission_outcome(option)
    }

    #[allow(clippy::too_many_arguments)]
    async fn acp_permission_allowed(
        &self,
        active: &ActiveRun,
        run: cookie_agent_protocol::RunId,
        agent: &RuntimeAgent,
        cwd: &str,
        home: Option<&str>,
        request: &wire::PermissionRequest,
        known: Option<&ToolCallFields>,
        tool_call_id: Option<ToolCallId>,
    ) -> bool {
        let (action, resources, by_rules) = match map_request(&request.tool_call, known) {
            MappedRequest::Allow => return true,
            MappedRequest::Rules { action, resources } => (action, resources, true),
            MappedRequest::Ask { label, resource } => (label, vec![resource], false),
        };
        if resources.is_empty() {
            return false;
        }
        let tool = format!("acp:{}", agent.id);
        let prepared = resources
            .iter()
            .map(|resource| {
                prepared_resource(
                    action.clone(),
                    action.as_str(),
                    resource.as_bytes(),
                    resource.as_bytes(),
                    PreparedBindingLifetime::RestartStable,
                    ApprovalResourceSource::PrimaryOperation,
                )
            })
            .collect::<Result<Vec<_>, _>>();
        let arguments = serde_json::json!({
            "tool_call": request.tool_call.tool_call_id,
            "action": action.as_str(),
            "resources": resources,
        });
        let Ok(prepared) = prepared else {
            return false;
        };
        let Ok(operation) = prepared_operation(
            &tool,
            &arguments,
            vec![(action.clone(), "request_permission")],
            prepared,
            agent.id.as_str().as_bytes(),
        ) else {
            return false;
        };
        let evaluations = if !by_rules {
            // No rule decides it: the kind maps onto no action, or the file
            // call names no path.
            operation
                .resources()
                .iter()
                .zip(&resources)
                .map(|(prepared, resource)| ApprovalEvaluation {
                    resource_digest: prepared.binding_digest.clone(),
                    effect: PermissionEffect::Ask,
                    trace: DecisionTrace {
                        action: action.clone(),
                        normalized_resource: resource.clone(),
                        candidates: Vec::new(),
                        effect: PermissionEffect::Ask,
                        precedence_reason: if action.is_path() {
                            format!(
                                "an ACP agent's `{}` request names no path and always asks",
                                action.as_str()
                            )
                        } else {
                            format!(
                                "an ACP agent's `{}` request maps onto no action and always asks",
                                action.as_str()
                            )
                        },
                    },
                })
                .collect()
        } else {
            let Ok(session) = self.inner.store.get(active.session) else {
                return false;
            };
            let overlay = session.permission_overlay.clone();
            let labels = resources.iter().cloned().map(Some).collect::<Vec<_>>();
            let workspace = std::path::Path::new(cwd);
            let decision = self.inner.permissions.decide_operation_with_grants(
                &active.policy.agent,
                Some(&overlay),
                None,
                &operation,
                &labels,
                crate::permissions::PermissionScope::runtime(
                    agent.runtime.as_str(),
                    Some(workspace),
                )
                .with_home(home),
            );
            match decision.effect {
                PermissionEffect::Allow => return true,
                PermissionEffect::Deny => return false,
                PermissionEffect::Ask => decision.evaluations,
            }
        };
        let tool_call_id = tool_call_id.unwrap_or_else(ToolCallId::new_v7);
        let request = match ApprovalRequest::new(
            ApprovalId::new_v7(),
            1,
            tool_call_id,
            ApprovalTrigger::PermissionPolicy,
            operation,
            evaluations,
            ApprovalConstraints {
                allow_once: true,
                // A tree grant names an action the rules decide; a request
                // no rule decides is answered once.
                allow_tree_grant: by_rules && !action.is_path(),
                cancellable: true,
                // The user's window starts at the escalation.
                expires_at: None,
            },
        ) {
            Ok(request) => request,
            Err(_) => return false,
        };
        let executor: cookie_agent_runtime_api::PreparedExecutorCell =
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(Box::new(AnsweredExecutor))));
        self.await_user_approval(
            active,
            run,
            request,
            executor,
            true,
            super::super::approval_flow::ApprovalToolInput {
                tool_call_id,
                origin: None,
            },
        )
        .await
        .is_ok_and(|outcome| outcome.approved)
    }
}

/// The executor behind an approval of an ACP permission request: the
/// agent runs the call itself, so there is nothing to run or revalidate.
struct AnsweredExecutor;

#[async_trait::async_trait]
impl cookie_agent_runtime_api::PreparedExecutor for AnsweredExecutor {
    async fn revalidate(&self) -> Result<(), cookie_agent_runtime_api::ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _ctx: cookie_agent_runtime_api::ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, cookie_agent_runtime_api::ToolError> {
        Err(cookie_agent_runtime_api::ToolError::execution(
            "an ACP agent runs its own calls",
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MappedRequest, map_request};
    use cookie_agent_protocol::PermissionAction;

    fn call(value: serde_json::Value) -> super::ToolCallFields {
        serde_json::from_value(value).expect("tool call")
    }

    fn rules(action: PermissionAction, resource: &str) -> MappedRequest {
        MappedRequest::Rules {
            action,
            resources: vec![resource.to_owned()],
        }
    }

    #[test]
    fn mcp_tools_map_by_each_adapters_naming() {
        // Claude Code, Codex, and Gemini CLI name an MCP tool each their way.
        for (fields, expected) in [
            (
                json!({"toolCallId": "a", "kind": "other", "title": "mcp__exa__web_search"}),
                rules(PermissionAction::MCP, "exa_web_search"),
            ),
            (
                json!({"toolCallId": "b", "kind": "other", "title": "Tool: exa/web.search",
                       "rawInput": {"server": "exa", "tool": "web.search", "arguments": {}}}),
                rules(PermissionAction::MCP, "exa_web_search"),
            ),
            (
                json!({"toolCallId": "c", "title": "web_search (exa MCP Server)"}),
                rules(PermissionAction::MCP, "exa_web_search"),
            ),
            (
                json!({"toolCallId": "d", "kind": "other", "title": "mcp__cookie__memory_read"}),
                MappedRequest::Allow,
            ),
            (
                json!({"toolCallId": "e", "kind": "other", "rawInput": {"server": "cookie", "tool": "cron_list"}}),
                MappedRequest::Allow,
            ),
            (
                json!({"toolCallId": "f", "kind": "other", "title": "memory_read (cookie MCP Server)"}),
                MappedRequest::Allow,
            ),
        ] {
            assert_eq!(
                map_request(&call(fields.clone()), None),
                expected,
                "{fields}"
            );
        }
    }

    #[test]
    fn calls_of_cookies_bridge_name_their_tool_in_each_adapters_form() {
        for fields in [
            json!({"toolCallId": "a", "title": "mcp__cookie__memory_read"}),
            json!({"toolCallId": "b", "title": "Read memory",
                   "_meta": {"claudeCode": {"toolName": "mcp__cookie__memory_read"}}}),
            json!({"toolCallId": "c", "rawInput": {"server": "cookie", "tool": "memory_read"}}),
            json!({"toolCallId": "d", "title": "memory_read (cookie MCP Server)"}),
        ] {
            assert_eq!(
                super::bridge_tool_of(&call(fields.clone())).as_deref(),
                Some("memory_read"),
                "{fields}"
            );
        }
        for fields in [
            json!({"toolCallId": "e", "title": "mcp__exa__web_search"}),
            json!({"toolCallId": "f", "title": "Read",
                   "_meta": {"claudeCode": {"toolName": "Read"}}}),
            json!({"toolCallId": "g", "title": "run mcp__cookie__memory_read"}),
        ] {
            assert_eq!(
                super::bridge_tool_of(&call(fields.clone())),
                None,
                "{fields}"
            );
        }
    }

    #[test]
    fn only_a_whole_title_names_an_mcp_tool() {
        for title in [
            "run mcp__cookie__memory_read",
            "mcp__cookie__memory_read now",
            "mcp__cookie__",
            "Tool: /x",
        ] {
            let mapped = map_request(
                &call(json!({"toolCallId": "a", "kind": "other", "title": title})),
                None,
            );
            assert!(
                matches!(&mapped, MappedRequest::Ask { label, .. } if label.as_str() == "other"),
                "{title}: {mapped:?}"
            );
        }
        // A command is a command, whatever it names.
        assert_eq!(
            map_request(
                &call(
                    json!({"toolCallId": "a", "kind": "execute", "title": "mcp__cookie__memory_read"})
                ),
                None,
            ),
            rules(PermissionAction::BASH, "mcp__cookie__memory_read")
        );
    }

    #[test]
    fn think_allows_and_the_rest_asks() {
        assert_eq!(
            map_request(&call(json!({"toolCallId": "a", "kind": "think"})), None),
            MappedRequest::Allow
        );
        for (kind, label) in [
            ("switch_mode", "switch_mode"),
            ("other", "other"),
            ("teleport", "other"),
            ("Not A Label", "other"),
        ] {
            let mapped = map_request(
                &call(json!({"toolCallId": "a", "kind": kind, "title": "Go to plan"})),
                None,
            );
            assert_eq!(
                mapped,
                MappedRequest::Ask {
                    label: PermissionAction::new(label).unwrap(),
                    resource: "Go to plan".to_owned(),
                },
                "{kind}"
            );
        }
    }

    #[test]
    fn file_paths_are_labels_with_dots_collapsed() {
        assert_eq!(
            map_request(
                &call(json!({"toolCallId": "a", "kind": "edit", "title": "Edit x",
                       "locations": [{"path": "/ws/../../etc/cron.d/x"}]})),
                None,
            ),
            rules(PermissionAction::WRITE, "/etc/cron.d/x")
        );
        assert_eq!(
            map_request(
                &call(json!({"toolCallId": "a", "kind": "read",
                       "rawInput": {"file_path": r"C:\ws\.\src\..\.env"}})),
                None,
            ),
            rules(PermissionAction::READ, "C:/ws/.env")
        );
    }

    #[test]
    fn a_file_call_naming_no_path_asks_by_its_title() {
        for (kind, action) in [
            ("edit", PermissionAction::WRITE),
            ("delete", PermissionAction::WRITE),
            ("read", PermissionAction::READ),
        ] {
            assert_eq!(
                map_request(
                    &call(json!({"toolCallId": "a", "kind": kind, "title": "Edit /etc/hosts"})),
                    None,
                ),
                MappedRequest::Ask {
                    label: action.clone(),
                    resource: "Edit /etc/hosts".to_owned(),
                },
                "{kind}"
            );
        }
        assert_eq!(
            map_request(&call(json!({"toolCallId": "a", "kind": "edit"})), None),
            MappedRequest::Ask {
                label: PermissionAction::WRITE,
                resource: "write".to_owned(),
            }
        );
    }

    #[test]
    fn the_known_call_fills_what_the_request_leaves_out() {
        let known = call(json!({"toolCallId": "a", "kind": "other", "title": "mcp__exa__search"}));
        assert_eq!(
            map_request(&call(json!({"toolCallId": "a"})), Some(&known)),
            rules(PermissionAction::MCP, "exa_search")
        );
    }
}
