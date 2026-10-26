//! ACP agents' config options beyond one session
//! (`docs/site/specs/acp-agents.md` section 15, input and pickers): what
//! each agent last offered a new session on each runtime, which drafts offer
//! before their session exists; what a new session starts with; and the
//! starting values a session was created with.
//!
//! The daemon keeps the last-known options in `acp-agents.json` in its data
//! folder, a cache: a file it cannot read is ignored and written again.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_config::{AcpDocumentSettings, AcpModelChoice};
use cookie_agent_protocol::{AcpConfigOption, AcpOptionChoice, AgentId, RuntimeId};
use serde::{Deserialize, Serialize};

use crate::events::EventSnapshot;
use cookie_agent_protocol::EventPayload as Event;

/// The cache file's name in the daemon's data folder.
pub(crate) const KNOWN_AGENTS_FILE: &str = "acp-agents.json";

/// What one agent offered on one runtime when it last started there.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct KnownAgent {
    /// The options its last new session answered with, before cookie set
    /// any.
    #[serde(default)]
    pub options: Vec<AcpConfigOption>,
    /// The files it takes in a prompt, from its `initialize` answer.
    #[serde(default)]
    pub input_media: Vec<cookie_agent_protocol::InputMedia>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct KnownFile {
    #[serde(default)]
    agents: Vec<KnownEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct KnownEntry {
    agent: String,
    runtime_id: String,
    #[serde(flatten)]
    known: KnownAgent,
}

/// The last-known state of every declaration on every runtime, by the
/// declaration's key and runtime ID: every document that runs a
/// declaration shares what it offered.
#[derive(Debug, Default)]
pub(crate) struct KnownAgents {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<(String, String), KnownAgent>>,
}

impl KnownAgents {
    /// The cache of the daemon whose data folder is `data_dir`, as its file
    /// holds it; empty when the file is missing or unreadable.
    pub(crate) fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(KNOWN_AGENTS_FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<KnownFile>(&bytes).ok())
            .map(|file| {
                file.agents
                    .into_iter()
                    .map(|entry| ((entry.agent, entry.runtime_id), entry.known))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: Mutex::new(entries),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), KnownAgent>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What `agent` offered on `runtime` last.
    pub(crate) fn get(&self, agent: &AgentId, runtime: &RuntimeId) -> KnownAgent {
        self.lock()
            .get(&(agent.to_string(), runtime.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// Changes what `agent` offered on `runtime` with `change`, and writes
    /// the cache when that changed it. Whether it changed.
    pub(crate) fn update(
        &self,
        agent: &AgentId,
        runtime: &RuntimeId,
        change: impl FnOnce(&mut KnownAgent),
    ) -> bool {
        let snapshot = {
            let mut entries = self.lock();
            let entry = entries
                .entry((agent.to_string(), runtime.to_string()))
                .or_default();
            let before = entry.clone();
            change(entry);
            if *entry == before {
                return false;
            }
            entries
                .iter()
                .map(|((agent, runtime_id), known)| KnownEntry {
                    agent: agent.clone(),
                    runtime_id: runtime_id.clone(),
                    known: known.clone(),
                })
                .collect::<Vec<_>>()
        };
        if let Some(path) = &self.path
            && let Err(error) = write_atomic(path, &KnownFile { agents: snapshot })
        {
            eprintln!("cookie-agent: could not write {}: {error}", path.display());
        }
        true
    }
}

fn write_atomic(path: &Path, file: &KnownFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// The option a session's mode lives in: category `mode`, else ID `mode`.
pub(crate) fn mode_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("mode") || option.id == "mode")
}

/// The mode a new session of a document with `settings` starts in, for a
/// mode option `option`: the document's `acp.mode`, else the agent's
/// `auto` when it offers one.
pub(crate) fn wanted_mode(
    option: &AcpConfigOption,
    settings: &AcpDocumentSettings,
) -> Option<String> {
    settings.mode.clone().or_else(|| {
        option
            .values
            .iter()
            .any(|value| value.value == "auto")
            .then(|| "auto".to_owned())
    })
}

/// The option an agent's model lives in: category `model`, else ID `model`.
pub(crate) fn model_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("model"))
        .or_else(|| options.iter().find(|option| option.id == "model"))
}

/// The option an agent's reasoning effort lives in: category
/// `thought_level`.
pub(crate) fn effort_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("thought_level"))
}

/// The document's `models` entries that name a model value: those naming a
/// variable are skipped (a configuration diagnostic says so).
pub(crate) fn document_models(settings: &AcpDocumentSettings) -> Vec<&AcpModelChoice> {
    settings
        .models
        .iter()
        .filter(|choice| choice.variable().is_none())
        .collect()
}

