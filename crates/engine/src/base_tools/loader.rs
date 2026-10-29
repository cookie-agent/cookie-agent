//! `tool_search`, the loader tool of deferred tools
//! (`docs/site/specs/stable-prefix.md` section 2).
//!
//! A base-layer tool that declares no permission: it sees and loads only the
//! tools the session's latest request already offers. The engine publishes
//! it only when that request defers a tool.

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    base_presentation, parse_args, prepared_operation, prepared_resource, safe_title, schema,
    tool_error,
};
use crate::Engine;
use crate::deferred_tools::{DeferralView, DeferredEntry, TOOL_SEARCH};

/// How many matches a keyword query loads when the call sets no `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 5;

/// The prefix of a query that names the tools to load exactly.
const SELECT_PREFIX: &str = "select:";

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolSearchArgs {
    /// "select:<name>,<name>" loads those tools by exact name; any other text is keywords, such as "browser click", and loads the best matches. "+word" requires the word in a tool's name, as in "+slack send".
    query: String,
    /// At most how many keyword matches to load; 5 when absent, and every match when 0 or less. A select: query loads every name it lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
}

/// The loader tool.
pub struct LoaderTools {
    engine: Engine,
}

impl LoaderTools {
    pub(crate) fn new(engine: Engine) -> Self {
        Self { engine }
    }

    #[must_use]
    pub fn search_spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: cookie_agent_runtime_api::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: TOOL_SEARCH.into(),
            permission_name: None,
            description: "Load deferred tools so you can call them. Use \"select:<name>,<name>\" to load tools by the exact names an announcement lists, or keywords to find and load the best matches by name, group, description, and parameters; \"+word\" requires the word in a tool's name. Loaded tools stay callable.".into(),
            parameters: schema::<ToolSearchArgs>(),
        }
    }

    fn check(name: &str) -> Result<(), ToolError> {
        if name == TOOL_SEARCH {
            Ok(())
        } else {
            Err(ToolError::execution(
                "loader provider received another tool",
            ))
        }
    }

    fn argument(name: &str, arguments: &serde_json::Value) -> Result<String, ToolError> {
        Self::check(name)?;
        Ok(parse_args::<ToolSearchArgs>(TOOL_SEARCH, arguments.clone())?.query)
    }
}

