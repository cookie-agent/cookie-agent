//! Test-only: an ACP session's log, grown to any length, for what section 6 of
//! `docs/site/specs/forever-sessions.md` bounds.
//!
//! Its shape follows the store's largest logs, by counts and sizes alone (section
//! 12): how many turns a run has, how many calls a turn, how large each record
//! is, and how often an approval, a producer's message, an internal agent, a
//! delegation, or a new ACP session comes. Its records are the frozen golden logs'
//! with their identities changed, so they have real shapes, and whatever takes them
//! (a real append, or a real open) validates them as any others.
//!
//! It also does what makes a bound dangerous, each at the rate the store's logs
//! show or above: a call open across many turns, a background job that ends after
//! its run does and after later runs, a delegation that finishes runs later, an
//! internal agent that starts after its run ended, an input recalled, and a call
//! that never ends.

use std::{
    collections::HashMap,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use cookie_agent_protocol::{EventOrigin, EventPayload, RunId, SessionId, StoredEvent};
use jiff::Timestamp;
use serde_json::{Value, json};

use super::{EventLog, EventLogError, LineReader, blobs, stored_line};

/// The session every synthetic log is: the golden ACP scenario's, whose creation
/// it starts with.
pub const SESSION: &str = "01a1b46f-0f55-7ba3-86e8-f8d942e26916";

/// How long a synthetic log is, and what it holds per run. The defaults of
/// [`Self::largest_times`] are this store's largest log, 12.5 MB of 7,033 records,
/// 159 runs, 1,163 turns, and 1,007 tool calls, with what the other large logs do
/// and that one does not (approvals, delegations) at their rates.
#[derive(Clone, Debug)]
pub struct Mix {
    /// How many runs it holds.
    pub runs: u64,
    /// What sizes the texts in it are multiplied by.
    pub size: f64,
    /// How many delegations a run makes, on average: one per megabyte of the
    /// largest log, above what the store's logs do, as each costs what it does
    /// for as long as the log.
    pub delegations: f64,
    pub seed: u64,
}

impl Mix {
    /// A log `times` the length of this store's largest.
    #[must_use]
    pub fn largest_times(times: f64) -> Self {
        Self {
            runs: (159.0 * times).round() as u64,
            size: 1.0,
            delegations: 0.08,
            seed: 0x5eed,
        }
    }

    /// `runs` runs of texts a tenth of their real size, for a test: the same
    /// records, as many of them per run, and fewer bytes to read.
    #[must_use]
    pub fn small(runs: u64) -> Self {
        Self {
            runs,
            size: 0.1,
            delegations: 0.08,
            seed: 0x5eed,
        }
    }
}

/// Where a synthetic log's records go.
pub trait Sink {
    fn record(
        &mut self,
        run: Option<RunId>,
        origin: &str,
        payload: EventPayload,
    ) -> Result<(), EventLogError>;
}

/// Writes each record as an append would, without its costs: no sync, and nothing
/// held. Validation is left to what opens the file, which is the point.
pub struct FileSink {
    writer: io::BufWriter<fs::File>,
    session: SessionId,
    blob_dir: PathBuf,
    next_seq: u64,
    bytes: u64,
    path: PathBuf,
}

impl FileSink {
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            writer: io::BufWriter::with_capacity(1 << 20, fs::File::create(path)?),
            session: SESSION.parse().expect("the golden session's ID"),
            blob_dir: blobs::tree_blob_dir(path),
            next_seq: 1,
            bytes: 0,
            path: path.to_owned(),
        })
    }

    /// How many bytes and records it wrote.
    pub fn finish(mut self) -> io::Result<(u64, u64)> {
        self.writer.flush()?;
        Ok((self.bytes, self.next_seq - 1))
    }
}

