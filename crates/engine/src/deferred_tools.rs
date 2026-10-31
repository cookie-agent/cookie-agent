//! Deferred tools (`docs/site/specs/deferred-tools-cookie.md`): which tools a
//! request defers, the tool round its eager tools come from, and the
//! announcements that tell the model what it can load.
//!
//! Deferral is generic: [`policy_defers`] alone decides which composed tools
//! are deferred. Everything else here asks only whether a tool is deferred.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashSet},
};

use cookie_agent_config::{DEFERRED_BUILTIN_GROUPS, DeferredToolsConfig};
use cookie_agent_protocol::{
    AnnouncedToolGroup, EventPayload, RoundToolDefinition, RoundToolSource, ToolGroupSource,
};
use cookie_agent_runtime_api::ProvidedToolSource;
use oven_sdk::{JsonSchema, ToolDefinition};

use crate::events::DeferralFold;
use crate::runtimes::SourcedProvider;

pub(crate) mod record;

/// The loader tool: finds and loads deferred tools.
pub(crate) const TOOL_SEARCH: &str = "tool_search";
/// The retired loader that loaded deferred tools by name; history written
/// before `tool_search` loaded holds its calls.
pub(crate) const LEGACY_TOOL_LOAD: &str = "tool_load";

/// Whether `name` is a loader tool, the retired one included.
pub(crate) fn is_loader(name: &str) -> bool {
    matches!(name, TOOL_SEARCH | LEGACY_TOOL_LOAD)
}

/// The deferral policy: whether a composed tool is deferred on a model that
/// defers. It picks MCP and plugin tools (section 4), and the tools of the
/// built-in groups in [`DEFERRED_BUILTIN_GROUPS`] (stable prefix spec,
/// section 3), unless `config` sends their group up front.
pub(crate) fn policy_defers(
    source: &SourcedProvider,
    tool: &str,
    config: &DeferredToolsConfig,
) -> bool {
    match source.provider.tool_source(tool) {
        Some(ProvidedToolSource::Mcp { .. } | ProvidedToolSource::Plugin { .. }) => {
            !source.provider.always_load(tool)
        }
        None => source.provider.tool_group().is_some_and(|group| {
            DEFERRED_BUILTIN_GROUPS.contains(&group.name) && !config.always_loads(group.name)
        }),
    }
}

/// The group a tool is announced under (section 7.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Group {
    pub(crate) name: String,
    pub(crate) source: ToolGroupSource,
    pub(crate) summary: Option<String>,
    /// The summary is the source author's text, not cookie's.
    pub(crate) untrusted: bool,
}

pub(crate) fn group_of(source: &SourcedProvider, tool: &str) -> Group {
    match source.provider.tool_source(tool) {
        Some(ProvidedToolSource::Mcp { server }) => Group {
            name: server,
            source: ToolGroupSource::Mcp,
            summary: source.provider.source_summary(tool),
            untrusted: true,
        },
        Some(ProvidedToolSource::Plugin { plugin }) => Group {
            name: plugin,
            source: ToolGroupSource::Plugin,
            summary: source.provider.source_summary(tool),
            untrusted: true,
        },
        None => match source.provider.tool_group() {
            Some(group) => Group {
                name: group.name.to_owned(),
                source: ToolGroupSource::Builtin,
                summary: Some(group.summary.to_owned()),
                untrusted: false,
            },
            None => Group {
                name: source.attachment.as_ref().map_or_else(
                    || {
                        let id = source.provider.provider_id();
                        id.strip_prefix("builtin.").unwrap_or(id).to_owned()
                    },
                    ToString::to_string,
                ),
                source: ToolGroupSource::Builtin,
                summary: None,
                untrusted: false,
            },
        },
    }
}

/// A tool provider as prompt sections know it: its ID and the attachment
/// that offers it.
pub(crate) type ProviderKey = (String, Option<String>);

/// How a round records a provider in `prompt_groups`.
pub(crate) fn provider_label((id, attachment): &ProviderKey) -> String {
    attachment
        .as_ref()
        .map_or_else(|| id.clone(), |attachment| format!("{id}@{attachment}"))
}

/// The providers whose prompt sections leave the system prompt: each has a
/// tool, and none of its tools is eager (section 7.3).
pub(crate) fn deferred_providers<'a>(
    tools: impl IntoIterator<Item = (&'a str, &'a ProviderKey)>,
    eager: &BTreeSet<String>,
) -> BTreeSet<ProviderKey> {
    let mut providers = BTreeMap::<&ProviderKey, bool>::new();
    for (name, provider) in tools {
        *providers.entry(provider).or_default() |= eager.contains(name);
    }
    providers
        .into_iter()
        .filter(|(_, has_eager)| !has_eager)
        .map(|(provider, _)| provider.clone())
        .collect()
}

