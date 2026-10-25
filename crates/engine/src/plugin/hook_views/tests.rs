use bytes::Bytes;
use cookie_agent_protocol::{
    ArtifactRef, ArtifactReference, ExtensionMessageRole, HookAssistantPart, HookContentPart,
    HookHistoryMessage, MimeType, ModelCallId, Open, PersistedAssistantPart, PersistedFilePart,
    PersistedFileSource, SafeCode, Sha256Digest, UserContent, UserContentPart,
};
use oven_sdk::{
    AssistantMessage, AssistantPart, CompletedTurn, ContentValue, CustomPart, FilePart, FileSource,
    Finish, FinishReason, HistoryTurn, InputPart, ReasoningPart, SystemMessage, SystemPart,
    TextPart, ToolCallPart, ToolContent, ToolMessage, ToolResultPart, Usage, UserMessage,
};
use serde_json::json;

use super::*;

const IMAGE: &[u8] = b"\x89PNG fixture bytes";
const LOG: &[u8] = b"build log";

fn file(media_type: &str, filename: &str, bytes: &'static [u8]) -> FilePart {
    let mut file = FilePart::new(media_type, FileSource::Bytes(Bytes::from_static(bytes)));
    file.filename = Some(filename.into());
    file
}

fn artifact(media_type: &str, filename: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        sha256: Sha256Digest::of_bytes(bytes).as_str().to_owned(),
        byte_length: bytes.len() as u64,
        media_type: media_type.into(),
        filename: Some(filename.into()),
    }
}

fn metadata() -> oven_sdk::PartMetadata {
    serde_json::from_value(json!({"signature": "opaque"})).expect("part metadata")
}

/// A history with every role, parts the view shows, and parts it leaves out.
fn history() -> Vec<HistoryTurn> {
    let mut reasoning = ReasoningPart::new("thinking it over");
    reasoning.metadata = metadata();
    let mut call = ToolCallPart::new("call-1", "read", json!({"path": "build.log"}));
    call.provider_item_id = Some("item-1".into());
    let mut result = ToolResultPart::new(
        "call-1",
        ToolContent::Mixed(vec![
            ContentValue::Text("the log:".into()),
            ContentValue::File(file("text/plain", "build.log", LOG)),
            ContentValue::Json(json!({"lines": 1})),
        ]),
    );
    result.is_error = true;
    vec![
        HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new("You are a test.")),
            SystemPart::Custom(CustomPart::new("test.marker", json!(1))),
        ])),
        HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("What is in this image?")),
            InputPart::File(file("image/png", "shot.png", IMAGE)),
        ])),
        HistoryTurn::assistant(CompletedTurn::new(
            AssistantMessage::new(vec![
                AssistantPart::Reasoning(reasoning),
                AssistantPart::Text(TextPart::new("Let me read the log.")),
                AssistantPart::ToolCall(call),
            ]),
            Finish::new(Usage::default(), FinishReason::ToolCalls),
        )),
        HistoryTurn::tool(ToolMessage::new(vec![result])),
    ]
}

fn text(text: &str) -> Open<HookContentPart> {
    Open::Known(HookContentPart::Text { text: text.into() })
}

#[test]
fn the_history_view_shows_parts_and_files_as_metadata() {
    let view = history_view(&history());
    let expected = [
        (ExtensionMessageRole::System, vec![text("You are a test.")]),
        (
            ExtensionMessageRole::User,
            vec![
                text("What is in this image?"),
                Open::Known(HookContentPart::File {
                    artifact: artifact("image/png", "shot.png", IMAGE),
                }),
            ],
        ),
        (
            ExtensionMessageRole::Assistant,
            vec![
                text("Let me read the log."),
                Open::Known(HookContentPart::ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "build.log"}),
                }),
            ],
        ),
        (
            ExtensionMessageRole::Tool,
            vec![
                Open::Known(HookContentPart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "the log:\n{\"lines\":1}".into(),
                    is_error: true,
                }),
                Open::Known(HookContentPart::File {
                    artifact: artifact("text/plain", "build.log", LOG),
                }),
            ],
        ),
    ]
    .map(|(role, content)| HookHistoryMessage { role, content });
    assert_eq!(view, expected);
    // The wire form names each part's type and carries no bytes.
    let wire = serde_json::to_value(&view).expect("view JSON");
    assert_eq!(wire[1]["content"][1]["type"], "file");
    assert_eq!(
        wire[1]["content"][1]["artifact"]["byte_length"],
        IMAGE.len()
    );
    assert!(!wire.to_string().contains("PNG fixture"));
}