/// Whether the document's `models` decide option `id` of `options` rather
/// than its `acp.config`: the model and effort options, when it has
/// entries.
pub(crate) fn models_decide(
    settings: &AcpDocumentSettings,
    options: &[AcpConfigOption],
    id: &str,
) -> bool {
    !document_models(settings).is_empty()
        && (model_option(options).is_some_and(|option| option.id == id)
            || effort_option(options).is_some_and(|option| option.id == id))
}

/// Which `models` entry a session of the document starts with.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ModelPick<'a> {
    /// The document has no entries with a model value.
    Unset,
    /// The agent offers no model option.
    NoModelOption,
    /// The agent's model option takes none of the entries' models.
    NoneOffered,
    /// The first entry whose model the agent offers, with the ID of its
    /// model option.
    Chosen {
        option: String,
        choice: &'a AcpModelChoice,
    },
}

impl ModelPick<'_> {
    /// Why no entry applies, for a session notice; `None` when one does or
    /// the document has none.
    pub(crate) fn problem(&self, settings: &AcpDocumentSettings) -> Option<String> {
        match self {
            Self::Unset | Self::Chosen { .. } => None,
            Self::NoModelOption => Some("the agent offers no model option".to_owned()),
            Self::NoneOffered => Some(format!(
                "the agent's model option takes none of {}",
                document_models(settings)
                    .iter()
                    .map(|choice| format!("`{}`", choice.model))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// The first of the document's `models` whose model `options` offer.
pub(crate) fn pick_model<'a>(
    options: &[AcpConfigOption],
    settings: &'a AcpDocumentSettings,
) -> ModelPick<'a> {
    let models = document_models(settings);
    if models.is_empty() {
        return ModelPick::Unset;
    }
    let Some(option) = model_option(options) else {
        return ModelPick::NoModelOption;
    };
    models
        .into_iter()
        .find(|choice| {
            option
                .values
                .iter()
                .any(|value| value.value == choice.model)
        })
        .map_or(ModelPick::NoneOffered, |choice| ModelPick::Chosen {
            option: option.id.clone(),
            choice,
        })
}

/// The effort option's ID and `variant`, when `options` take it: else why
/// not.
pub(crate) fn pick_variant(options: &[AcpConfigOption], variant: &str) -> Result<String, String> {
    let option = effort_option(options)
        .ok_or_else(|| "the agent offers no effort option (category `thought_level`)".to_owned())?;
    if option.values.iter().any(|value| value.value == variant) {
        Ok(option.id.clone())
    } else {
        Err(format!("option `{}` takes no value `{variant}`", option.id))
    }
}

/// Whether `option_id` of `options` takes `value`: `None` when there is no
/// such option.
pub(crate) fn takes(options: &[AcpConfigOption], option_id: &str, value: &str) -> Option<bool> {
    options
        .iter()
        .find(|option| option.id == option_id)
        .map(|option| {
            option
                .values
                .iter()
                .any(|candidate| candidate.value == value)
        })
}

/// Sets `option_id` of `options` to `value` when it takes it.
fn choose(options: &mut [AcpConfigOption], option_id: &str, value: &str) {
    if let Some(option) = options.iter_mut().find(|option| {
        option.id == option_id
            && option
                .values
                .iter()
                .any(|candidate| candidate.value == value)
    }) {
        value.clone_into(&mut option.current_value);
    }
}

/// What a new session starts with, as a draft shows it: `options`, the
/// agent's own starting values, with the document's mode, `acp.config`,
/// and `models` applied where the agent takes them (`models` before
/// `acp.config` for the model and effort options).
pub(crate) fn starting_view(
    options: &[AcpConfigOption],
    settings: &AcpDocumentSettings,
) -> Vec<AcpConfigOption> {
    let mut options = options.to_vec();
    if let Some((id, mode)) = mode_option(&options)
        .and_then(|option| Some((option.id.clone(), wanted_mode(option, settings)?)))
    {
        choose(&mut options, &id, &mode);
    }
    for (id, value) in &settings.config {
        if !models_decide(settings, &options, id) {
            choose(&mut options, id, value);
        }
    }
    if let ModelPick::Chosen { option, choice } = pick_model(&options, settings) {
        choose(&mut options, &option, &choice.model);
        if let Some(variant) = &choice.variant
            && let Ok(option) = pick_variant(&options, variant)
        {
            choose(&mut options, &option, variant);
        }
    }
    options
}

/// The starting values the session of `events` was created with, the
/// latest `acp_session_updated.starting_options`.
pub(crate) fn recorded_starting_options(events: &EventSnapshot) -> Vec<AcpOptionChoice> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Event::AcpSessionUpdated { update } => update.starting_options.clone(),
            _ => None,
        })
        .unwrap_or_default()
}

