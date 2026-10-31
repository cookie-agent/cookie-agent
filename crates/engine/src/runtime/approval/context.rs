//! The approval reviewer's conversation (`docs/site/specs/approval-reviewer.md`
//! sections 4 and 7).
//!
//! Each session has one append-only reviewer conversation: a `user` turn per
//! review, holding what happened since the previous review and the request
//! block of the call under review, and an `assistant` turn holding each
//! earlier verdict. It is rendered from the event log alone, the same bytes
//! after a restart, so the provider's cache keeps hitting: nothing about it
//! is stored separately.
//!
//! A review is a cut in the log at its call's `approval_requested`. Every
//! entry has one position in the log, and the turn of review `k` holds the
//! entries positioned after cut `k - 1` and up to cut `k`, rendered only
//! from events before its position:
//!
//! - a user message at its `user_input_submitted`, or at the
//!   `producer_message_consumed` that put a message into the agent's
//!   history;
//! - a tool call at its `tool_call_terminated`, when its outcome is known;
//! - a user's answer to an escalated call at its `approval_finalized`.
//!
//! A review whose reviewer failed leaves no verdict, so it is no turn: its
//! entries fall into the next review's turn.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::Arc,
};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, InternalAgentKind, Open, PermissionAction, ProducerOwner,
    RunId, StoredEvent, ToolCallId, ToolSource, ToolTerminationOutcome,
};
use serde_json::Value;

use crate::runtime::{Event, approval::flow::escape_untrusted};

/// Tokens a message entry keeps (section 4.2).
pub(in crate::runtime) const MESSAGE_TOKEN_CAP: usize = 2_000;
/// Tokens a trajectory entry keeps (section 4.4).
pub(in crate::runtime) const TRAJECTORY_TOKEN_CAP: usize = 300;
/// Tokens the project's instructions keep (section 4.5).
pub(in crate::runtime) const AGENTS_MD_TOKEN_CAP: usize = 4_000;
/// Tokens a `write` content or an `edit` string of the call under review
/// keeps (section 4.6).
pub(in crate::runtime) const REQUEST_STRING_TOKEN_CAP: usize = 16_000;
/// Lines of a `write` content a trajectory entry keeps.
const WRITE_LINES: usize = 20;
/// Lines of each `edit` string a trajectory entry keeps.
const EDIT_LINES: usize = 10;

/// The longest reason a verdict keeps, in characters (section 7).
pub(in crate::runtime) const MAX_REASON_CHARS: usize =
    cookie_agent_protocol::MAX_APPROVAL_RATIONALE_CHARS;

/// Tools that only read; the trajectory leaves their calls out (section
/// 4.4).
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "list",
    "ls",
    "read_artifact",
    "list_runtimes",
    "list_agents",
    "cron_list",
    "memory_search",
    "memory_read",
    "goal_get",
    "get_subagent_result",
    "skill",
];

/// The origin a delegated session's instructions from its parent carry.
const DELEGATION_ORIGIN: &str = "engine:delegation";

/// The fixed text of the request block (section 4.6), which keeps the
/// output contract out of reach of a custom `approval.md`.
pub(in crate::runtime) const REQUEST_OPEN: &str = "<approval_request>\n";
pub(in crate::runtime) const REQUEST_CLOSE: &str = "</approval_request>";
pub(in crate::runtime) const REQUEST_CALL_INTRO: &str = "The agent requests this tool call:\n";
pub(in crate::runtime) const REQUEST_CONTRACT: &str = "Evaluate only this call. Return strict JSON only:\n{\"decision\":\"allow\"|\"deny\"|\"ask\",\"reason\":\"<one sentence>\"}\n";
pub(in crate::runtime) const MEMORY_RUBRIC_OPEN: &str = "<memory_rubric>\nThe tool call changes memory. The agent was told to keep memory by this rubric; deny a change that fails it.\n\n";
pub(in crate::runtime) const MEMORY_RUBRIC_CLOSE: &str = "\n</memory_rubric>\n";

/// Roughly how many tokens `text` takes: a byte count over four, the
/// engine's usual estimate.
pub(in crate::runtime) fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// `text` within `max_tokens`: a longer text keeps its head and tail
/// around a `[… N tokens omitted …]` marker.
pub(in crate::runtime) fn cap_tokens(text: &str, max_tokens: usize) -> Cow<'_, str> {
    let max_bytes = max_tokens.saturating_mul(4);
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }
    let half = max_bytes / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let omitted = estimate_tokens(&text[head..tail]);
    Cow::Owned(format!(
        "{}[… {omitted} tokens omitted …]{}",
        &text[..head],
        &text[tail..]
    ))
}

/// The first `lines` lines of `text`, and how many more there are.
fn first_lines(text: &str, lines: usize) -> String {
    let total = text.lines().count();
    let mut kept = text.lines().take(lines).collect::<Vec<_>>().join("\n");
    if total > lines {
        let _ = write!(kept, "\n[… {} more lines]", total - lines);
    }
    kept
}

/// `value` with every object's keys sorted, so equal arguments render
/// equal bytes.
pub(in crate::runtime) fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_json(&values[key])))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

/// A reviewer's answer, as the conversation records it and the parser
/// reads it (section 7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) decision: ApprovalInternalDecisionKind,
    pub(crate) reason: Option<String>,
}

