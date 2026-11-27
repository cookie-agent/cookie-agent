//! Request-side media handling: fitting history's media to the attempt's
//! model, and per-request media counts on outgoing model requests.
//! Recognizing, validating, and gating tool attachments lives in
//! `cookie_agent_runtime_api::media`.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    InputMedia, MediaKind as CapabilityMediaKind, ModelCapabilities, ModelKey, human_bytes,
    runtime_protocol::{MediaDelivery, ToolInputMedia},
};
use cookie_agent_runtime_api::media::mime_type_matches;

/// Replaces every file part of `history` that `model` cannot take with a
/// placeholder, so a model switch or a fallback to a model without that kind
/// sends a request the model accepts instead of failing it
/// (`docs/site/specs/compatibility-v1.md` section 11.5). `user` is what the
/// model accepts in a user turn, which holds for user input, tool-emitted
/// user messages, and assistant files; `tool` is what it accepts from a
/// tool, of which only what its adapter carries in a tool result counts for
/// a tool result. A part fits when the model accepts its kind and media type
/// within the byte limit.
///
/// The placeholder names the reason, the model, and the file, as one of:
///
/// ```text
/// [image omitted: provider/model does not accept images (screenshot.png, 471 KB)]
/// [image omitted: provider/model does not accept image/webp (screenshot.png, 471 KB)]
/// [image omitted: larger than the 3.8 MB provider/model accepts (screenshot.png, 9.1 MB)]
/// [image omitted: provider/model does not accept images in tool results (screenshot.png, 471 KB)]
/// ```
///
/// A user's file has its label just before it, which
/// [`settle_file_labels`] then ends by whether the file is still there.
///
/// It is a pure function of the history and the model, so a replay of the
/// same history to the same model sends the same request and keeps the
/// cached prompt prefix. It runs before [`elide_excess_media`], which then
/// counts only what fits.
pub(crate) fn fit_media(
    history: &mut [oven_sdk::HistoryTurn],
    model: &ModelKey,
    user: &[InputMedia],
    tool: &[ToolInputMedia],
) {
    let user = user.iter().map(Accepts::of_user).collect::<Vec<_>>();
    let tool_result = tool
        .iter()
        .filter(|media| media.delivery == MediaDelivery::ToolResult)
        .map(Accepts::of_tool)
        .collect::<Vec<_>>();
    visit_media_parts(history, |_, kind, slot| {
        let (file, accepted, in_tool_result) = match &slot {
            MediaSlot::Input(oven_sdk::InputPart::File(file))
            | MediaSlot::Assistant(oven_sdk::AssistantPart::File(file)) => (file, &user, false),
            MediaSlot::Tool(oven_sdk::ContentValue::File(file)) => (file, &tool_result, true),
            _ => return,
        };
        let Some(placeholder) = misfit(file, kind_label(kind), model, accepted, in_tool_result)
        else {
            return;
        };
        match slot {
            MediaSlot::Input(part) => {
                *part = oven_sdk::InputPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Assistant(part) => {
                *part = oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Tool(value) => *value = oven_sdk::ContentValue::Text(placeholder),
        }
    });
    // A user's text file (and any other file of no media kind) goes only to
    // a model that lists it, as an ACP agent does; native models list none,
    // so their label says `read_artifact` returns it.
    for turn in history {
        let oven_sdk::HistoryTurn::User(message) = turn else {
            continue;
        };
        for part in &mut message.content {
            let oven_sdk::InputPart::File(file) = part else {
                continue;
            };
            if capability_media_kind(&file.media_type).is_some() {
                continue;
            }
            let kind = other_kind(&file.media_type);
            if let Some(placeholder) = misfit(file, kind, model, &user, false) {
                *part = oven_sdk::InputPart::Text(oven_sdk::TextPart::new(placeholder));
            }
        }
    }
}

/// What a model takes of one kind, as fitting checks a file against it.
struct Accepts<'a> {
    kind: &'a str,
    mime_types: &'a [String],
    max_bytes: u64,
}

impl<'a> Accepts<'a> {
    fn of_user(media: &'a InputMedia) -> Self {
        Self {
            kind: media.kind.as_str(),
            mime_types: &media.mime_types,
            max_bytes: media.max_bytes,
        }
    }

    fn of_tool(media: &'a ToolInputMedia) -> Self {
        Self {
            kind: media.kind.as_str(),
            mime_types: &media.mime_types,
            max_bytes: media.max_bytes,
        }
    }
}

/// The kind label of a file of no media kind: `text` for text, else
/// `file`, which no model lists.
fn other_kind(media_type: &str) -> &'static str {
    if media_type.starts_with("text/") {
        cookie_agent_protocol::MediaKindLabel::TEXT
    } else {
        "file"
    }
}

