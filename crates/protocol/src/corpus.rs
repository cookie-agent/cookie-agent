//! The recorded protocol corpora, and what the tests that replay them share.
//!
//! # Layout
//!
//! `crates/protocol/compat/corpus/` holds two kinds of fixtures:
//!
//! - **`<surface>/<version>/*.jsonl`**: conversations recorded at that
//!   version of the surface (`client`, `plugin`, or `runtime`). Each line is
//!   one JSON-RPC message as it crossed the connection,
//!   `{"from": <side>, "message": <message>}`, in the order it crossed. The
//!   sides are `client` and `daemon` on the client surface, `daemon` and
//!   `plugin` on the plugin surface, and `daemon` and `runtime` on the
//!   runtime surface.
//!   - `client/1.0.0/session.jsonl`: a TUI-like client against an in-process
//!     daemon whose model is a scripted provider: the handshake, the catalog,
//!     a session, its event subscription, an image upload, a run with the
//!     image whose `bash` call waits for approval while two steers arrive,
//!     the approval, and reads of the session and the artifact.
//!   - `runtime/1.0.0/coding.jsonl`: the same daemon and its local coding
//!     runtime during that session: the handshake, `capability/open`,
//!     `runtime/context`, the `bash` call's preparation, revalidation, and
//!     execution with its detach point and progress, and the shutdown.
//!   - `plugin/1.0.0/reference.jsonl`: the scripted conversation of
//!     `examples/plugins/python/check_reference_plugin.py` with the reference
//!     plugin: the handshake, producer recovery, a tool call with its hook
//!     and progress, events, a producer message, and shutdown.
//! - **`future/*.json`**: what a newer peer may send: unknown members,
//!   enum values, event tags, display block kinds, methods, and
//!   notifications, one file per reader: `client.json` (results and
//!   notifications a client reads). Each file says how its test reads it.
//!   Unlike the recorded corpora they grow as readers learn to tolerate
//!   more.
//!
//! # Rules
//!
//! - **A recorded version directory is frozen.** [`frozen`] reads it only
//!   when its contents still have the digest [`FROZEN`] pins for it, and
//!   fails otherwise. A frozen corpus is never rewritten and no
//!   `COOKIE_UPDATE_*` switch applies to it: what an old peer sent stays
//!   what it sent. A later minor records its own `<surface>/<version>/`
//!   directory beside the earlier ones and pins it; the replays then read
//!   every pinned version.
//! - **Recorders refuse to overwrite.** They write `<root>/<surface>/<version>/`
//!   for this build's version and refuse when it exists
//!   ([`Recording::write_new`]):
//!   `COOKIE_RECORD_CORPUS=<root> cargo test -p cookie_agent_server --
//!   --ignored record_the_client_and_runtime_corpora` (a relative root is
//!   read from the workspace root) and
//!   `python3 examples/plugins/python/check_reference_plugin.py --record <root>`.
//!   `update-compat-baseline.sh` never touches `corpus/`.
//! - **Recordings keep what changes per run** (IDs, timestamps, digests of
//!   per-run data), except that the recorders write their scratch directory
//!   as [`SCRATCH_PLACEHOLDER`] and the daemon's machine ID as
//!   [`MACHINE_ID_PLACEHOLDER`]. A replay depends on none of them: a request
//!   it replays gets the values the replay's own replies handed out in place
//!   of the recorded ones ([`Substitutions`]), and a replay that runs a
//!   workspace puts its own directory in place of the placeholder.
//!
//! # What the replays prove
//!
//! New builds accept old data:
//!
//! - `cookie_agent_server` (`corpus_tests`): the recorded client requests,
//!   replayed against a fresh daemon in the recorded order, are answered
//!   as they were (no `-32602` or `-32601`).
//! - `cookie_agent_protocol` (`corpus_tests`): every corpus directory is
//!   pinned and unchanged; the recorded replies read as their method's
//!   result and the recorded notifications as their params, and the
//!   notifications reach a real [`crate::Client`] without a diagnostic or
//!   an unknown event.
//!
//! Readers tolerate future data: each of those tests also reads its
//! surface's recorded information with [`add_future_members`] applied, and
//! the fixtures of `future/` for its reader.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{MessageFrame, Transport, TransportError};

