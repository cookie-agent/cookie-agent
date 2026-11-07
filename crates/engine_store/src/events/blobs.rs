//! History format 2's blobs (`docs/site/specs/session-storage-v3.md`,
//! section 4.2): the large values of a session log's records live in the
//! tree's `blobs/` folder, each named by the SHA-256 of its content, and the
//! record names them by digest.
//!
//! What moves out of a record:
//!
//! - a tool result's `output` longer than [`OUTPUT_BLOB_MIN_BYTES`]: the
//!   record keeps its first [`OUTPUT_HEAD_BYTES`] (cut at a character
//!   boundary) as `output`, its byte length as `output_bytes`, and
//!   `output_blob`;
//! - a tool result's `display` and every part of its `metadata` whose JSON
//!   is longer than [`OUTPUT_BLOB_MIN_BYTES`], each whole: an object of the
//!   metadata only once the large members inside it moved and it is still
//!   that long, and the metadata itself the same way. The result lists
//!   them in `moved_values`, each `{"pointer", "value_blob"}`, `pointer` a
//!   JSON pointer into the result (`/display`, `/metadata/acp/raw_output`),
//!   in the order they moved;
//! - `acp_session_updated`'s `commands` and `options`, and `run_started`'s
//!   `agent.fallback_chain` and each internal agent's `fallbacks`, whose
//!   JSON reaches [`VALUE_BLOB_MIN_BYTES`]: the member becomes
//!   `<member>_blob`, and the blob holds its JSON. Repeated lists are stored
//!   once.
//!
//! A reference is the string `artifact://sha256/<digest>`, so artifact
//! liveness (forks, reverts) sees blobs as it sees every other artifact:
//! the folder is the one the tree's artifacts live in.
//!
//! Records in memory are whole: the loader reads every value back as it
//! reads a line, and the writer moves them out as it writes one. So
//! everything that reads a record (model replay, compaction, the approval
//! reviewer, clients, plugins) gets the whole value.
//!
//! An image an ACP tool returned is a file of the tree. Its copy in
//! `metadata.acp.raw_output` is a reference to that file since history format
//! 3 ([`reference_stored_images`]), which a reader that wants the base64 can
//! encode again exactly.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use cookie_agent_protocol::EventPayload;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

/// A tree's blobs, beside its root's log. It replaced `artifacts/`.
pub const BLOBS_DIR: &str = "blobs";
/// Where a tree kept its artifacts before history format 2: a tree that was
/// not converted yet (one another process holds) is read from there.
pub const LEGACY_ARTIFACTS_DIR: &str = "artifacts";
/// A tool result's output longer than this moves to a blob.
pub const OUTPUT_BLOB_MIN_BYTES: usize = 8 * 1024;
/// How much of a moved output the record keeps.
pub const OUTPUT_HEAD_BYTES: usize = 2 * 1024;
/// A list whose JSON is at least this long moves to a blob; a shorter one is
/// smaller than its reference would be worth.
pub const VALUE_BLOB_MIN_BYTES: usize = 512;

const URI_PREFIX: &str = "artifact://sha256/";
/// Only a member name can be followed by `":"` straight away: inside a
/// string, a quote is escaped.
const MARKER: &[u8] = b"_blob\":\"artifact://sha256/";

/// Blobs a conversion produced in memory, by digest, until they are written
/// or, for a read-only open, read back.
pub type Blobs = HashMap<String, Vec<u8>>;

/// The `blobs/` folder of the tree the log at `log` belongs to: beside the
/// root's log, also for a child's under `subagents/`.
#[must_use]
pub fn tree_blob_dir(log: &Path) -> PathBuf {
    let member = log.parent().unwrap_or_else(|| Path::new("."));
    let root = match member.parent() {
        Some(parent)
            if parent.file_name() == Some(std::ffi::OsStr::new(crate::session::SUBAGENTS_DIR)) =>
        {
            parent.parent().unwrap_or(parent)
        }
        _ => member,
    };
    root.join(BLOBS_DIR)
}