impl Verdict {
    /// The verdict as the assistant turn that records it: canonical JSON,
    /// so a restart renders the same bytes.
    pub(in crate::runtime) fn render(&self) -> String {
        let decision = match self.decision {
            ApprovalInternalDecisionKind::Allow => "allow",
            ApprovalInternalDecisionKind::Deny => "deny",
            ApprovalInternalDecisionKind::Ask | ApprovalInternalDecisionKind::Escalate => "ask",
        };
        let mut text = format!("{{\"decision\":\"{decision}\"");
        if let Some(reason) = &self.reason {
            let _ = write!(
                text,
                ",\"reason\":{}",
                serde_json::to_string(reason).expect("a string serializes")
            );
        }
        text.push('}');
        text
    }
}

/// Reads a reviewer's reply: `{"decision":…}` with an optional `reason`,
/// cut to [`MAX_REASON_CHARS`]; any other member fails it (section 7).
pub(crate) fn parse_verdict(value: &str) -> Option<Verdict> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        decision: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let reply = serde_json::from_str::<Reply>(value.trim()).ok()?;
    let decision = match reply.decision.as_str() {
        "allow" => ApprovalInternalDecisionKind::Allow,
        "deny" => ApprovalInternalDecisionKind::Deny,
        "ask" => ApprovalInternalDecisionKind::Ask,
        _ => return None,
    };
    Some(Verdict {
        decision,
        reason: reply.reason.as_deref().and_then(clean_reason),
    })
}

/// `reason` as clients show it: control characters become spaces, blank
/// runs collapse, and it is cut to [`MAX_REASON_CHARS`]. A blank reason is
/// none.
fn clean_reason(reason: &str) -> Option<String> {
    let cleaned = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(MAX_REASON_CHARS).collect())
}

/// What a delegated session's conversation opens with, from its root and
/// ancestors (section 4.3): rendered entries, and the root's instructions.
#[derive(Clone, Debug, Default)]
pub(in crate::runtime) struct Inherited {
    pub(in crate::runtime) entries: Vec<String>,
    pub(in crate::runtime) agents_md: Option<String>,
}

/// The reviewer conversation of one session, up to the call under review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::runtime) struct Conversation {
    /// Each earlier review: its `user` turn and the verdict that answered
    /// it.
    pub(in crate::runtime) reviews: Vec<(String, String)>,
    /// The `user` turn of the call under review.
    pub(in crate::runtime) request: String,
}

impl Conversation {
    /// The model history: the policy as the system turn, then the
    /// alternating turns.
    pub(in crate::runtime) fn history(&self, system_prompt: String) -> Vec<oven_sdk::HistoryTurn> {
        let mut history = vec![oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(
            vec![oven_sdk::SystemPart::Text(oven_sdk::TextPart::new(
                system_prompt,
            ))],
        ))];
        for (request, verdict) in &self.reviews {
            history.push(user_turn(request.clone()));
            history.push(oven_sdk::HistoryTurn::assistant(
                oven_sdk::CompletedTurn::new(
                    oven_sdk::AssistantMessage::new(vec![oven_sdk::AssistantPart::Text(
                        oven_sdk::TextPart::new(verdict.clone()),
                    )]),
                    oven_sdk::Finish::new(oven_sdk::Usage::default(), oven_sdk::FinishReason::Stop),
                ),
            ));
        }
        history.push(user_turn(self.request.clone()));
        history
    }
}

fn user_turn(text: String) -> oven_sdk::HistoryTurn {
    oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![oven_sdk::InputPart::Text(
        oven_sdk::TextPart::new(text),
    )]))
}

/// An entry of the conversation at its position in the log.
struct Positioned {
    seq: u64,
    text: String,
    message: bool,
}

/// A review the log records: its call, and its verdict once the reviewer
/// gave one.
struct Review {
    approval_id: ApprovalId,
    cut: u64,
    verdict: Option<Verdict>,
}

/// What the log says about one tool call.
#[derive(Default)]
struct CallFacts {
    name: Option<String>,
    source: Option<ToolSource>,
    arguments: Option<Value>,
}

/// The facts the renderer gathers from one session's log in a single pass.
#[derive(Default)]
struct LogFacts {
    entries: Vec<Positioned>,
    reviews: Vec<Review>,
    requests: HashMap<ApprovalId, (u64, ApprovalRequest)>,
    calls: HashMap<ToolCallId, CallFacts>,
    attachments: HashMap<String, (String, String, Option<String>)>,
    agents_md: Vec<(u64, Option<RunId>, String)>,
}

/// The token bounds of a conversation (section 6): the reviewer model's
/// window, net of its output reserve and a margin, and what its system
/// prompt takes of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) struct Bounds {
    pub(in crate::runtime) window: usize,
    pub(in crate::runtime) system_tokens: usize,
}

/// The context window assumed for a model the catalog gives none.
pub(in crate::runtime) const UNKNOWN_CONTEXT_TOKENS: usize = 128_000;
/// The output reserve of a reviewer whose agent sets no output cap.
pub(in crate::runtime) const DEFAULT_OUTPUT_RESERVE: usize = 4_000;
/// Tokens kept free beyond the output reserve.
pub(in crate::runtime) const WINDOW_MARGIN: usize = 2_000;
/// Verdicts a new epoch carries forward as trajectory entries.
pub(in crate::runtime) const CARRIED_VERDICTS: usize = 5;
/// User messages a budgeted context always keeps from the start: the task.
const KEPT_FIRST_MESSAGES: usize = 3;