/// One composed tool as the plan sees it.
pub(crate) struct Candidate {
    pub(crate) definition: ToolDefinition,
    pub(crate) group: Group,
    pub(crate) provider: ProviderKey,
    /// The policy defers it.
    pub(crate) deferred: bool,
    /// Where a runtime tool comes from, recorded with the round.
    pub(crate) source: Option<RoundToolSource>,
}

/// A composed tool on a model that does not defer.
pub(crate) struct EagerCandidate {
    pub(crate) definition: ToolDefinition,
    pub(crate) provider: ProviderKey,
    pub(crate) source: Option<RoundToolSource>,
}

/// A deferred tool of a request, as the loader tools read it.
#[derive(Clone, Debug)]
pub(crate) struct DeferredEntry {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: serde_json::Value,
    pub(crate) group: Group,
    pub(crate) provider: ProviderKey,
}

/// What the loader tools of a session see: the deferred and eager tools of
/// its latest request, and the tools its results loaded.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeferralView {
    /// Shared with the view of the request before, while an entry stays
    /// the same.
    pub(crate) deferred: Vec<std::sync::Arc<DeferredEntry>>,
    pub(crate) eager: BTreeSet<String>,
    pub(crate) loaded: BTreeSet<String>,
    /// The rendered prompt sections of each deferred provider none of whose
    /// tools is loaded yet; the first load of one of its tools carries them.
    pub(crate) sections: BTreeMap<ProviderKey, String>,
}

/// A request's tools under deferral.
pub(crate) struct DeferralPlan {
    /// Every tool the request declares, sorted by name.
    pub(crate) definitions: Vec<ToolDefinition>,
    /// Round tools that left the session but stay declared until the round
    /// ends; a call to one is refused.
    pub(crate) departed: HashSet<String>,
    /// Records to append before the request: a round start, an announcement.
    pub(crate) pending: Vec<EventPayload>,
    pub(crate) view: DeferralView,
    /// Providers whose prompt sections leave the system prompt.
    pub(crate) deferred_providers: BTreeSet<ProviderKey>,
}

fn round_definition(
    definition: &ToolDefinition,
    source: Option<&RoundToolSource>,
) -> RoundToolDefinition {
    RoundToolDefinition {
        name: definition.name.clone(),
        description: definition.description.clone(),
        parameters: definition.input_schema.as_value().clone(),
        provider_options: definition.provider_options.clone(),
        source: source.cloned(),
    }
}

fn declared(definition: &RoundToolDefinition) -> Option<ToolDefinition> {
    let mut declared = ToolDefinition::new(
        definition.name.clone(),
        definition.description.clone(),
        JsonSchema::new(definition.parameters.clone()).ok()?,
    );
    declared.provider_options = definition.provider_options.clone();
    Some(declared)
}

/// [`plan_reusing`] with no request before.
#[cfg(test)]
pub(crate) fn plan(
    candidates: Vec<Candidate>,
    loaders: &[ToolDefinition],
    fold: &DeferralFold,
) -> DeferralPlan {
    plan_reusing(candidates, loaders, fold, &DeferralView::default())
}