#[test]
fn an_unchanged_history_converts_back_to_itself_with_what_the_view_leaves_out() {
    let original = history();
    let view = history_view(&original);
    // Through the wire, as a plugin would send it back.
    let returned: Vec<HookHistoryMessage> =
        serde_json::from_value(serde_json::to_value(&view).expect("view JSON")).expect("decode");
    assert_eq!(
        history_from_view(&original, returned).expect("replacement"),
        original
    );
    // Dropping and reordering messages keeps each message's own turn.
    let reordered = vec![view[3].clone(), view[0].clone()];
    assert_eq!(
        history_from_view(&original, reordered).expect("replacement"),
        vec![original[3].clone(), original[0].clone()]
    );
}

#[test]
fn a_changed_message_is_rebuilt_from_its_parts_and_keeps_its_files() {
    let original = history();
    let mut view = history_view(&original);
    view[1].content[0] = text("Describe this image.");
    view[2].content.truncate(1);
    view[3].content[0] = Open::Known(HookContentPart::ToolResult {
        tool_call_id: "call-1".into(),
        content: "redacted".into(),
        is_error: false,
    });
    let rebuilt = history_from_view(&original, view).expect("replacement");
    assert_eq!(rebuilt[0], original[0]);
    assert_eq!(
        rebuilt[1],
        HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("Describe this image.")),
            InputPart::File(file("image/png", "shot.png", IMAGE)),
        ]))
    );
    // A rebuilt assistant message has no reasoning, metadata, or replay.
    assert_eq!(
        rebuilt[2],
        HistoryTurn::assistant(CompletedTurn::new(
            AssistantMessage::new(vec![AssistantPart::Text(TextPart::new(
                "Let me read the log."
            ))]),
            Finish::new(Usage::default(), FinishReason::Stop),
        ))
    );
    assert_eq!(
        rebuilt[3],
        HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
            "call-1",
            ToolContent::Mixed(vec![
                ContentValue::Text("redacted".into()),
                ContentValue::File(file("text/plain", "build.log", LOG)),
            ]),
        )]))
    );
}

#[test]
fn a_history_replacement_may_not_add_files_or_misplace_parts() {
    let original = history();
    let refused = |messages: Vec<HookHistoryMessage>| {
        history_from_view(&original, messages).expect_err("an invalid replacement")
    };
    let message = |role, content| HookHistoryMessage { role, content };
    assert!(refused(Vec::new()).contains("must not be empty"));
    let unknown_file = Open::Known(HookContentPart::File {
        artifact: artifact("image/png", "other.png", b"other bytes"),
    });
    assert!(
        refused(vec![message(
            ExtensionMessageRole::User,
            vec![unknown_file.clone()]
        )])
        .starts_with("messages[0].content[0]: file ")
    );
    assert!(
        refused(vec![message(
            ExtensionMessageRole::Tool,
            vec![Open::Known(HookContentPart::File {
                artifact: artifact("text/plain", "build.log", LOG),
            })]
        )]) == "messages[0].content[0]: a file in a tool message must follow a tool result"
    );
    assert!(
        refused(vec![message(
            ExtensionMessageRole::System,
            vec![Open::Known(HookContentPart::ToolCall {
                id: "call-2".into(),
                name: "read".into(),
                arguments: json!({}),
            })]
        )]) == "messages[0].content[0]: a system message cannot hold a `tool_call` part"
    );
    let future: Open<HookContentPart> =
        serde_json::from_value(json!({"type": "hologram", "depth": 3})).expect("an open part");
    assert!(
        refused(vec![message(ExtensionMessageRole::User, vec![future])])
            == "messages[0].content[0]: unknown type `hologram`"
    );
    // The path is the plugin's own array positions, from 0.
    assert!(
        refused(vec![
            message(ExtensionMessageRole::User, vec![text("first")]),
            message(
                ExtensionMessageRole::User,
                vec![text("second"), unknown_file.clone()]
            ),
        ])
        .starts_with("messages[1].content[1]: file ")
    );
}

fn persisted_file(media_type: &str, bytes: &[u8]) -> PersistedAssistantPart {
    let sha256 = Sha256Digest::of_bytes(bytes);
    PersistedAssistantPart::File {
        file: PersistedFilePart {
            media_type: MimeType::new(media_type).expect("media type"),
            filename: None,
            source: PersistedFileSource::Artifact {
                byte_length: bytes.len() as u64,
                reference: ArtifactReference {
                    uri: format!("artifact://sha256/{}", sha256.as_str()),
                },
                sha256,
            },
            metadata: None,
        },
    }
}

