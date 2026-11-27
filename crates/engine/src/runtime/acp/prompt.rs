//! What an ACP prompt carries of a user's files
//! (`docs/site/specs/acp-agents.md` section 15, input and pickers): images,
//! audio, and text files as the agent's prompt capabilities allow, each
//! after the label models get (`docs/site/specs/attachment-handling.md`
//! section 4.1), else the label alone; and what the clients are told the
//! agent takes.

use cookie_agent_protocol::{
    ArtifactRef, InputMedia, MediaKindLabel, media_sniff,
    runtime_protocol::{MediaDelivery, ToolInputMedia},
};
use cookie_agent_runtime_api::media::{
    MEDIA_SNIFF_BYTES, recognize_media_type, upload_media_types,
};

use crate::media::{FileContent, FileFacts, FileTools, missing_file_ending};
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

/// The media cookie's tools return to an agent with `caps` over the MCP
/// bridge, as tool results: the images and audio its prompts take
/// (`docs/site/specs/attachment-handling.md` section 4.3).
pub(crate) fn bridge_input_media(caps: &PromptCapabilities) -> Vec<ToolInputMedia> {
    input_media(caps)
        .into_iter()
        .filter(|media| {
            [MediaKindLabel::IMAGE, MediaKindLabel::AUDIO].contains(&media.kind.as_str())
        })
        .map(|media| ToolInputMedia {
            kind: media.kind,
            mime_types: media.mime_types,
            max_bytes: media.max_bytes,
            delivery: MediaDelivery::ToolResult,
        })
        .collect()
}

/// The prompt blocks of `artifact`, a file the user attached, for an agent
/// with `caps` whose bridge offers `tools`: its label
/// ([`cookie_agent_protocol::model_text::user_file_label`]), which names the
/// artifact for `read_artifact`, `transfer`, and `<attach artifact=…/>`,
/// then an `image`, an `audio`, or a text `resource` block when the agent
/// takes it and `read` has its bytes. A file the prompt does not carry is
/// its label alone, which says what the bridged tools can do with it (as
/// [`crate::media::missing_file_ending`] decides for a model); one whose
/// stored copy is gone is its label and a note saying so.
pub(crate) fn file_blocks(
    artifact: &ArtifactRef,
    caps: &PromptCapabilities,
    tools: FileTools,
    read: impl FnOnce() -> Option<bytes::Bytes>,
) -> Vec<Value> {
    use base64::Engine as _;
    use cookie_agent_protocol::model_text::{FileLabelEnding, user_file_label};
    let label = |ending| wire::text_block(&user_file_label(artifact, ending));
    let how = delivery(artifact, caps);
    let uploaded = artifact.media_type == cookie_agent_protocol::FILE_UPLOAD_MEDIA_TYPE;
    // A `file` upload's content says what `read_artifact` can do with it.
    let bytes = (how != Delivery::Note || uploaded).then(read).flatten();
    let block = match (&how, &bytes) {
        (Delivery::Image, Some(bytes)) => Some(json!({
            "type": "image",
            "mimeType": artifact.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        })),
        (Delivery::Audio, Some(bytes)) => Some(json!({
            "type": "audio",
            "mimeType": artifact.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        })),
        (Delivery::Text, Some(bytes)) => std::str::from_utf8(bytes).ok().map(|text| {
            json!({
                "type": "resource",
                "resource": {
                    "uri": attachment_uri(artifact),
                    "mimeType": artifact.media_type,
                    "text": text,
                },
            })
        }),
        _ => None,
    };
    if let Some(block) = block {
        return vec![label(FileLabelEnding::IncludedBelow), block];
    }
    let content = match bytes {
        None if how != Delivery::Note || uploaded => FileContent::Missing,
        None if artifact.media_type == media_sniff::TEXT_MEDIA_TYPE => FileContent::Text,
        None => FileContent::Media(artifact.media_type.clone()),
        Some(bytes) => {
            let head = &bytes[..bytes.len().min(MEDIA_SNIFF_BYTES)];
            if let Some(media_type) = recognize_media_type(head, bytes.len() as u64) {
                FileContent::Media(media_type.to_owned())
            } else if std::str::from_utf8(head).is_err_and(|error| error.error_len().is_some()) {
                FileContent::Binary
            } else {
                FileContent::Text
            }
        }
    };
    let facts = FileFacts {
        byte_length: artifact.byte_length,
        content,
    };
    let ending = missing_file_ending(&facts, &bridge_input_media(caps), tools);
    if facts.content == FileContent::Missing {
        return vec![
            label(ending),
            wire::text_block(&format!(
                "[{} ({}) is unavailable: its stored copy is missing or damaged]",
                artifact.filename.as_deref().unwrap_or("file"),
                artifact.media_type
            )),
        ];
    }
    vec![label(ending)]
}