impl Bounds {
    /// The bounds of a reviewer model with `context` tokens (unknown:
    /// [`UNKNOWN_CONTEXT_TOKENS`]) and an output cap of `output` (none:
    /// [`DEFAULT_OUTPUT_RESERVE`]), whose system prompt is `system`.
    pub(in crate::runtime) fn new(context: Option<u64>, output: Option<u64>, system: &str) -> Self {
        let context = context.map_or(UNKNOWN_CONTEXT_TOKENS, |context| {
            usize::try_from(context).unwrap_or(usize::MAX)
        });
        let output = output.map_or(DEFAULT_OUTPUT_RESERVE, |output| {
            usize::try_from(output).unwrap_or(usize::MAX)
        });
        Self {
            window: context.saturating_sub(output).saturating_sub(WINDOW_MARGIN),
            system_tokens: estimate_tokens(system),
        }
    }

    /// The user messages' share of the window: 30%.
    fn messages(self) -> usize {
        self.window / 10 * 3
    }

    /// The trajectory's share of the window: 50%.
    fn trajectory(self) -> usize {
        self.window / 2
    }
}

/// How the request block names what arrived since the previous review.
#[derive(Clone, Copy)]
enum Since {
    FirstReview,
    StartedOver,
    Delta(usize),
}

/// The session's reviewer conversation, up to and including the review of
/// `current` (sections 4.1 and 6). `inherited` is a delegated session's
/// opening; a root's instructions come from its own log.
///
/// The conversation is an epoch: it starts over, from a fresh render of the
/// log within the budgets, when the next review's turn would not fit the
/// window. Epoch boundaries follow from the log and `bounds` alone, so a
/// restart reproduces them.
pub(in crate::runtime) fn render(
    events: &[Arc<StoredEvent>],
    inherited: Option<&Inherited>,
    current: ApprovalId,
    bounds: Bounds,
) -> Option<Conversation> {
    let facts = gather(events, current);
    let current_cut = facts.requests.get(&current)?.0;
    let completed = facts
        .reviews
        .iter()
        .filter(|review| review.cut < current_cut)
        .filter_map(|review| {
            review
                .verdict
                .as_ref()
                .map(|verdict| (review.approval_id, review.cut, verdict))
        })
        .collect::<Vec<_>>();
    let first_cut = completed.first().map_or(current_cut, |(_, cut, _)| *cut);
    let agents_md = match inherited {
        Some(inherited) => inherited.agents_md.clone(),
        None => agents_md_before(&facts.agents_md, first_cut),
    };
    let opening = Opening {
        inherited,
        agents_md: agents_md.as_deref(),
    };
    let mut reviews = Vec::<(String, String)>::new();
    let mut tokens = bounds.system_tokens;
    let mut previous = 0;
    let mut epochs = 0;
    let steps = completed
        .iter()
        .map(|(approval_id, cut, verdict)| (*approval_id, *cut, Some(*verdict)))
        .chain(std::iter::once((current, current_cut, None)));
    for (index, (approval_id, cut, verdict)) in steps.enumerate() {
        let carried = || {
            completed[index.saturating_sub(CARRIED_VERDICTS)..index]
                .iter()
                .map(|(approval_id, cut, verdict)| {
                    (*cut, carried_verdict(&facts, *approval_id, verdict))
                })
                .collect::<Vec<_>>()
        };
        let mut text = if reviews.is_empty() {
            fresh_turn(
                &facts,
                cut,
                approval_id,
                opening,
                &carried(),
                bounds,
                epochs > 0,
            )
        } else {
            delta_turn(&facts, (previous, cut), approval_id)
        };
        if !reviews.is_empty() && tokens + estimate_tokens(&text) > bounds.window {
            reviews.clear();
            epochs += 1;
            tokens = bounds.system_tokens;
            text = fresh_turn(&facts, cut, approval_id, opening, &carried(), bounds, true);
        }
        tokens += estimate_tokens(&text);
        previous = cut;
        match verdict {
            Some(verdict) => {
                let verdict = verdict.render();
                tokens += estimate_tokens(&verdict);
                reviews.push((text, verdict));
            }
            None => {
                return Some(Conversation {
                    reviews,
                    request: text,
                });
            }
        }
    }
    None
}

/// What opens the first turn of every epoch.
#[derive(Clone, Copy)]
struct Opening<'a> {
    inherited: Option<&'a Inherited>,
    agents_md: Option<&'a str>,
}

/// An earlier verdict as a new epoch carries it: a trajectory entry.
fn carried_verdict(facts: &LogFacts, approval_id: ApprovalId, verdict: &Verdict) -> String {
    let name = facts
        .requests
        .get(&approval_id)
        .and_then(|(_, request)| facts.calls.get(&request.tool_call_id()))
        .and_then(|call| call.name.as_deref())
        .unwrap_or("a tool");
    let mut text = format!("[your earlier verdict] on {name}: {}", verdict.render());
    text = escape_untrusted(&cap_tokens(&text, TRAJECTORY_TOKEN_CAP)).into_owned();
    text
}

/// Whether a rendered message carries the user's authority.
fn has_authority(text: &str) -> bool {
    text.starts_with("[user] ") || text.starts_with("[scheduled prompt ")
}