/// The content address of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Whether the record of `payload` may have a value that moves to a blob,
/// so it is written through [`externalize`].
#[must_use]
pub fn may_move(payload: &EventPayload) -> bool {
    match payload {
        // Its display and metadata are sized as JSON, as it is written.
        EventPayload::ToolCallTerminated { termination } => termination.result.is_some(),
        EventPayload::AcpSessionUpdated { update } => {
            update.commands.is_some() || update.options.is_some()
        }
        EventPayload::RunStarted { .. } => true,
        _ => false,
    }
}

/// Whether the stored line `line` names a blob, so it has to be read
/// through [`internalize`]. A false positive (a member of that name
/// somewhere else) only costs the slower read.
#[must_use]
pub fn has_references(line: &[u8]) -> bool {
    let mut rest = line;
    while let Some(at) = rest.iter().position(|byte| *byte == b'_') {
        if rest[at..].starts_with(MARKER) {
            return true;
        }
        rest = &rest[at + 1..];
    }
    false
}

/// Moves the large values of a record's `payload` (its JSON in the storage
/// encoding) to blobs, handing each blob's digest and content to `keep`.
pub fn externalize(payload: &mut Value, keep: &mut dyn FnMut(&str, Vec<u8>)) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("tool_call_terminated") => {
            if let Some(result) = object.get_mut("result").and_then(Value::as_object_mut) {
                move_output(result, keep);
                move_result_values(result, keep);
            }
        }
        Some("acp_session_updated") => {
            move_value(object, "commands", keep);
            move_value(object, "options", keep);
        }
        Some("run_started") => {
            if let Some(agent) = object.get_mut("agent").and_then(Value::as_object_mut) {
                move_value(agent, "fallback_chain", keep);
            }
            if let Some(agents) = object
                .get_mut("internal_agents")
                .and_then(Value::as_array_mut)
            {
                for agent in agents.iter_mut().filter_map(Value::as_object_mut) {
                    move_value(agent, "fallbacks", keep);
                }
            }
        }
        _ => {}
    }
}

fn move_output(result: &mut Map<String, Value>, keep: &mut dyn FnMut(&str, Vec<u8>)) {
    if result.contains_key("output_blob") {
        return;
    }
    let Some(Value::String(output)) = result.get_mut("output") else {
        return;
    };
    if output.len() <= OUTPUT_BLOB_MIN_BYTES {
        return;
    }
    let mut end = OUTPUT_HEAD_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let head = output[..end].to_owned();
    let whole = std::mem::replace(output, head).into_bytes();
    let length = whole.len();
    let digest = digest(&whole);
    result.insert("output_bytes".into(), Value::from(length));
    result.insert("output_blob".into(), Value::String(reference(&digest)));
    keep(&digest, whole);
}

/// Moves a tool result's display and every large part of its metadata, each
/// whole, naming them in `moved_values` by JSON pointer into the result, in
/// the order they moved: a member of the metadata whose JSON is longer than
/// [`OUTPUT_BLOB_MIN_BYTES`] moves, an object only once the large members
/// inside it moved and it is still that long, and so does the metadata as
/// a whole.
fn move_result_values(result: &mut Map<String, Value>, keep: &mut dyn FnMut(&str, Vec<u8>)) {
    if result.contains_key(MOVED_VALUES) {
        return;
    }
    let mut moved = Vec::new();
    if let Some(Value::Object(metadata)) = result.get_mut("metadata") {
        move_large_members(metadata, "/metadata", &mut moved, keep);
    }
    for member in ["metadata", "display"] {
        if result
            .get(member)
            .is_some_and(|value| json_len(value) > OUTPUT_BLOB_MIN_BYTES)
        {
            let value = result.remove(member).expect("present");
            moved.push(move_whole(&format!("/{member}"), &value, keep));
        }
    }
    if !moved.is_empty() {
        result.insert(MOVED_VALUES.into(), Value::Array(moved));
    }
}