/// Plans a request's tools on a model that defers (sections 4, 7 and 8).
///
/// `candidates` are the composed tools, the loader tools excluded;
/// `loaders` are the loader tools' definitions. The tool round comes from
/// `fold`, or starts here when the log has none since its checkpoint. The
/// view takes each deferred tool's entry from `previous`, the view of the
/// request before, while the tool is unchanged: a step copies no
/// description or schema of a tool that stayed deferred.
pub(crate) fn plan_reusing(
    mut candidates: Vec<Candidate>,
    loaders: &[ToolDefinition],
    fold: &DeferralFold,
    previous: &DeferralView,
) -> DeferralPlan {
    let mut pending = Vec::new();
    // The round's record is borrowed, not copied, unless it changes here.
    let mut round: Cow<'_, [RoundToolDefinition]> = match &fold.round {
        Some(round) => Cow::Borrowed(&round.tools),
        None => {
            // A round after a checkpoint declares the tools it folded.
            for candidate in candidates.iter_mut() {
                candidate.deferred &= !fold.folded.contains(&candidate.definition.name);
            }
            let mut tools = candidates
                .iter()
                .filter(|candidate| !candidate.deferred)
                .map(|candidate| round_definition(&candidate.definition, candidate.source.as_ref()))
                .collect::<Vec<_>>();
            if candidates.iter().any(|candidate| candidate.deferred) {
                tools.extend(loaders.iter().map(|loader| round_definition(loader, None)));
            }
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            let mut prompt_groups = candidates
                .iter()
                .filter(|candidate| !candidate.deferred)
                .map(|candidate| provider_label(&candidate.provider))
                .collect::<Vec<_>>();
            prompt_groups.sort();
            prompt_groups.dedup();
            pending.push(EventPayload::ToolRoundStarted {
                tools: tools.clone(),
                prompt_groups,
            });
            Cow::Owned(tools)
        }
    };
    let eager_names = round
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let present = candidates
        .iter()
        .map(|candidate| candidate.definition.name.clone())
        .collect::<HashSet<_>>();
    let deferred_providers = deferred_providers(
        candidates
            .iter()
            .map(|candidate| (candidate.definition.name.as_str(), &candidate.provider)),
        &eager_names,
    );
    // A tool not in the round is deferred: the policy defers it, or it
    // arrived mid-round (section 8.2).
    let deferred = candidates
        .into_iter()
        .filter(|candidate| !eager_names.contains(&candidate.definition.name))
        .collect::<Vec<_>>();
    if fold.round.is_some()
        && !deferred.is_empty()
        && !loaders.is_empty()
        && !eager_names.contains(TOOL_SEARCH)
    {
        // The loader tool joins mid-round, once.
        let tools = round.to_mut();
        tools.extend(loaders.iter().map(|loader| round_definition(loader, None)));
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        pending.push(EventPayload::ToolRoundStarted {
            tools: round.to_vec(),
            prompt_groups: fold
                .round
                .as_ref()
                .map(|round| round.prompt_groups.clone())
                .unwrap_or_default(),
        });
    }
    let mut definitions = Vec::with_capacity(round.len() + deferred.len());
    let mut departed = HashSet::new();
    let mut eager = BTreeSet::new();
    for tool in round.iter() {
        if let Some(definition) = declared(tool) {
            eager.insert(tool.name.clone());
            definitions.push(definition);
        }
        if !present.contains(&tool.name) && !is_loader(&tool.name) {
            departed.insert(tool.name.clone());
        }
    }
    let previous = previous
        .deferred
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect::<std::collections::HashMap<_, _>>();
    let mut entries = Vec::with_capacity(deferred.len());
    for candidate in deferred {
        let definition = &candidate.definition;
        let reused = previous
            .get(definition.name.as_str())
            .filter(|entry| {
                entry.description == definition.description
                    && entry.parameters == *definition.input_schema.as_value()
                    && entry.group == candidate.group
                    && entry.provider == candidate.provider
            })
            .map(|entry| std::sync::Arc::clone(entry));
        entries.push(reused.unwrap_or_else(|| {
            std::sync::Arc::new(DeferredEntry {
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: definition.input_schema.as_value().clone(),
                group: candidate.group,
                provider: candidate.provider,
            })
        }));
        definitions.push(candidate.definition.deferred());
    }
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    if let Some(announcement) = announcement(&entries, fold) {
        pending.push(announcement);
    }
    DeferralPlan {
        definitions,
        departed,
        pending,
        view: DeferralView {
            deferred: entries,
            eager,
            loaded: fold.loaded.clone(),
            sections: BTreeMap::new(),
        },
        deferred_providers,
    }
}

/// A request's tools on a model that does not defer: every tool up front.
pub(crate) struct EagerPlan {
    /// Every tool the request declares, sorted by name.
    pub(crate) definitions: Vec<ToolDefinition>,
    /// Round tools that left the session but stay declared until the round
    /// ends; a call to one is refused.
    pub(crate) departed: HashSet<String>,
    /// The round start to append before the request, if one starts.
    pub(crate) pending: Vec<EventPayload>,
}

