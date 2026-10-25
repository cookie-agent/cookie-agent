use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use cookie_agent_protocol::{
    AdaptorId, DisplayBlock, MediaCapability, MediaKind, MimeType, Modality, Open,
    PermissionAction, PersistedToolResult, RunId, SessionId, ToolCallId, ToolEmittedContent,
};
use cookie_agent_runtime_api::{
    SessionToolContext, ToolCall, ToolError, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, TurnAgentContext,
    text_tools::{LineWindow, MAX_LINE_BYTES},
};

use super::{ReadTool, directory_page, text_result};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

fn pdf() -> Vec<u8> {
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for object in [
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>\nendobj\n",
    ] {
        offsets.push(bytes.len());
        bytes.extend_from_slice(object.as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    bytes
}

fn turn_context(
    adapter: AdaptorId,
    media: Option<(MediaKind, Modality, &str, u64)>,
) -> Arc<TurnAgentContext> {
    let base = crate::test_turn_context();
    let mut capabilities = crate::test_capabilities();
    if let Some((kind, modality, mime_type, max_bytes)) = media {
        capabilities.input.insert(modality);
        capabilities.media = BTreeMap::from([(
            kind,
            MediaCapability {
                mime_types: [MimeType::new(mime_type).unwrap()].into_iter().collect(),
                max_bytes,
                max_count: 1,
            },
        )]);
    }
    Arc::new(TurnAgentContext {
        agent: base.agent.clone(),
        model: base.model.clone(),
        input_media: cookie_agent_runtime_api::media::tool_input_media(
            match adapter.as_str() {
                "anthropic" => cookie_agent_runtime_api::AdapterFamily::AnthropicCompatible,
                "openai-compatible" => cookie_agent_runtime_api::AdapterFamily::OpenaiCompatible,
                "aws-bedrock-converse" => {
                    cookie_agent_runtime_api::AdapterFamily::AwsBedrockConverse
                }
                "google-gemini" => cookie_agent_runtime_api::AdapterFamily::GoogleGemini,
                "google-vertex-gemini" => {
                    cookie_agent_runtime_api::AdapterFamily::GoogleVertexGemini
                }
                _ => cookie_agent_runtime_api::AdapterFamily::OpenaiChat,
            },
            &capabilities,
        ),
        adapter,
    })
}

/// The window from 1-based line `offset`, at most `limit` lines.
fn window(offset: usize, limit: usize) -> LineWindow {
    LineWindow::new(Some(offset), Some(limit)).unwrap()
}

fn try_page(text: &str, offset: usize, limit: usize) -> Result<PersistedToolResult, ToolError> {
    text_result(
        Path::new("/file.txt"),
        Path::new("file.txt"),
        &mut std::io::Cursor::new(text.as_bytes()),
        window(offset, limit),
    )
}

fn page(text: &str, offset: usize, limit: usize) -> PersistedToolResult {
    try_page(text, offset, limit).unwrap()
}

#[test]
fn long_lines_are_cut_so_any_page_fits_the_result_limit() {
    let long = "x".repeat(3 * 1024 * 1024);
    let result = page(&format!("{long}\nshort\n"), 1, 1);
    let first = result.output.lines().nth(3).unwrap();
    assert!(first.starts_with(&format!("1: {}...", "x".repeat(MAX_LINE_BYTES))));
    assert!(first.ends_with(&format!(
        "[line truncated: {} more bytes]",
        long.len() - MAX_LINE_BYTES
    )));
    assert_eq!(result.metadata["total_lines"], 2);
    // The second line is left out by the limit: the page says where it is.
    assert_eq!(result.metadata["next_offset"], 2);
    assert!(
        result
            .output
            .ends_with("</content>\n[1 more line; continue with offset=2]")
    );

    let exact = "y".repeat(MAX_LINE_BYTES);
    assert!(page(&exact, 1, 1).output.contains(&format!("1: {exact}\n")));

    // Many long lines stop at the page budget and say where to continue:
    // the 1-based number of the first line left out.
    let text = format!("{}\n", "z".repeat(MAX_LINE_BYTES)).repeat(2_000);
    let result = page(&text, 1, 2_000);
    assert!(result.output.len() <= PersistedToolResult::MAX_OUTPUT_BYTES);
    let next = result.metadata["next_offset"].as_u64().unwrap();
    assert!(next > 1 && next < 2_000);
    assert!(result.output.ends_with(&format!(
        "</content>\n[page cut at the result size limit; continue with offset={next}]"
    )));
    let last_shown = result.output.lines().rev().nth(2).unwrap();
    assert!(
        last_shown.starts_with(&format!("{}: z", next - 1)),
        "{last_shown}"
    );
    let resumed = page(&text, next as usize, 2_000);
    assert!(
        resumed
            .output
            .lines()
            .nth(3)
            .unwrap()
            .starts_with(&format!("{next}: z")),
        "the next page starts at the line the marker names"
    );
    assert_eq!(result.metadata["total_lines"], 2_000);

    let spec = ReadTool::new(&crate::test_workspace(std::env::temp_dir()))
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .unwrap()
        .remove(0);
    assert_eq!(spec.result_truncation, ToolResultTruncationPolicy::OptOut);
}

#[test]
fn multibyte_characters_are_not_split_by_a_line_cut() {
    let line = format!("{}é tail", "a".repeat(MAX_LINE_BYTES - 1));
    let output = page(&line, 1, 1).output;
    assert!(output.contains(&format!("1: {}...", "a".repeat(MAX_LINE_BYTES - 1))));
    assert!(output.contains("[line truncated: 7 more bytes]"));
}

#[test]
fn permission_resource_is_the_file_path() {
    let tool = ReadTool::new(&crate::test_workspace(std::env::temp_dir()));
    assert_eq!(
        tool.get_permission_resource("read", &serde_json::json!({"filePath":"src/lib.rs"}))
            .expect("permission resource"),
        (Some("read"), Some("src/lib.rs".into()))
    );
    assert!(matches!(
        tool.get_permission_resource("read", &serde_json::json!({})),
        Err(ToolError::Failed(_))
    ));
    assert!(matches!(
        tool.get_permission_resource(
            "read",
            &serde_json::json!({"filePath":"src/lib.rs","byteOffset":0})
        ),
        Err(ToolError::Failed(_))
    ));
}

#[tokio::test]
async fn offsets_are_one_based_and_checked_before_approval() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("value.txt"), "one\ntwo\n").expect("fixture");
    let tool = ReadTool::new(&crate::test_workspace(root.path()));
    let prepare = |arguments: serde_json::Value| {
        tool.prepare(
            context(root.path()),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments,
            },
        )
    };
    for (arguments, message) in [
        (
            serde_json::json!({"filePath":"value.txt","offset":0}),
            "offset is 1-based: the first line is offset=1",
        ),
        (
            serde_json::json!({"filePath":"value.txt","limit":0}),
            "limit must be positive",
        ),
    ] {
        let Err(error) = prepare(arguments).await else {
            panic!("an invalid window is refused");
        };
        assert_eq!(error.message(), message);
    }
    let prepared = prepare(serde_json::json!({"filePath":"value.txt","offset":2,"limit":1}))
        .await
        .expect("prepare read");
    assert_eq!(prepared.normalized_arguments()["offset"], 2);
    assert_eq!(prepared.normalized_arguments()["limit"], 1);
}