/// The kind label a request fits a user file of `media_type` under: its
/// media kind, `text`, or `file`.
pub(crate) fn user_file_kind(media_type: &str) -> &'static str {
    capability_media_kind(media_type).map_or_else(|| other_kind(media_type), kind_label)
}

/// Why `model`, which takes `user` in a user turn, does not take one user
/// file of kind `kind` ([`user_file_kind`]), `media_type`, and `size`: its
/// kind, its media type, or its size, as [`fit_media`] decides; `None` when
/// it takes it.
pub(crate) fn user_file_fit(
    kind: &str,
    media_type: &str,
    size: u64,
    model: &dyn std::fmt::Display,
    user: &[InputMedia],
) -> Option<String> {
    let accepted = user.iter().map(Accepts::of_user).collect::<Vec<_>>();
    misfit_reason(kind, media_type, Some(size), model, &accepted, false)
}

/// What a stored copy of `total` bytes starting with `head` (its first
/// bytes, or all of it) holds, recognized as `artifact.read` recognizes a
/// blob.
pub(crate) fn sniffed_content(head: &[u8], total: u64) -> FileContent {
    use cookie_agent_runtime_api::media::{MEDIA_SNIFF_BYTES, recognize_media_type};
    let head = &head[..head.len().min(MEDIA_SNIFF_BYTES)];
    if let Some(media_type) = recognize_media_type(head, total) {
        FileContent::Media(media_type.to_owned())
    } else if std::str::from_utf8(head).is_err_and(|error| error.error_len().is_some()) {
        FileContent::Binary
    } else {
        FileContent::Text
    }
}

/// Why `model`, which takes `user` in a user turn, gets the user files of
/// one message only by their labels, in order: `None` for a file the request
/// carries, else the reason, as [`fit_media`] and then [`elide_excess_media`]
/// decide for the latest user message (which keeps the first of a kind up
/// to its count). Each file is its kind label ([`user_file_kind`]), media
/// type, and size.
pub(crate) fn user_files_fit(
    files: &[(&str, &str, u64)],
    model: &dyn std::fmt::Display,
    user: &[InputMedia],
) -> Vec<Option<String>> {
    let mut counts = BTreeMap::<&str, u32>::new();
    files
        .iter()
        .map(|(kind, media_type, size)| {
            if let Some(reason) = user_file_fit(kind, media_type, *size, model, user) {
                return Some(reason);
            }
            let max_count = user
                .iter()
                .find(|media| media.kind.as_str() == *kind)
                .map_or(0, |media| media.max_count);
            let count = counts.entry(kind).or_default();
            *count += 1;
            (*count > max_count).then(|| {
                format!(
                    "{model} takes at most {max_count} {} in one request",
                    plural(kind)
                )
            })
        })
        .collect()
}

/// What a request's history says of each user file it labels, by the
/// label's head ([`cookie_agent_protocol::model_text::user_file_label_head`]):
/// what [`settle_file_labels`] decides the label's ending from.
pub(crate) type FileLabels = std::collections::HashMap<String, FileFacts>;

/// A user file as its label's ending needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileFacts {
    pub(crate) byte_length: u64,
    pub(crate) content: FileContent,
}

/// What a user file's stored copy holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileContent {
    /// The copy is missing or damaged.
    Missing,
    /// UTF-8 text, which `read_artifact` pages.
    Text,
    /// A media file of this type.
    Media(String),
    /// Anything else.
    Binary,
}

/// The tools of a request that can do something with a file its user
/// message does not carry.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FileTools {
    pub(crate) read_artifact: bool,
    pub(crate) transfer: bool,
}