fn move_large_members(
    object: &mut Map<String, Value>,
    pointer: &str,
    moved: &mut Vec<Value>,
    keep: &mut dyn FnMut(&str, Vec<u8>),
) {
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let path = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
        let Some(member) = object.get_mut(&key) else {
            continue;
        };
        if json_len(member) <= OUTPUT_BLOB_MIN_BYTES {
            continue;
        }
        if let Value::Object(inner) = member {
            move_large_members(inner, &path, moved, keep);
            if json_len(member) <= OUTPUT_BLOB_MIN_BYTES {
                continue;
            }
        }
        let value = object.remove(&key).expect("present");
        moved.push(move_whole(&path, &value, keep));
    }
}

/// Moves `value`, which was at `pointer`, to a blob of its JSON, returning
/// its `moved_values` entry.
fn move_whole(pointer: &str, value: &Value, keep: &mut dyn FnMut(&str, Vec<u8>)) -> Value {
    let bytes = serde_json::to_vec(value).expect("a JSON value serializes");
    let digest = digest(&bytes);
    keep(&digest, bytes);
    serde_json::json!({ "pointer": pointer, "value_blob": reference(&digest) })
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// The member of a tool result naming the values it moved whole.
const MOVED_VALUES: &str = "moved_values";

fn move_value(object: &mut Map<String, Value>, member: &str, keep: &mut dyn FnMut(&str, Vec<u8>)) {
    let key = format!("{member}_blob");
    let Some(value) = object.get(member).filter(|value| !value.is_null()) else {
        return;
    };
    if object.contains_key(&key) {
        return;
    }
    let bytes = serde_json::to_vec(value).expect("a JSON value serializes");
    if bytes.len() < VALUE_BLOB_MIN_BYTES {
        return;
    }
    let digest = digest(&bytes);
    object.remove(member);
    object.insert(key, Value::String(reference(&digest)));
    keep(&digest, bytes);
}

fn reference(digest: &str) -> String {
    format!("{URI_PREFIX}{digest}")
}

/// The digest a reference names.
fn referenced(reference: &Value) -> Result<&str, String> {
    reference
        .as_str()
        .and_then(|reference| reference.strip_prefix(URI_PREFIX))
        .filter(|digest| is_digest(digest))
        .ok_or_else(|| format!("{reference} is not a blob reference"))
}

/// A string this long or longer may be the base64 of a stored file.
pub const STORED_IMAGE_MIN_BYTES: usize = 1024;

/// Replaces each string at any depth of `value` that is entirely the canonical
/// base64 of a file `stored` knows by digest with a reference to that file:
/// whether it replaced any. Nothing is lost, as encoding the file again gives
/// the string back (`docs/site/specs/history-format-3.md`).
pub fn reference_stored_images(value: &mut Value, stored: &mut dyn FnMut(&str) -> bool) -> bool {
    use base64::Engine as _;
    match value {
        Value::Array(items) => {
            // Every item, not up to the first that replaces.
            let mut replaced = false;
            for item in items {
                replaced |= reference_stored_images(item, stored);
            }
            replaced
        }
        Value::Object(members) => {
            let mut replaced = false;
            for member in members.values_mut() {
                replaced |= reference_stored_images(member, stored);
            }
            replaced
        }
        Value::String(text) if text.len() >= STORED_IMAGE_MIN_BYTES => {
            let engine = base64::engine::general_purpose::STANDARD;
            let Ok(bytes) = engine.decode(text.as_bytes()) else {
                return false;
            };
            let name = digest(&bytes);
            if !stored(&name) || engine.encode(&bytes) != *text {
                return false;
            }
            *text = reference(&name);
            true
        }
        _ => false,
    }
}

/// `metadata.acp.raw_output` of a tool result.
#[must_use]
pub fn acp_raw_output(result: &mut Value) -> Option<&mut Value> {
    result
        .get_mut("metadata")?
        .get_mut("acp")?
        .get_mut("raw_output")
}

/// Whether `name` is a blob's name: 64 lowercase hex digits.
#[must_use]
pub fn is_digest(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Reads the values of a record's `payload` back from the blobs it names,
/// each through `fetch`, which returns a blob's verified content. Fails when
/// one does not read: the record cannot be read whole.
pub fn internalize(
    payload: &mut Value,
    fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<(), String> {
    let Some(object) = payload.as_object_mut() else {
        return Ok(());
    };
    match object.get("type").and_then(Value::as_str) {
        Some("tool_call_terminated") => {
            if let Some(result) = object.get_mut("result").and_then(Value::as_object_mut) {
                restore_result_values(result, fetch)?;
                restore_output(result, fetch)?;
            }
        }
        Some("acp_session_updated") => {
            restore_value(object, "commands", fetch)?;
            restore_value(object, "options", fetch)?;
        }
        Some("run_started") => {
            if let Some(agent) = object.get_mut("agent").and_then(Value::as_object_mut) {
                restore_value(agent, "fallback_chain", fetch)?;
            }
            if let Some(agents) = object
                .get_mut("internal_agents")
                .and_then(Value::as_array_mut)
            {
                for agent in agents.iter_mut().filter_map(Value::as_object_mut) {
                    restore_value(agent, "fallbacks", fetch)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn restore_output(
    result: &mut Map<String, Value>,
    fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<(), String> {
    let Some(reference) = result.remove("output_blob") else {
        return Ok(());
    };
    let digest = referenced(&reference)?;
    let length = result
        .remove("output_bytes")
        .and_then(|length| length.as_u64())
        .ok_or("the tool result's output_blob has no output_bytes")?;
    let bytes = fetch(digest)
        .map_err(|error| format!("blob {digest} of the tool result's output: {error}"))?;
    if bytes.len() as u64 != length {
        return Err(format!(
            "blob {digest} holds {} bytes, the tool result's output {length}",
            bytes.len()
        ));
    }
    let output = String::from_utf8(bytes)
        .map_err(|_| format!("blob {digest} of the tool result's output is not UTF-8"))?;
    result.insert("output".into(), Value::String(output));
    Ok(())
}

/// Puts back the values `moved_values` names, the last moved first, so an
/// object that moved after members inside it is in place before they are.
fn restore_result_values(
    result: &mut Map<String, Value>,
    fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<(), String> {
    let Some(moved) = result.remove(MOVED_VALUES) else {
        return Ok(());
    };
    let moved = moved
        .as_array()
        .ok_or("the tool result's moved_values is not a list")?;
    for entry in moved.iter().rev() {
        let pointer = entry
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or("a moved value has no pointer")?;
        let digest = referenced(entry.get("value_blob").unwrap_or(&Value::Null))?;
        let bytes =
            fetch(digest).map_err(|error| format!("blob {digest} of `{pointer}`: {error}"))?;
        let value = serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("blob {digest} of `{pointer}` is not JSON: {error}"))?;
        let (parent, key) = pointer
            .rsplit_once('/')
            .ok_or_else(|| format!("{pointer} is not a JSON pointer"))?;
        let key = key.replace("~1", "/").replace("~0", "~");
        let target = if parent.is_empty() {
            Some(&mut *result)
        } else {
            let mut current = None::<&mut Map<String, Value>>;
            let mut segments = parent.split('/').skip(1);
            let first = segments
                .next()
                .map(|first| first.replace("~1", "/").replace("~0", "~"));
            if let Some(first) = first {
                current = result.get_mut(&first).and_then(Value::as_object_mut);
                for segment in segments {
                    let segment = segment.replace("~1", "/").replace("~0", "~");
                    current = current.and_then(|object| object.get_mut(&segment)?.as_object_mut());
                }
            }
            current
        };
        target
            .ok_or_else(|| format!("{pointer} names no place in the tool result"))?
            .insert(key, value);
    }
    Ok(())
}

fn restore_value(
    object: &mut Map<String, Value>,
    member: &str,
    fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
) -> Result<(), String> {
    let Some(reference) = object.remove(&format!("{member}_blob")) else {
        return Ok(());
    };
    let digest = referenced(&reference)?;
    let bytes = fetch(digest).map_err(|error| format!("blob {digest} of `{member}`: {error}"))?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| format!("blob {digest} of `{member}` is not JSON: {error}"))?;
    object.insert(member.to_owned(), value);
    Ok(())
}

/// The content of blob `digest` in `dir`, verified against its digest.
pub fn read(dir: &Path, digest: &str) -> io::Result<Vec<u8>> {
    if !is_digest(digest) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a blob digest",
        ));
    }
    let bytes = fs::read(dir.join(digest))?;
    if self::digest(&bytes) != digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the blob's content does not match its digest",
        ));
    }
    Ok(bytes)
}

/// Stores `bytes` as blob `digest` in `dir`, unless a blob of that length is
/// there already, without syncing it. Returns the blob's path when this call
/// wrote it.
///
/// The content goes to a private temporary first, closed before it is
/// renamed to its address: two writers of one digest write the same bytes,
/// and Windows renames no open file.
pub fn write(dir: &Path, digest: &str, bytes: &[u8]) -> io::Result<Option<PathBuf>> {
    let path = dir.join(digest);
    if holds(&path, bytes.len()) {
        return Ok(None);
    }
    create_private_dir(dir)?;
    let temporary = dir.join(format!(".{digest}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(bytes)?;
        drop(file);
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(()),
            // A concurrent writer of the same content took the name first.
            Err(_) if holds(&path, bytes.len()) => fs::remove_file(&temporary),
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| Some(path))
}

/// Whether `path` is a blob of `length` bytes. Its name is its content's
/// address, so that is the content; a torn one (another length) is
/// replaced, and readers verify the digest anyway.
fn holds(path: &Path, length: usize) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == length as u64)
}

/// Syncs the blob at `path`, which a writer closed. The handle that syncs
/// has write access, which Windows needs to flush a file, and is closed
/// again at once, so it never keeps the blob or its folder from being
/// deleted. A blob deleted meanwhile has nothing left to sync.
pub fn sync(path: &Path) -> io::Result<()> {
    match OpenOptions::new().write(true).open(path) {
        Ok(file) => file.sync_data(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Writes every blob of `blobs` to `dir` and syncs the ones it wrote, and
/// the folder: a log that names them is synced after this.
pub fn write_synced(dir: &Path, blobs: &Blobs) -> io::Result<()> {
    let mut wrote = false;
    for (digest, bytes) in blobs {
        if let Some(path) = write(dir, digest, bytes)? {
            sync(&path)?;
            wrote = true;
        }
    }
    if wrote {
        super::fsync_directory(dir).map_err(|error| match error {
            super::EventLogError::Io { source, .. } => source,
            error => io::Error::other(error),
        })?;
    }
    Ok(())
}

/// Creates the private `blobs/` folder in a tree folder that exists: a tree
/// deleted meanwhile is not brought back.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates the private `blobs/` folder in a tree folder that exists: a tree
/// deleted meanwhile is not brought back.
#[cfg(windows)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if !dir.parent().is_some_and(Path::is_dir) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the session tree's folder does not exist",
        ));
    }
    cookie_agent_models::secure_store::create_windows_private_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    cookie_agent_models::secure_store::create_windows_private_file(path)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn kept(payload: &mut Value) -> Blobs {
        let mut blobs = Blobs::new();
        externalize(payload, &mut |digest, bytes| {
            blobs.insert(digest.to_owned(), bytes);
        });
        blobs
    }

    fn read_back(payload: &mut Value, blobs: &Blobs) -> Result<(), String> {
        internalize(payload, &mut |digest| {
            blobs
                .get(digest)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        })
    }

    #[test]
    fn an_output_over_the_threshold_keeps_its_head_length_and_digest() {
        // A multi-byte character straddles the head's end.
        let output = format!("{}é{}", "a".repeat(OUTPUT_HEAD_BYTES - 1), "b".repeat(9000));
        let whole = json!({"type": "tool_call_terminated", "result": {"output": output}});
        let mut payload = whole.clone();
        let blobs = kept(&mut payload);
        let result = &payload["result"];
        assert_eq!(result["output"], "a".repeat(OUTPUT_HEAD_BYTES - 1));
        assert_eq!(result["output_bytes"], output.len());
        let digest = digest(output.as_bytes());
        assert_eq!(result["output_blob"], format!("{URI_PREFIX}{digest}"));
        assert_eq!(blobs[&digest], output.as_bytes());
        assert!(has_references(&serde_json::to_vec(&payload).unwrap()));

        read_back(&mut payload, &blobs).expect("reads back");
        assert_eq!(payload, whole);
    }

    #[test]
    fn a_results_large_display_and_metadata_parts_move_whole() {
        let large = "m".repeat(OUTPUT_BLOB_MIN_BYTES);
        let whole = json!({"type": "tool_call_terminated", "result": {
            "output": "short",
            "display": [{"type": "text", "text": large}],
            "metadata": {
                "acp": {"raw_output": large, "raw_input": {"query": large}, "kind": "fetch"},
                "small": 1,
                "a/b~": large,
            },
        }});
        let mut payload = whole.clone();
        let blobs = kept(&mut payload);
        let result = &payload["result"];
        assert!(result.get("display").is_none());
        // An object is entered first: what is left of it is small.
        assert_eq!(
            result["metadata"]["acp"],
            json!({"kind": "fetch", "raw_input": {}})
        );
        assert_eq!(result["metadata"]["small"], 1);
        let pointers = result["moved_values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["pointer"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            pointers,
            [
                "/display",
                "/metadata/a~1b~0",
                "/metadata/acp/raw_input/query",
                "/metadata/acp/raw_output",
            ]
            .map(str::to_owned)
            .into()
        );
        assert_eq!(result["output"], "short");
        assert!(has_references(&serde_json::to_vec(&payload).unwrap()));
        assert_eq!(blobs.len(), 2, "the same content is one blob");
        read_back(&mut payload, &blobs).expect("reads back");
        assert_eq!(payload, whole);
    }

    #[test]
    fn metadata_of_many_small_members_moves_as_a_whole() {
        let members = (0..1_000)
            .map(|index| (format!("key-{index}"), json!(index)))
            .collect::<serde_json::Map<_, _>>();
        let whole = json!({"type": "tool_call_terminated", "result": {
            "output": "short", "display": [], "metadata": members}});
        let mut payload = whole.clone();
        let blobs = kept(&mut payload);
        assert!(payload["result"].get("metadata").is_none());
        assert_eq!(payload["result"]["moved_values"][0]["pointer"], "/metadata");
        read_back(&mut payload, &blobs).expect("reads back");
        assert_eq!(payload, whole);
    }

    #[test]
    fn an_output_at_the_threshold_stays_in_the_record() {
        let mut payload = json!({"type": "tool_call_terminated",
            "result": {"output": "x".repeat(OUTPUT_BLOB_MIN_BYTES)}});
        let before = payload.clone();
        assert!(kept(&mut payload).is_empty());
        assert_eq!(payload, before);
        assert!(!has_references(&serde_json::to_vec(&payload).unwrap()));
    }

    #[test]
    fn lists_move_from_their_threshold_and_repeat_as_one_blob() {
        let long = json!([{"name": "x".repeat(VALUE_BLOB_MIN_BYTES)}]);
        let short = json!([{"name": "y"}]);
        let mut first = json!({"type": "acp_session_updated", "commands": long, "options": short});
        let mut second = json!({"type": "acp_session_updated", "commands": long});
        let before = first.clone();
        let mut blobs = kept(&mut first);
        blobs.extend(kept(&mut second));
        assert_eq!(blobs.len(), 1);
        assert!(first.get("commands").is_none());
        assert_eq!(first["commands_blob"], second["commands_blob"]);
        assert_eq!(first["options"], short, "a short list stays");
        read_back(&mut first, &blobs).expect("reads back");
        assert_eq!(first, before);
    }

    #[test]
    fn run_started_moves_its_model_settings_chains() {
        let chain = json!([{"selection": "x".repeat(VALUE_BLOB_MIN_BYTES)}]);
        let whole = json!({"type": "run_started", "agent": {"fallback_chain": chain},
            "internal_agents": [{"fallbacks": chain}, {"fallbacks": [{"type": "inherit"}]}]});
        let mut payload = whole.clone();
        let blobs = kept(&mut payload);
        assert_eq!(blobs.len(), 1);
        assert!(payload["agent"].get("fallback_chain").is_none());
        assert!(payload["internal_agents"][0].get("fallbacks").is_none());
        assert_eq!(payload["internal_agents"][1], whole["internal_agents"][1]);
        read_back(&mut payload, &blobs).expect("reads back");
        assert_eq!(payload, whole);
    }

    #[test]
    fn a_missing_or_wrong_blob_does_not_read() {
        let mut payload = json!({"type": "tool_call_terminated",
            "result": {"output": "z".repeat(OUTPUT_BLOB_MIN_BYTES + 1)}});
        let mut blobs = kept(&mut payload);
        assert!(read_back(&mut payload.clone(), &Blobs::new()).is_err());
        for bytes in blobs.values_mut() {
            bytes.pop();
        }
        assert!(read_back(&mut payload, &blobs).is_err());
    }

    #[test]
    fn a_member_named_like_a_reference_inside_a_string_is_no_reference() {
        let text = json!({"output": "\"x_blob\":\"artifact://sha256/"}).to_string();
        assert!(!has_references(text.as_bytes()));
    }

    #[test]
    fn blobs_are_written_once_and_read_verified() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join(BLOBS_DIR);
        let content = b"blob content".to_vec();
        let digest = digest(&content);
        let path = write(&dir, &digest, &content).unwrap().expect("written");
        sync(&path).unwrap();
        assert_eq!(write(&dir, &digest, &content).unwrap(), None);
        assert_eq!(read(&dir, &digest).unwrap(), content);
        fs::write(&path, b"other content").unwrap();
        assert_eq!(
            read(&dir, &digest).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_child_log_shares_its_root_tree_blobs() {
        let root = Path::new("/s/2026-10/root");
        assert_eq!(
            tree_blob_dir(&root.join("events.jsonl")),
            root.join(BLOBS_DIR)
        );
        assert_eq!(
            tree_blob_dir(&root.join("subagents/child/events.jsonl")),
            root.join(BLOBS_DIR)
        );
    }

    #[test]
    fn a_stored_images_base64_becomes_a_reference_at_any_depth() {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        let bytes = vec![7_u8; STORED_IMAGE_MIN_BYTES];
        let encoded = engine.encode(&bytes);
        let name = digest(&bytes);
        let mut stored = |candidate: &str| candidate == name;
        let reference = format!("artifact://sha256/{name}");

        let mut nested = json!({"content": [{"source": {"data": encoded}}], "kept": "text"});
        assert!(reference_stored_images(&mut nested, &mut stored));
        assert_eq!(
            nested,
            json!({"content": [{"source": {"data": reference}}], "kept": "text"})
        );

        // Every image of a value, not only the first.
        let mut two = json!([{"data": encoded}, {"data": encoded}]);
        assert!(reference_stored_images(&mut two, &mut stored));
        assert_eq!(two, json!([{"data": reference}, {"data": reference}]));

        // Each condition of the rule leaves the value alone when it fails.
        let short = engine.encode(&bytes[..16]);
        let other = engine.encode(vec![8_u8; STORED_IMAGE_MIN_BYTES]);
        let surrounded = format!("{encoded}\n");
        // Valid base64 but for its last character's unused bits.
        let mut not_canonical = engine.encode(vec![7_u8; STORED_IMAGE_MIN_BYTES + 1]);
        not_canonical.pop();
        not_canonical.pop();
        not_canonical.pop();
        not_canonical.push_str("x==");
        for text in [short, other, surrounded, not_canonical] {
            let before = json!({"data": text});
            let mut value = before.clone();
            let mut known = |candidate: &str| candidate == name;
            assert!(!reference_stored_images(&mut value, &mut known));
            assert_eq!(value, before);
        }
    }
}