#[test]
fn an_offset_past_the_last_line_is_an_error() {
    let text = "one\ntwo\nthree\n";
    // The last line is the last page that can start.
    assert!(page(text, 3, 10).output.contains("\n3: three\n</content>"));
    assert_eq!(
        try_page(text, 4, 10).unwrap_err().message(),
        "offset=4 is past the end of /file.txt, which has 3 lines"
    );
    assert_eq!(
        try_page("one", 2, 10).unwrap_err().message(),
        "offset=2 is past the end of /file.txt, which has 1 line"
    );
    // Line 1 of an empty file is an empty page.
    assert_eq!(page("", 1, 10).metadata["total_lines"], 0);
    assert!(try_page("", 2, 10).is_err());
}

#[test]
fn unknown_arguments_are_refused() {
    let tool = ReadTool::new(&crate::test_workspace(std::env::temp_dir()));
    let Err(error) = tool.get_permission_resource(
        "read",
        &serde_json::json!({"filePath":"src/lib.rs","lines":10}),
    ) else {
        panic!("an unknown argument is refused");
    };
    assert!(
        error.message().contains("unknown field `lines`"),
        "{}",
        error.message()
    );
}

#[test]
fn display_argument_abbreviates_paths_and_includes_explicit_window() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tool = ReadTool::new(&crate::test_workspace(workspace.path()));
    assert_eq!(
        tool.get_display_argument("read", &serde_json::json!({"filePath":"src/lib.rs"}))
            .expect("relative"),
        "src/lib.rs"
    );
    assert_eq!(
            tool.get_display_argument(
                "read",
                &serde_json::json!({"filePath":workspace.path().join("src/lib.rs"),"offset":1,"limit":100})
            )
            .expect("workspace"),
            "src/lib.rs [offset=1, limit=100]"
        );
    let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
    assert_eq!(
        tool.get_display_argument(
            "read",
            &serde_json::json!({"filePath": home.join(".bashrc"),"offset":4})
        )
        .expect("home"),
        "~/.bashrc [offset=4]"
    );
    assert_eq!(
        tool.get_display_argument(
            "read",
            &serde_json::json!({"filePath":"src/lib.rs","limit":25})
        )
        .expect("limit"),
        "src/lib.rs [limit=25]"
    );
    assert!(matches!(
        tool.get_display_argument("read", &serde_json::json!({})),
        Err(ToolError::Failed(_))
    ));
    let presentation = tool.presentation(&ToolCall {
        id: ToolCallId::new_v7(),
        name: "read".into(),
        arguments: serde_json::json!({"filePath":workspace.path().join("src/lib.rs")}),
    });
    assert_eq!(presentation.title.as_str(), "Read src/lib.rs");
    assert_eq!(
        presentation
            .primary_argument
            .as_ref()
            .map(cookie_agent_protocol::BoundedDisplayText::as_str),
        Some("src/lib.rs")
    );
    assert_eq!(
        presentation.hint.as_ref().map(|hint| hint.as_str()),
        Some("file_read")
    );
    // The title and primary argument name the file; a whole-file read has
    // no window to add.
    assert!(presentation.arguments.is_empty());
    let windowed = tool.presentation(&ToolCall {
        id: ToolCallId::new_v7(),
        name: "read".into(),
        arguments: serde_json::json!({"filePath":"src/lib.rs","offset":10,"limit":5}),
    });
    assert_eq!(
        windowed.arguments,
        [Open::Known(DisplayBlock::key_value([
            ("offset", "10"),
            ("limit", "5"),
        ]))]
    );
}