/// The corpus root, `crates/protocol/compat/corpus`.
pub const CORPUS_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/compat/corpus");

/// The protocol baselines, `crates/protocol/compat`.
pub const COMPAT_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/compat");

/// The environment variable naming the corpus root a recorder writes to.
pub const RECORD_VARIABLE: &str = "COOKIE_RECORD_CORPUS";

/// How recordings spell the recorder's scratch directory.
pub const SCRATCH_PLACEHOLDER: &str = "/cookie-corpus";

/// The machine ID recordings carry in place of the recording machine's.
pub const MACHINE_ID_PLACEHOLDER: &str = "0123456789abcdef0123456789abcdef";

/// Every frozen corpus directory, as surface, version, and the SHA-256 of
/// its contents ([`directory_digest`]). An entry is added once, when its
/// directory is recorded, and never changed.
pub const FROZEN: &[(&str, &str, &str)] = &[
    (
        "client",
        "1.0.0",
        "30b9835b7a8a3ba22e06dcfdaa0cc2914a47dabc9cbbd44f4a1c51862bd71600",
    ),
    (
        "plugin",
        "1.0.0",
        "faf1d3e6fdb93b10ace75fe609801c36ab863399f1188d2da5b6ae0ade79610f",
    ),
    (
        "runtime",
        "1.0.0",
        "0d52ff3f3e6921f2144708d9cfb8881f89abe0d8a73bc3a3e0a2eb444b99a57c",
    ),
];

/// One recorded message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Recorded {
    /// The side that sent it.
    pub from: String,
    /// The JSON-RPC message.
    pub message: Value,
}

impl Recorded {
    /// The message's method, for a request or a notification.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.message.get("method").and_then(Value::as_str)
    }

    /// The message's ID, for a request or a reply.
    #[must_use]
    pub fn id(&self) -> Option<&Value> {
        self.message.get("id")
    }

    #[must_use]
    pub fn is_request(&self) -> bool {
        self.method().is_some() && self.id().is_some()
    }

    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.method().is_some() && self.id().is_none()
    }

    #[must_use]
    pub fn is_reply(&self) -> bool {
        self.method().is_none() && self.id().is_some()
    }
}

/// One file of a corpus directory.
#[derive(Clone, Debug)]
pub struct CorpusFile {
    pub name: String,
    pub messages: Vec<Recorded>,
}

