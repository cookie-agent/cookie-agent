//! An ACP agent's questions to the user: `elicitation/create` and
//! `elicitation/complete` (ACP v1 elicitation; `docs/site/specs/acp-agents.md`
//! section 15).
//!
//! Cookie advertises form and URL elicitation. A session-scoped
//! `elicitation/create` that arrives while a turn of its session runs
//! becomes a question of that run: `elicitation_requested`, a wait for the
//! user's `elicitation.respond`, and `elicitation_resolved`, whose action is
//! the agent's answer. A question is the agent asking the user, not an
//! action, so no permission rule or mode answers it; a session no one
//! attends (permission mode `auto_approve_n`, a cron run's default, or a
//! headless `cookie run`) declines it at once, the run's cancel or end
//! cancels it, and the agent's `elicitation/complete` for a URL question
//! still waiting accepts it. Anything else, a request-scoped question or
//! one outside a turn, is cancelled; a mode other than form and URL is
//! refused as invalid params, as ACP asks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    AgentId, ElicitationAction, ElicitationAnsweredBy, ElicitationChoice, ElicitationContent,
    ElicitationField, ElicitationForm, ElicitationId, ElicitationInput, ElicitationRequest,
    ElicitationResolution, ElicitationRespondParams, EventPayload as Event, Open, PermissionMode,
    RunId, SessionId, ToolCallId,
};
use cookie_agent_runtime_api::rpc::{IncomingNotification, IncomingRequest, RpcError};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::super::{ActiveRun, Engine, EngineError, helpers::root_id};
use super::process::{AgentProcess, SessionEvent};

pub(crate) const CREATE: &str = "elicitation/create";
pub(crate) const COMPLETE: &str = "elicitation/complete";

/// What `initialize` declares: form and URL elicitation.
pub(crate) fn capabilities() -> Value {
    json!({"form": {}, "url": {}})
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An `elicitation/create` as the engine reads it, tolerant of members it
/// does not know.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Create {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
    mode: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    requested_schema: Option<Schema>,
    #[serde(default)]
    elicitation_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl Create {
    /// The agent's ID of the tool call the question belongs to.
    pub(crate) fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }
}

#[derive(Debug, Default, Deserialize)]
struct Schema {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    properties: Map<String, Value>,
    #[serde(default)]
    required: Option<Vec<String>>,
}

/// A name's sort key in natural order: runs of digits compare as numbers,
/// so `question_2` comes before `question_10`.
fn natural_key(name: &str) -> Vec<(bool, u128, String)> {
    let mut key = Vec::new();
    let mut chars = name.chars().peekable();
    while let Some(&first) = chars.peek() {
        let digits = first.is_ascii_digit();
        let mut run = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() != digits {
                break;
            }
            run.push(c);
            chars.next();
        }
        let number = if digits {
            run.parse().unwrap_or(u128::MAX)
        } else {
            0
        };
        key.push((digits, number, run));
    }
    key
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn unsigned(value: &Value, name: &str) -> Option<u64> {
    value.get(name).and_then(Value::as_u64)
}

/// The choices of an `oneOf`/`anyOf` list of `{const, title}`, else of an
/// `enum` of strings (titled by `enumNames` when the agent sends them).
fn choices(schema: &Value, titled: &str) -> Option<Vec<ElicitationChoice>> {
    if let Some(options) = schema.get(titled).and_then(Value::as_array) {
        return Some(
            options
                .iter()
                .filter_map(|option| {
                    let value = option.get("const")?.as_str()?.to_owned();
                    Some(ElicitationChoice {
                        title: text(option, "title").unwrap_or_else(|| value.clone()),
                        description: text(option, "description"),
                        value,
                    })
                })
                .collect(),
        );
    }
    let values = schema.get("enum")?.as_array()?;
    let names = schema.get("enumNames").and_then(Value::as_array);
    Some(
        values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let value = value.as_str()?.to_owned();
                let title = names
                    .and_then(|names| names.get(index))
                    .and_then(Value::as_str)
                    .map_or_else(|| value.clone(), str::to_owned);
                Some(ElicitationChoice {
                    value,
                    title,
                    description: None,
                })
            })
            .collect(),
    )
}

