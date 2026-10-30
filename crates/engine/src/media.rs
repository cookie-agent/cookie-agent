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
/// A user's file has its label just before it (`[file screenshot.png, 471
/// KB, artifact 5f2c81d0]`); the reason joins the label instead, so the
/// model can still pass the file on with `transfer`:
///
/// ```text
/// [file screenshot.png, 471 KB, artifact 5f2c81d0; not shown: provider/model does not accept images]
/// ```
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
    let user = user
        .iter()
        .map(|media| {
            (
                media.kind.as_str(),
                media.mime_types.as_slice(),
                media.max_bytes,
            )
        })
        .collect::<Vec<_>>();
    let tool_result = tool
        .iter()
        .filter(|media| media.delivery == MediaDelivery::ToolResult)
        .map(|media| {
            (
                media.kind.as_str(),
                media.mime_types.as_slice(),
                media.max_bytes,
            )
        })
        .collect::<Vec<_>>();
    // The reason of each user input placeholder, which joins the file's
    // label when one comes just before it.
    let mut reasons = std::collections::HashMap::new();
    visit_media_parts(history, |_, kind, slot| {
        let (file, accepted, in_tool_result) = match &slot {
            MediaSlot::Input(oven_sdk::InputPart::File(file))
            | MediaSlot::Assistant(oven_sdk::AssistantPart::File(file)) => (file, &user, false),
            MediaSlot::Tool(oven_sdk::ContentValue::File(file)) => (file, &tool_result, true),
            _ => return,
        };
        let Some((placeholder, reason)) = misfit(file, kind, model, accepted, in_tool_result)
        else {
            return;
        };
        match slot {
            MediaSlot::Input(part) => {
                reasons.insert(placeholder.clone(), reason);
                *part = oven_sdk::InputPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Assistant(part) => {
                *part = oven_sdk::AssistantPart::Text(oven_sdk::TextPart::new(placeholder));
            }
            MediaSlot::Tool(value) => *value = oven_sdk::ContentValue::Text(placeholder),
        }
    });
    if reasons.is_empty() {
        return;
    }
    for turn in history {
        let oven_sdk::HistoryTurn::User(message) = turn else {
            continue;
        };
        let mut parts = Vec::with_capacity(message.content.len());
        for part in std::mem::take(&mut message.content) {
            if let oven_sdk::InputPart::Text(text) = &part
                && let Some(reason) = reasons.get(&text.text)
                && let Some(oven_sdk::InputPart::Text(label)) = parts.last_mut()
                && is_file_label(&label.text)
            {
                let joined = format!(
                    "{}; not shown: {reason}]",
                    &label.text[..label.text.len() - 1]
                );
                *label = oven_sdk::TextPart::new(joined);
                continue;
            }
            parts.push(part);
        }
        message.content = parts;
    }
}

/// Whether `text` is a user file's label, as the history writes it.
fn is_file_label(text: &str) -> bool {
    text.starts_with("[file") && text.ends_with(']') && text.contains(", artifact ")
}

/// The placeholder of a file `model` does not take, and the reason it
/// names; `None` when it fits.
fn misfit(
    file: &oven_sdk::FilePart,
    kind: CapabilityMediaKind,
    model: &ModelKey,
    accepted: &[(&str, &[String], u64)],
    in_tool_result: bool,
) -> Option<(String, String)> {
    let label = media_label(kind);
    let size = match &file.source {
        oven_sdk::FileSource::Bytes(bytes) => Some(bytes.len() as u64),
        oven_sdk::FileSource::Text(text) => Some(text.len() as u64),
        oven_sdk::FileSource::Url(_) | oven_sdk::FileSource::ProviderReference { .. } => None,
    };
    let reason = match accepted.iter().find(|(of, _, _)| *of == kind_label(kind)) {
        None if in_tool_result => {
            format!("{model} does not accept {} in tool results", plural(kind))
        }
        None => format!("{model} does not accept {}", plural(kind)),
        Some((_, mime_types, _))
            if !mime_types
                .iter()
                .any(|accepted| mime_type_matches(accepted, &file.media_type)) =>
        {
            format!("{model} does not accept {}", file.media_type)
        }
        Some((_, _, max_bytes)) if size.is_some_and(|size| size > *max_bytes) => {
            format!(
                "larger than the {} {model} accepts",
                human_bytes(*max_bytes)
            )
        }
        Some(_) => return None,
    };
    let about = match (file.filename.as_deref(), size) {
        (Some(name), Some(size)) => format!(" ({name}, {})", human_bytes(size)),
        (Some(name), None) => format!(" ({name})"),
        (None, Some(size)) => format!(" ({})", human_bytes(size)),
        (None, None) => String::new(),
    };
    Some((format!("[{label} omitted: {reason}{about}]"), reason))
}