#[test]
fn schema_documents_one_based_offset_and_limit_default() {
    let root = tempfile::tempdir().expect("workspace");
    let tool = ReadTool::new(&crate::test_workspace(root.path()));
    let parameters = &tool
        .tools_for_session(&cookie_agent_runtime_api::SessionToolContext::new(
            SessionId::new_v7(),
        ))
        .expect("read spec")[0]
        .parameters;
    assert_eq!(
        parameters["properties"]["offset"]["description"],
        "The 1-based line, or directory entry, to start at. Defaults to 1."
    );
    assert_eq!(
        parameters["properties"]["limit"]["description"],
        "The most lines, or directory entries, to return. Defaults to 2000."
    );
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn text_pagination_handles_one_based_boundaries() {
    let numbered = |text: &str, offset, limit| {
        let output = page(text, offset, limit).output;
        output
            .lines()
            .skip(3)
            .take_while(|line| *line != "</content>")
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert!(numbered("", 1, usize::MAX).is_empty());
    assert_eq!(page("", 1, 1).metadata["total_lines"], 0);

    let text = "first\r\nsecond\nthird";
    // The first page.
    assert_eq!(numbered(text, 1, 2), ["1: first", "2: second"]);
    assert_eq!(page(text, 1, 2).metadata["next_offset"], 3);
    assert!(
        page(text, 1, 2)
            .output
            .ends_with("</content>\n[1 more line; continue with offset=3]")
    );
    assert_eq!(numbered(text, 2, 1), ["2: second"]);
    // The last page: no marker.
    assert_eq!(numbered(text, 3, usize::MAX), ["3: third"]);
    assert!(page(text, 3, usize::MAX).metadata["next_offset"].is_null());
    assert!(page(text, 3, usize::MAX).output.ends_with("</content>"));
    assert!(try_page(text, 4, usize::MAX).is_err());
    assert!(try_page(text, usize::MAX, usize::MAX).is_err());
    assert_eq!(
        numbered(text, 1, usize::MAX),
        ["1: first", "2: second", "3: third"]
    );
    assert_eq!(page(text, 1, 1).metadata["total_lines"], 3);
    assert_eq!(page(text, 2, 1).metadata["offset"], 2);
    assert_eq!(page("a\nb\n", 1, 1).metadata["total_lines"], 2);
    assert!(
        text_result(
            Path::new("/file.txt"),
            Path::new("file.txt"),
            &mut std::io::Cursor::new(b"ok\n\xff\n".as_slice()),
            window(1, 2),
        )
        .is_err()
    );
}

#[test]
fn directory_pagination_handles_one_based_boundaries() {
    let empty = Vec::<(String, bool)>::new();
    assert!(
        directory_page(&empty, window(1, usize::MAX))
            .next()
            .is_none()
    );

    let entries = [
        ("alpha".to_owned(), false),
        ("beta".to_owned(), true),
        ("gamma".to_owned(), false),
    ];
    assert_eq!(
        directory_page(&entries, window(1, 2)).collect::<Vec<_>>(),
        [&entries[0], &entries[1]]
    );
    assert_eq!(
        directory_page(&entries, window(2, usize::MAX)).collect::<Vec<_>>(),
        [&entries[1], &entries[2]]
    );
    assert_eq!(
        directory_page(&entries, window(3, usize::MAX)).collect::<Vec<_>>(),
        [&entries[2]]
    );
    assert!(
        directory_page(&entries, window(4, usize::MAX))
            .next()
            .is_none()
    );
}

fn context_with_turn(root: &Path, turn_context: Arc<TurnAgentContext>) -> ToolPreparationContext {
    ToolPreparationContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        origin: crate::test_coding_origin(root),
        turn_context,
    }
}

fn context(root: &Path) -> ToolPreparationContext {
    context_with_turn(root, crate::test_turn_context())
}

/// A read of `path` prepared and identified as the daemon identifies it.
async fn prepared(root: &Path, path: &str) -> cookie_agent_runtime_api::PreparedTool {
    let tool = ReadTool::new(&crate::test_workspace(root));
    let prepared = tool
        .prepare(
            context(root),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":path}),
            },
        )
        .await
        .expect("prepare read");
    crate::identified(&tool, "read", root, prepared)
}

