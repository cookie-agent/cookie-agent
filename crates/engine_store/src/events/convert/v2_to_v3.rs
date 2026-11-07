//! History format 2 to 3 (`docs/site/specs/history-format-3.md`).
//!
//! An image an ACP tool returned is stored once: a string in a tool result's
//! `metadata.acp.raw_output` that is entirely the canonical base64 of a file
//! the tree holds becomes a reference to the file
//! ([`blobs::reference_stored_images`]). The result's large values then move
//! out as a format-3 writer moves them, and a blob the record stops naming is
//! obsolete. Reading the record back and encoding the file again gives the
//! format-2 record.
//!
//! A record with nothing to replace is copied unchanged, and so is one whose
//! blobs do not read: the loader judges it as it would have.

use serde_json::Value;

use super::Step;
use crate::events::blobs;

pub(super) fn convert(mut lines: Vec<Value>, step: &mut Step<'_>) -> Result<Vec<Value>, String> {
    for line in &mut lines {
        if let Some(payload) = line.get_mut("payload") {
            rewrite(payload, step);
        }
    }
    Ok(lines)
}

fn rewrite(payload: &mut Value, step: &mut Step<'_>) {
    if payload.get("type").and_then(Value::as_str) != Some("tool_call_terminated")
        || !payload.get("result").is_some_and(may_hold_raw_output)
    {
        return;
    }
    let mut whole = payload.clone();
    if blobs::internalize(&mut whole, &mut |digest| step.read(digest)).is_err() {
        return;
    }
    let replaced = whole
        .get_mut("result")
        .and_then(blobs::acp_raw_output)
        .is_some_and(|output| {
            blobs::reference_stored_images(output, &mut |digest| step.holds(digest))
        });
    if !replaced {
        return;
    }
    blobs::externalize(&mut whole, &mut |digest, bytes| {
        if !step.holds(digest) {
            step.moved.insert(digest.to_owned(), bytes);
        }
    });
    let kept = moved_values(&whole);
    step.obsolete.extend(
        moved_values(payload)
            .into_iter()
            .filter(|digest| !kept.contains(digest)),
    );
    *payload = whole;
}

/// Whether a result can hold `metadata.acp.raw_output`: inline, or in a blob
/// its metadata or something in it moved to.
fn may_hold_raw_output(result: &Value) -> bool {
    result.pointer("/metadata/acp/raw_output").is_some()
        || result
            .get("moved_values")
            .and_then(Value::as_array)
            .is_some_and(|moved| {
                moved.iter().any(|entry| {
                    entry
                        .get("pointer")
                        .and_then(Value::as_str)
                        .is_some_and(|pointer| pointer.starts_with("/metadata"))
                })
            })
}