/// The first turn of an epoch: the opening, then every entry up to `cut`
/// within the budgets, the carried verdicts among them, then the request
/// block (section 6).
fn fresh_turn(
    facts: &LogFacts,
    cut: u64,
    approval_id: ApprovalId,
    opening: Opening<'_>,
    carried: &[(u64, String)],
    bounds: Bounds,
    started_over: bool,
) -> String {
    // Inherited messages come first, at position zero.
    let inherited = opening
        .inherited
        .map(|inherited| inherited.entries.as_slice())
        .unwrap_or_default();
    let mut messages = inherited
        .iter()
        .map(|text| (0, text.as_str()))
        .chain(
            facts
                .entries
                .iter()
                .filter(|entry| entry.message && entry.seq <= cut)
                .map(|entry| (entry.seq, entry.text.as_str())),
        )
        .collect::<Vec<_>>();
    let omitted_messages = budget_messages(&mut messages, bounds.messages());
    let mut trajectory = facts
        .entries
        .iter()
        .filter(|entry| !entry.message && entry.seq <= cut)
        .map(|entry| (entry.seq, entry.text.as_str()))
        .chain(carried.iter().map(|(seq, text)| (*seq, text.as_str())))
        .collect::<Vec<_>>();
    trajectory.sort_by_key(|(seq, _)| *seq);
    let omitted_calls = budget_trajectory(&mut trajectory, bounds.trajectory());

    let mut text = String::from("<context>\n");
    let mut wrote = false;
    let mut push = |text: &mut String, entry: &str| {
        text.push_str(entry);
        text.push_str("\n\n");
        wrote = true;
    };
    if let Some(agents_md) = opening.agents_md {
        push(&mut text, &format!("[project instructions]\n{agents_md}"));
    }
    // Messages and trajectory interleave by position; an omission marker
    // stands where it dropped entries.
    let mut merged = messages
        .iter()
        .map(|(seq, entry)| (*seq, Cow::Borrowed(*entry)))
        .chain(
            trajectory
                .iter()
                .map(|(seq, entry)| (*seq, Cow::Borrowed(*entry))),
        )
        .collect::<Vec<_>>();
    if let Some((at, count)) = omitted_messages {
        merged.push((
            at,
            Cow::Owned(format!("[… {count} earlier messages omitted …]")),
        ));
    }
    if let Some((at, count)) = omitted_calls {
        merged.push((
            at,
            Cow::Owned(format!("[… {count} earlier trajectory entries omitted …]")),
        ));
    }
    merged.sort_by_key(|(seq, _)| *seq);
    for (_, entry) in &merged {
        push(&mut text, entry);
    }
    if wrote {
        text.pop();
    } else {
        text.push_str("[nothing new]\n");
    }
    text.push_str("</context>\n\n");
    text.push_str(&request_block(
        facts,
        approval_id,
        if started_over {
            Since::StartedOver
        } else {
            Since::FirstReview
        },
    ));
    text
}

/// Keeps `messages` within `budget`: the first [`KEPT_FIRST_MESSAGES`]
/// messages with the user's authority, the first instructions from a
/// parent agent, and the newest messages, dropping a
/// block from the middle down to half the budget, so it happens rarely.
/// Returns where the omission marker goes and how many it stands for.
fn budget_messages(messages: &mut Vec<(u64, &str)>, budget: usize) -> Option<(u64, usize)> {
    let total = messages
        .iter()
        .map(|(_, text)| estimate_tokens(text))
        .sum::<usize>();
    if total <= budget {
        return None;
    }
    let target = budget / 2;
    let mut keep = vec![false; messages.len()];
    let mut used = 0;
    let mut first = 0;
    let mut instructions = false;
    for (index, (_, text)) in messages.iter().enumerate() {
        // A delegated session's own instructions are its task too: the
        // first of them stays beside the root's first messages.
        let task = if has_authority(text) && first < KEPT_FIRST_MESSAGES {
            first += 1;
            true
        } else if !instructions && text.starts_with("[instructions from the parent agent]") {
            instructions = true;
            true
        } else {
            false
        };
        if task {
            keep[index] = true;
            used += estimate_tokens(text);
        }
    }
    let last_first = keep.iter().rposition(|kept| *kept);
    for index in (0..messages.len()).rev() {
        if keep[index] || last_first.is_some_and(|last| index < last) {
            continue;
        }
        let cost = estimate_tokens(messages[index].1);
        if used + cost > target {
            break;
        }
        keep[index] = true;
        used += cost;
    }
    let dropped = keep.iter().filter(|kept| !**kept).count();
    let at = messages
        .iter()
        .zip(&keep)
        .find(|(_, kept)| !**kept)
        .map_or(0, |((seq, _), _)| *seq);
    let mut kept = keep.into_iter();
    messages.retain(|_| kept.next().unwrap_or(false));
    (dropped > 0).then_some((at, dropped))
}

/// Keeps `trajectory` within `budget` by dropping its oldest half, as
/// often as it takes. Returns where the omission marker goes and how many
/// entries it stands for.
fn budget_trajectory(trajectory: &mut Vec<(u64, &str)>, budget: usize) -> Option<(u64, usize)> {
    let mut dropped = 0;
    let mut at = 0;
    while trajectory
        .iter()
        .map(|(_, text)| estimate_tokens(text))
        .sum::<usize>()
        > budget
        && !trajectory.is_empty()
    {
        let half = trajectory.len().div_ceil(2);
        at = trajectory[half - 1].0;
        trajectory.drain(..half);
        dropped += half;
    }
    (dropped > 0).then_some((at, dropped))
}