async fn execute_media(
    root: &Path,
    path: &str,
    turn_context: Arc<TurnAgentContext>,
) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
    let prepared = ReadTool::new(&crate::test_workspace(root))
        .prepare(
            context_with_turn(root, Arc::clone(&turn_context)),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":path}),
            },
        )
        .await?;
    prepared
        .execute_for_test(cookie_agent_engine::testing::tool_execution_context(
            root.join("artifacts"),
            turn_context,
        )?)
        .await
}

#[tokio::test]
async fn media_reads_follow_capability_family_and_size_gates() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pixel.png"), PNG).unwrap();
    let pdf = pdf();
    fs::write(root.path().join("page.pdf"), &pdf).unwrap();
    let mut video = 16_u32.to_be_bytes().to_vec();
    video.extend_from_slice(b"ftypisom");
    video.extend_from_slice(&[0; 4]);
    fs::write(root.path().join("clip.mp4"), &video).unwrap();

    let image_capable = turn_context(
        AdaptorId::ANTHROPIC,
        Some((
            MediaKind::Image,
            Modality::Image,
            "image/png",
            PNG.len() as u64,
        )),
    );
    let image = execute_media(root.path(), "pixel.png", image_capable)
        .await
        .unwrap();
    assert_eq!(image.attachments.len(), 1);
    assert_eq!(image.attachments[0].mime_type.as_str(), "image/png");
    // Clients see the retained attachment itself.
    assert_eq!(
        image.display,
        [Open::Known(DisplayBlock::Media {
            artifact: image.attachments[0].artifact_ref(),
            alt: Some("pixel.png".into()),
        })]
    );
    assert_eq!(
        image.output,
        format!("Attached image/png ({} bytes).", PNG.len())
    );

    let image_incapable = execute_media(
        root.path(),
        "pixel.png",
        turn_context(AdaptorId::ANTHROPIC, None),
    )
    .await
    .unwrap_err();
    assert_eq!(
        image_incapable.message(),
        "Cannot attach image/png: the active model \"test/model\" does not accept image inputs"
    );

    let pdf_capable = turn_context(
        AdaptorId::ANTHROPIC,
        Some((
            MediaKind::Pdf,
            Modality::Pdf,
            "application/pdf",
            pdf.len() as u64,
        )),
    );
    let document = execute_media(root.path(), "page.pdf", pdf_capable)
        .await
        .unwrap();
    assert_eq!(document.attachments.len(), 1);
    assert_eq!(
        document.attachments[0].mime_type.as_str(),
        "application/pdf"
    );

    let pdf_incapable = execute_media(
        root.path(),
        "page.pdf",
        turn_context(AdaptorId::ANTHROPIC, None),
    )
    .await
    .unwrap_err();
    assert_eq!(
        pdf_incapable.message(),
        "Cannot attach application/pdf: the active model \"test/model\" does not accept PDF inputs"
    );

    let openai_compatible_image = execute_media(
        root.path(),
        "pixel.png",
        turn_context(
            AdaptorId::OPENAI_COMPATIBLE,
            Some((
                MediaKind::Image,
                Modality::Image,
                "image/png",
                PNG.len() as u64,
            )),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        openai_compatible_image.output,
        format!(
            "Attached image/png ({} bytes), delivered in the following message.",
            PNG.len()
        )
    );
    assert!(openai_compatible_image.attachments.is_empty());
    assert!(matches!(
        openai_compatible_image.additional_messages[0].content.as_slice(),
        [ToolEmittedContent::File(attachment)]
            if attachment.mime_type.as_str() == "image/png"
    ));

    let size_rejected = execute_media(
        root.path(),
        "pixel.png",
        turn_context(
            AdaptorId::ANTHROPIC,
            Some((
                MediaKind::Image,
                Modality::Image,
                "image/png",
                PNG.len() as u64 - 1,
            )),
        ),
    )
    .await
    .unwrap_err();
    assert!(
        size_rejected
            .message()
            .contains("inline limit for this provider")
    );

    for family in [
        AdaptorId::OPENAI_COMPATIBLE,
        AdaptorId::ANTHROPIC,
        AdaptorId::GOOGLE_GEMINI,
        AdaptorId::GOOGLE_VERTEX_GEMINI,
    ] {
        let result = execute_media(
            root.path(),
            "clip.mp4",
            turn_context(
                family.clone(),
                Some((
                    MediaKind::Video,
                    Modality::Video,
                    "video/mp4",
                    video.len() as u64,
                )),
            ),
        )
        .await
        .unwrap();
        assert!(result.attachments.is_empty(), "{family:?}");
        assert_eq!(
            result.output,
            format!(
                "Attached video/mp4 ({} bytes), delivered in the following message.",
                video.len()
            )
        );
        assert!(matches!(
            result.additional_messages[0].content.as_slice(),
            [ToolEmittedContent::File(attachment)]
                if attachment.mime_type.as_str() == "video/mp4"
        ));
    }

    let video_incapable = execute_media(
        root.path(),
        "clip.mp4",
        turn_context(AdaptorId::OPENAI_COMPATIBLE, None),
    )
    .await
    .unwrap_err();
    assert_eq!(
        video_incapable.message(),
        "Cannot attach video/mp4: the active model \"test/model\" does not accept video inputs"
    );
}