const fn kind_label(kind: CapabilityMediaKind) -> &'static str {
    match kind {
        CapabilityMediaKind::Image => cookie_agent_protocol::MediaKindLabel::IMAGE,
        CapabilityMediaKind::Audio => cookie_agent_protocol::MediaKindLabel::AUDIO,
        CapabilityMediaKind::Pdf => cookie_agent_protocol::MediaKindLabel::PDF,
        CapabilityMediaKind::Video => cookie_agent_protocol::MediaKindLabel::VIDEO,
    }
}

fn plural(kind: CapabilityMediaKind) -> &'static str {
    match kind {
        CapabilityMediaKind::Image => "images",
        CapabilityMediaKind::Audio => "audio",
        CapabilityMediaKind::Pdf => "PDFs",
        CapabilityMediaKind::Video => "videos",
    }
}

/// Enforces each media kind's per-request `max_count` on an outgoing request.
///
/// Media accumulates over a long session (screenshots, read PDFs), so the whole history can
/// exceed a count that each turn respects. Rather than failing every later request, the oldest
/// excess parts are replaced with a short text placeholder and the newest are kept. Only the
/// current (latest) user message is protected: when it alone exceeds the limit the request fails,
/// because eliding it would silently drop what was just attached. This is request assembly only;
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
) -> Result<(), String> {
    let mut counts = BTreeMap::<CapabilityMediaKind, (usize, usize)>::new();
    visit_media_parts(history, |protected, kind, _| {
        let (total, current) = counts.entry(kind).or_default();
        *total += 1;
        *current += usize::from(protected);
    });
    let mut elide = BTreeMap::new();
    for (kind, capability) in &capabilities.media {
        let limit = capability.max_count as usize;
        let (total, current) = counts.get(kind).copied().unwrap_or_default();
        if total <= limit {
            continue;
        }
        if current > limit {
            return Err(format!(
                "the current message contains {current} {} file parts; the model accepts at most {limit} per request",
                media_label(*kind)
            ));
        }
        let step = (limit / 2).max(1);
        let elided = (total - limit)
            .div_ceil(step)
            .saturating_mul(step)
            .min(total - current);
        elide.insert(*kind, elided);
    }
    if elide.is_empty() {
        return Ok(());
    }
    visit_media_parts(history, |protected, kind, slot| {
        let Some(remaining) = elide.get_mut(&kind) else {
            return;
        };
        if protected || *remaining == 0 {
            return;
        }
        *remaining -= 1;
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
    Ok(())
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
        elide_excess_media(&mut history, &image_capabilities(1)).unwrap();
        let (kept, placeholders) = summarize(&history);
        assert_eq!(kept, [b"p".to_vec()]);
        assert_eq!(placeholders, 0);
    }

    #[test]
    fn media_within_the_limit_is_left_untouched() {
        let mut history = vec![tool_image(b"a"), user_images(&[b"b"])];
        let original = history.clone();
        elide_excess_media(&mut history, &image_capabilities(2)).unwrap();
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
        elide_excess_media(&mut history, &image_capabilities(4)).unwrap();
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
        elide_excess_media(&mut history, &image_capabilities(4)).unwrap();
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
            elide_excess_media(&mut history, &image_capabilities(limit)).unwrap();
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
    fn current_user_message_media_is_protected_and_fails_only_alone_over_the_limit() {
        // Older tool media is elided before anything the latest user message attached.
        let mut history = vec![
            tool_image(b"t1"),
            tool_image(b"t2"),
            user_images(&[b"u1", b"u2"]),
        ];
        elide_excess_media(&mut history, &image_capabilities(2)).unwrap();
        let (kept, placeholders) = summarize(&history);
        assert_eq!(kept, [b"u1".to_vec(), b"u2".to_vec()]);
        assert_eq!(placeholders, 2);

        let mut history = vec![
            user_images(&[b"old"]),
            user_images(&[b"u1", b"u2", b"u3"]),
            tool_image(b"t1"),
        ];
        let original = history.clone();
        let error = elide_excess_media(&mut history, &image_capabilities(2))
            .expect_err("the current message alone exceeds the limit");
        assert_eq!(
            error,
            "the current message contains 3 image file parts; the model accepts at most 2 per request"
        );
        assert_eq!(history, original);
    }
}