/// A later turn of an epoch: the entries after `range.0` up to `range.1`,
/// then the request block of `approval_id`.
fn delta_turn(facts: &LogFacts, range: (u64, u64), approval_id: ApprovalId) -> String {
    let delta = facts
        .entries
        .iter()
        .filter(|entry| entry.seq > range.0 && entry.seq <= range.1)
        .collect::<Vec<_>>();
    let mut text = String::from("<context_delta>\n");
    for entry in &delta {
        text.push_str(&entry.text);
        text.push_str("\n\n");
    }
    if delta.is_empty() {
        text.push_str("[nothing new]\n");
    } else {
        text.pop();
    }
    text.push_str("</context_delta>\n\n");
    let messages = delta.iter().filter(|entry| entry.message).count();
    text.push_str(&request_block(facts, approval_id, Since::Delta(messages)));
    text
}

/// The request block of the call `approval_id` (section 4.6), saying what
/// arrived since the previous review.
fn request_block(facts: &LogFacts, approval_id: ApprovalId, since: Since) -> String {
    let mut text = String::from(REQUEST_OPEN);
    match since {
        Since::FirstReview => text.push_str("Messages since the previous review: none, this is the first review.\n"),
        Since::StartedOver => text.push_str(
            "Messages since the previous review: this conversation started over, so the context above is the session so far, within its budgets.\n",
        ),
        Since::Delta(0) => text.push_str("Messages since the previous review: none.\n"),
        Since::Delta(1) => text.push_str(
            "Messages since the previous review: 1, the last message entry in the context delta above.\n",
        ),
        Since::Delta(count) => {
            let _ = writeln!(
                text,
                "Messages since the previous review: {count}, the last {count} message entries in the context delta above."
            );
        }
    }
    let Some((_, request)) = facts.requests.get(&approval_id) else {
        text.push_str(REQUEST_CLOSE);
        return text;
    };
    if request
        .operation()
        .capabilities()
        .iter()
        .any(|capability| capability.action == PermissionAction::MEMORY_WRITE)
    {
        text.push_str(MEMORY_RUBRIC_OPEN);
        text.push_str(crate::memory_tools::PART_B);
        text.push_str(MEMORY_RUBRIC_CLOSE);
    }
    text.push_str(REQUEST_CALL_INTRO);
    text.push_str(&escape_untrusted(&request_call_json(facts, request)));
    text.push('\n');
    text.push_str(REQUEST_CONTRACT);
    text.push_str(REQUEST_CLOSE);
    text
}

/// The call under review as one JSON line: its name, where it runs, what
/// the permission rules saw, and its arguments, each from the log.
fn request_call_json(facts: &LogFacts, request: &ApprovalRequest) -> String {
    let call = facts.calls.get(&request.tool_call_id());
    let name = call
        .and_then(|call| call.name.clone())
        .or_else(|| {
            request
                .operation()
                .capabilities()
                .first()
                .map(|capability| capability.action.as_str().to_owned())
        })
        .unwrap_or_default();
    let executes_in = call.and_then(|call| call.source.as_ref()).map_or_else(
        || {
            request.runtime_id().map_or_else(
                || "unknown".to_owned(),
                |runtime| format!("the `{}` runtime", runtime.as_str()),
            )
        },
        |source| describe_source(facts, source),
    );
    let permission = request
        .evaluations()
        .iter()
        .map(|evaluation| {
            serde_json::json!({
                "action": evaluation.trace.action.as_str(),
                "resource": evaluation.trace.normalized_resource,
            })
        })
        .collect::<Vec<_>>();
    let arguments = call
        .and_then(|call| call.arguments.as_ref())
        .map_or(Value::Null, |arguments| {
            cap_request_strings(&name, &canonical_json(arguments))
        });
    format!(
        "{{\"name\":{},\"executes_in\":{},\"permission\":{},\"arguments\":{}}}",
        serde_json::to_string(&name).expect("a string serializes"),
        serde_json::to_string(&executes_in).expect("a string serializes"),
        serde_json::to_string(&permission).expect("JSON serializes"),
        serde_json::to_string(&arguments).expect("JSON serializes"),
    )
}

/// The arguments of a call under review with a long `write` content or
/// `edit` string kept to its head and tail.
fn cap_request_strings(name: &str, arguments: &Value) -> Value {
    let keys: &[&str] = match name {
        "write" => &["content"],
        "edit" => &["oldString", "newString"],
        _ => return arguments.clone(),
    };
    let mut arguments = arguments.clone();
    if let Value::Object(object) = &mut arguments {
        for key in keys {
            if let Some(Value::String(text)) = object.get_mut(*key)
                && let Cow::Owned(capped) = cap_tokens(text, REQUEST_STRING_TOKEN_CAP)
            {
                *text = format!(
                    "{capped}\n[this {key} was cut for review; judge the call with more caution]"
                );
            }
        }
    }
    arguments
}