/// A committed message with every part `message_end` shows and one it does
/// not (a source) between them.
fn assistant() -> Vec<PersistedAssistantPart> {
    let metadata = serde_json::from_value(json!({"signature": "opaque"})).expect("metadata");
    vec![
        PersistedAssistantPart::Reasoning {
            text: "planning".into(),
            metadata: Some(metadata),
        },
        PersistedAssistantPart::Text {
            text: "Here is the chart.".into(),
            metadata: None,
        },
        PersistedAssistantPart::Source {
            id: None,
            url: Some("https://example.com/data".into()),
            title: None,
            media_type: None,
            excerpt: None,
            metadata: None,
        },
        persisted_file("image/png", IMAGE),
        PersistedAssistantPart::ToolCall {
            id: ModelCallId::new("call-9").expect("call ID"),
            provider_item_id: None,
            name: SafeCode::new("publish").expect("name"),
            input: json!({"draft": true}),
            raw_input: Some("{\"draft\":true}".into()),
            metadata: None,
        },
    ]
}

#[test]
fn an_unchanged_assistant_message_converts_back_to_itself() {
    let original = assistant();
    let view = assistant_view(&original);
    assert_eq!(
        view,
        vec![
            Open::Known(HookAssistantPart::Reasoning {
                text: "planning".into()
            }),
            Open::Known(HookAssistantPart::Text {
                text: "Here is the chart.".into()
            }),
            Open::Known(HookAssistantPart::File {
                artifact: ArtifactRef {
                    filename: None,
                    ..artifact("image/png", "unused", IMAGE)
                },
            }),
            Open::Known(HookAssistantPart::ToolCall {
                id: "call-9".into(),
                name: "publish".into(),
                arguments: json!({"draft": true}),
            }),
        ]
    );
    let returned: Vec<Open<HookAssistantPart>> =
        serde_json::from_value(serde_json::to_value(&view).expect("view JSON")).expect("decode");
    assert_eq!(
        assistant_from_view(&original, returned).expect("replacement"),
        original
    );
}

#[test]
fn a_changed_assistant_part_is_rebuilt_and_hidden_parts_keep_their_place() {
    let original = assistant();
    let mut view = assistant_view(&original);
    // Rewrite the text, which the source followed, and drop the reasoning.
    view[1] = Open::Known(HookAssistantPart::Text {
        text: "Here is the redacted chart.".into(),
    });
    view.remove(0);
    let rebuilt = assistant_from_view(&original, view).expect("replacement");
    assert_eq!(
        rebuilt,
        vec![
            PersistedAssistantPart::Text {
                text: "Here is the redacted chart.".into(),
                metadata: None,
            },
            original[3].clone(),
            original[4].clone(),
            // Its anchor was rewritten, so the source ends the message.
            original[2].clone(),
        ]
    );
    let call = Open::Known(HookAssistantPart::ToolCall {
        id: "call-10".into(),
        name: "publish".into(),
        arguments: json!({}),
    });
    assert_eq!(
        assistant_from_view(&original, vec![call]).expect("a new call")[0],
        PersistedAssistantPart::ToolCall {
            id: ModelCallId::new("call-10").expect("call ID"),
            provider_item_id: None,
            name: SafeCode::new("publish").expect("name"),
            input: json!({}),
            raw_input: None,
            metadata: None,
        }
    );
    let added = Open::Known(HookAssistantPart::File {
        artifact: artifact("image/png", "other.png", b"other bytes"),
    });
    assert!(
        assistant_from_view(&original, vec![added])
            .expect_err("an added file")
            .starts_with("content[0]: file ")
    );
}

#[test]
fn an_input_transform_may_change_or_drop_parts_but_not_add_files() {
    let shot = artifact("image/png", "shot.png", IMAGE);
    let input = UserContent::new(vec![
        Open::Known(UserContentPart::Text {
            text: "look".into(),
        }),
        Open::Known(UserContentPart::File {
            artifact: shot.clone(),
        }),
    ])
    .expect("input");
    let text_only = UserContent::text("look closely");
    assert_eq!(
        transformed_input(&input, text_only.clone()).expect("a dropped file"),
        text_only
    );
    assert_eq!(
        transformed_input(&input, input.clone()).expect("unchanged"),
        input
    );
    let added = UserContent::new(vec![Open::Known(UserContentPart::File {
        artifact: artifact("image/png", "other.png", b"other bytes"),
    })])
    .expect("content");
    assert!(
        transformed_input(&input, added)
            .expect_err("an added file")
            .starts_with("content[0]: file ")
    );
    // The same digest under another name or size is another file.
    let renamed = UserContent::new(vec![Open::Known(UserContentPart::File {
        artifact: ArtifactRef {
            filename: Some("renamed.png".into()),
            ..shot
        },
    })])
    .expect("content");
    assert!(transformed_input(&input, renamed).is_err());
    assert!(
        transformed_input(&input, UserContent::text("  "))
            .expect_err("blank")
            .contains("empty")
    );
}