impl FileTools {
    /// The tools `request` offers, at once or deferred.
    pub(crate) fn of(request: &oven_sdk::Request) -> Self {
        let offers = |name: &str| request.tools.iter().any(|tool| tool.name == name);
        Self {
            read_artifact: offers(crate::base_tools::READ_ARTIFACT),
            transfer: offers(crate::machines_tool::TRANSFER),
        }
    }
}

/// How the label of a file of `facts` ends when the request does not carry
/// the file: `read_artifact` when it returns the file to a model that takes
/// `tool` media from a tool, else `transfer` when the request offers it and
/// there is a copy to send.
pub(crate) fn missing_file_ending(
    facts: &FileFacts,
    tool: &[ToolInputMedia],
    tools: FileTools,
) -> cookie_agent_protocol::model_text::FileLabelEnding {
    use cookie_agent_protocol::model_text::FileLabelEnding;
    if facts.content == FileContent::Missing {
        return FileLabelEnding::OnlyName;
    }
    if returns_file(facts, tool) && tools.read_artifact {
        FileLabelEnding::ReadArtifactReturnsIt
    } else if tools.transfer {
        FileLabelEnding::UseTransfer
    } else {
        FileLabelEnding::OnlyName
    }
}

/// Whether `read_artifact` returns a file of `facts` to a model that takes
/// `tool` media from a tool: text, or media `gate_attachment` lets through.
pub(crate) fn returns_file(facts: &FileFacts, tool: &[ToolInputMedia]) -> bool {
    use cookie_agent_runtime_api::media::{AttachmentGate, gate_attachment};
    match &facts.content {
        FileContent::Missing | FileContent::Binary => false,
        FileContent::Text => true,
        FileContent::Media(mime) => matches!(
            gate_attachment(tool, mime, facts.byte_length),
            AttachmentGate::AttachToolResult | AttachmentGate::DeliverViaUserTurn
        ),
    }
}

/// Ends each user file label of `history` by what the request carries
/// (`docs/site/specs/attachment-handling.md` section 4.1): `included below`
/// when the file follows its label, which holds only once [`fit_media`] and
/// [`elide_excess_media`] have put placeholders in place of the files the
/// model does not get, else what `tools` can do with it
/// ([`missing_file_ending`]). `labels` are the history's labels; a label
/// settles again the same way, so this may run after each fitting.
pub(crate) fn settle_file_labels(
    history: &mut [oven_sdk::HistoryTurn],
    labels: &FileLabels,
    tool: &[ToolInputMedia],
    tools: FileTools,
) {
    use cookie_agent_protocol::model_text::FileLabelEnding;
    if labels.is_empty() {
        return;
    }
    for turn in history {
        let oven_sdk::HistoryTurn::User(message) = turn else {
            continue;
        };
        for index in 0..message.content.len() {
            let included = matches!(
                message.content.get(index + 1),
                Some(oven_sdk::InputPart::File(_))
            );
            let oven_sdk::InputPart::Text(text) = &mut message.content[index] else {
                continue;
            };
            let Some(facts) = text
                .text
                .strip_suffix(']')
                .and_then(|label| label.rsplit_once(" · "))
                .and_then(|(head, _)| labels.get(head).map(|facts| (head, facts)))
            else {
                continue;
            };
            let (head, facts) = facts;
            let ending = if included {
                FileLabelEnding::IncludedBelow
            } else {
                missing_file_ending(facts, tool, tools)
            };
            let settled = format!("{head} · {}]", ending.text());
            if settled != text.text {
                *text = oven_sdk::TextPart::new(settled);
            }
        }
    }
}