/// One property schema as a field's input; a type cookie does not know is
/// kept as an unknown input, which no one can fill in.
fn input_of(schema: &Value) -> Open<ElicitationInput> {
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");
    let input = match kind {
        "string" => match choices(schema, "oneOf") {
            Some(choices) => ElicitationInput::Choice {
                choices,
                default: text(schema, "default"),
            },
            None => ElicitationInput::Text {
                min_length: unsigned(schema, "minLength")
                    .map(|value| u32::try_from(value).unwrap_or(u32::MAX)),
                max_length: unsigned(schema, "maxLength")
                    .map(|value| u32::try_from(value).unwrap_or(u32::MAX)),
                pattern: text(schema, "pattern"),
                format: text(schema, "format"),
                default: schema
                    .get("default")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        },
        "number" => ElicitationInput::Number {
            minimum: schema.get("minimum").and_then(Value::as_f64),
            maximum: schema.get("maximum").and_then(Value::as_f64),
            default: schema.get("default").and_then(Value::as_f64),
        },
        "integer" => ElicitationInput::Integer {
            minimum: schema.get("minimum").and_then(Value::as_i64),
            maximum: schema.get("maximum").and_then(Value::as_i64),
            default: schema.get("default").and_then(Value::as_i64),
        },
        "boolean" => ElicitationInput::Boolean {
            default: schema.get("default").and_then(Value::as_bool),
        },
        "array" => match schema
            .get("items")
            .and_then(|items| choices(items, "anyOf"))
        {
            Some(choices) => ElicitationInput::MultiChoice {
                choices,
                min_items: unsigned(schema, "minItems"),
                max_items: unsigned(schema, "maxItems"),
                default: schema
                    .get("default")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            None => {
                return Open::Unknown {
                    tag: "array".to_owned(),
                    raw: schema.clone(),
                };
            }
        },
        other => {
            return Open::Unknown {
                tag: other.to_owned(),
                raw: schema.clone(),
            };
        }
    };
    Open::Known(input)
}

/// The form `schema` asks for. The agent's messages reach the daemon
/// re-encoded by the runtime relay, which loses the order of the schema's
/// properties, so fields come in the natural order of their names; that
/// keeps Claude Code's `question_<n>` and `question_<n>_custom` fields in
/// its order.
fn form_of(schema: Schema) -> ElicitationForm {
    let required = schema.required.unwrap_or_default();
    let mut properties = schema.properties.into_iter().collect::<Vec<_>>();
    properties.sort_by_cached_key(|(name, _)| natural_key(name));
    ElicitationForm {
        title: schema.title.filter(|title| !title.trim().is_empty()),
        description: schema
            .description
            .filter(|description| !description.trim().is_empty()),
        fields: properties
            .into_iter()
            .map(|(name, property)| ElicitationField {
                required: required.contains(&name),
                title: text(&property, "title"),
                description: text(&property, "description"),
                input: input_of(&property),
                name,
            })
            .collect(),
    }
}

/// The question `create` asks, as cookie records it, and the agent's ID of
/// a URL question; an error message for a request cookie cannot show.
fn request_of(
    create: Create,
    agent: AgentId,
    tool_call_id: Option<ToolCallId>,
) -> Result<(ElicitationRequest, Option<String>), String> {
    let (form, url, url_id) = match create.mode.as_str() {
        "form" => (
            Some(form_of(create.requested_schema.unwrap_or_default())),
            None,
            None,
        ),
        "url" => {
            let url = create
                .url
                .filter(|url| !url.is_empty())
                .ok_or("a URL question needs a `url`")?;
            (None, Some(url), create.elicitation_id)
        }
        other => return Err(format!("cookie does not offer elicitation mode `{other}`")),
    };
    let request = ElicitationRequest {
        elicitation_id: ElicitationId::new_v7(),
        agent,
        tool_call_id,
        message: create.message,
        form,
        url,
    };
    if request.is_valid() {
        Ok((request, url_id))
    } else {
        Err("the question is larger than cookie records, or names a field twice".to_owned())
    }
}

/// The ACP answer to a question.
fn answer_json(answer: &Answer) -> Value {
    match answer.action {
        ElicitationAction::Accept if answer.content.is_empty() => json!({"action": "accept"}),
        ElicitationAction::Accept => {
            let content = answer
                .content
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        serde_json::to_value(value).unwrap_or(Value::Null),
                    )
                })
                .collect::<Map<_, _>>();
            json!({"action": "accept", "content": content})
        }
        ElicitationAction::Decline => json!({"action": "decline"}),
        ElicitationAction::Cancel | ElicitationAction::Unknown => json!({"action": "cancel"}),
    }
}