/// Where a tool of `source` runs: the runtime, its kind, and the workspace
/// its attachment recorded.
fn describe_source(facts: &LogFacts, source: &ToolSource) -> String {
    match source {
        ToolSource::Base => "the daemon's base layer (no workspace)".to_owned(),
        ToolSource::Runtime {
            runtime_id,
            attachment,
            ..
        } => match facts.attachments.get(attachment.as_str()) {
            Some((_, kind, Some(workspace))) => format!(
                "the `{}` {kind} runtime with workspace {workspace}",
                runtime_id.as_str()
            ),
            Some((_, kind, None)) => {
                format!(
                    "the `{}` {kind} runtime (no workspace)",
                    runtime_id.as_str()
                )
            }
            None => format!("the `{}` runtime", runtime_id.as_str()),
        },
        ToolSource::Plugin { plugin } => format!("the plugin `{plugin}`"),
        ToolSource::Mcp { server, runtime_id } => match runtime_id {
            Some(runtime) => format!("the MCP server `{server}` on `{}`", runtime.as_str()),
            None => format!("the MCP server `{server}`"),
        },
        ToolSource::Acp { agent, runtime_id } => format!(
            "the ACP agent `{}` on `{}`",
            agent.as_str(),
            runtime_id.as_str()
        ),
    }
}

/// A short "on …" label for a trajectory entry.
fn source_label(source: Option<&ToolSource>) -> String {
    match source {
        Some(ToolSource::Base) => " on the base layer".to_owned(),
        Some(ToolSource::Runtime { runtime_id, .. }) => format!(" on {}", runtime_id.as_str()),
        Some(ToolSource::Plugin { plugin }) => format!(" from plugin {plugin}"),
        Some(ToolSource::Mcp { server, .. }) => format!(" on MCP server {server}"),
        Some(ToolSource::Acp { agent, .. }) => format!(" of ACP agent {}", agent.as_str()),
        None => String::new(),
    }
}

/// The newest run's `AGENTS.md` texts before `cut`, cut to
/// [`AGENTS_MD_TOKEN_CAP`] and escaped.
fn agents_md_before(loaded: &[(u64, Option<RunId>, String)], cut: u64) -> Option<String> {
    let before = loaded
        .iter()
        .filter(|(seq, _, _)| *seq < cut)
        .collect::<Vec<_>>();
    let run = before.last()?.1;
    let text = before
        .iter()
        .filter(|(_, owner, _)| *owner == run)
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(escape_untrusted(&cap_tokens(&text, AGENTS_MD_TOKEN_CAP)).into_owned())
}

/// The `AGENTS.md` texts of the newest run that loaded any at or before
/// `until`: what a delegated session inherits from its root.
pub(in crate::runtime) fn agents_md_until(
    events: &[Arc<StoredEvent>],
    until: jiff::Timestamp,
) -> Option<String> {
    let loaded = events
        .iter()
        .filter(|event| event.timestamp <= until)
        .filter_map(|event| agents_md_text(event).map(|text| (event.seq, event.run_id, text)))
        .collect::<Vec<_>>();
    agents_md_before(&loaded, u64::MAX)
}