/// Plans a request's tools on a model that does not defer (stable prefix
/// spec, section 4). The round's tools are declared from its record, those
/// that left included; a tool that joined is declared from its live
/// definition. A round starts here when the log has none since its
/// checkpoint, with every composed tool. A loader tool a deferring model's
/// round declared is left out: nothing is loadable here.
pub(crate) fn plan_eager(candidates: Vec<EagerCandidate>, fold: &DeferralFold) -> EagerPlan {
    let mut pending = Vec::new();
    // The round's record is borrowed, not copied.
    let round: Cow<'_, [RoundToolDefinition]> = match &fold.round {
        Some(round) => Cow::Borrowed(&round.tools),
        None => {
            let mut tools = candidates
                .iter()
                .map(|candidate| round_definition(&candidate.definition, candidate.source.as_ref()))
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            let mut prompt_groups = candidates
                .iter()
                .map(|candidate| provider_label(&candidate.provider))
                .collect::<Vec<_>>();
            prompt_groups.sort();
            prompt_groups.dedup();
            pending.push(EventPayload::ToolRoundStarted {
                tools: tools.clone(),
                prompt_groups,
            });
            Cow::Owned(tools)
        }
    };
    let present = candidates
        .iter()
        .map(|candidate| candidate.definition.name.clone())
        .collect::<HashSet<_>>();
    let mut in_round = HashSet::with_capacity(round.len());
    let mut definitions = Vec::with_capacity(round.len() + candidates.len());
    let mut departed = HashSet::new();
    for tool in round.iter() {
        in_round.insert(tool.name.clone());
        if is_loader(&tool.name) {
            continue;
        }
        if let Some(definition) = declared(tool) {
            definitions.push(definition);
        }
        if !present.contains(&tool.name) {
            departed.insert(tool.name.clone());
        }
    }
    definitions.extend(
        candidates
            .into_iter()
            .filter(|candidate| !in_round.contains(&candidate.definition.name))
            .map(|candidate| candidate.definition),
    );
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    EagerPlan {
        definitions,
        departed,
        pending,
    }
}

/// The announcement of what became loadable and what left since the
/// announcements `fold` adds up to; `None` when nothing changed (section 7.2).
fn announcement(
    entries: &[std::sync::Arc<DeferredEntry>],
    fold: &DeferralFold,
) -> Option<EventPayload> {
    let mut loadable = BTreeMap::<&str, (&Group, BTreeSet<&str>)>::new();
    for entry in entries {
        if !fold.loaded.contains(&entry.name) {
            loadable
                .entry(entry.group.name.as_str())
                .or_insert_with(|| (&entry.group, BTreeSet::new()))
                .1
                .insert(entry.name.as_str());
        }
    }
    let declared = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<HashSet<_>>();
    let mut added = Vec::new();
    for (name, (group, tools)) in &loadable {
        let known = fold.announced.get(*name);
        let new = tools
            .iter()
            .filter(|tool| known.is_none_or(|state| !state.tools.contains(**tool)))
            .map(|tool| (*tool).to_owned())
            .collect::<Vec<_>>();
        if new.is_empty() {
            continue;
        }
        let summary = group
            .summary
            .clone()
            .filter(|summary| known.is_none_or(|state| state.summary.as_ref() != Some(summary)))
            .map(|summary| bounded_summary(&summary));
        added.push(AnnouncedToolGroup {
            name: (*name).to_owned(),
            source: group.source,
            untrusted: group.untrusted && summary.is_some(),
            summary,
            tools: new,
        });
    }
    // A loaded tool stays declared, so only a tool that left the request
    // altogether is removed.
    let mut removed = Vec::new();
    for (name, state) in &fold.announced {
        let gone = state
            .tools
            .iter()
            .filter(|tool| !declared.contains(tool.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !gone.is_empty() {
            removed.push(AnnouncedToolGroup {
                name: name.clone(),
                source: entries
                    .iter()
                    .find(|entry| &entry.group.name == name)
                    .map_or(ToolGroupSource::Unknown, |entry| entry.group.source),
                summary: None,
                untrusted: false,
                tools: gone,
            });
        }
    }
    (!added.is_empty() || !removed.is_empty())
        .then_some(EventPayload::DeferredToolsAnnounced { added, removed })
}

/// A summary within its byte bound, cut at a character boundary.
fn bounded_summary(summary: &str) -> String {
    const MARKER: &str = " [truncated]";
    let limit = AnnouncedToolGroup::MAX_SUMMARY_BYTES;
    if summary.len() <= limit {
        return summary.to_owned();
    }
    let mut end = limit - MARKER.len();
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &summary[..end])
}

/// The user turn an announcement renders as in the model's history
/// (section 7.2), as the protocol crate renders it for the clients too.
pub(crate) use cookie_agent_protocol::model_text::deferred_tools_announcement as render_announcement;

#[cfg(test)]
mod tests;