/// The placeholder of a file of kind `kind` that `model` does not take,
/// naming why; `None` when it fits.
fn misfit(
    file: &oven_sdk::FilePart,
    kind: &str,
    model: &ModelKey,
    accepted: &[Accepts<'_>],
    in_tool_result: bool,
) -> Option<String> {
    let size = match &file.source {
        oven_sdk::FileSource::Bytes(bytes) => Some(bytes.len() as u64),
        oven_sdk::FileSource::Text(text) => Some(text.len() as u64),
        oven_sdk::FileSource::Url(_) | oven_sdk::FileSource::ProviderReference { .. } => None,
    };
    let reason = misfit_reason(
        kind,
        &file.media_type,
        size,
        model,
        accepted,
        in_tool_result,
    )?;
    let about = match (file.filename.as_deref(), size) {
        (Some(name), Some(size)) => format!(" ({name}, {})", human_bytes(size)),
        (Some(name), None) => format!(" ({name})"),
        (None, Some(size)) => format!(" ({})", human_bytes(size)),
        (None, None) => String::new(),
    };
    let label = match kind {
        cookie_agent_protocol::MediaKindLabel::PDF => "PDF",
        kind => kind,
    };
    Some(format!("[{label} omitted: {reason}{about}]"))
}

/// Why a file of kind `kind`, `media_type`, and `size` does not fit what
/// `model` takes (`accepted`): its kind, its media type, or its size;
/// `None` when it fits.
fn misfit_reason(
    kind: &str,
    media_type: &str,
    size: Option<u64>,
    model: &dyn std::fmt::Display,
    accepted: &[Accepts<'_>],
    in_tool_result: bool,
) -> Option<String> {
    Some(match accepted.iter().find(|of| of.kind == kind) {
        None if in_tool_result => {
            format!("{model} does not accept {} in tool results", plural(kind))
        }
        None => format!("{model} does not accept {}", plural(kind)),
        Some(of)
            if !of
                .mime_types
                .iter()
                .any(|accepted| mime_type_matches(accepted, media_type)) =>
        {
            format!("{model} does not accept {media_type}")
        }
        Some(of) if size.is_some_and(|size| size > of.max_bytes) => {
            format!(
                "larger than the {} {model} accepts",
                human_bytes(of.max_bytes)
            )
        }
        Some(_) => return None,
    })
}

const fn kind_label(kind: CapabilityMediaKind) -> &'static str {
    match kind {
        CapabilityMediaKind::Image => cookie_agent_protocol::MediaKindLabel::IMAGE,
        CapabilityMediaKind::Audio => cookie_agent_protocol::MediaKindLabel::AUDIO,
        CapabilityMediaKind::Pdf => cookie_agent_protocol::MediaKindLabel::PDF,
        CapabilityMediaKind::Video => cookie_agent_protocol::MediaKindLabel::VIDEO,
    }
}

/// A kind label in the plural, as a reason names it.
pub(crate) fn plural(kind: &str) -> &'static str {
    match kind {
        cookie_agent_protocol::MediaKindLabel::IMAGE => "images",
        cookie_agent_protocol::MediaKindLabel::AUDIO => "audio",
        cookie_agent_protocol::MediaKindLabel::PDF => "PDFs",
        cookie_agent_protocol::MediaKindLabel::VIDEO => "videos",
        cookie_agent_protocol::MediaKindLabel::TEXT => "text files",
        _ => "files",
    }
}