/// Answers `request` `cancel`: no turn of its session runs to ask in.
pub(crate) fn cancel(request: IncomingRequest) {
    tokio::spawn(async move {
        let _ = request.respond(&json!({"action": "cancel"})).await;
    });
}

/// Takes an `elicitation/create` of `process`'s agent to the runner of its
/// session; `request` back for any other request. A request-scoped
/// question, or one for a session no turn runs in, is cancelled, and a
/// malformed one refused.
pub(crate) fn route(process: &AgentProcess, request: IncomingRequest) -> Option<IncomingRequest> {
    if request.method != CREATE {
        return Some(request);
    }
    let create = match request.params::<Create>() {
        Ok(create) => create,
        Err(error) => {
            tokio::spawn(async move {
                let _ = request.fail(error).await;
            });
            return None;
        }
    };
    let Some(session) = create.session_id.clone() else {
        cancel(request);
        return None;
    };
    if let Err(SessionEvent::Elicitation(_, request)) = process.send_to(
        &session,
        SessionEvent::Elicitation(Box::new(create), request),
    ) {
        cancel(request);
    }
    None
}

/// Whether `notification` is an `elicitation/complete`, which it then
/// hands to the URL question it names.
pub(crate) fn completed(process: &AgentProcess, notification: &IncomingNotification) -> bool {
    if notification.method != COMPLETE {
        return false;
    }
    if let Ok(params) = notification.params::<Value>()
        && let Some(id) = params.get("elicitationId").and_then(Value::as_str)
    {
        process.url_completions.complete(id);
    }
    true
}

/// The URL questions of one agent process waiting on the user, by the
/// agent's elicitation ID: an `elicitation/complete` for one ends its wait.
/// An unknown or finished ID is ignored, as ACP asks.
#[derive(Default)]
pub(crate) struct UrlCompletions(Mutex<HashMap<String, oneshot::Sender<()>>>);

impl UrlCompletions {
    fn watch(&self, id: &str) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.0).insert(id.to_owned(), sender);
        receiver
    }

    fn complete(&self, id: &str) {
        if let Some(sender) = lock(&self.0).remove(id) {
            let _ = sender.send(());
        }
    }

    fn forget(&self, id: &str) {
        lock(&self.0).remove(id);
    }
}

/// How a question was answered.
#[derive(Clone, Debug)]
pub(crate) struct Answer {
    action: ElicitationAction,
    content: ElicitationContent,
    answered_by: ElicitationAnsweredBy,
    reason: Option<String>,
}

impl Answer {
    fn by_daemon(
        action: ElicitationAction,
        answered_by: ElicitationAnsweredBy,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            action,
            content: ElicitationContent::new(),
            answered_by,
            reason: Some(reason.into()),
        }
    }
}

struct Waiting {
    session: SessionId,
    run: RunId,
    form: Option<ElicitationForm>,
    answer: oneshot::Sender<Answer>,
}

/// The questions waiting on the user, engine-wide.
#[derive(Default)]
pub(crate) struct Questions(Mutex<HashMap<ElicitationId, Waiting>>);

impl Questions {
    fn answer(&self, id: ElicitationId, answer: Answer) {
        if let Some(waiting) = lock(&self.0).remove(&id) {
            let _ = waiting.answer.send(answer);
        }
    }
}

const UNATTENDED_REASON: &str =
    "no one attends this session (permission mode auto_approve_n), so no one can answer";