/// Whether `title` is the start of the context cookie put ahead of the
/// first prompt: an agent that names its session after its first message
/// (Codex) names it after cookie's context there, which names nothing. The
/// title is `agent`'s, so it is that agent's context it may repeat.
pub(crate) fn title_repeats_context(
    state: &crate::events::LogState,
    agent: &cookie_agent_protocol::AgentId,
    title: &str,
) -> bool {
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = words(title);
    let title = title.trim_end_matches(['…', '.']).trim_end();
    if title.is_empty() {
        return false;
    }
    state
        .acp_prompt_context(agent)
        .first()
        .is_some_and(|context| words(context).starts_with(title))
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
        use cookie_agent_protocol::model_text::FileLabelEnding;
        let label = |artifact: &ArtifactRef, ending| {
            wire::text_block(&cookie_agent_protocol::model_text::user_file_label(
                artifact, ending,
            ))
        };
        let both = FileTools {
            read_artifact: true,
            transfer: true,
        };
        let notes = artifact("text/plain", Some("my notes.md"));
        assert_eq!(
            file_blocks(&notes, &all, both, bytes),
            [
                label(&notes, FileLabelEnding::IncludedBelow),
                json!({"type": "resource", "resource": {
                    "uri": "attachment:///my%20notes.md", "mimeType": "text/plain", "text": "hello",
                }})
            ]
        );
        let wav = artifact("audio/wav", None);
        let audio = file_blocks(&wav, &all, both, bytes);
        assert_eq!(audio[0], label(&wav, FileLabelEnding::IncludedBelow));
        assert_eq!(audio[1]["type"], "audio");
        assert_eq!(audio[1]["mimeType"], "audio/wav");
        assert_eq!(audio[1]["data"], "aGVsbG8=");
        // Files the agent does not take are their label alone, which says
        // what the bridged tools can do; the bytes are not even read.
        for (media_type, caps, tools, ending) in [
            // `read_artifact` returns no PDF over the bridge.
            (
                "application/pdf",
                all.clone(),
                both,
                FileLabelEnding::UseTransfer,
            ),
            (
                "application/pdf",
                all.clone(),
                FileTools::default(),
                FileLabelEnding::OnlyName,
            ),
            (
                "text/plain",
                caps(true, true, false),
                both,
                FileLabelEnding::ReadArtifactReturnsIt,
            ),
            (
                "audio/wav",
                caps(true, false, true),
                both,
                FileLabelEnding::UseTransfer,
            ),
        ] {
            let file = artifact(media_type, Some("x"));
            let blocks = file_blocks(&file, &caps, tools, || {
                panic!("read a file the agent cannot take")
            });
            assert_eq!(blocks, [label(&file, ending)], "{media_type}");
        }
        // A file the store no longer has is its label and a note.
        let png = artifact("image/png", Some("a.png"));
        let gone = file_blocks(&png, &all, both, || None);
        assert_eq!(gone.len(), 2);
        assert_eq!(gone[0], label(&png, FileLabelEnding::OnlyName));
        assert_eq!(gone[1]["type"], "text");
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