/// The blobs a record's result names in `moved_values`.
fn moved_values(payload: &Value) -> Vec<String> {
    payload
        .pointer("/result/moved_values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("value_blob")?.as_str())
        .filter_map(|reference| reference.strip_prefix("artifact://sha256/"))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use base64::Engine as _;
    use serde_json::json;

    use super::*;
    use crate::events::convert::Blobs;

    /// An image's bytes, and its base64 in `raw_output`, long enough to move.
    fn image() -> (Vec<u8>, String) {
        let bytes = (0..12_000_u32)
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        (bytes, encoded)
    }

    /// A format-2 line of a result with `raw_output`, with its blobs in `moved`.
    fn stored(raw_output: Value, moved: &mut Blobs) -> Value {
        let mut payload = json!({
            "type": "tool_call_terminated",
            "tool_call_id": "c",
            "result": {
                "title": "Read",
                "output": "an output",
                "display": [],
                "metadata": {"acp": {"kind": "read", "raw_output": raw_output}},
            },
        });
        blobs::externalize(&mut payload, &mut |digest, bytes| {
            moved.insert(digest.to_owned(), bytes);
        });
        json!({"seq": 2, "payload": payload})
    }

    fn converted(line: Value, moved: &mut Blobs) -> (Value, BTreeSet<String>) {
        let mut obsolete = BTreeSet::new();
        let mut lines =
            convert(vec![line], &mut Step::in_memory(moved, &mut obsolete)).expect("converts");
        (lines.remove(0), obsolete)
    }

    /// The record's result, read whole from `moved`.
    fn whole(line: &Value, moved: &Blobs) -> Value {
        let mut payload = line["payload"].clone();
        blobs::internalize(&mut payload, &mut |digest| {
            moved
                .get(digest)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .expect("whole");
        payload["result"].clone()
    }

    #[test]
    fn an_image_becomes_a_reference_and_the_record_can_be_rebuilt_exactly() {
        let (bytes, encoded) = image();
        let mut moved = Blobs::new();
        moved.insert(blobs::digest(&bytes), bytes.clone());
        let raw_output = |data: &str| {
            json!([{"type": "image", "source": {"data": data, "media_type": "image/png"}},
                   {"type": "text", "text": "kept"}])
        };
        let line = stored(raw_output(&encoded), &mut moved);
        let before = whole(&line, &moved);
        let old_blob = line["payload"]["result"]["moved_values"][0]["value_blob"]
            .as_str()
            .and_then(|reference| reference.strip_prefix("artifact://sha256/"))
            .expect("the raw output moved to a blob")
            .to_owned();

        let (line, obsolete) = converted(line, &mut moved);

        assert_eq!(obsolete, BTreeSet::from([old_blob]));
        let after = whole(&line, &moved);
        let reference = format!("artifact://sha256/{}", blobs::digest(&bytes));
        assert_eq!(
            after["metadata"]["acp"]["raw_output"],
            raw_output(&reference),
            "only the string changes"
        );
        // Encoding the file again gives the old record, whole.
        let mut rebuilt = after.clone();
        rebuilt["metadata"]["acp"]["raw_output"][0]["source"]["data"] = json!(encoded);
        assert_eq!(rebuilt, before);
        // What is left is short enough to stay in the record.
        assert!(line["payload"]["result"].get("moved_values").is_none());
    }

    #[test]
    fn what_is_not_a_stored_image_stays() {
        let (bytes, encoded) = image();
        let mut moved = Blobs::new();
        // A base64 string the tree holds no file of.
        let line = stored(json!([{"data": encoded}]), &mut moved);
        let (after, obsolete) = converted(line.clone(), &mut moved);
        assert_eq!(after, line);
        assert!(obsolete.is_empty());
        // The file is held, but the string is not all base64 of it.
        moved.insert(blobs::digest(&bytes), bytes);
        let line = stored(
            json!([{"data": format!("data:image/png;base64,{encoded}")}]),
            &mut moved,
        );
        assert_eq!(converted(line.clone(), &mut moved).0, line);
    }

    #[test]
    fn a_record_whose_blob_is_gone_is_left_for_the_loader_to_judge() {
        let (bytes, encoded) = image();
        let mut moved = Blobs::new();
        let line = stored(json!([{"data": encoded}]), &mut moved);
        let mut only_the_image = Blobs::new();
        only_the_image.insert(blobs::digest(&bytes), bytes);
        let (after, obsolete) = converted(line.clone(), &mut only_the_image);
        assert_eq!(after, line);
        assert!(obsolete.is_empty());
    }

    #[test]
    fn other_records_are_copied_unchanged() {
        let (bytes, encoded) = image();
        let mut moved = Blobs::new();
        moved.insert(blobs::digest(&bytes), bytes);
        for line in [
            json!({"seq": 1, "payload": {"type": "tool_call_terminated", "result":
                {"metadata": {"data": encoded}}}}),
            json!({"seq": 1, "payload": {"type": "user_input_submitted", "data": encoded}}),
            json!({"seq": 1, "payload": {"type": "from_a_newer_build"}, "unknown": true}),
        ] {
            assert_eq!(converted(line.clone(), &mut moved).0, line);
        }
    }
}