#[tokio::test]
async fn malformed_media_still_errors_and_text_path_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("bad.png"), b"not a PNG").unwrap();
    let malformed = execute_media(
        root.path(),
        "bad.png",
        turn_context(
            AdaptorId::ANTHROPIC,
            Some((MediaKind::Image, Modality::Image, "image/png", 1024)),
        ),
    )
    .await
    .unwrap_err();
    assert!(
        malformed
            .message()
            .contains("malformed image, PDF, audio, or video")
    );
    assert!(!malformed.message().contains("Cannot attach"));

    fs::write(root.path().join("note.txt"), "plain text\n").unwrap();
    let text = execute_media(
        root.path(),
        "note.txt",
        turn_context(AdaptorId::OPENAI_COMPATIBLE, None),
    )
    .await
    .unwrap();
    assert!(text.output.contains("1: plain text"));
    assert!(text.attachments.is_empty());
}

#[tokio::test]
async fn prepared_read_exposes_lexical_normalized_arguments_not_raw_traversal() {
    let root = tempfile::TempDir::new().expect("temp directory");
    std::fs::create_dir(root.path().join("safe")).expect("safe directory");
    std::fs::write(root.path().join(".env"), "secret").expect("env file");
    let prepared = prepared(root.path(), "safe/../.env").await;
    let normalized = prepared
        .normalized_arguments()
        .get("filePath")
        .and_then(serde_json::Value::as_str)
        .expect("normalized file path");
    let expected = crate::shown_root(root.path()).join(".env");
    assert_eq!(Path::new(normalized), expected);
    assert!(
        !prepared
            .normalized_arguments()
            .to_string()
            .contains("safe/..")
    );
    assert_eq!(prepared.normalized_arguments()["offset"], 1);
}