#[async_trait]
impl ToolProvider for LoaderTools {
    fn provider_id(&self) -> &'static str {
        "builtin.loader"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::search_spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Self::check(tool_name)?;
        Ok(None)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((None, Some(Self::argument(name, arguments)?)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Self::argument(name, arguments)
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        match Self::argument(&call.name, &call.arguments) {
            Ok(argument) => match selected(&argument) {
                Some(names) => {
                    let names = names.join(", ");
                    base_presentation(
                        &format!("Load {names}"),
                        cookie_agent_protocol::ToolHint::GENERIC,
                        &names,
                        vec![("query", argument.clone())],
                    )
                }
                None => base_presentation(
                    &format!("Search tools: {argument}"),
                    cookie_agent_protocol::ToolHint::GENERIC,
                    &argument,
                    vec![("query", argument.clone())],
                ),
            },
            Err(_) => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::check(&call.name)?;
        let args: ToolSearchArgs = parse_args(TOOL_SEARCH, call.arguments)?;
        let request = match selected(&args.query) {
            Some(names) if names.is_empty() => {
                return Err(ToolError::execution("select: must name at least one tool"));
            }
            Some(names) => LoaderRequest::Select { names },
            None => LoaderRequest::Search {
                query: args.query.clone(),
                limit: match args.limit {
                    Some(limit) if limit <= 0 => usize::MAX,
                    Some(limit) => usize::try_from(limit).unwrap_or(usize::MAX),
                    None => DEFAULT_SEARCH_LIMIT,
                },
            },
        };
        let normalized = serde_json::to_value(&args).map_err(tool_error)?;
        // No rule is evaluated; the capability only names the operation.
        let action = PermissionAction::new(TOOL_SEARCH).map_err(tool_error)?;
        let binding = serde_json::to_vec(&normalized).map_err(tool_error)?;
        let resource = prepared_resource(
            action.clone(),
            "deferred-tools",
            TOOL_SEARCH.as_bytes(),
            &binding,
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            TOOL_SEARCH,
            &normalized,
            vec![(action, "load")],
            vec![resource],
            ctx.session.to_string().as_bytes(),
        )?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(LoaderExecutor {
                engine: self.engine.clone(),
                request,
            }),
        )
    }
}

/// The names a `select:` query lists, or `None` for a keyword query.
fn selected(query: &str) -> Option<Vec<String>> {
    let names = query.trim_start().strip_prefix(SELECT_PREFIX)?;
    Some(
        names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

enum LoaderRequest {
    Search { query: String, limit: usize },
    Select { names: Vec<String> },
}

struct LoaderExecutor {
    engine: Engine,
    request: LoaderRequest,
}

#[async_trait]
impl PreparedExecutor for LoaderExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_runtime_api::ToolCompletion, ToolError> {
        let view = self.engine.deferral_view(context.session);
        let result = match self.request {
            LoaderRequest::Search { query, limit } => search_result(&view, &query, limit),
            LoaderRequest::Select { names } => load_result(&view, &names, None),
        };
        Ok(cookie_agent_runtime_api::ToolCompletion::single(result))
    }
}

fn result(title: &str, output: String, loaded_tools: Vec<String>) -> ToolResult {
    ToolResult {
        title: safe_title(title),
        output,
        display: Vec::new(),
        background: None,
        retained_output: None,
        metadata: serde_json::Value::Null,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
        loaded_tools,
    }
}

/// The first sentence of a description, on one line.
fn first_sentence(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default();
    let end = line
        .char_indices()
        .find(|(index, character)| {
            *character == '.'
                && line[index + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map_or(line.len(), |(index, _)| index + 1);
    line[..end].trim().to_owned()
}

/// The words a tool is found by (section 5.1).
fn haystack(entry: &DeferredEntry) -> String {
    let mut text = format!(
        "{} {} {} {} {}",
        entry.name,
        entry.name.replace(['_', '-'], " "),
        entry.group.name,
        entry.group.summary.as_deref().unwrap_or_default(),
        entry.description
    );
    if let Some(properties) = entry
        .parameters
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        for (name, property) in properties {
            text.push(' ');
            text.push_str(name);
            if let Some(description) = property
                .get("description")
                .and_then(serde_json::Value::as_str)
            {
                text.push(' ');
                text.push_str(description);
            }
        }
    }
    text.to_lowercase()
}

/// Loads the best keyword matches for `query`, at most `limit` of them.
pub(crate) fn search_result(view: &DeferralView, query: &str, limit: usize) -> ToolResult {
    let words = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    // `+word` must be in the tool's name; every word counts toward the rank.
    let required = words
        .iter()
        .filter_map(|word| word.strip_prefix('+'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let terms = words
        .iter()
        .map(|word| word.trim_start_matches('+'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if view.deferred.is_empty() {
        return result(
            "No loadable tools",
            "No tools are loadable in this session.".into(),
            Vec::new(),
        );
    }
    let mut matches = view
        .deferred
        .iter()
        .filter(|entry| {
            let name = entry.name.to_lowercase();
            required.iter().all(|word| name.contains(word))
        })
        .filter_map(|entry| {
            let haystack = haystack(entry);
            let score = terms
                .iter()
                .filter(|term| haystack.contains(**term))
                .count();
            (score > 0).then_some((score, entry))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.name.cmp(&right.name))
    });
    if matches.is_empty() {
        return result(
            "No matching tools",
            format!(
                "No loadable tools match \"{query}\". Try other words, a group's name, or select:<name>."
            ),
            Vec::new(),
        );
    }
    let total = matches.len();
    let names = matches
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry.name.clone())
        .collect::<Vec<_>>();
    let more = (names.len() < total).then(|| {
        format!(
            "{} more match; raise limit or select them by name.",
            total - names.len()
        )
    });
    load_result(view, &names, more)
}

/// Loads `names`, recording the tools that newly load, and says why the
/// others did not; `more` ends the output.
pub(crate) fn load_result(
    view: &DeferralView,
    names: &[String],
    more: Option<String>,
) -> ToolResult {
    let mut loaded = Vec::new();
    let mut lines = Vec::new();
    let mut notes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let entry = view.deferred.iter().find(|entry| &entry.name == name);
        match entry {
            _ if view.eager.contains(name) => {
                notes.push(format!("{name}: already callable, no need to load it"));
            }
            None => notes.push(format!("{name}: no loadable tool has this name")),
            Some(entry) => {
                let already = if view.loaded.contains(name) {
                    " (already loaded)"
                } else {
                    loaded.push(name.clone());
                    ""
                };
                lines.push(format!(
                    "- {} [{}]{already}: {}",
                    entry.name,
                    entry.group.name,
                    first_sentence(&entry.description)
                ));
            }
        }
    }
    let mut output = match loaded.len() {
        0 => "Loaded no new tools.".to_owned(),
        1 => "Loaded 1 tool:".to_owned(),
        count => format!("Loaded {count} tools:"),
    };
    for line in lines {
        output.push('\n');
        output.push_str(&line);
    }
    for note in notes.into_iter().chain(more) {
        output.push('\n');
        output.push_str(&note);
    }
    let instructions = provider_instructions(view, &loaded);
    let mut result = result(
        if loaded.is_empty() {
            "No tools loaded"
        } else {
            "Tools loaded"
        },
        output,
        loaded,
    );
    result.additional_messages.extend(instructions);
    result
}

/// The prompt sections of the deferred providers whose tools `loaded`
/// loads first, as one message after the result (deferred tools in cookie,
/// section 7.3). Stored with the result, so history never shifts.
fn provider_instructions(
    view: &DeferralView,
    loaded: &[String],
) -> Option<cookie_agent_protocol::ToolEmittedMessage> {
    let mut providers = Vec::new();
    for name in loaded {
        if let Some(entry) = view.deferred.iter().find(|entry| &entry.name == name)
            && view.sections.contains_key(&entry.provider)
            && !providers.contains(&&entry.provider)
        {
            providers.push(&entry.provider);
        }
    }
    let blocks = providers
        .into_iter()
        .filter_map(|provider| view.sections.get(provider))
        .cloned()
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        return None;
    }
    cookie_agent_protocol::ToolEmittedMessage::new(
        cookie_agent_protocol::ToolEmittedMessageRole::System,
        vec![cookie_agent_protocol::ToolEmittedContent::Text(
            blocks.join("\n\n"),
        )],
    )
    .ok()
}

#[cfg(test)]
mod tests;
