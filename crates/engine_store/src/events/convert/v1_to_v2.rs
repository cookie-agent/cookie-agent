//! History format 1 to 2 (`docs/site/specs/session-storage-v3.md` section
//! 4.2, and `docs/site/specs/store-locks-and-durability.md` section 4.3 for
//! the ACP agent's live state).
//!
//! - The large values of a record move to the tree's blobs, as a format-2
//!   writer moves them ([`blobs::externalize`]): a tool result's output
//!   longer than 8 KiB keeps its first 2 KiB, its length, and its digest;
//!   its display and the large parts of its metadata move whole;
//!   `acp_session_updated`'s option and command lists and `run_started`'s
//!   model settings chains are named by digest. A display stored as ranges
//!   of the output keeps its ranges, which the whole output the reader
//!   restores first resolves.
//! - The ACP agent's live state is stored as a format-2 writer stores it:
//!   an `acp_session_updated` that carries nothing but a tool presentation
//!   or context use (or both) is live state. A call's last presentation
//!   goes into its `tool_call_terminated` as `presentation`, when it differs
//!   from the one its `tool_call_started` stored; a call that never ended
//!   keeps the record of its last presentation. Of the context use, the
//!   record in effect at each `model_usage_recorded` and `run_finished` (a
//!   reply's footer and the run's end read it) and at the log's end stays.
//!   Every other live-state record becomes a `record_omitted` at its
//!   sequence, run, and time, so the sequence stays whole; a record that
//!   stays keeps only the member it stays for.
//!
//! Everything else, unknown records included, is copied unchanged.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::Step;
use crate::events::blobs;

const PRESENTATION: &str = "tool_presentation";
const CONTEXT: &str = "context";

pub(super) fn convert(lines: Vec<Value>, step: &mut Step<'_>) -> Result<Vec<Value>, String> {
    let moved = &mut *step.moved;
    let live = LiveState::of(&lines);
    Ok(lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            live.rewrite(index, &mut line);
            if let Some(payload) = line.get_mut("payload") {
                blobs::externalize(payload, &mut |digest, bytes| {
                    moved.entry(digest.to_owned()).or_insert(bytes);
                });
            }
            line
        })
        .collect())
}

fn payload_type(line: &Value) -> Option<&str> {
    line.get("payload")?.get("type")?.as_str()
}

/// The members of a live-state record that it carries: `Some` only for an
/// `acp_session_updated` with nothing but a presentation and context use.
fn live_members(line: &Value) -> Option<(bool, bool)> {
    let payload = line.get("payload")?.as_object()?;
    if payload.get("type")?.as_str()? != "acp_session_updated" {
        return None;
    }
    let presentation = payload.contains_key(PRESENTATION);
    let context = payload.contains_key(CONTEXT);
    let only_live = payload
        .keys()
        .all(|key| matches!(key.as_str(), "type" | PRESENTATION | CONTEXT));
    (only_live && (presentation || context)).then_some((presentation, context))
}

fn call_of(value: &Value) -> Option<String> {
    value.get("tool_call_id")?.as_str().map(str::to_owned)
}

/// What the scan of a log decided about its live state.
#[derive(Default)]
struct LiveState {
    /// Live-state records, by line, with the members each keeps.
    records: HashMap<usize, (bool, bool)>,
    /// The final presentation each ended call's termination takes.
    final_presentations: HashMap<usize, Value>,
}