#[tokio::test]
async fn outside_workspace_read_has_one_absolute_labeled_resource() {
    let workspace = tempfile::tempdir().expect("workspace");
    let external = tempfile::NamedTempFile::new().expect("external file");
    fs::write(external.path(), "external").expect("fixture");
    let prepared = ReadTool::new(&crate::test_workspace(workspace.path()))
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: crate::test_coding_origin(workspace.path()),
                turn_context: crate::test_turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":external.path()}),
            },
        )
        .await
        .expect("prepare external read");
    let prepared = crate::identified(
        &ReadTool::new(&crate::test_workspace(workspace.path())),
        "read",
        workspace.path(),
        prepared,
    );
    let expected_label = external
        .path()
        .canonicalize()
        .expect("canonical external path")
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();
    assert_eq!(prepared.operation().capabilities().len(), 1);
    assert_eq!(
        prepared.operation().resources()[0].capability,
        PermissionAction::READ
    );
    assert_eq!(prepared.policy_labels(), [Some(expected_label.clone())]);
    assert_eq!(
        ReadTool::new(&crate::test_workspace(workspace.path()))
            .get_permission_resource("read", prepared.normalized_arguments())
            .expect("resource from prepared args"),
        (Some("read"), Some(expected_label))
    );
}

#[tokio::test]
async fn workspace_read_label_is_canonical_workspace_relative() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::create_dir(workspace.path().join("nested")).expect("nested");
    fs::write(workspace.path().join("nested/value.txt"), "value").expect("fixture");
    let prepared = prepared(workspace.path(), "nested/value.txt").await;
    assert_eq!(prepared.policy_labels(), [Some("nested/value.txt".into())]);
    assert_eq!(
        ReadTool::new(&crate::test_workspace(workspace.path()))
            .get_permission_resource("read", prepared.normalized_arguments())
            .expect("resource from prepared args"),
        (Some("read"), Some("nested/value.txt".into()))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hard_link_through_distinct_parent_capabilities_has_distinct_fingerprint() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir(root.path().join("left")).expect("left");
    fs::create_dir(root.path().join("right")).expect("right");
    fs::write(root.path().join("left/value"), "same inode").expect("fixture");
    fs::hard_link(
        root.path().join("left/value"),
        root.path().join("right/value"),
    )
    .expect("hard link");

    let left_capability = crate::fs_cap::prepare_existing(root.path(), Path::new("left/value"))
        .expect("left capability");
    let right_capability = crate::fs_cap::prepare_existing(root.path(), Path::new("right/value"))
        .expect("right capability");
    assert_ne!(
        left_capability.binding_bytes(),
        right_capability.binding_bytes()
    );

    let left = prepared(root.path(), "left/value").await;
    let right = prepared(root.path(), "right/value").await;
    assert_ne!(
        left.operation().resources()[0].binding_digest,
        right.operation().resources()[0].binding_digest
    );
    assert_ne!(
        cookie_agent_protocol::OperationFingerprint::from_prepared_operation(left.operation()),
        cookie_agent_protocol::OperationFingerprint::from_prepared_operation(right.operation())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_routes_keep_lexical_read_permissions_and_arguments() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let external = root.path().join("external");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(workspace.join("routes")).expect("routes");
    fs::create_dir(&external).expect("external");
    fs::write(external.join("value.txt"), "external value").expect("fixture");
    symlink(
        "../../external/value.txt",
        workspace.join("routes/relative-leaf"),
    )
    .expect("relative leaf");
    symlink(
        external.join("value.txt"),
        workspace.join("routes/absolute-leaf"),
    )
    .expect("absolute leaf");
    symlink(
        "absolute-ancestor",
        workspace.join("routes/relative-ancestor"),
    )
    .expect("relative ancestor");
    symlink(&external, workspace.join("routes/absolute-ancestor")).expect("absolute ancestor");

    let tool = ReadTool::new(&crate::test_workspace(&workspace));
    for requested in [
        "routes/relative-leaf",
        "routes/absolute-leaf",
        "routes/relative-ancestor/value.txt",
        "routes/absolute-ancestor/value.txt",
    ] {
        let prepared = tool
            .prepare(
                context(&workspace),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":requested}),
                },
            )
            .await
            .expect("prepare symlink read");
        let prepared = crate::identified(&tool, "read", &workspace, prepared);
        let expected_path = workspace.join(requested);
        assert_eq!(prepared.operation().resources().len(), 1);
        assert_eq!(prepared.policy_labels(), [Some(requested.to_owned())]);
        assert_eq!(
            prepared.normalized_arguments()["filePath"],
            expected_path.to_string_lossy().as_ref()
        );
        assert_eq!(
            tool.get_permission_resource("read", prepared.normalized_arguments())
                .expect("normalized permission resource"),
            (Some("read"), Some(requested.to_owned()))
        );
        crate::assert_workspace_rule_allows(
            &prepared,
            &workspace,
            PermissionAction::READ,
            "routes/*",
        );
        let result = prepared
            .execute_for_test(
                cookie_agent_engine::testing::tool_execution_context(
                    root.path().join("artifacts"),
                    crate::test_turn_context(),
                )
                .expect("execution context"),
            )
            .await
            .expect("execute symlink read");
        assert!(result.output.contains("external value"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_symlink_read_errors_and_retargeted_routes_do_not_redirect() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join("value.txt"), "value").expect("fixture");
    fs::write(root.path().join("other.txt"), "other").expect("other fixture");
    symlink("missing.txt", root.path().join("dangling")).expect("dangling link");
    let dangling = ReadTool::new(&crate::test_workspace(root.path()))
        .prepare(
            context(root.path()),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":"dangling"}),
            },
        )
        .await;
    assert!(matches!(dangling, Err(ToolError::Failed(_))));

    symlink("value.txt", root.path().join("route")).expect("route");
    let prepared = prepared(root.path(), "route").await;
    fs::remove_file(root.path().join("route")).expect("remove route");
    symlink("other.txt", root.path().join("route")).expect("retarget route");
    let result = prepared
        .execute_for_test(
            cookie_agent_engine::testing::tool_execution_context(
                root.path().join("artifacts"),
                crate::test_turn_context(),
            )
            .expect("execution context"),
        )
        .await
        .expect("read the approved target");
    assert!(result.output.contains("1: value"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_routes_to_the_same_read_target_have_distinct_fingerprints() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join("value.txt"), "value").expect("fixture");
    symlink("value.txt", root.path().join("left")).expect("left route");
    symlink("value.txt", root.path().join("right")).expect("right route");

    let left = prepared(root.path(), "left").await;
    let right = prepared(root.path(), "right").await;
    assert_ne!(
        left.operation().resources()[0].binding_digest,
        right.operation().resources()[0].binding_digest
    );
    assert_ne!(
        cookie_agent_protocol::OperationFingerprint::from_prepared_operation(left.operation()),
        cookie_agent_protocol::OperationFingerprint::from_prepared_operation(right.operation())
    );
}

/// Reads `path` under `root` with `arguments` merged into the call.
async fn read(root: &Path, arguments: serde_json::Value) -> PersistedToolResult {
    let tool = ReadTool::new(&crate::test_workspace(root));
    let prepared = tool
        .prepare(
            context(root),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments,
            },
        )
        .await
        .expect("prepare read");
    prepared
        .execute_for_test(
            cookie_agent_engine::testing::tool_execution_context(
                root.join("artifacts"),
                crate::test_turn_context(),
            )
            .expect("execution context"),
        )
        .await
        .expect("read")
}

#[tokio::test]
async fn a_text_read_shows_an_excerpt_with_file_line_numbers() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "one\ntwo\r\nthree\nfour\n").unwrap();
    let result = read(
        root.path(),
        serde_json::json!({"filePath":"src/lib.rs","offset":2,"limit":2}),
    )
    .await;
    assert_eq!(
        result.display,
        [Open::Known(DisplayBlock::FileExcerpt {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            start_line: 2,
            lines: vec!["two".into(), "three".into()],
            total_lines: Some(4),
            omitted_lines: None,
            full: None,
        })]
    );
    // The model's page numbers the same lines.
    assert_eq!(
        result.output,
        format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n2: two\n3: three\n</content>\n[1 more line; continue with offset=4]",
            crate::shown_root(root.path())
                .join("src")
                .join("lib.rs")
                .display()
        )
    );
}