impl CorpusFile {
    /// The method of each request, by the sender and ID that replies name.
    #[must_use]
    pub fn request_methods(&self) -> BTreeMap<(String, String), String> {
        self.messages
            .iter()
            .filter(|message| message.is_request())
            .map(|message| {
                (
                    (message.from.clone(), id_key(message.id())),
                    message.method().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    /// The method of the request `reply` answers, which `requester` sent.
    #[must_use]
    pub fn replied_method<'a>(
        methods: &'a BTreeMap<(String, String), String>,
        requester: &str,
        reply: &Recorded,
    ) -> Option<&'a str> {
        methods
            .get(&(requester.to_owned(), id_key(reply.id())))
            .map(String::as_str)
    }
}

fn id_key(id: Option<&Value>) -> String {
    id.map_or_else(String::new, Value::to_string)
}

/// The SHA-256 of a corpus directory's files: each file's name and
/// contents, in name order.
pub fn directory_digest(directory: &Path) -> std::io::Result<String> {
    let mut names = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    let mut digest = Sha256::new();
    for name in names {
        let contents = fs::read(directory.join(&name))?;
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(&contents);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// The versions of `surface` with a frozen corpus, oldest first.
#[must_use]
pub fn frozen_versions(surface: &str) -> Vec<&'static str> {
    let mut versions = FROZEN
        .iter()
        .filter(|(frozen, _, _)| *frozen == surface)
        .map(|(_, version, _)| *version)
        .collect::<Vec<_>>();
    versions.sort_by_key(|version| version.parse::<crate::SemVer>().ok());
    versions
}

/// The files of the frozen corpus `surface/version`.
///
/// # Panics
///
/// When the directory is not pinned in [`FROZEN`], cannot be read, or no
/// longer has the pinned digest.
#[must_use]
pub fn frozen(surface: &str, version: &str) -> Vec<CorpusFile> {
    let directory = Path::new(CORPUS_DIRECTORY).join(surface).join(version);
    let pinned = FROZEN
        .iter()
        .find(|(frozen, at, _)| *frozen == surface && *at == version)
        .map(|(_, _, digest)| *digest)
        .unwrap_or_else(|| {
            panic!(
                "corpus/{surface}/{version} is not pinned in cookie_agent_protocol::corpus::FROZEN"
            )
        });
    let digest = directory_digest(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    assert_eq!(
        digest, pinned,
        "corpus/{surface}/{version} changed. A frozen corpus is never rewritten, and no \
         COOKIE_UPDATE_* switch applies to it: restore it from git. A new minor version records \
         its own corpus/<surface>/<version>/ directory beside it."
    );
    let mut names = fs::read_dir(&directory)
        .expect("corpus directory")
        .map(|entry| {
            entry
                .expect("corpus entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let text = fs::read_to_string(directory.join(&name)).expect("corpus file");
            let messages = text
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    serde_json::from_str(line).unwrap_or_else(|error| {
                        panic!("corpus/{surface}/{version}/{name}:{}: {error}", index + 1)
                    })
                })
                .collect();
            CorpusFile { name, messages }
        })
        .collect()
}

/// The fixture `corpus/future/<name>`.
///
/// # Panics
///
/// When it cannot be read or is not JSON.
#[must_use]
pub fn future(name: &str) -> Value {
    let path = Path::new(CORPUS_DIRECTORY).join("future").join(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// One entry of a surface's `methods.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct MethodEntry {
    pub method: String,
    pub direction: String,
    pub kind: String,
    pub params: Option<String>,
    pub params_role: Option<String>,
    pub result: Option<String>,
}

impl MethodEntry {
    /// Whether its params are a command, which readers refuse unknown
    /// members in.
    #[must_use]
    pub fn params_are_command(&self) -> bool {
        self.params_role.as_deref() == Some("command")
    }
}

/// The methods of `surface`'s baseline, by name.
///
/// # Panics
///
/// When the baseline cannot be read.
#[must_use]
pub fn methods(surface: &str) -> BTreeMap<String, MethodEntry> {
    #[derive(Deserialize)]
    struct Methods {
        methods: Vec<MethodEntry>,
    }
    let path = Path::new(COMPAT_DIRECTORY)
        .join(surface)
        .join("methods.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str::<Methods>(&text)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .methods
        .into_iter()
        .map(|entry| (entry.method.clone(), entry))
        .collect()
}

/// Adds a member no build knows to `value` and to every object inside it,
/// except inside the members named in `opaque`, whose contents are data a
/// reader passes on (tool arguments, metadata, schemas) or maps whose keys
/// are typed.
pub fn add_future_members(value: &mut Value, opaque: &[&str]) {
    match value {
        Value::Object(members) => {
            for (key, member) in members.iter_mut() {
                if !opaque.contains(&key.as_str()) {
                    add_future_members(member, opaque);
                }
            }
            members.insert(
                "future_member".into(),
                serde_json::json!({"from": "a newer peer", "nested": [1, {"deeper": true}]}),
            );
        }
        Value::Array(items) => {
            for item in items {
                add_future_members(item, opaque);
            }
        }
        _ => {}
    }
}

/// The values a replay's replies handed out in place of the recorded ones:
/// a string the recording's reply held where the replay's held another
/// string stands for that one in every request replayed after it.
#[derive(Debug, Default)]
pub struct Substitutions {
    values: BTreeMap<String, String>,
}

impl Substitutions {
    /// Learns from a recorded reply and the replay's reply to the same
    /// request: their strings that differ at the same place.
    pub fn learn(&mut self, recorded: &Value, replayed: &Value) {
        match (recorded, replayed) {
            (Value::String(recorded), Value::String(replayed)) if recorded != replayed => {
                self.values
                    .entry(recorded.clone())
                    .or_insert_with(|| replayed.clone());
            }
            (Value::Object(recorded), Value::Object(replayed)) => {
                for (key, value) in recorded {
                    if let Some(other) = replayed.get(key) {
                        self.learn(value, other);
                    }
                }
            }
            (Value::Array(recorded), Value::Array(replayed)) => {
                for (value, other) in recorded.iter().zip(replayed) {
                    self.learn(value, other);
                }
            }
            _ => {}
        }
    }

    /// Records that `recorded` stands for `replayed`.
    pub fn insert(&mut self, recorded: impl Into<String>, replayed: impl Into<String>) {
        self.values.insert(recorded.into(), replayed.into());
    }

    /// `value` with every string learned replaced.
    #[must_use]
    pub fn apply(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.values.get(text).unwrap_or(text).to_owned()),
            Value::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(key, value)| (key.clone(), self.apply(value)))
                    .collect(),
            ),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.apply(item)).collect())
            }
            other => other.clone(),
        }
    }
}