fn agents_md_text(event: &StoredEvent) -> Option<String> {
    let Event::AgentMdLoaded { entries, .. } = &event.payload else {
        return None;
    };
    Some(
        entries
            .iter()
            .map(|entry| format!("{}:\n{}", entry.source.as_str(), entry.content))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

/// The user messages of a root's log at or before `until`, rendered: the
/// opening of a delegated session's conversation (section 4.3).
pub(in crate::runtime) fn root_messages_until(
    events: &[Arc<StoredEvent>],
    until: jiff::Timestamp,
) -> Vec<String> {
    let facts = gather(events, ApprovalId::new_v7());
    let cut = events
        .iter()
        .take_while(|event| event.timestamp <= until)
        .last()
        .map_or(0, |event| event.seq);
    facts
        .entries
        .into_iter()
        .filter(|entry| entry.message && entry.seq <= cut)
        .map(|entry| entry.text)
        .collect()
}

/// The first instructions a delegated session got from its parent: what a
/// descendant inherits of it (section 4.3).
pub(in crate::runtime) fn delegation_prompt(events: &[Arc<StoredEvent>]) -> Option<String> {
    events.iter().find_map(|event| match &event.payload {
        Event::UserInputSubmitted { content }
            if event
                .origin
                .as_ref()
                .is_some_and(|origin| origin.as_str() == DELEGATION_ORIGIN) =>
        {
            Some(format!(
                "[instructions from the parent agent] {}\n{}",
                timestamp_label(event.timestamp),
                escape_untrusted(&cap_tokens(&content.as_plain_text(), MESSAGE_TOKEN_CAP))
            ))
        }
        _ => None,
    })
}

fn timestamp_label(timestamp: jiff::Timestamp) -> String {
    timestamp.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The label of a user input by its envelope origin (section 4.2). A
/// person in a client is `[user]`; scheduled prompts and agents' messages
/// arrive as producer messages instead ([`producer_label`]).
fn input_label(origin: Option<&str>) -> String {
    match origin {
        Some(origin) if origin == "user" || origin.starts_with("client:") => "[user]".to_owned(),
        Some(DELEGATION_ORIGIN) => "[instructions from the parent agent]".to_owned(),
        Some(origin) => format!("[{origin}]"),
        None => "[input of unknown origin]".to_owned(),
    }
}

/// The label of a producer message the agent received, or `None` for one
/// the conversation leaves out: a background tool's or a delegation's
/// result is output, and a goal reminder is the engine's.
fn producer_label(owner: &ProducerOwner) -> Option<String> {
    match owner {
        ProducerOwner::External { source, name } => Some(match source.cron_job() {
            Some(job) => format!("[scheduled prompt {job}]"),
            None => format!("[message from {} {}]", source.as_str(), name.as_str()),
        }),
        ProducerOwner::Agent { session_id } => Some(format!("[message from agent {session_id}]")),
        ProducerOwner::Subagent { session_id } => {
            Some(format!("[report from subagent {session_id}]"))
        }
        // Why the agent's own work started: never the user's word.
        ProducerOwner::AcpAgent { agent } => Some(format!("[{agent} started work on its own]")),
        ProducerOwner::Delegation { .. }
        | ProducerOwner::Goal { .. }
        | ProducerOwner::GoalControl { .. }
        | ProducerOwner::Tool { .. } => None,
    }
}

/// One pass over `events`: every entry with its position, every review
/// with its verdict, and what the request blocks need. `current` is the
/// approval under review, which never has a verdict yet.
fn gather(events: &[Arc<StoredEvent>], current: ApprovalId) -> LogFacts {
    let mut facts = LogFacts::default();
    let mut run_ordinals = HashMap::<RunId, usize>::new();
    let mut runs_with_input = HashSet::<RunId>::new();
    let mut turn_calls = HashMap::<(u64, u32), (String, Value)>::new();
    let mut producer_bodies = HashMap::new();
    let mut pending_review = None::<ApprovalId>;
    let mut reviewed = HashSet::<ApprovalId>::new();
    for event in events {
        // Runs count in the order they first appear, which is their
        // `run_started`.
        if let Some(run) = event.run_id {
            let next = run_ordinals.len() + 1;
            run_ordinals.entry(run).or_insert(next);
        }
        match &event.payload {
            Event::RuntimeAttached {
                name,
                runtime_id,
                kind,
                workspace,
                ..
            } => {
                facts.attachments.insert(
                    name.as_str().to_owned(),
                    (
                        runtime_id.as_str().to_owned(),
                        kind.as_str().to_owned(),
                        workspace.clone(),
                    ),
                );
            }
            Event::AgentMdLoaded { .. } => {
                if let Some(text) = agents_md_text(event) {
                    facts.agents_md.push((event.seq, event.run_id, text));
                }
            }
            Event::UserInputSubmitted { content } => {
                let origin = event.origin.as_ref().map(|origin| origin.as_str());
                let when = run_label(event.run_id, &run_ordinals, &mut runs_with_input);
                facts.entries.push(Positioned {
                    seq: event.seq,
                    text: format!(
                        "{} {}{when}\n{}",
                        input_label(origin),
                        timestamp_label(event.timestamp),
                        escape_untrusted(&cap_tokens(&content.as_plain_text(), MESSAGE_TOKEN_CAP))
                    ),
                    message: true,
                });
            }
            Event::ProducerMessageAccepted {
                message_id,
                producer_owner,
                body,
                ..
            } => {
                producer_bodies.insert(*message_id, (producer_owner.clone(), body.clone()));
            }
            Event::ProducerMessageConsumed { message_id, run_id } => {
                if let Some((owner, body)) = producer_bodies.get(message_id)
                    && let Some(label) = producer_label(owner)
                {
                    let when = run_label(Some(*run_id), &run_ordinals, &mut runs_with_input);
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text: format!(
                            "{label} {}{when}\n{}",
                            timestamp_label(event.timestamp),
                            escape_untrusted(&cap_tokens(&body.as_plain_text(), MESSAGE_TOKEN_CAP))
                        ),
                        message: true,
                    });
                }
            }
            Event::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                for (index, part) in turn.content.iter().enumerate() {
                    if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        name,
                        input,
                        ..
                    } = part
                    {
                        turn_calls.insert(
                            (*model_turn_seq, u32::try_from(index).unwrap_or(u32::MAX)),
                            (name.as_str().to_owned(), input.clone()),
                        );
                    }
                }
            }
            Event::ToolCallStarted { start } => {
                let committed =
                    turn_calls.get(&(start.owner.model_turn_seq, start.owner.content_index));
                facts.calls.insert(
                    start.tool_call_id,
                    CallFacts {
                        name: Some(start.presentation.tool.name.clone()),
                        source: start
                            .presentation
                            .tool
                            .source
                            .as_ref()
                            .and_then(|source| source.known().cloned()),
                        arguments: committed.map(|(_, input)| input.clone()),
                    },
                );
            }
            Event::ToolCallTerminated { termination } => {
                if let Some(call) = facts.calls.get(&termination.tool_call_id)
                    && let Some(text) = trajectory_entry(call, termination)
                {
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text,
                        message: false,
                    });
                }
            }
            Event::ApprovalRequested { request } => {
                facts
                    .requests
                    .insert(request.approval_id(), (event.seq, request.clone()));
                if request.approval_id() != current {
                    pending_review = Some(request.approval_id());
                }
            }
            Event::InternalAgentFinished { kind, outcome, .. } => {
                if *kind == InternalAgentKind::Approval
                    && matches!(
                        outcome.known(),
                        Some(cookie_agent_protocol::InternalAgentOutcome::Completed { .. })
                    )
                    && let Some(approval_id) = pending_review.take()
                {
                    reviewed.insert(approval_id);
                }
            }
            Event::ApprovalStep {
                approval_id,
                step: Open::Known(cookie_agent_protocol::ApprovalStep::Evaluated { decision }),
            } => {
                if pending_review == Some(*approval_id) {
                    pending_review = None;
                }
                let decided = match decision.reason_code {
                    ApprovalReasonCode::InternalAgentAllowed => {
                        Some(ApprovalInternalDecisionKind::Allow)
                    }
                    ApprovalReasonCode::InternalAgentDenied => {
                        Some(ApprovalInternalDecisionKind::Deny)
                    }
                    ApprovalReasonCode::Escalated => Some(ApprovalInternalDecisionKind::Ask),
                    _ => None,
                };
                let verdict = decided
                    .filter(|_| {
                        reviewed.contains(approval_id)
                            && decision.source == ApprovalDecisionSource::InternalAgent
                    })
                    .map(|decision_kind| Verdict {
                        decision: decision_kind,
                        reason: decision.rationale.clone(),
                    });
                if let Some((cut, _)) = facts.requests.get(approval_id) {
                    facts.reviews.push(Review {
                        approval_id: *approval_id,
                        cut: *cut,
                        verdict,
                    });
                }
            }
            Event::ApprovalFinalized {
                outcome:
                    Open::Known(cookie_agent_protocol::ApprovalFinalization::Decided { decision }),
                ..
            } if decision.source == ApprovalDecisionSource::User => {
                let text = match decision.outcome {
                    ApprovalFinalOutcome::Approved => Some("[user decision] approved".to_owned()),
                    ApprovalFinalOutcome::Rejected => Some(match &decision.feedback {
                        Some(feedback) => format!(
                            "[user decision] rejected: {}",
                            escape_untrusted(&cap_tokens(
                                feedback.message.as_str(),
                                TRAJECTORY_TOKEN_CAP
                            ))
                        ),
                        None => "[user decision] rejected".to_owned(),
                    }),
                    ApprovalFinalOutcome::Cancelled | ApprovalFinalOutcome::Expired => None,
                };
                if let Some(text) = text {
                    facts.entries.push(Positioned {
                        seq: event.seq,
                        text,
                        message: false,
                    });
                }
            }
            _ => {}
        }
    }
    facts
}