impl LiveState {
    fn of(lines: &[Value]) -> Self {
        let mut starts = HashMap::<String, Value>::new();
        let mut ends = HashMap::<String, usize>::new();
        // Each call's last presentation, and the live record it is in.
        let mut last = HashMap::<String, (Value, Option<usize>)>::new();
        let mut live = HashMap::<usize, (bool, bool)>::new();
        let mut pending_context = None;
        let mut kept_context = HashSet::new();
        for (index, line) in lines.iter().enumerate() {
            let Some(payload) = line.get("payload") else {
                continue;
            };
            match payload_type(line) {
                Some("tool_call_started") => {
                    // The start's presentation without its tool identity,
                    // which a later presentation does not carry.
                    if let (Some(call), Some(Value::Object(presentation))) =
                        (call_of(payload), payload.get("presentation"))
                    {
                        let mut prepared = presentation.clone();
                        prepared.remove("tool");
                        starts.insert(call, Value::Object(prepared));
                    }
                }
                Some("tool_call_terminated") => {
                    if let Some(call) = call_of(payload) {
                        ends.insert(call, index);
                    }
                }
                Some("acp_session_updated") => {
                    let members = live_members(line);
                    if let Some(presented) = payload.get(PRESENTATION)
                        && let (Some(call), Some(presentation)) =
                            (call_of(presented), presented.get("presentation"))
                    {
                        last.insert(call, (presentation.clone(), members.map(|_| index)));
                    }
                    if payload.get(CONTEXT).is_some() {
                        // A record that stays anyway carries the context
                        // from here on.
                        pending_context = members.map(|_| index);
                    }
                    if let Some(members) = members {
                        live.insert(index, members);
                    }
                }
                Some("model_usage_recorded" | "run_finished") => {
                    kept_context.extend(pending_context.take());
                }
                _ => {}
            }
        }
        kept_context.extend(pending_context);

        let mut state = Self::default();
        let mut kept_presentation = HashSet::new();
        for (call, (presentation, record)) in last {
            match ends.get(&call) {
                Some(&end) => {
                    let has_own = lines[end]["payload"]
                        .get("presentation")
                        .is_some_and(|own| !own.is_null());
                    if !has_own && starts.get(&call) != Some(&presentation) {
                        state.final_presentations.insert(end, presentation);
                    }
                }
                // A call that never ended keeps its last presentation where
                // it was.
                None => kept_presentation.extend(record),
            }
        }
        for (index, (presentation, context)) in live {
            state.records.insert(
                index,
                (
                    presentation && kept_presentation.contains(&index),
                    context && kept_context.contains(&index),
                ),
            );
        }
        state
    }