impl crate::runtime::Engine {
    /// Remembers what `agent` offered as its session started: always for a
    /// new session, whose options are the agent's own starting values;
    /// for a resumed or loaded one only while nothing is known, since its
    /// values are that session's. Publishes the agents again when that
    /// changed what a draft offers.
    pub(super) fn remember_acp_options(
        &self,
        agent: &crate::runtime_snapshot::RuntimeAgent,
        options: &[AcpConfigOption],
        new_session: bool,
    ) {
        if options.is_empty() {
            return;
        }
        let changed = self
            .inner
            .acp
            .known
            .update(&agent.agent, &agent.runtime, |known| {
                if new_session || known.options.is_empty() {
                    known.options = options.to_vec();
                }
            });
        if changed {
            self.refresh_runtime_agents();
        }
    }

    /// Remembers the files `agent` takes in a prompt, as its process
    /// answered `initialize`, and publishes the agents again when that
    /// changed.
    pub(super) fn remember_acp_input(
        &self,
        agent: &crate::runtime_snapshot::RuntimeAgent,
        caps: &super::wire::PromptCapabilities,
    ) {
        let media = super::prompt::input_media(caps);
        if self
            .inner
            .acp
            .known
            .update(&agent.agent, &agent.runtime, |known| {
                known.input_media = media
            })
        {
            self.refresh_runtime_agents();
        }
    }

    /// Applies the starting values the session was created with
    /// (`session.create`'s `agent_options`) to its new ACP session, after
    /// the document's `acp.config`; one the agent does not take is skipped
    /// with a diagnostic.
    pub(super) async fn apply_session_starting_options(
        &self,
        session: cookie_agent_protocol::SessionId,
        run: Option<cookie_agent_protocol::RunId>,
        live: &mut super::runner::LiveSession,
    ) {
        let Ok(projection) = self.inner.store.get(session) else {
            return;
        };
        let choices = recorded_starting_options(&projection.log.event_snapshot());
        for choice in choices {
            let current = live
                .options
                .iter()
                .find(|option| option.id == choice.option_id)
                .map(|option| option.current_value.clone());
            let problem = match takes(&live.options, &choice.option_id, &choice.value) {
                None => Some(format!("the agent offers no option `{}`", choice.option_id)),
                Some(false) => Some(format!(
                    "option `{}` takes no value `{}`",
                    choice.option_id, choice.value
                )),
                Some(true) if current.as_deref() == Some(choice.value.as_str()) => None,
                Some(true) => self
                    .set_live_option(live, &choice.option_id, &choice.value)
                    .await
                    .err(),
            };
            if let Some(problem) = problem {
                self.acp_notice(
                    session,
                    run,
                    format!("The session's starting agent option was skipped: {problem}"),
                )
                .await;
            }
        }
    }