#[tokio::test]
async fn a_long_page_shows_the_excerpt_that_fits_and_counts_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let line = "x".repeat(100);
    fs::write(
        root.path().join("long.txt"),
        format!("{line}\n").repeat(2_000),
    )
    .unwrap();
    let result = read(root.path(), serde_json::json!({"filePath":"long.txt"})).await;
    let [
        Open::Known(DisplayBlock::FileExcerpt {
            lines,
            omitted_lines: Some(omitted),
            total_lines: Some(2_000),
            full: None,
            ..
        }),
    ] = result.display.as_slice()
    else {
        panic!("a cut excerpt: {:?}", result.display);
    };
    assert_eq!(lines.len() as u64 + omitted, 2_000);
    cookie_agent_protocol::validate_display(&result.display).expect("within the budget");
}

#[tokio::test]
async fn a_directory_read_shows_a_table_of_entries() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("tree/src")).unwrap();
    fs::write(root.path().join("tree/Cargo.toml"), "").unwrap();
    let result = read(root.path(), serde_json::json!({"filePath":"tree"})).await;
    assert_eq!(
        result.display,
        [Open::Known(DisplayBlock::Table {
            columns: vec!["name".into(), "type".into()],
            rows: vec![
                vec!["Cargo.toml".into(), "file".into()],
                vec!["src".into(), "directory".into()],
            ],
            omitted_rows: None,
        })]
    );
    assert_eq!(
        result.title.as_str(),
        format!(
            "Read {}",
            crate::shown_root(root.path()).join("tree").display()
        )
    );
}