/// Messages recorded on one connection, in the order they crossed it.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    messages: Arc<Mutex<Vec<Recorded>>>,
}

impl Recording {
    fn push(&self, from: &str, frame: &MessageFrame) {
        let message = match frame {
            MessageFrame::Value(value) => value.clone(),
            MessageFrame::Text(text) => serde_json::from_str(text).unwrap_or(Value::Null),
            MessageFrame::Bytes(bytes) => serde_json::from_slice(bytes).unwrap_or(Value::Null),
        };
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Recorded {
                from: from.to_owned(),
                message,
            });
    }

    /// What was recorded so far.
    #[must_use]
    pub fn messages(&self) -> Vec<Recorded> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// `transport` with what crosses it recorded: what it sends as from
    /// `local`, and what it receives as from `remote`.
    pub fn tap<T: Transport>(
        &self,
        transport: T,
        local: &'static str,
        remote: &'static str,
    ) -> RecordingTransport<T> {
        RecordingTransport {
            inner: transport,
            recording: self.clone(),
            local,
            remote,
        }
    }

    /// Writes `messages` as `<root>/<surface>/<version>/<file>`, one per
    /// line, with every occurrence of `scratch` written as
    /// [`SCRATCH_PLACEHOLDER`].
    ///
    /// # Errors
    ///
    /// When `<root>/<surface>/<version>` exists (a recorded corpus is
    /// never rewritten), or on a write error.
    pub fn write_new(
        root: &Path,
        surface: &str,
        version: &str,
        file: &str,
        messages: &[Recorded],
        scratch: &Path,
    ) -> std::io::Result<PathBuf> {
        let directory = root.join(surface).join(version);
        if directory.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists: a recorded corpus is never rewritten; a new version records its \
                     own directory",
                    directory.display()
                ),
            ));
        }
        let scratch = scratch.to_string_lossy();
        let mut text = String::new();
        for message in messages {
            let line = serde_json::to_string(message).map_err(std::io::Error::other)?;
            text.push_str(&line.replace(scratch.as_ref(), SCRATCH_PLACEHOLDER));
            text.push('\n');
        }
        fs::create_dir_all(&directory)?;
        let path = directory.join(file);
        fs::write(&path, text)?;
        Ok(path)
    }
}

/// A transport whose traffic a [`Recording`] keeps.
pub struct RecordingTransport<T> {
    inner: T,
    recording: Recording,
    local: &'static str,
    remote: &'static str,
}

#[async_trait]
impl<T: Transport> Transport for RecordingTransport<T> {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.recording.push(self.local, &frame);
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        let frame = self.inner.recv().await?;
        if let Some(frame) = &frame {
            self.recording.push(self.remote, frame);
        }
        Ok(frame)
    }
}