    /// `session.create` with starting values for the options of its ACP
    /// agent (client protocol 1.26.0): checked before anything is created,
    /// and recorded as `acp_session_updated.starting_options` with the
    /// session. Refused for an agent that is not an ACP agent.
    pub async fn create_session_with_agent_options(
        &self,
        selection: cookie_agent_protocol::RunSelection,
        mode: Option<cookie_agent_protocol::PermissionMode>,
        rules: Vec<cookie_agent_protocol::PermissionRule>,
        runtimes: crate::runtime::attachments::SessionRuntimes,
        project: Option<cookie_agent_protocol::ProjectId>,
        agent_options: Vec<AcpOptionChoice>,
    ) -> Result<cookie_agent_protocol::SessionMeta, crate::EngineError> {
        if !agent_options.is_empty() {
            AcpOptionChoice::validate_list(&agent_options).map_err(crate::EngineError::Acp)?;
            let acp = self
                .current_runtime()
                .agents_for_preset(selection.preset.as_deref())?
                .get(&selection.agent)
                .is_some_and(|agent| agent.acp.is_some());
            if !acp {
                return Err(crate::EngineError::Acp(format!(
                    "agent `{}` is not an ACP agent, so it takes no agent options",
                    selection.agent
                )));
            }
        }
        let meta = self
            .create_session_in_project(selection, mode, rules, runtimes, project)
            .await?;
        if !agent_options.is_empty() {
            self.append(
                meta.session_id,
                None,
                crate::runtime::event_origin("user"),
                Event::AcpSessionUpdated {
                    update: cookie_agent_protocol::AcpSessionUpdate {
                        starting_options: Some(agent_options),
                        ..cookie_agent_protocol::AcpSessionUpdate::default()
                    },
                },
            )
            .await?;
        }
        Ok(meta)
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::AcpConfigValue;

    use super::*;

    fn option(id: &str, category: Option<&str>, current: &str, values: &[&str]) -> AcpConfigOption {
        AcpConfigOption {
            kind: Default::default(),
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            category: category.map(str::to_owned),
            current_value: current.to_owned(),
            values: values
                .iter()
                .map(|value| AcpConfigValue {
                    value: (*value).to_owned(),
                    name: value.to_uppercase(),
                    description: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_draft_sees_the_documents_mode_and_config_where_the_agent_takes_them() {
        let options = [
            option("mode", Some("mode"), "default", &["default", "auto"]),
            option("model", Some("model"), "sonnet", &["sonnet", "opus"]),
            option("effort", Some("thought_level"), "low", &["low", "high"]),
        ];
        let settings = AcpDocumentSettings {
            config: BTreeMap::from([
                ("model".to_owned(), "opus".to_owned()),
                ("effort".to_owned(), "extreme".to_owned()),
                ("missing".to_owned(), "x".to_owned()),
            ]),
            ..AcpDocumentSettings::default()
        };
        let view = starting_view(&options, &settings);
        let current = view
            .iter()
            .map(|option| (option.id.as_str(), option.current_value.as_str()))
            .collect::<Vec<_>>();
        // `auto` because the document names no mode; `effort` keeps the
        // agent's value, since the agent takes no `extreme`.
        assert_eq!(
            current,
            [("mode", "auto"), ("model", "opus"), ("effort", "low")]
        );
        let settings = AcpDocumentSettings {
            mode: Some("default".to_owned()),
            ..AcpDocumentSettings::default()
        };
        assert_eq!(
            starting_view(&options, &settings)[0].current_value,
            "default"
        );
    }

    #[test]
    fn a_draft_sees_the_documents_models_over_its_config() {
        let options = [
            option("model", Some("model"), "sonnet", &["sonnet", "opus"]),
            option("effort", Some("thought_level"), "low", &["low", "high"]),
        ];
        let entry = |model: &str, variant: Option<&str>| AcpModelChoice {
            model: model.to_owned(),
            variant: variant.map(str::to_owned),
        };
        let settings = AcpDocumentSettings {
            config: BTreeMap::from([
                ("model".to_owned(), "sonnet".to_owned()),
                ("effort".to_owned(), "high".to_owned()),
            ]),
            models: vec![
                entry("${default_model}", Some("high")),
                entry("haiku", Some("high")),
                entry("opus", None),
            ],
            ..AcpDocumentSettings::default()
        };
        let current = |view: Vec<AcpConfigOption>| {
            view.into_iter()
                .map(|option| option.current_value)
                .collect::<Vec<_>>()
        };
        // `opus` is the first offered; its null variant keeps the agent's
        // `low`, though `acp.config` says `high`.
        assert_eq!(current(starting_view(&options, &settings)), ["opus", "low"]);
        assert!(models_decide(&settings, &options, "effort"));
        assert!(!models_decide(&settings, &options, "mode"));
        let settings = AcpDocumentSettings {
            models: vec![entry("opus", Some("high"))],
            ..AcpDocumentSettings::default()
        };
        assert_eq!(
            current(starting_view(&options, &settings)),
            ["opus", "high"]
        );
        // Only variables: `acp.config` decides.
        let settings = AcpDocumentSettings {
            config: BTreeMap::from([("model".to_owned(), "opus".to_owned())]),
            models: vec![entry("${small_model}", None)],
            ..AcpDocumentSettings::default()
        };
        assert_eq!(pick_model(&options, &settings), ModelPick::Unset);
        assert_eq!(current(starting_view(&options, &settings)), ["opus", "low"]);
    }

    #[test]
    fn the_cache_keeps_each_agent_on_each_runtime_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let claude = AgentId::new("claude").unwrap();
        let (local, lab) = (
            RuntimeId::new("local").unwrap(),
            RuntimeId::new("lab").unwrap(),
        );
        let known = KnownAgents::open(dir.path());
        let options = vec![option("model", Some("model"), "opus", &["opus", "sonnet"])];
        assert!(known.update(&claude, &local, |known| known.options = options.clone()));
        assert!(!known.update(&claude, &local, |known| known.options = options.clone()));
        assert!(known.get(&claude, &lab).options.is_empty());
        let reopened = KnownAgents::open(dir.path());
        assert_eq!(reopened.get(&claude, &local).options, options);
        // An unreadable file is a cold cache.
        std::fs::write(dir.path().join(KNOWN_AGENTS_FILE), b"{not json").unwrap();
        assert!(
            KnownAgents::open(dir.path())
                .get(&claude, &local)
                .options
                .is_empty()
        );
    }
}