#[tokio::test]
async fn a_directory_page_counts_entries_from_one_and_says_where_to_continue() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("tree")).unwrap();
    for name in ["a", "b", "c"] {
        fs::write(root.path().join("tree").join(name), "").unwrap();
    }
    let first = read(
        root.path(),
        serde_json::json!({"filePath":"tree","offset":1,"limit":2}),
    )
    .await;
    assert!(
        first
            .output
            .ends_with("<entries>\na\nb\n</entries>\n[1 more entry; continue with offset=3]"),
        "{}",
        first.output
    );
    assert_eq!(first.metadata["next_offset"], 3);
    let last = read(
        root.path(),
        serde_json::json!({"filePath":"tree","offset":3,"limit":2}),
    )
    .await;
    assert!(
        last.output.ends_with("<entries>\nc\n</entries>"),
        "{}",
        last.output
    );
    assert!(last.metadata["next_offset"].is_null());
    let tool = ReadTool::new(&crate::test_workspace(root.path()));
    let Err(error) = tool
        .prepare(
            context(root.path()),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":"tree","offset":4}),
            },
        )
        .await
        .expect("prepare read")
        .execute_for_test(
            cookie_agent_engine::testing::tool_execution_context(
                root.path().join("artifacts"),
                crate::test_turn_context(),
            )
            .expect("execution context"),
        )
        .await
    else {
        panic!("an offset past the last entry is an error");
    };
    assert_eq!(
        error.message(),
        format!(
            "offset=4 is past the end of {}, which has 3 entries",
            crate::shown_root(root.path()).join("tree").display()
        )
    );
}