    fn rewrite(&self, index: usize, line: &mut Value) {
        if let Some(presentation) = self.final_presentations.get(&index)
            && let Some(payload) = line.get_mut("payload").and_then(Value::as_object_mut)
        {
            payload.insert("presentation".into(), presentation.clone());
        }
        let Some(&(presentation, context)) = self.records.get(&index) else {
            return;
        };
        let Some(envelope) = line.as_object_mut() else {
            return;
        };
        if presentation || context {
            if let Some(payload) = envelope.get_mut("payload").and_then(Value::as_object_mut) {
                if !presentation {
                    payload.remove(PRESENTATION);
                }
                if !context {
                    payload.remove(CONTEXT);
                }
            }
            return;
        }
        let mut omitted = Map::new();
        omitted.insert("type".into(), Value::from("record_omitted"));
        omitted.insert("omitted_type".into(), Value::from("acp_session_updated"));
        envelope.insert("payload".into(), Value::Object(omitted));
        envelope.insert("skippable".into(), Value::Bool(true));
        // Only its sequence, run, and time stand for it.
        envelope.remove("engine_version");
        envelope.remove("origin");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn line(seq: u64, run: Option<&str>, payload: Value) -> Value {
        json!({"seq": seq, "run_id": run, "session_id": "s", "timestamp": "t", "payload": payload})
    }

    fn presentation(title: &str) -> Value {
        json!({"title": title, "arguments": [], "preview": []})
    }

    fn started(title: &str) -> Value {
        let mut presentation = presentation(title);
        presentation["tool"] = json!({"name": "fetch"});
        presentation
    }

    fn update(call: &str, title: &str) -> Value {
        json!({"type": "acp_session_updated",
            "tool_presentation": {"tool_call_id": call, "presentation": presentation(title)}})
    }

    fn context(used: u64) -> Value {
        json!({"type": "acp_session_updated", "context": {"used": used}})
    }

    fn converted(lines: Vec<Value>) -> Vec<Value> {
        let (mut moved, mut obsolete) = Default::default();
        convert(lines, &mut Step::in_memory(&mut moved, &mut obsolete)).expect("converts")
    }

    fn omitted(line: &Value) -> bool {
        line["payload"]["type"] == "record_omitted" && line["skippable"] == true
    }

    #[test]
    fn a_calls_last_presentation_moves_to_its_end_and_the_updates_go() {
        let lines = converted(vec![
            line(
                1,
                Some("r"),
                json!({"type": "tool_call_started", "tool_call_id": "a",
                "presentation": started("start")}),
            ),
            line(2, Some("r"), update("a", "first")),
            line(3, Some("r"), update("a", "last")),
            line(
                4,
                Some("r"),
                json!({"type": "tool_call_terminated", "tool_call_id": "a"}),
            ),
            // The start's presentation again: nothing to carry.
            line(
                5,
                Some("r"),
                json!({"type": "tool_call_started", "tool_call_id": "b",
                "presentation": started("same")}),
            ),
            line(6, Some("r"), update("b", "same")),
            line(
                7,
                Some("r"),
                json!({"type": "tool_call_terminated", "tool_call_id": "b"}),
            ),
            // A call that never ended keeps its last presentation's record.
            line(
                8,
                Some("r"),
                json!({"type": "tool_call_started", "tool_call_id": "c",
                "presentation": started("start")}),
            ),
            line(9, Some("r"), update("c", "early")),
            line(10, Some("r"), update("c", "late")),
        ]);
        assert!(omitted(&lines[1]) && omitted(&lines[2]));
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["run_id"], "r");
        assert_eq!(lines[3]["payload"]["presentation"], presentation("last"));
        assert!(omitted(&lines[5]));
        assert!(lines[6]["payload"].get("presentation").is_none());
        assert!(omitted(&lines[8]));
        assert_eq!(lines[9]["payload"], update("c", "late"));
    }

    #[test]
    fn the_context_in_effect_at_each_reader_stays() {
        let lines = converted(vec![
            line(1, Some("r"), context(1)),
            line(2, Some("r"), context(2)),
            line(3, Some("r"), json!({"type": "model_usage_recorded"})),
            line(4, Some("r"), context(3)),
            line(5, Some("r"), context(4)),
            line(6, Some("r"), json!({"type": "run_finished"})),
            line(7, None, context(5)),
            // A record carrying more than live state stays as it is and
            // carries the context from there.
            line(8, None, context(6)),
            line(
                9,
                None,
                json!({"type": "acp_session_updated", "context": {"used": 7},
                "notice": "n"}),
            ),
        ]);
        assert!(omitted(&lines[0]));
        assert_eq!(lines[1]["payload"], context(2));
        assert!(omitted(&lines[3]));
        assert_eq!(lines[4]["payload"], context(4));
        assert!(omitted(&lines[6]) && omitted(&lines[7]));
        assert_eq!(lines[8]["payload"]["notice"], "n");
    }

    #[test]
    fn a_record_of_both_keeps_only_the_member_it_stays_for() {
        let mut both = update("a", "late");
        both["context"] = json!({"used": 9});
        let lines = converted(vec![
            line(
                1,
                Some("r"),
                json!({"type": "tool_call_started", "tool_call_id": "a",
                "presentation": started("start")}),
            ),
            line(2, Some("r"), both),
            line(
                3,
                Some("r"),
                json!({"type": "tool_call_terminated", "tool_call_id": "a"}),
            ),
        ]);
        assert_eq!(lines[1]["payload"], context(9));
        assert_eq!(lines[2]["payload"]["presentation"], presentation("late"));
    }
}