impl Engine {
    /// Asks the user `create`, a question of `agent` (whose call
    /// `tool_call_id` it belongs to) during run `run`, and answers the
    /// agent's `request` with what the user, or the daemon for them,
    /// decides. `url_completions` are the agent process's.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::runtime) fn ask_acp_question(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        agent: AgentId,
        tool_call_id: Option<ToolCallId>,
        process: &Arc<AgentProcess>,
        create: Create,
        request: IncomingRequest,
    ) -> tokio::task::JoinHandle<()> {
        let engine = self.clone();
        let process = Arc::clone(process);
        tokio::spawn(async move {
            let (question, url_id) = match request_of(create, agent, tool_call_id) {
                Ok(question) => question,
                Err(message) => {
                    let _ = request.fail(RpcError::remote(-32602, message)).await;
                    return;
                }
            };
            let completion = url_id
                .as_deref()
                .map(|id| process.url_completions.watch(id));
            let answer = engine
                .ask_question(&active, run, question, completion)
                .await;
            if let Some(id) = &url_id {
                process.url_completions.forget(id);
            }
            let _ = request.respond(&answer_json(&answer)).await;
        })
    }

    /// Records `question` of run `run`, waits for its answer, and records
    /// that.
    async fn ask_question(
        &self,
        active: &ActiveRun,
        run: RunId,
        question: ElicitationRequest,
        completion: Option<oneshot::Receiver<()>>,
    ) -> Answer {
        let id = question.elicitation_id;
        let session = active.session;
        let immediate = if self.unattended(session) {
            Some(Answer::by_daemon(
                ElicitationAction::Decline,
                ElicitationAnsweredBy::Unattended,
                UNATTENDED_REASON,
            ))
        } else if question
            .form
            .as_ref()
            .is_some_and(ElicitationForm::has_unknown_required_field)
        {
            Some(Answer::by_daemon(
                ElicitationAction::Decline,
                ElicitationAnsweredBy::Unsupported,
                "the form has a required field cookie cannot show",
            ))
        } else {
            None
        };
        let (sender, receiver) = oneshot::channel();
        if immediate.is_none() {
            lock(&self.inner.acp.questions.0).insert(
                id,
                Waiting {
                    session,
                    run,
                    form: question.form.clone(),
                    answer: sender,
                },
            );
        }
        if self
            .append(
                session,
                Some(run),
                super::super::event_origin("engine:elicitation"),
                Event::ElicitationRequested { request: question },
            )
            .await
            .is_err()
        {
            lock(&self.inner.acp.questions.0).remove(&id);
            return Answer::by_daemon(
                ElicitationAction::Cancel,
                ElicitationAnsweredBy::RunEnded,
                "the question could not be recorded",
            );
        }
        let answer = match immediate {
            Some(answer) => answer,
            None => {
                let completed = async {
                    match completion {
                        Some(completion) => completion.await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    answer = receiver => answer.unwrap_or_else(|_| Answer::by_daemon(
                        ElicitationAction::Cancel,
                        ElicitationAnsweredBy::RunEnded,
                        "the run ended",
                    )),
                    () = active.cancellation.cancelled() => Answer::by_daemon(
                        ElicitationAction::Cancel,
                        ElicitationAnsweredBy::RunEnded,
                        "the run was cancelled",
                    ),
                    true = completed => Answer::by_daemon(
                        ElicitationAction::Accept,
                        ElicitationAnsweredBy::Completed,
                        "the agent reported the page's interaction finished",
                    ),
                }
            }
        };
        lock(&self.inner.acp.questions.0).remove(&id);
        let _ = self
            .append(
                session,
                Some(run),
                super::super::event_origin("engine:elicitation"),
                Event::ElicitationResolved {
                    resolution: ElicitationResolution {
                        elicitation_id: id,
                        action: answer.action,
                        content: answer.content.clone(),
                        answered_by: answer.answered_by,
                        reason: answer.reason.clone(),
                    },
                },
            )
            .await;
        answer
    }

    /// Whether no one attends `session`: its tree's permission mode is
    /// `auto_approve_n`, which answers escalated approvals for an absent
    /// user too.
    fn unattended(&self, session: SessionId) -> bool {
        self.permission_mode(session) == PermissionMode::AutoApproveN
    }

    /// Cancels the questions of run `run` still waiting: the run's turn
    /// ended.
    pub(in crate::runtime) fn end_acp_questions(&self, run: RunId) {
        let ended = lock(&self.inner.acp.questions.0)
            .iter()
            .filter(|(_, waiting)| waiting.run == run)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ended {
            self.inner.acp.questions.answer(
                id,
                Answer::by_daemon(
                    ElicitationAction::Cancel,
                    ElicitationAnsweredBy::RunEnded,
                    "the agent's turn ended",
                ),
            );
        }
    }

    /// `elicitation.respond`: hands the user's answer to the question it
    /// names. An accepted form must fit the form's fields; a refused answer
    /// leaves the question waiting.
    pub async fn respond_elicitation(
        &self,
        params: ElicitationRespondParams,
    ) -> Result<cookie_agent_protocol::ElicitationRespondResult, EngineError> {
        let mut questions = lock(&self.inner.acp.questions.0);
        let Some(waiting) = questions
            .get(&params.elicitation_id)
            .filter(|waiting| waiting.session == params.session_id)
        else {
            return Err(EngineError::ElicitationNotPending(params.elicitation_id));
        };
        let content = match (params.action, &waiting.form) {
            (ElicitationAction::Accept, Some(form)) => {
                let content = params.content.unwrap_or_default();
                form.check(&content).map_err(|reason| {
                    EngineError::ElicitationAnswerInvalid(params.elicitation_id, reason)
                })?;
                content
            }
            (ElicitationAction::Unknown, _) => {
                return Err(EngineError::ElicitationAnswerInvalid(
                    params.elicitation_id,
                    "the action must be accept, decline, or cancel".to_owned(),
                ));
            }
            _ => ElicitationContent::new(),
        };
        let waiting = questions
            .remove(&params.elicitation_id)
            .expect("the question is waiting");
        drop(questions);
        waiting
            .answer
            .send(Answer {
                action: params.action,
                content,
                answered_by: ElicitationAnsweredBy::User,
                reason: None,
            })
            .map_err(|_| EngineError::ElicitationNotPending(params.elicitation_id))?;
        Ok(cookie_agent_protocol::ElicitationRespondResult {})
    }

    /// Declines every question waiting in the tree of root session `root`
    /// for `reason`: no one is there to answer, as in a headless `cookie
    /// run`. Returns how many it declined.
    pub fn decline_unattended_questions(&self, root: SessionId, reason: &str) -> usize {
        let waiting = lock(&self.inner.acp.questions.0)
            .iter()
            .map(|(id, waiting)| (*id, waiting.session))
            .collect::<Vec<_>>();
        let mut declined = 0;
        for (id, session) in waiting {
            let in_tree = self
                .inner
                .store
                .get(session)
                .is_ok_and(|stored| root_id(&stored.meta.origin, session) == root);
            if in_tree {
                self.inner.acp.questions.answer(
                    id,
                    Answer::by_daemon(
                        ElicitationAction::Decline,
                        ElicitationAnsweredBy::Unattended,
                        reason,
                    ),
                );
                declined += 1;
            }
        }
        declined
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::ElicitationValue;
    use serde_json::json;

    use super::*;

    fn create(value: Value) -> Create {
        serde_json::from_str(&value.to_string()).expect("a create request")
    }

    fn agent() -> AgentId {
        AgentId::new("claude").unwrap()
    }

    #[test]
    fn an_ask_user_question_form_lists_its_fields_in_natural_order() {
        // As Claude Code's adapter sends AskUserQuestion: a titled single
        // choice, its free-text companion, and a titled multi-choice.
        let text = r#"{"sessionId":"s","toolCallId":"t","mode":"form","message":"Pick",
            "requestedSchema":{"type":"object","properties":{
              "question_1":{"type":"string","title":"Plan","oneOf":[
                {"const":"a","title":"Option A","description":"The first"},
                {"const":"b","title":"Option B"}]},
              "question_1_custom":{"type":"string","title":"Other","description":"Type your own"},
              "question_0":{"type":"array","title":"Tags","items":{"anyOf":[
                {"const":"x","title":"X"},{"const":"y","title":"Y"}]}},
              "count":{"type":"integer","minimum":1,"default":2},
              "ok":{"type":"boolean","default":true},
              "colour":{"type":"colour"},
              "question_10":{"type":"string"}
            },"required":["question_1"]}}"#;
        let create: Create = serde_json::from_str(text).unwrap();
        assert_eq!(create.tool_call_id(), Some("t"));
        let (request, url_id) = request_of(create, agent(), None).unwrap();
        assert_eq!(url_id, None);
        let form = request.form.unwrap();
        let names = form
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "colour",
                "count",
                "ok",
                "question_0",
                "question_1",
                "question_1_custom",
                "question_10"
            ]
        );
        let field = |name: &str| {
            form.fields
                .iter()
                .find(|field| field.name == name)
                .expect("the field")
        };
        assert!(field("question_1").required);
        assert!(!field("question_1_custom").required);
        assert_eq!(
            field("question_1").input,
            Open::Known(ElicitationInput::Choice {
                choices: vec![
                    ElicitationChoice {
                        value: "a".into(),
                        title: "Option A".into(),
                        description: Some("The first".into()),
                    },
                    ElicitationChoice {
                        value: "b".into(),
                        title: "Option B".into(),
                        description: None,
                    },
                ],
                default: None,
            })
        );
        assert_eq!(
            field("question_1_custom").description.as_deref(),
            Some("Type your own")
        );
        assert!(matches!(
            &field("question_0").input,
            Open::Known(ElicitationInput::MultiChoice { choices, .. }) if choices.len() == 2
        ));
        assert_eq!(
            field("count").input,
            Open::Known(ElicitationInput::Integer {
                minimum: Some(1),
                maximum: None,
                default: Some(2),
            })
        );
        assert_eq!(
            field("ok").input,
            Open::Known(ElicitationInput::Boolean {
                default: Some(true)
            })
        );
        assert!(field("colour").input.is_unknown());
    }

    #[test]
    fn plain_enums_take_their_values_as_titles() {
        let (request, _) = request_of(
            create(json!({"sessionId": "s", "mode": "form", "message": "m",
                "requestedSchema": {"properties": {"pick": {"type": "string", "enum": ["one", "two"],
                    "enumNames": ["One"]}}}})),
            agent(),
            None,
        )
        .unwrap();
        let Some(ElicitationInput::Choice { choices, .. }) =
            request.form.unwrap().fields[0].input.known().cloned()
        else {
            panic!("a choice");
        };
        let titles = choices
            .iter()
            .map(|choice| (choice.value.as_str(), choice.title.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(titles, [("one", "One"), ("two", "two")]);
    }

    #[test]
    fn a_url_question_keeps_the_agents_id_and_other_modes_are_refused() {
        let (request, url_id) = request_of(
            create(
                json!({"sessionId": "s", "mode": "url", "message": "Sign in",
                "elicitationId": "oauth-1", "url": "https://example.com/auth"}),
            ),
            agent(),
            None,
        )
        .unwrap();
        assert_eq!(request.url.as_deref(), Some("https://example.com/auth"));
        assert!(request.form.is_none());
        assert_eq!(url_id.as_deref(), Some("oauth-1"));
        assert_eq!(
            request_of(
                create(json!({"sessionId": "s", "mode": "_voice", "message": "m"})),
                agent(),
                None,
            )
            .unwrap_err(),
            "cookie does not offer elicitation mode `_voice`"
        );
        assert!(
            request_of(
                create(json!({"sessionId": "s", "mode": "url", "message": "m"})),
                agent(),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn answers_go_back_in_acps_shape() {
        let accept = Answer {
            action: ElicitationAction::Accept,
            content: ElicitationContent::from([
                ("a".to_owned(), ElicitationValue::Text("x".into())),
                ("b".to_owned(), ElicitationValue::List(vec!["y".into()])),
                ("c".to_owned(), ElicitationValue::Integer(3)),
            ]),
            answered_by: ElicitationAnsweredBy::User,
            reason: None,
        };
        assert_eq!(
            answer_json(&accept),
            json!({"action": "accept", "content": {"a": "x", "b": ["y"], "c": 3}})
        );
        for (action, wire) in [
            (ElicitationAction::Decline, "decline"),
            (ElicitationAction::Cancel, "cancel"),
        ] {
            assert_eq!(
                answer_json(&Answer::by_daemon(action, ElicitationAnsweredBy::User, "")),
                json!({"action": wire})
            );
        }
        assert_eq!(
            answer_json(&Answer::by_daemon(
                ElicitationAction::Accept,
                ElicitationAnsweredBy::Completed,
                ""
            )),
            json!({"action": "accept"})
        );
    }
}