impl Sink for FileSink {
    fn record(
        &mut self,
        run: Option<RunId>,
        origin: &str,
        payload: EventPayload,
    ) -> Result<(), EventLogError> {
        let io_error = |source| EventLogError::Io {
            path: self.path.clone(),
            source,
        };
        // A millisecond apart from a fixed start, so that a log's bytes are a
        // function of its mix.
        let timestamp = Timestamp::from_millisecond(1_790_000_000_000 + self.next_seq as i64)
            .expect("a time in range");
        let event = StoredEvent {
            engine_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            origin: Some(EventOrigin::new(origin).expect("a valid origin")),
            session_id: self.session,
            run_id: run,
            seq: self.next_seq,
            timestamp,
            payload,
        };
        let mut moved = Vec::new();
        let mut line = stored_line(&event, &mut |digest, bytes| {
            moved.push((digest.to_owned(), bytes));
        })
        .map_err(|source| EventLogError::Json {
            path: self.path.clone(),
            source,
        })?;
        for (digest, bytes) in &moved {
            blobs::write(&self.blob_dir, digest, bytes).map_err(io_error)?;
        }
        line.push(b'\n');
        self.writer.write_all(&line).map_err(io_error)?;
        self.bytes += line.len() as u64;
        self.next_seq += 1;
        Ok(())
    }
}

/// Appends each record to a log, each validated as strictly as a live append is,
/// with what the append path checks in a test besides.
pub struct AppendSink<'a>(pub &'a EventLog);

impl Sink for AppendSink<'_> {
    fn record(
        &mut self,
        run: Option<RunId>,
        origin: &str,
        payload: EventPayload,
    ) -> Result<(), EventLogError> {
        self.0
            .append(
                run,
                EventOrigin::new(origin).expect("a valid origin"),
                payload,
            )
            .map(|_| ())
    }
}

/// A golden log's records by type, the first of each, as they read with their
/// values in blobs read back.
struct Templates(HashMap<String, Value>);

impl Templates {
    fn golden_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../engine/tests/fixtures/history")
            .join(cookie_agent_protocol::HISTORY_FORMAT.to_string())
    }

    fn load() -> Self {
        let root = Self::golden_root();
        let mut templates = HashMap::new();
        // The ACP scenario's first, so what it holds is an ACP agent's.
        Self::read_log(
            &root.join("acp").join(SESSION).join("events.jsonl"),
            &mut templates,
        );
        // What it does not reach, of a native model: an internal agent.
        Self::read_log(
            &root
                .join("native-compaction")
                .join("01a1b355-0fa9-7c93-a8ee-c51cff73938d")
                .join("events.jsonl"),
            &mut templates,
        );
        Self(templates)
    }

    fn read_log(path: &Path, templates: &mut HashMap<String, Value>) {
        let blob_dir = blobs::tree_blob_dir(path);
        let mut fetch = |digest: &str| blobs::read(&blob_dir, digest);
        let bytes = fs::read(path).expect("a golden log");
        let mut reader = LineReader::new(path);
        cookie_agent_protocol::read_stored_history(|| {
            for (index, line) in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .enumerate()
            {
                let record = reader
                    .read(index, line, &mut fetch)
                    .expect("a golden line reads")
                    .record
                    .expect("a golden line holds a record");
                let value = to_stored_value(&record.payload);
                let kind = value["type"].as_str().expect("a payload's type").to_owned();
                templates.entry(kind).or_insert(value);
            }
        });
    }

    fn of(&self, kind: &str) -> Value {
        self.0
            .get(kind)
            .unwrap_or_else(|| panic!("no golden log holds a `{kind}`"))
            .clone()
    }
}

fn to_stored_value(payload: &EventPayload) -> Value {
    cookie_agent_protocol::write_stored_history(|| {
        serde_json::to_value(payload).expect("a payload serializes")
    })
}

fn payload_of(value: Value) -> EventPayload {
    cookie_agent_protocol::read_stored_history(|| {
        serde_json::from_value(value).expect("a synthetic payload deserializes")
    })
}

