//! Request-side media handling: per-request media counts on outgoing model
//! requests. Recognizing, validating, and gating tool attachments lives in
//! `cookie_agent_runtime_api::media`.

use std::collections::BTreeMap;

use cookie_agent_protocol::{MediaKind as CapabilityMediaKind, ModelCapabilities};

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
