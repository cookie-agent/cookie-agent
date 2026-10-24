//! What an ACP prompt carries of a user's files
//! (`docs/site/specs/acp-agents.md` section 15, input and pickers): images,
//! audio, and text files as the agent's prompt capabilities allow, else a
//! note that the agent cannot read the file; and what the clients are told
//! the agent takes.

use cookie_agent_protocol::{ArtifactRef, InputMedia, MediaKindLabel, media_sniff};
use cookie_agent_runtime_api::media::upload_media_types;
use serde_json::{Value, json};

use super::wire::{self, PromptCapabilities};

/// The context window the first prompt's skill listing is sized for: the
/// agent's own is unknown, and Claude Code's and Codex's models have at
/// least this.
pub(crate) const SKILL_LISTING_CONTEXT_TOKENS: u64 = 200_000;

/// The most files of one kind the clients attach to one message for an
/// ACP agent.
const MAX_FILES_PER_KIND: u32 = 20;

/// What an agent with `caps` takes in a prompt, as the clients check
/// attachments against: images, audio, and text files, each up to its
/// upload cap.
pub(crate) fn input_media(caps: &PromptCapabilities) -> Vec<InputMedia> {
    let entry = |kind: &str, mime_types: Vec<String>| {
        let image = kind == MediaKindLabel::IMAGE;
        InputMedia {
            kind: MediaKindLabel::new(kind).expect("static media kind"),
            mime_types,
            max_bytes: media_sniff::max_attachment_bytes(kind),
            max_dimension: image.then_some(media_sniff::MAX_IMAGE_DIMENSION),
            max_pixels: image.then_some(media_sniff::MAX_IMAGE_PIXELS),
            max_count: MAX_FILES_PER_KIND,
        }
    };
    let uploads = |kind: &str| {
        upload_media_types(kind)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut media = Vec::new();
    if caps.image {
        media.push(entry(MediaKindLabel::IMAGE, uploads(MediaKindLabel::IMAGE)));
    }
    if caps.audio {
        media.push(entry(MediaKindLabel::AUDIO, uploads(MediaKindLabel::AUDIO)));
    }
    if caps.embedded_context {
        media.push(entry(
            MediaKindLabel::TEXT,
            vec![media_sniff::TEXT_MEDIA_TYPE.to_owned()],
        ));
    }
    media
}

/// How a file reaches the agent.
#[derive(Debug, Eq, PartialEq)]
enum Delivery {
    Image,
    Audio,
    Text,
    Note,
}

fn delivery(artifact: &ArtifactRef, caps: &PromptCapabilities) -> Delivery {
    let media_type = artifact.media_type.as_str();
    if caps.image && media_type.starts_with("image/") {
        Delivery::Image
    } else if caps.audio && media_type.starts_with("audio/") {
        Delivery::Audio
    } else if caps.embedded_context && media_type == media_sniff::TEXT_MEDIA_TYPE {
        Delivery::Text
    } else {
        // A PDF or a video: ACP's blob resource would carry it, but the
        // agents that take embedded context read only text resources.
        Delivery::Note
    }
}

/// The URI a text file the user attached goes to the agent under: its
/// name, under a scheme that names no path on the agent's machine.
fn attachment_uri(artifact: &ArtifactRef) -> String {
    let name = artifact.filename.as_deref().unwrap_or("attachment.txt");
    let mut uri = String::from("attachment:///");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The prompt block of `artifact`, a file the user attached, for an agent
/// with `caps`: an `image`, an `audio`, or a text `resource` block when the
/// agent takes it and `read` has its bytes, else a note naming the file.
pub(crate) fn file_block(
    artifact: &ArtifactRef,
    caps: &PromptCapabilities,
    read: impl FnOnce() -> Option<bytes::Bytes>,
) -> Value {
    use base64::Engine as _;
    let how = delivery(artifact, caps);
    let bytes = (how != Delivery::Note).then(read).flatten();
    match (how, bytes) {
        (Delivery::Image, Some(bytes)) => json!({
            "type": "image",
            "mimeType": artifact.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        (Delivery::Audio, Some(bytes)) => json!({
            "type": "audio",
            "mimeType": artifact.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        (Delivery::Text, Some(bytes)) if std::str::from_utf8(&bytes).is_ok() => json!({
            "type": "resource",
            "resource": {
                "uri": attachment_uri(artifact),
                "mimeType": artifact.media_type,
                "text": String::from_utf8_lossy(&bytes),
            },
        }),
        _ => wire::text_block(&format!(
            "[attached file {} ({}), which this agent cannot read]",
            artifact.filename.as_deref().unwrap_or("unnamed"),
            artifact.media_type
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(media_type: &str, filename: Option<&str>) -> ArtifactRef {
        ArtifactRef {
            sha256: "0".repeat(64),
            byte_length: 5,
            media_type: media_type.to_owned(),
            filename: filename.map(str::to_owned),
        }
    }

    fn caps(image: bool, audio: bool, embedded_context: bool) -> PromptCapabilities {
        PromptCapabilities {
            image,
            audio,
            embedded_context,
        }
    }

    #[test]
    fn files_go_as_the_agent_takes_them_and_the_rest_as_a_note() {
        let all = caps(true, true, true);
        let bytes = || Some(bytes::Bytes::from_static(b"hello"));
        let text = file_block(&artifact("text/plain", Some("my notes.md")), &all, bytes);
        assert_eq!(
            text,
            json!({"type": "resource", "resource": {
                "uri": "attachment:///my%20notes.md", "mimeType": "text/plain", "text": "hello",
            }})
        );
        let audio = file_block(&artifact("audio/wav", None), &all, bytes);
        assert_eq!(audio["type"], "audio");
        assert_eq!(audio["mimeType"], "audio/wav");
        assert_eq!(audio["data"], "aGVsbG8=");
        // A PDF, and files the agent does not take, are a note; the bytes
        // are not even read.
        for (media_type, caps) in [
            ("application/pdf", all.clone()),
            ("text/plain", caps(true, true, false)),
            ("audio/wav", caps(true, false, true)),
        ] {
            let block = file_block(&artifact(media_type, Some("x")), &caps, || {
                panic!("read a file the agent cannot take")
            });
            assert_eq!(block["type"], "text");
            assert!(
                block["text"]
                    .as_str()
                    .unwrap()
                    .contains("which this agent cannot read"),
                "{block}"
            );
        }
        // A file the store no longer has is a note too.
        let gone = file_block(&artifact("image/png", Some("a.png")), &all, || None);
        assert_eq!(gone["type"], "text");
    }

    #[test]
    fn the_clients_are_told_what_the_agent_takes() {
        let kinds = |caps: &PromptCapabilities| {
            input_media(caps)
                .iter()
                .map(|media| media.kind.as_str().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&caps(true, true, true)), ["image", "audio", "text"]);
        assert_eq!(kinds(&caps(false, false, true)), ["text"]);
        assert!(kinds(&caps(false, false, false)).is_empty());
        let text = &input_media(&caps(false, false, true))[0];
        assert_eq!(text.mime_types, ["text/plain"]);
        let image = &input_media(&caps(true, false, false))[0];
        assert!(image.mime_types.iter().any(|mime| mime == "image/png"));
        assert!(image.max_dimension.is_some());
    }
}