/// When an input arrived: `, starts run N` for a run's first input, else
/// `, during run N`.
fn run_label(
    run: Option<RunId>,
    ordinals: &HashMap<RunId, usize>,
    with_input: &mut HashSet<RunId>,
) -> String {
    let Some(run) = run else {
        return String::new();
    };
    let ordinal = ordinals.get(&run).copied().unwrap_or(0);
    if with_input.insert(run) {
        format!(", starts run {ordinal}")
    } else {
        format!(", during run {ordinal}")
    }
}

/// A finished call as a trajectory entry, or `None` for a read-only one
/// (section 4.4).
fn trajectory_entry(
    call: &CallFacts,
    termination: &cookie_agent_protocol::ToolCallTermination,
) -> Option<String> {
    let name = call.name.as_deref().unwrap_or("unknown");
    if READ_ONLY_TOOLS.contains(&name) {
        return None;
    }
    let denied = termination.error.as_ref().is_some_and(|error| {
        serde_json::from_str::<Value>(error.message.as_str())
            .ok()
            .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|kind| kind == "tool_denied")
    });
    let outcome = match termination.outcome {
        _ if denied => "denied",
        ToolTerminationOutcome::Completed => "completed",
        ToolTerminationOutcome::Failed => "failed",
        ToolTerminationOutcome::Cancelled | ToolTerminationOutcome::Interrupted => "cancelled",
    };
    let summary = call
        .arguments
        .as_ref()
        .map(|arguments| summarize_arguments(name, arguments))
        .unwrap_or_default();
    let mut text = format!(
        "[tool call] {name}{}: {outcome}",
        source_label(call.source.as_ref())
    );
    if !summary.is_empty() {
        text.push('\n');
        text.push_str(&summary);
    }
    Some(escape_untrusted(&cap_tokens(&text, TRAJECTORY_TOKEN_CAP)).into_owned())
}

/// A call's arguments as a trajectory entry keeps them (section 4.4): an
/// MCP tool's, like any other tool's, are its canonical argument object,
/// which the entry's cap cuts.
fn summarize_arguments(name: &str, arguments: &Value) -> String {
    let string = |key: &str| arguments.get(key).and_then(Value::as_str);
    let path = || {
        string("filePath")
            .or_else(|| string("path"))
            .unwrap_or("?")
            .to_owned()
    };
    match name {
        "bash" if string("command").is_some() => {
            format!("command: {}", string("command").unwrap_or_default())
        }
        "write" => {
            let content = string("content").unwrap_or_default();
            format!(
                "path: {} ({} bytes)\n{}",
                path(),
                content.len(),
                first_lines(content, WRITE_LINES)
            )
        }
        "edit" => format!(
            "path: {}\nold:\n{}\nnew:\n{}",
            path(),
            first_lines(string("oldString").unwrap_or_default(), EDIT_LINES),
            first_lines(string("newString").unwrap_or_default(), EDIT_LINES)
        ),
        _ => canonical_json(arguments).to_string(),
    }
}

#[cfg(test)]
mod tests;