/// A small generator of numbers: a log's bytes are a function of its mix.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let shifted = self.0 ^ (self.0 >> 33);
        shifted.wrapping_mul(0xff51_afd7_ed55_8ccd) >> 16
    }

    /// Whether something that happens `rate` of the time happens now.
    fn chance(&mut self, rate: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1_000_000.0 < rate
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[(self.next() % from.len() as u64) as usize]
    }
}

/// Text of about `length` bytes that is not all alike, built from a few stems so
/// that values long enough to leave for a blob share them.
fn text(numbers: &mut Numbers, length: usize) -> String {
    const STEM: &str = "fn check(&self) -> Result<(), Error> { self.inner.validate()?; Ok(()) }\n";
    let mut text = String::with_capacity(length + STEM.len());
    let seed = numbers.next() % 8;
    while text.len() < length {
        text.push_str(STEM);
        text.push_str(&seed.to_string());
    }
    text.truncate(length);
    text
}

/// What the generator knows of the run it is writing.
struct RunWriter<'a> {
    sink: &'a mut dyn Sink,
    templates: &'a Templates,
    numbers: Numbers,
    size: f64,
    delegation_rate: f64,
    ids: u128,
    seq: u64,
    model_turn_seq: u64,
    /// Where the latest input went in, which a committed turn covers.
    input_through: u64,
    acp_session: u64,
    /// Calls that outlive their turn, by how many turns they have left.
    open_calls: Vec<OpenCall>,
    /// Background jobs that outlive their run, by how many runs they have left.
    jobs: Vec<(u64, OpenCall)>,
    /// Delegations that outlive their run.
    delegations: Vec<(u64, RunId, Value)>,
    /// Inputs admitted with no run that a later step recalls.
    admitted: Vec<(u64, Value)>,
}

#[derive(Clone)]
struct OpenCall {
    run: RunId,
    tool_call_id: String,
    owner: Value,
    turns_left: u64,
}

const ORIGIN_ACP: &str = "engine:acp";
const ORIGIN_LOOP: &str = "engine:model-loop";
const ORIGIN_CLIENT: &str = "client:synthetic";
const ORIGIN_APPROVALS: &str = "engine:approvals";
const ORIGIN_RESULT: &str = "engine:tool-result";
const ORIGIN_DELEGATION: &str = "engine:delegation";
const ORIGIN_INTERNAL: &str = "engine:internal-agent";