/// Enforces each media kind's per-request `max_count` on an outgoing request.
///
/// Media accumulates over a long session (screenshots, read PDFs), so the whole history can
/// exceed a count that each turn respects. Rather than failing every later request, the oldest
/// excess parts are replaced with a short text placeholder and the newest are kept. The current
/// (latest) user message is elided last: when it alone exceeds the limit, every older part of
/// the kind goes and the message keeps its first parts up to the limit, the rest becoming
/// placeholders whose labels say they were not included (admission no longer refuses them,
/// `docs/site/specs/attachment-handling.md` question Q2b). This is request assembly only;
/// durable history is untouched.
///
/// Elision is a pure function of the history, so replays of the same history produce the same
/// request. To keep the cached prompt prefix stable, the elided count grows in steps of half the
/// limit: once over the limit, the oldest parts are elided down to between half and all of the
/// limit, and later media only appends until the limit is reached again. A step of one would keep
/// the most media but move the elision boundary, and so invalidate the cache from the oldest
/// media part onward, on every request once at the limit.
pub(crate) fn elide_excess_media(
    history: &mut [oven_sdk::HistoryTurn],
    capabilities: &ModelCapabilities,
) {
    let mut counts = BTreeMap::<CapabilityMediaKind, (usize, usize)>::new();
    visit_media_parts(history, |protected, kind, _| {
        let (total, current) = counts.entry(kind).or_default();
        *total += 1;
        *current += usize::from(protected);
    });
    // Per kind: how many older parts go, and how many of the current
    // message's stay.
    let mut elide = BTreeMap::new();
    for (kind, capability) in &capabilities.media {
        let limit = capability.max_count as usize;
        let (total, current) = counts.get(kind).copied().unwrap_or_default();
        if total <= limit {
            continue;
        }
        if current > limit {
            elide.insert(*kind, (total - current, limit));
            continue;
        }
        let step = (limit / 2).max(1);
        let elided = (total - limit)
            .div_ceil(step)
            .saturating_mul(step)
            .min(total - current);
        elide.insert(*kind, (elided, current));
    }
    if elide.is_empty() {
        return;
    }
    visit_media_parts(history, |protected, kind, slot| {
        let Some((older, kept)) = elide.get_mut(&kind) else {
            return;
        };
        if protected {
            if *kept > 0 {
                *kept -= 1;
                return;
            }
        } else if *older == 0 {
            return;
        } else {
            *older -= 1;
        }
        let label = media_label(kind);
        let placeholder = format!("[{label} omitted: over the model's per-request {label} limit]");
        match slot {
            MediaSlot::Input(part) => {
                *part = oven_sdk::InputPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Assistant(part) => {
                *part = oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Tool(value) => *value = oven_sdk::ContentValue::Text(placeholder),
        }
    });
}

fn media_label(kind: CapabilityMediaKind) -> &'static str {
    match kind {
        CapabilityMediaKind::Image => "image",
        CapabilityMediaKind::Audio => "audio",
        CapabilityMediaKind::Pdf => "PDF",
        CapabilityMediaKind::Video => "video",
    }
}

enum MediaSlot<'a> {
    Input(&'a mut oven_sdk::InputPart),
    Assistant(&'a mut oven_sdk::AssistantPart),
    Tool(&'a mut oven_sdk::ContentValue),
}

/// Visits media file parts oldest first; `protected` marks parts of the latest user message.
fn visit_media_parts(
    history: &mut [oven_sdk::HistoryTurn],
    mut visit: impl FnMut(bool, CapabilityMediaKind, MediaSlot<'_>),
) {
    let current = history
        .iter()
        .rposition(|turn| matches!(turn, oven_sdk::HistoryTurn::User(_)));
    for (index, turn) in history.iter_mut().enumerate() {
        match turn {
            oven_sdk::HistoryTurn::System(_) => {}
            oven_sdk::HistoryTurn::User(message) => {
                let protected = current == Some(index);
                for part in &mut message.content {
                    let kind = match part {
                        oven_sdk::InputPart::File(file) => capability_media_kind(&file.media_type),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        visit(protected, kind, MediaSlot::Input(part));
                    }
                }
            }
            oven_sdk::HistoryTurn::Assistant(turn) => {
                for part in &mut turn.message.content {
                    let kind = match part {
                        oven_sdk::AssistantPart::ToolResult(result) => {
                            visit_tool_media(&mut result.content, &mut visit);
                            None
                        }
                        oven_sdk::AssistantPart::File(file) => {
                            capability_media_kind(&file.media_type)
                        }
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        visit(false, kind, MediaSlot::Assistant(part));
                    }
                }
            }
            oven_sdk::HistoryTurn::Tool(message) => {
                for result in &mut message.results {
                    visit_tool_media(&mut result.content, &mut visit);
                }
            }
        }
    }
}

fn visit_tool_media(
    content: &mut oven_sdk::ToolContent,
    visit: &mut impl FnMut(bool, CapabilityMediaKind, MediaSlot<'_>),
) {
    if let oven_sdk::ToolContent::Mixed(values) = content {
        for value in values {
            let kind = match value {
                oven_sdk::ContentValue::File(file) => capability_media_kind(&file.media_type),
                _ => None,
            };
            if let Some(kind) = kind {
                visit(false, kind, MediaSlot::Tool(value));
            }
        }
    }
}

fn capability_media_kind(mime_type: &str) -> Option<CapabilityMediaKind> {
    if mime_type.starts_with("image/") {
        Some(CapabilityMediaKind::Image)
    } else if mime_type == "application/pdf" {
        Some(CapabilityMediaKind::Pdf)
    } else if mime_type.starts_with("audio/") {
        Some(CapabilityMediaKind::Audio)
    } else if mime_type.starts_with("video/") {
        Some(CapabilityMediaKind::Video)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_protocol::{
        CancellationCapability, MediaCapability, MediaKind, MimeType, Modality, ModelCapabilities,
        ReplayCapability,
    };

    use super::elide_excess_media;

    fn capabilities(kind: Option<(MediaKind, &str, u64)>) -> ModelCapabilities {
        let mut media = BTreeMap::new();
        let mut input = std::collections::BTreeSet::from([Modality::Text]);
        if let Some((kind, mime_type, max_bytes)) = kind {
            let modality = match kind {
                MediaKind::Image => Modality::Image,
                MediaKind::Audio => Modality::Audio,
                MediaKind::Pdf => Modality::Pdf,
                MediaKind::Video => Modality::Video,
            };
            input.insert(modality);
            media.insert(
                kind,
                MediaCapability {
                    mime_types: [MimeType::new(mime_type).unwrap()].into_iter().collect(),
                    max_bytes,
                    max_count: 1,
                },
            );
        }
        ModelCapabilities {
            input,
            output: [Modality::Text].into_iter().collect(),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Optional,
            cancellation: CancellationCapability::LocalOnly,
            media,
        }
    }

    fn image(tag: &'static [u8]) -> oven_sdk::FilePart {
        oven_sdk::FilePart::image(
            "image/png",
            oven_sdk::FileSource::Bytes(bytes::Bytes::from_static(tag)),
        )
    }

    fn image_capabilities(max_count: u32) -> ModelCapabilities {
        let mut capabilities = capabilities(Some((MediaKind::Image, "image/png", u64::MAX)));
        capabilities
            .media
            .get_mut(&MediaKind::Image)
            .unwrap()
            .max_count = max_count;
        capabilities
    }

    fn user_images(tags: &[&'static [u8]]) -> oven_sdk::HistoryTurn {
        let mut content = vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new("look"))];
        content.extend(tags.iter().map(|tag| oven_sdk::InputPart::File(image(tag))));
        oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(content))
    }

    fn tool_image(tag: &'static [u8]) -> oven_sdk::HistoryTurn {
        oven_sdk::HistoryTurn::tool(oven_sdk::ToolMessage::new(vec![
            oven_sdk::ToolResultPart::new(
                "call",
                oven_sdk::ToolContent::Mixed(vec![oven_sdk::ContentValue::File(image(tag))]),
            ),
        ]))
    }

    /// Kept image tags oldest first, plus the number of elision placeholders.
    fn summarize(history: &[oven_sdk::HistoryTurn]) -> (Vec<Vec<u8>>, usize) {
        let mut kept = Vec::new();
        let mut placeholders = 0;
        let mut file = |file: &oven_sdk::FilePart| {
            let oven_sdk::FileSource::Bytes(bytes) = &file.source else {
                panic!("inline image");
            };
            kept.push(bytes.to_vec());
        };
        let placeholder =
            |text: &str| text == "[image omitted: over the model's per-request image limit]";
        for turn in history {
            match turn {
                oven_sdk::HistoryTurn::User(message) => {
                    for part in &message.content {
                        match part {
                            oven_sdk::InputPart::File(value) => file(value),
                            oven_sdk::InputPart::Text(text) if placeholder(&text.text) => {
                                placeholders += 1;
                            }
                            _ => {}
                        }
                    }
                }
                oven_sdk::HistoryTurn::Tool(message) => {
                    for result in &message.results {
                        let oven_sdk::ToolContent::Mixed(values) = &result.content else {
                            continue;
                        };
                        for value in values {
                            match value {
                                oven_sdk::ContentValue::File(value) => file(value),
                                oven_sdk::ContentValue::Text(text) if placeholder(text) => {
                                    placeholders += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (kept, placeholders)
    }

    fn named_image(media_type: &str, tag: &'static [u8], name: &str) -> oven_sdk::FilePart {
        let mut file = oven_sdk::FilePart::image(
            media_type,
            oven_sdk::FileSource::Bytes(bytes::Bytes::from_static(tag)),
        );
        file.filename = Some(name.into());
        file
    }

    fn user_turn(files: Vec<oven_sdk::FilePart>) -> oven_sdk::HistoryTurn {
        let mut content = vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new("look"))];
        content.extend(files.into_iter().map(oven_sdk::InputPart::File));
        oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(content))
    }

    fn texts(history: &[oven_sdk::HistoryTurn]) -> Vec<String> {
        let mut texts = Vec::new();
        for turn in history {
            match turn {
                oven_sdk::HistoryTurn::User(message) => {
                    for part in &message.content {
                        if let oven_sdk::InputPart::Text(text) = part {
                            texts.push(text.text.clone());
                        }
                    }
                }
                oven_sdk::HistoryTurn::Tool(message) => {
                    for result in &message.results {
                        if let oven_sdk::ToolContent::Mixed(values) = &result.content {
                            for value in values {
                                if let oven_sdk::ContentValue::Text(text) = value {
                                    texts.push(text.clone());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        texts
    }

    fn input_media(
        kind: &str,
        mime_types: &[&str],
        max_bytes: u64,
    ) -> cookie_agent_protocol::InputMedia {
        cookie_agent_protocol::InputMedia {
            kind: cookie_agent_protocol::MediaKindLabel::new(kind).unwrap(),
            mime_types: mime_types.iter().map(|value| (*value).to_owned()).collect(),
            max_bytes,
            max_dimension: None,
            max_pixels: None,
            max_count: 20,
        }
    }

    fn tool_media(
        kind: &str,
        delivery: cookie_agent_protocol::runtime_protocol::MediaDelivery,
    ) -> cookie_agent_protocol::runtime_protocol::ToolInputMedia {
        cookie_agent_protocol::runtime_protocol::ToolInputMedia {
            kind: cookie_agent_protocol::MediaKindLabel::new(kind).unwrap(),
            mime_types: vec!["image/png".into()],
            max_bytes: 1024,
            delivery,
        }
    }

    #[test]
    fn media_a_model_does_not_take_becomes_a_placeholder_naming_why() {
        use cookie_agent_protocol::runtime_protocol::MediaDelivery;
        let model: cookie_agent_protocol::ModelKey = "acme/text-only".parse().unwrap();
        let history =
            vec![
                user_turn(vec![
                    named_image("image/png", b"png bytes", "shot.png"),
                    named_image("image/webp", b"webp", "shot.webp"),
                ]),
                oven_sdk::HistoryTurn::tool(oven_sdk::ToolMessage::new(vec![
                    oven_sdk::ToolResultPart::new(
                        "call",
                        oven_sdk::ToolContent::Mixed(vec![oven_sdk::ContentValue::File(
                            named_image("image/png", b"tool png", "read.png"),
                        )]),
                    ),
                ])),
            ];

        // No images at all.
        let mut fitted = history.clone();
        super::fit_media(&mut fitted, &model, &[], &[]);
        assert_eq!(
            texts(&fitted),
            [
                "look",
                "[image omitted: acme/text-only does not accept images (shot.png, 9 B)]",
                "[image omitted: acme/text-only does not accept images (shot.webp, 4 B)]",
                "[image omitted: acme/text-only does not accept images in tool results (read.png, 8 B)]",
            ]
        );
        // The same history fits to the same request.
        let mut again = history.clone();
        super::fit_media(&mut again, &model, &[], &[]);
        assert_eq!(again, fitted);

        // PNG only, up to 8 bytes, and images only in user turns.
        let mut fitted = history.clone();
        super::fit_media(
            &mut fitted,
            &model,
            &[input_media("image", &["image/png"], 8)],
            &[tool_media("image", MediaDelivery::UserMessage)],
        );
        assert_eq!(
            texts(&fitted),
            [
                "look",
                "[image omitted: larger than the 8 B acme/text-only accepts (shot.png, 9 B)]",
                "[image omitted: acme/text-only does not accept image/webp (shot.webp, 4 B)]",
                "[image omitted: acme/text-only does not accept images in tool results (read.png, 8 B)]",
            ]
        );

        // What fits is left as it is.
        let mut fitted = history.clone();
        super::fit_media(
            &mut fitted,
            &model,
            &[input_media("image", &["image/*"], 1024)],
            &[tool_media("image", MediaDelivery::ToolResult)],
        );
        assert_eq!(fitted, history);
    }

    #[test]
    fn fitting_runs_before_the_count_so_only_media_that_fits_counts() {
        let model: cookie_agent_protocol::ModelKey = "acme/png-only".parse().unwrap();
        let mut history = vec![
            user_turn(vec![named_image("image/webp", b"w", "old.webp")]),
            user_turn(vec![named_image("image/png", b"p", "new.png")]),
        ];
        super::fit_media(
            &mut history,
            &model,
            &[input_media("image", &["image/png"], 1024)],
            &[],
        );
        // One image is left for a limit of one: nothing more is elided.
        elide_excess_media(&mut history, &image_capabilities(1));
        let (kept, placeholders) = summarize(&history);
        assert_eq!(kept, [b"p".to_vec()]);
        assert_eq!(placeholders, 0);
    }

    #[test]
    fn media_within_the_limit_is_left_untouched() {
        let mut history = vec![tool_image(b"a"), user_images(&[b"b"])];
        let original = history.clone();
        elide_excess_media(&mut history, &image_capabilities(2));
        assert_eq!(history, original);
    }

    #[test]
    fn excess_history_media_is_elided_oldest_first_across_tool_results() {
        let mut history = vec![
            user_images(&[b"u1", b"u2"]),
            tool_image(b"t1"),
            tool_image(b"t2"),
            user_images(&[b"u3"]),
            tool_image(b"t3"),
        ];
        let original = history.clone();
        // Six images against a limit of four elide down in steps of two: the two oldest go.
        elide_excess_media(&mut history, &image_capabilities(4));
        let (kept, placeholders) = summarize(&history);
        assert_eq!(
            kept,
            [
                b"t1".to_vec(),
                b"t2".to_vec(),
                b"u3".to_vec(),
                b"t3".to_vec()
            ]
        );
        assert_eq!(placeholders, 2);
        assert_eq!(history.len(), original.len());

        // Eliding an already elided request is a no-op.
        let elided = history.clone();
        elide_excess_media(&mut history, &image_capabilities(4));
        assert_eq!(history, elided);
    }

    #[test]
    fn elision_boundary_moves_in_steps_to_keep_the_cached_prefix_stable() {
        let limit = 4;
        let mut previous_placeholders = 0;
        let mut boundary_moves = 0;
        for total in 5..=12_usize {
            let tags: Vec<&'static [u8]> = [
                b"0" as &'static [u8],
                b"1",
                b"2",
                b"3",
                b"4",
                b"5",
                b"6",
                b"7",
                b"8",
                b"9",
                b"10",
                b"11",
            ][..total]
                .to_vec();
            let mut history = tags.iter().map(|tag| tool_image(tag)).collect::<Vec<_>>();
            history.insert(0, user_images(&[]));
            elide_excess_media(&mut history, &image_capabilities(limit));
            let (kept, placeholders) = summarize(&history);
            assert!(kept.len() <= limit as usize && kept.len() > limit as usize / 2);
            assert_eq!(kept.last().unwrap(), tags.last().unwrap());
            if placeholders != previous_placeholders {
                boundary_moves += 1;
            }
            previous_placeholders = placeholders;
        }
        // Eight successive requests, each adding one image, move the boundary only four times.
        assert_eq!(boundary_moves, 4);
    }

    #[test]
    fn current_user_message_media_is_elided_last_keeping_its_first_parts() {
        // Older tool media is elided before anything the latest user message attached.
        let mut history = vec![
            tool_image(b"t1"),
            tool_image(b"t2"),
            user_images(&[b"u1", b"u2"]),
        ];
        elide_excess_media(&mut history, &image_capabilities(2));
        let (kept, placeholders) = summarize(&history);
        assert_eq!(kept, [b"u1".to_vec(), b"u2".to_vec()]);
        assert_eq!(placeholders, 2);

        let mut history = vec![
            user_images(&[b"old"]),
            user_images(&[b"u1", b"u2", b"u3"]),
            tool_image(b"t1"),
        ];
        // The current message alone over the limit keeps its first parts up
        // to it; everything older goes.
        elide_excess_media(&mut history, &image_capabilities(2));
        let (kept, placeholders) = summarize(&history);
        assert_eq!(kept, [b"u1".to_vec(), b"u2".to_vec()]);
        assert_eq!(placeholders, 3);
    }
}