impl RunWriter<'_> {
    fn id(&mut self) -> String {
        self.ids += 1;
        uuid::Uuid::from_u128((0x0191_0000_0000_7000_8000_0000_0000_0000_u128) | self.ids)
            .to_string()
    }

    fn size(&self, bytes: usize) -> usize {
        ((bytes as f64) * self.size) as usize
    }

    fn write(
        &mut self,
        run: Option<RunId>,
        origin: &str,
        value: Value,
    ) -> Result<(), EventLogError> {
        self.seq += 1;
        self.sink.record(run, origin, payload_of(value))
    }

    /// Calibrated, as the sizes of [`Mix`] are: a run's records hold this much of
    /// its bytes in their prompts and memory.
    fn start_run(&mut self, index: u64, run: RunId) -> Result<(), EventLogError> {
        let mut started = self.templates.of("run_started");
        let prompt_length = self.size(9_000);
        started["agent"]["composed_prompt"] = json!(text(&mut self.numbers, prompt_length));
        started["client_run_id"] = json!(format!("synthetic-{index}"));
        started["input_through_seq"] = json!(self.seq);
        self.write(Some(run), ORIGIN_CLIENT, started)
    }

    fn submit(&mut self, run: RunId) -> Result<(), EventLogError> {
        let length = self.size(300);
        let body = text(&mut self.numbers, length);
        self.write(
            Some(run),
            ORIGIN_CLIENT,
            json!({
                "type": "user_input_submitted",
                "content": [{"type": "text", "text": body}],
            }),
        )?;
        let submitted = self.seq;
        self.apply(run, submitted)
    }

    fn apply(&mut self, run: RunId, input: u64) -> Result<(), EventLogError> {
        self.write(
            Some(run),
            ORIGIN_LOOP,
            json!({"type": "user_input_applied", "user_input_seq": input}),
        )?;
        self.input_through = self.seq;
        Ok(())
    }

    fn acp_session(&mut self, run: RunId, index: u64) -> Result<(), EventLogError> {
        // About 61 starts per 159 runs, and most of them resume.
        if !self.numbers.chance(0.38) {
            return Ok(());
        }
        let fresh = index == 0 || self.numbers.chance(1.0 / 30.0);
        if fresh {
            self.acp_session += 1;
        }
        let mut started = self.templates.of("acp_session_started");
        started["acp_session_id"] = json!(format!("synthetic-session-{}", self.acp_session));
        started["how"] = json!(if fresh { "new" } else { "resumed" });
        self.write(Some(run), ORIGIN_ACP, started)
    }

    fn update_acp(&mut self, run: Option<RunId>) -> Result<(), EventLogError> {
        let used = self.numbers.next() % 200_000;
        self.write(
            run,
            ORIGIN_ACP,
            json!({
                "type": "acp_session_updated",
                "context": {"used": used, "size": 200_000},
            }),
        )
    }

    fn turn(&mut self, run: RunId, calls: u64, ordinal: u32) -> Result<(), EventLogError> {
        let attempt = self.id();
        let mut started = self.templates.of("model_attempt_started");
        started["attempt_id"] = json!(attempt);
        started["attempt_ordinal"] = json!(ordinal);
        started["fallback_index"] = json!(0);
        started["retry_ordinal"] = json!(0);
        self.write(Some(run), ORIGIN_ACP, started)?;
        if self.numbers.chance(0.78) {
            let mut output = self.templates.of("model_output_started");
            output["attempt_id"] = json!(attempt);
            self.write(Some(run), ORIGIN_ACP, output)?;
        }
        if self.numbers.chance(0.14) {
            self.update_acp(Some(run))?;
        }
        self.model_turn_seq += 1;
        let turn_seq = self.model_turn_seq;
        let length = self.turn_text_length();
        let mut content = vec![json!({
            "type": "text",
            "text": text(&mut self.numbers, length),
            "metadata": null,
        })];
        let mut parts = Vec::new();
        for _ in 0..calls {
            let call = format!("call-{}", self.id());
            parts.push((content.len(), call.clone()));
            content.push(json!({
                "type": "tool_call",
                "id": call,
                "provider_item_id": null,
                "name": "acp.edit",
                "input": {"path": "/work/notes.txt"},
                "raw_input": null,
                "metadata": null,
            }));
        }
        let mut committed = self.templates.of("model_turn_committed");
        committed["attempt_id"] = json!(attempt);
        committed["model_turn_seq"] = json!(turn_seq);
        committed["input_through_seq"] = json!(self.input_through);
        committed["turn"]["content"] = Value::Array(content);
        committed["turn"]["finish_reason"] = json!(if calls == 0 { "stop" } else { "tool_calls" });
        self.write(Some(run), ORIGIN_ACP, committed)?;
        for (index, call) in parts {
            let tool_call_id = self.id();
            let owner = json!({
                "model_turn_seq": turn_seq,
                "content_index": index,
                "model_call_id": call,
                "provider_item_id": null,
            });
            let mut started = self.templates.of("tool_call_started");
            started["tool_call_id"] = json!(tool_call_id);
            started["owner"] = owner.clone();
            let title_length = self.size(400);
            started["presentation"]["title"] =
                json!(text(&mut self.numbers, title_length).replace('\n', " "));
            self.write(Some(run), ORIGIN_ACP, started)?;
            let open = OpenCall {
                run,
                tool_call_id,
                owner,
                turns_left: 0,
            };
            // A call open across turns: up to 28 of them in the store's logs.
            if self.numbers.chance(0.04) {
                self.open_calls.push(OpenCall {
                    turns_left: self.numbers.next() % 28 + 1,
                    ..open
                });
            // A job in the background, which outlives its run.
            } else if self.numbers.chance(0.002) {
                let runs = self.numbers.next() % 8 + 1;
                self.jobs.push((runs, open));
            } else {
                if self.numbers.chance(0.19 / 5.8) {
                    self.approval(run, &open.tool_call_id)?;
                }
                self.terminate(&open)?;
            }
        }
        // The calls whose turn has come.
        let mut still_open = Vec::new();
        for mut call in std::mem::take(&mut self.open_calls) {
            call.turns_left -= 1;
            if call.turns_left == 0 {
                self.terminate(&call)?;
            } else {
                still_open.push(call);
            }
        }
        self.open_calls = still_open;
        Ok(())
    }

    /// A committed turn's text, by the quantiles of the store's logs: 1.5 KB at
    /// the median, 3 KB at the 90th percentile, and 5.7 KB at the 99th.
    fn turn_text_length(&mut self) -> usize {
        let roll = self.numbers.next() % 100;
        self.size(if roll < 50 {
            250
        } else if roll < 90 {
            900
        } else if roll < 99 {
            1_900
        } else {
            4_500
        })
    }

    /// A call's result, by the same logs' quantiles: 4.4 KB at the median, 9.9 KB
    /// at the 90th percentile, and 18 KB at the 99th.
    fn result_length(&mut self) -> usize {
        let roll = self.numbers.next() % 100;
        self.size(if roll < 50 {
            2_500
        } else if roll < 90 {
            5_000
        } else if roll < 99 {
            7_800
        } else {
            16_000
        })
    }

    fn terminate(&mut self, call: &OpenCall) -> Result<(), EventLogError> {
        let length = self.result_length();
        let mut terminated = self.templates.of("tool_call_terminated");
        terminated["tool_call_id"] = json!(call.tool_call_id);
        terminated["owner"] = call.owner.clone();
        terminated["result"]["output"] = json!(text(&mut self.numbers, length));
        self.write(Some(call.run), ORIGIN_RESULT, terminated)
    }

    fn approval(&mut self, run: RunId, tool_call: &str) -> Result<(), EventLogError> {
        let approval = self.id();
        let mut requested = self.templates.of("approval_requested");
        requested["request"]["approval_id"] = json!(approval);
        requested["request"]["tool_call_id"] = json!(tool_call);
        self.write(Some(run), ORIGIN_APPROVALS, requested)?;
        // The golden scenario's first step: an evaluation.
        let mut evaluated = self.templates.of("approval_step");
        evaluated["approval_id"] = json!(approval);
        self.write(Some(run), ORIGIN_APPROVALS, evaluated)?;
        let response = self.id();
        self.write(
            Some(run),
            ORIGIN_APPROVALS,
            json!({
                "type": "approval_step",
                "approval_id": approval,
                "step": {
                    "type": "user_decision",
                    "client_response_id": response,
                    "decision": "approve_once",
                },
            }),
        )?;
        let mut finalized = self.templates.of("approval_finalized");
        finalized["approval_id"] = json!(approval);
        self.write(Some(run), ORIGIN_APPROVALS, finalized)
    }

    fn internal_agent(&mut self, run: RunId) -> Result<(), EventLogError> {
        let invocation = self.id();
        let internal_run = self.id();
        let mut started = self.templates.of("internal_agent_started");
        started["invocation_id"] = json!(invocation);
        started["internal_run_id"] = json!(internal_run);
        self.write(Some(run), ORIGIN_INTERNAL, started)?;
        let mut usage = self.templates.of("internal_agent_usage_recorded");
        usage["internal_run_id"] = json!(internal_run);
        self.write(Some(run), ORIGIN_INTERNAL, usage)?;
        let mut finished = self.templates.of("internal_agent_finished");
        finished["invocation_id"] = json!(invocation);
        finished["internal_run_id"] = json!(internal_run);
        self.write(Some(run), ORIGIN_INTERNAL, finished)
    }

    fn producer_message(&mut self) -> Result<String, EventLogError> {
        let message = self.id();
        let idempotency_key = self.id();
        let length = self.size(1_300);
        let body = text(&mut self.numbers, length);
        let mut accepted = self.templates.of("producer_message_accepted");
        accepted["message_id"] = json!(message);
        accepted["idempotency_key"] = json!(idempotency_key);
        accepted["body"] = json!([{"type": "text", "text": body}]);
        self.write(None, ORIGIN_ACP, accepted)?;
        Ok(message)
    }

    fn delegation(&mut self, run: RunId) -> Result<(), EventLogError> {
        let invocation = self.id();
        let child_session = self.id();
        let child_run = self.id();
        let call = self.id();
        let mut reserved = self.templates.of("delegation_reserved");
        reserved["reservation"]["invocation_id"] = json!(invocation);
        reserved["reservation"]["parent_run_id"] = json!(run.to_string());
        reserved["reservation"]["parent_tool_call_id"] = json!(call);
        reserved["reservation"]["child_session_id"] = json!(child_session);
        reserved["request"]["resume_session_id"] = json!(null);
        self.write(Some(run), ORIGIN_DELEGATION, reserved)?;
        self.write(
            Some(run),
            ORIGIN_DELEGATION,
            json!({
                "type": "delegation_progressed",
                "invocation_id": invocation,
                "stage": {"type": "started", "child_session_id": child_session},
            }),
        )?;
        self.write(
            Some(run),
            ORIGIN_DELEGATION,
            json!({
                "type": "delegation_progressed",
                "invocation_id": invocation,
                "stage": {"type": "run_started", "child_run_id": child_run},
            }),
        )?;
        let mut finished = self.templates.of("delegation_finished");
        finished["invocation_id"] = json!(invocation);
        finished["child_session_id"] = json!(child_session);
        finished["child_run_id"] = json!(child_run);
        // Up to 6 runs later in the store's logs, and mostly at once.
        if self.numbers.chance(0.3) {
            let runs = self.numbers.next() % 7 + 1;
            self.delegations.push((runs, run, finished));
            return Ok(());
        }
        self.write(Some(run), ORIGIN_DELEGATION, finished)
    }

    /// What was waiting on a run's having ended.
    fn after_run(&mut self) -> Result<(), EventLogError> {
        let mut jobs = Vec::new();
        for (left, call) in std::mem::take(&mut self.jobs) {
            if left <= 1 {
                self.terminate(&call)?;
            } else {
                jobs.push((left - 1, call));
            }
        }
        self.jobs = jobs;
        let mut delegations = Vec::new();
        for (left, run, finished) in std::mem::take(&mut self.delegations) {
            if left <= 1 {
                self.write(Some(run), ORIGIN_DELEGATION, finished)?;
            } else {
                delegations.push((left - 1, run, finished));
            }
        }
        self.delegations = delegations;
        Ok(())
    }

    fn write_run(&mut self, index: u64) -> Result<(), EventLogError> {
        // A message that wakes the session starts a run of its own.
        let woken = index > 0 && self.numbers.chance(0.5);
        let message = if woken {
            Some(self.producer_message()?)
        } else {
            None
        };
        if self.numbers.chance(0.12) {
            let length = self.size(200);
            let body = text(&mut self.numbers, length);
            self.write(
                None,
                ORIGIN_CLIENT,
                json!({
                    "type": "user_input_admitted",
                    "content": [{"type": "text", "text": body.clone()}],
                }),
            )?;
            let admitted = self.seq;
            // Recalled at once or left, as the queue is.
            self.admitted
                .push((admitted, json!([{"type": "text", "text": body}])));
            if let Some((seq, content)) = self.admitted.pop() {
                self.write(
                    None,
                    ORIGIN_CLIENT,
                    json!({
                        "type": "user_input_recalled",
                        "user_input_seq": seq,
                        "content": content,
                    }),
                )?;
            }
        }
        let run: RunId = self.id().parse().expect("a run's ID");
        self.start_run(index, run)?;
        let claim = if let Some(message) = &message {
            self.write(
                Some(run),
                ORIGIN_ACP,
                json!({"type": "producer_message_admitted", "message_id": message}),
            )?;
            self.input_through = self.seq;
            self.write(
                Some(run),
                ORIGIN_ACP,
                json!({"type": "producer_messages_claimed", "message_ids": [message]}),
            )?;
            Some(self.seq)
        } else {
            self.submit(run)?;
            None
        };
        self.acp_session(run, index)?;
        // The store's logs: from 1 turn to 87, 7.3 on average.
        let turns = if index % 159 == 158 {
            87
        } else {
            self.numbers.pick(&[
                1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 7, 8, 10, 12, 15, 40,
            ])
        };
        for turn in 0..turns {
            // 0.87 calls a turn, and the last turn answers.
            let calls = if turn + 1 == turns {
                0
            } else if self.numbers.chance(0.05) {
                2
            } else {
                1
            };
            self.turn(run, calls, turn as u32 + 1)?;
            if self.numbers.chance(self.delegation_rate / 7.3) {
                self.delegation(run)?;
            }
            if turn == 0
                && let (Some(message), Some(claim)) = (&message, claim)
            {
                self.write(
                    Some(run),
                    ORIGIN_ACP,
                    json!({
                        "type": "producer_message_consumed",
                        "message_id": message,
                        "run_id": run.to_string(),
                    }),
                )?;
                self.write(
                    Some(run),
                    ORIGIN_ACP,
                    json!({"type": "producer_messages_released", "claim_seq": claim}),
                )?;
            }
        }
        let mut usage = self.templates.of("model_usage_recorded");
        usage["model_turn_seq"] = json!(self.model_turn_seq);
        self.write(Some(run), ORIGIN_ACP, usage)?;
        let length = self.size(1_500);
        let final_text = text(&mut self.numbers, length);
        self.write(
            Some(run),
            ORIGIN_LOOP,
            json!({
                "type": "run_finished",
                "outcome": {"type": "completed", "final_text": final_text},
            }),
        )?;
        // An internal agent that starts once its run ended, as a title's does.
        if self.numbers.chance(0.19) {
            self.internal_agent(run)?;
        }
        self.after_run()
    }
}

/// Writes the log `mix` describes to `sink`.
pub fn write(mix: &Mix, sink: &mut dyn Sink) -> Result<(), EventLogError> {
    let templates = Templates::load();
    let creation = templates.of("session_created");
    sink.record(None, "client:synthetic", payload_of(creation))?;
    let mut writer = RunWriter {
        sink,
        templates: &templates,
        numbers: Numbers(mix.seed),
        size: mix.size,
        delegation_rate: mix.delegations,
        ids: 0,
        seq: 1,
        model_turn_seq: 0,
        input_through: 1,
        acp_session: 0,
        open_calls: Vec::new(),
        jobs: Vec::new(),
        delegations: Vec::new(),
        admitted: Vec::new(),
    };
    for index in 0..mix.runs {
        writer.write_run(index)?;
    }
    Ok(())
}

/// Writes the log `mix` describes to a file at `path`, and how many bytes and
/// records that took.
pub fn write_file(mix: &Mix, path: &Path) -> Result<(u64, u64), EventLogError> {
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    let mut sink = FileSink::create(path).map_err(io_error)?;
    write(mix, &mut sink)?;
    sink.finish().map_err(io_error)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ownership::WriteAuthority;

    fn creation() -> EventPayload {
        payload_of(Templates::load().of("session_created"))
    }

    /// Appends a synthetic log of `runs` runs, each record as strictly as a live
    /// append, past where a windowed log lets go of what settled.
    fn appended(directory: &Path, runs: u64) -> Arc<EventLog> {
        let path = directory.join(SESSION).join("events.jsonl");
        fs::create_dir_all(path.parent().expect("a directory")).expect("a directory");
        let log = EventLog::create(
            path,
            SESSION.parse().expect("a session"),
            EventOrigin::new("client:synthetic").expect("an origin"),
            creation(),
        )
        .expect("a log");
        let mut sink = SkipCreation {
            log: &log,
            skipped: false,
        };
        write(&Mix::small(runs), &mut sink).expect("a synthetic log appends");
        log
    }

    /// [`AppendSink`] for a log created with its creation already.
    struct SkipCreation<'a> {
        log: &'a EventLog,
        skipped: bool,
    }

    impl Sink for SkipCreation<'_> {
        fn record(
            &mut self,
            run: Option<RunId>,
            origin: &str,
            payload: EventPayload,
        ) -> Result<(), EventLogError> {
            if !std::mem::replace(&mut self.skipped, true) {
                return Ok(());
            }
            AppendSink(self.log).record(run, origin, payload)
        }
    }

    /// A long log through real appends: what it holds, what its folds give, and
    /// its validation agree with a reopening of its file, with what it let go of.
    #[test]
    fn a_synthetic_log_appends_and_answers_as_its_file() {
        let directory = tempfile::tempdir().expect("a directory");
        let log = appended(directory.path(), 48);
        assert!(log.window_for_test().is_some(), "a windowed log");
        assert!(
            log.validation
                .lock()
                .expect("validation lock poisoned")
                .let_go,
            "it let go of what settled"
        );
        log.assert_window_reads_as_reopened();
        let whole = log
            .history()
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .expect("a pass");
        super::super::state::reference::assert_matches_scans(&log.state(), &whole);
        let reopened = EventLog::open(log.path().to_owned(), log.session_id).expect("a reopening");
        assert_eq!(reopened.record_count(), log.record_count(), "the records");
    }

    /// A revert to well before the window, and a fork through there, read what
    /// the window let go of from the file, and answer as the file does.
    #[test]
    fn a_revert_and_a_fork_before_the_window_read_the_file() {
        let directory = tempfile::tempdir().expect("a directory");
        let log = appended(directory.path(), 40);
        let window = log.window_for_test().expect("a windowed log");
        let through = window / 3;
        // A fork of the log through there, written whole and opened.
        let authority = WriteAuthority::new();
        let fork_session: SessionId = "01a1b46f-0f55-7ba3-86e8-f8d942e26917"
            .parse()
            .expect("a session");
        let prefix = log
            .read_fork_prefix(fork_session, through)
            .expect("a fork's prefix");
        let fork_path = directory
            .path()
            .join(fork_session.to_string())
            .join("events.jsonl");
        fs::create_dir_all(fork_path.parent().expect("a directory")).expect("a directory");
        // A store's fork shares its source's blobs with the tree it makes.
        let blob_dir = blobs::tree_blob_dir(&fork_path);
        fs::create_dir_all(&blob_dir).expect("a directory");
        for entry in fs::read_dir(blobs::tree_blob_dir(log.path())).expect("the blobs") {
            let entry = entry.expect("a blob");
            fs::copy(entry.path(), blob_dir.join(entry.file_name())).expect("a copy");
        }
        let fork = EventLog::fork_buffered(fork_path.clone(), prefix, authority.capability());
        let lines = fork.write_whole(&fork_path).expect("a fork written whole");
        fork.mark_written(lines);
        let forked = EventLog::open_read_only(fork_path, fork_session).expect("the fork opens");
        let source = log
            .history_range(..=through)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .expect("a pass");
        let copied = forked
            .history()
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .expect("a pass");
        assert_eq!(copied.len(), source.len(), "the fork's records");
        // The revert, which cuts where only a mark is near.
        log.truncate_after_for_test(through).expect("a revert");
        assert_eq!(log.physical_tip_seq(), through, "the revert's tip");
        log.assert_window_reads_as_reopened();
        let reopened = EventLog::open(log.path().to_owned(), log.session_id).expect("a reopening");
        assert_eq!(reopened.physical_tip_seq(), through, "the cut");
    }
}
