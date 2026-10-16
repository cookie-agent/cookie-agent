use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cookie_agent_config::{LoadedMcpServer, McpServerConfig, McpServerSource};
use cookie_agent_protocol::{
    AgentId, CancellationCapability, Modality, ModelCapabilities, ReplayCapability, RunId,
    SessionId, ToolCallId,
};
use rmcp::model::Tool;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use cookie_agent_runtime_api::{
    ProgressSink, SessionToolContext, ToolArtifacts, ToolCall, ToolExecutionContext,
    ToolPreparationContext, ToolProvider as _, TurnAgentContext,
};

use super::{
    McpRegistry, McpRegistryOptions, McpServerFailure, McpServerState, TOOL_LIST_DEBOUNCE,
    convert_tool, sanitize_name,
};

#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

/// Bound for polling a fixture towards an expected state. Passing tests stop
/// polling as soon as it is reached; loaded CI runners need the headroom.
const WAIT: Duration = Duration::from_secs(30);

/// Attachment bytes a test call retained, by digest.
#[derive(Debug, Default)]
struct MemoryArtifacts {
    blobs: std::sync::Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl ToolArtifacts for MemoryArtifacts {
    fn retain(
        &self,
        _session: SessionId,
        content: &[u8],
    ) -> Result<(cookie_agent_protocol::ArtifactReference, String), crate::ToolError> {
        let digest = cookie_agent_protocol::Sha256Digest::of_bytes(content)
            .as_str()
            .to_owned();
        self.blobs.lock().unwrap().push(content.to_vec());
        Ok((
            cookie_agent_runtime_api::protocol::artifact_reference(&digest),
            digest,
        ))
    }

    async fn read_page(
        self: Arc<Self>,
        _session: SessionId,
        _target: cookie_agent_protocol::ArtifactReadTarget,
        _offset: u64,
        _limit: u64,
    ) -> Result<cookie_agent_runtime_api::ArtifactReadPage, crate::ToolError> {
        Err(crate::ToolError::execution(
            "no artifact reads in MCP tests",
        ))
    }
}

fn memory_artifacts() -> Arc<MemoryArtifacts> {
    Arc::new(MemoryArtifacts::default())
}

fn fixture_config() -> McpServerConfig {
    McpServerConfig {
        command: Some(PYTHON.into()),
        args: vec![
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("mcp_server.py")
                .to_string_lossy()
                .into_owned(),
        ],
        env: BTreeMap::new(),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        oauth: Default::default(),
        enabled: true,
        // Generous: under a loaded parallel test run (Windows CI especially)
        // starting Python and completing the handshake can take seconds. No
        // test relies on this timeout firing; slow-list tests cancel instead.
        timeout_ms: Some(30_000),
    }
}

fn registry(source: McpServerSource) -> McpRegistry {
    McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source,
                config: fixture_config(),
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("MCP registry")
}

fn notification_fixture_config(
    directory: &tempfile::TempDir,
    notifications: usize,
    relist_name: &str,
) -> McpServerConfig {
    let mut config = fixture_config();
    config.args = vec![
            "-c".into(),
            r#"import json
import os
import sys

count = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "server/discover":
        print(json.dumps({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"legacy"}}), flush=True)
    elif method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":True}},"serverInfo":{"name":"debounce-fixture","version":"1.0"}}}), flush=True)
    elif method == "tools/list":
        count += 1
        with open(os.environ["MCP_LIST_COUNT_FILE"], "w", encoding="utf-8") as count_file:
            count_file.write(str(count))
        name = "old" if count == 1 else os.environ["MCP_RELIST_NAME"]
        tools = [{"name":name,"description":"stable","inputSchema":{"type":"object","properties":{}}}]
        print(json.dumps({"jsonrpc":"2.0","id":request_id,"result":{"tools":tools}}), flush=True)
        if count == 1:
            for _ in range(int(os.environ["MCP_NOTIFICATION_COUNT"])):
                print(json.dumps({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}), flush=True)
"#
            .into(),
        ];
    config.env.insert(
        "MCP_LIST_COUNT_FILE".into(),
        directory
            .path()
            .join("list-count")
            .to_string_lossy()
            .into_owned(),
    );
    config
        .env
        .insert("MCP_NOTIFICATION_COUNT".into(), notifications.to_string());
    config
        .env
        .insert("MCP_RELIST_NAME".into(), relist_name.into());
    config
}

fn list_count(directory: &tempfile::TempDir) -> usize {
    std::fs::read_to_string(directory.path().join("list-count"))
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

async fn wait_for_list_count(directory: &tempfile::TempDir, expected: usize) {
    tokio::time::timeout(WAIT, async {
        while list_count(directory) < expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("tools/list count did not reach {expected}"));
}

async fn wait_until_debounce_armed(armed: &AtomicU64, expected: u64) {
    const MAX_YIELDS: usize = 10_000;

    for _ in 0..MAX_YIELDS {
        if armed.load(Ordering::Acquire) >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    let observed = armed.load(Ordering::Acquire);
    panic!(
        "debounce worker did not arm: expected counter >= {expected}, observed {observed} after {MAX_YIELDS} yields"
    );
}

fn turn_context() -> Arc<TurnAgentContext> {
    Arc::new(make_turn_context(
        cookie_agent_protocol::AdaptorId::OpenaiChat,
        false,
    ))
}

fn make_turn_context(
    adapter: cookie_agent_protocol::AdaptorId,
    with_image: bool,
) -> TurnAgentContext {
    let mut capabilities = ModelCapabilities {
        input: [Modality::Text].into_iter().collect(),
        output: [Modality::Text].into_iter().collect(),
        context_tokens: 8_192,
        output_tokens: 2_048,
        tool_calling: true,
        parallel_tool_calls: true,
        structured_output: false,
        reasoning: false,
        temperature: true,
        top_p: true,
        seed: false,
        native_replay: ReplayCapability::Optional,
        cancellation: CancellationCapability::LocalOnly,
        media: BTreeMap::new(),
    };
    if with_image {
        capabilities.input.insert(Modality::Image);
        capabilities.media = BTreeMap::from([(
            cookie_agent_protocol::MediaKind::Image,
            cookie_agent_protocol::MediaCapability {
                mime_types: [cookie_agent_protocol::MimeType::new("image/png").expect("mime")]
                    .into_iter()
                    .collect(),
                max_bytes: 20 * 1024 * 1024,
                max_count: 1,
            },
        )]);
    }
    TurnAgentContext {
        agent: AgentId::new("test").expect("agent"),
        model: "test/model".parse().expect("model key"),
        adapter,
        adapter_family: match adapter {
            cookie_agent_protocol::AdaptorId::Anthropic => {
                cookie_agent_runtime_api::AdapterFamily::AnthropicCompatible
            }
            cookie_agent_protocol::AdaptorId::OpenaiCompatible => {
                cookie_agent_runtime_api::AdapterFamily::OpenaiCompatible
            }
            cookie_agent_protocol::AdaptorId::GoogleGemini => {
                cookie_agent_runtime_api::AdapterFamily::GoogleGemini
            }
            _ => cookie_agent_runtime_api::AdapterFamily::OpenaiChat,
        },
        capabilities,
    }
}

async fn execute(
    registry: &McpRegistry,
    name: &str,
    arguments: Value,
) -> cookie_agent_protocol::PersistedToolResult {
    try_execute(registry, name, arguments)
        .await
        .expect("execute MCP call")
}

async fn try_execute(
    registry: &McpRegistry,
    name: &str,
    arguments: Value,
) -> Result<cookie_agent_protocol::PersistedToolResult, crate::ToolError> {
    let call_id = ToolCallId::new_v7();
    let prepared = registry
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: cookie_agent_runtime_api::ToolOrigin::Runtime(std::sync::Arc::new(
                    cookie_agent_runtime_api::RuntimeInfo::global(),
                )),
                turn_context: turn_context(),
            },
            ToolCall {
                id: call_id,
                name: name.into(),
                arguments,
            },
        )
        .await
        .expect("prepare MCP call");
    let executor = prepared
        .executor()
        .lock()
        .await
        .take()
        .expect("prepared executor");
    executor.revalidate().await.expect("revalidate MCP call");
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(8);
    executor
        .execute(ToolExecutionContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            progress: ProgressSink::new(progress_tx),
            cancellation: CancellationToken::new(),
            stdin: None,
            turn_context: turn_context(),
            artifacts: memory_artifacts(),
            background: None,
        })
        .await
        .map(|completion| {
            completion
                .into_result_for_test()
                .expect("terminal MCP output")
        })
}

#[test]
fn names_are_sanitized_without_prefix() {
    assert_eq!(sanitize_name("git hub"), "git_hub");
    assert_eq!(sanitize_name("search/repos"), "search_repos");
}

#[test]
fn tool_result_retains_only_distinct_structured_content() {
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(1);
    let context = ToolExecutionContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        progress: ProgressSink::new(progress_tx),
        cancellation: CancellationToken::new(),
        stdin: None,
        turn_context: Arc::new(make_turn_context(
            cookie_agent_protocol::AdaptorId::Anthropic,
            false,
        )),
        artifacts: memory_artifacts(),
        background: None,
    };
    let structured = json!({"a": 1, "b": [2]});
    for (texts, data, retain) in [
        (vec!["plain text"], None, false),
        (vec![r#"{"a":1,"b":[2]}"#], Some(structured.clone()), false),
        (
            vec!["{\n  \"b\": [2], \"a\": 1\n}"],
            Some(structured.clone()),
            false,
        ),
        (
            vec!["summary", r#"{"a":1,"b":[2]}"#],
            Some(structured.clone()),
            false,
        ),
        (
            vec!["{\"a\":1,", "\"b\":[2]}"],
            Some(structured.clone()),
            false,
        ),
        (vec![r#"{"a":3,"b":[2]}"#], Some(structured.clone()), true),
        (vec!["plain text"], Some(structured.clone()), true),
        (vec![], Some(structured), true),
    ] {
        let mut result = rmcp::model::CallToolResult::success(
            texts
                .iter()
                .map(|text| rmcp::model::ContentBlock::text(*text))
                .collect(),
        );
        result.structured_content = data.clone();
        let mapped = super::map_tool_result(&context, "structured", result).unwrap();
        assert_eq!(mapped.output, texts.join("\n"));
        let expected = if retain {
            json!({"mcp": {"is_error": false, "structured_content": data}})
        } else {
            json!({"mcp": {"is_error": false}})
        };
        assert_eq!(mapped.metadata, expected);
        assert!(mapped.attachments.is_empty());
    }
}

#[test]
fn base64_attachments_follow_the_media_gate() {
    use base64::Engine as _;
    use cookie_agent_protocol::AdaptorId;

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let data = base64::engine::general_purpose::STANDARD.encode(PNG);
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(1);
    let context = |turn: TurnAgentContext| ToolExecutionContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        progress: ProgressSink::new(progress_tx.clone()),
        cancellation: CancellationToken::new(),
        stdin: None,
        turn_context: Arc::new(turn),
        artifacts: memory_artifacts(),
        background: None,
    };

    let rejected = super::retain_base64_attachment(
        &context(make_turn_context(AdaptorId::Anthropic, false)),
        "image/png".into(),
        &data,
    )
    .expect_err("incapable model must reject");
    assert!(
        matches!(&rejected, super::McpAttachmentError::Gated(message) if message.contains("does not accept image inputs")),
        "unexpected error: {rejected:?}"
    );

    super::retain_base64_attachment(
        &context(make_turn_context(AdaptorId::Anthropic, true)),
        "image/png".into(),
        &data,
    )
    .expect("capable model must retain");

    let retained = super::retain_base64_attachment(
        &context(make_turn_context(AdaptorId::OpenaiChat, true)),
        "image/png".into(),
        &data,
    )
    .expect("user-turn image must retain");
    assert_eq!(
        retained.delivery,
        cookie_agent_runtime_api::media::AttachmentGate::DeliverViaUserTurn
    );

    let video_turn = |adapter| {
        let mut turn = make_turn_context(adapter, false);
        turn.capabilities.input.insert(Modality::Video);
        turn.capabilities.media.insert(
            cookie_agent_protocol::MediaKind::Video,
            cookie_agent_protocol::MediaCapability {
                mime_types: [cookie_agent_protocol::MimeType::new("video/mp4").unwrap()]
                    .into_iter()
                    .collect(),
                max_bytes: 1024,
                max_count: 1,
            },
        );
        turn
    };
    let mut video = 16_u32.to_be_bytes().to_vec();
    video.extend_from_slice(b"ftypisom");
    video.extend_from_slice(&[0; 4]);
    let encoded_video = base64::engine::general_purpose::STANDARD.encode(video);

    let rejected = super::retain_base64_attachment(
        &context(video_turn(AdaptorId::OpenaiChat)),
        "video/mp4".into(),
        &encoded_video,
    )
    .expect_err("undeliverable family must reject");
    assert!(
        matches!(&rejected, super::McpAttachmentError::Gated(message) if message.contains("not deliverable in tool results or user messages")),
        "unexpected error: {rejected:?}"
    );

    let video_context = context(video_turn(AdaptorId::OpenaiCompatible));
    let retained =
        super::retain_base64_attachment(&video_context, "video/mp4".into(), &encoded_video)
            .expect("user-turn video must retain");
    assert_eq!(
        retained.delivery,
        cookie_agent_runtime_api::media::AttachmentGate::DeliverViaUserTurn
    );

    let result: rmcp::model::CallToolResult = serde_json::from_value(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "file:///clip.mp4",
                "mimeType": "video/mp4",
                "blob": encoded_video
            }
        }]
    }))
    .expect("MCP blob result");
    let mapped =
        super::map_tool_result(&video_context, "video", result).expect("map MCP video result");
    assert_eq!(mapped.metadata, json!({"mcp": {"is_error": false}}));
    assert!(mapped.attachments.is_empty());
    assert!(matches!(
        mapped.additional_messages[0].content.as_slice(),
        [cookie_agent_protocol::ToolEmittedContent::File(attachment)]
            if attachment.mime_type.as_str() == "video/mp4"
    ));

    let ordinary = super::map_tool_result(
        &video_context,
        "text",
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text("plain")]),
    )
    .expect("map ordinary MCP result");
    assert!(ordinary.additional_messages.is_empty());

    let mut audio_turn = make_turn_context(AdaptorId::GoogleGemini, false);
    audio_turn.capabilities.input.insert(Modality::Audio);
    audio_turn.capabilities.media.insert(
        cookie_agent_protocol::MediaKind::Audio,
        cookie_agent_protocol::MediaCapability {
            mime_types: [cookie_agent_protocol::MimeType::new("audio/mpeg").unwrap()]
                .into_iter()
                .collect(),
            max_bytes: 1024,
            max_count: 1,
        },
    );
    let audio_context = context(audio_turn);
    let encoded_audio = base64::engine::general_purpose::STANDARD.encode(b"ID3payload");
    let audio =
        super::retain_base64_attachment(&audio_context, "audio/mpeg".into(), &encoded_audio)
            .expect("MCP audio must retain");
    assert_eq!(audio.attachment.mime_type.as_str(), "audio/mpeg");
    assert_eq!(
        audio.delivery,
        cookie_agent_runtime_api::media::AttachmentGate::DeliverViaUserTurn
    );

    let octet_stream: rmcp::model::CallToolResult = serde_json::from_value(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "file:///pixel.bin",
                "mimeType": "application/octet-stream",
                "blob": data
            }
        }]
    }))
    .expect("MCP octet-stream blob result");
    let mapped = super::map_tool_result(
        &context(make_turn_context(AdaptorId::Anthropic, true)),
        "blob",
        octet_stream,
    )
    .expect("sniffed octet-stream blob must map");
    assert_eq!(mapped.attachments[0].mime_type.as_str(), "image/png");
    assert_eq!(mapped.metadata, json!({"mcp": {"is_error": false}}));
    assert!(!mapped.output.contains(&data));
}

#[test]
fn tool_result_caps_combined_attachments_and_degrades_overflow_inline() {
    use base64::Engine as _;
    use cookie_agent_protocol::AdaptorId;

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let data = base64::engine::general_purpose::STANDARD.encode(PNG);
    let content = (0..=cookie_agent_protocol::PersistedToolResult::MAX_ATTACHMENTS)
        .map(|_| {
            serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": data
            })
        })
        .collect::<Vec<_>>();
    let result = serde_json::from_value(serde_json::json!({ "content": content }))
        .expect("oversized MCP attachment result");
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(1);
    let context = ToolExecutionContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        progress: ProgressSink::new(progress_tx),
        cancellation: CancellationToken::new(),
        stdin: None,
        turn_context: Arc::new(make_turn_context(AdaptorId::Anthropic, true)),
        artifacts: memory_artifacts(),
        background: None,
    };

    let mapped = super::map_tool_result(&context, "images", result).expect("bounded result");
    assert_eq!(
        mapped.attachments.len(),
        cookie_agent_protocol::PersistedToolResult::MAX_ATTACHMENTS
    );
    assert!(mapped.additional_messages.is_empty());
    assert_eq!(
        mapped
            .output
            .lines()
            .filter(|line| line.contains("combined attachment limit reached"))
            .count(),
        1
    );
    mapped.validate().expect("bounded MCP result validates");
}

#[test]
fn tool_specs_force_closed_object_schema_and_keep_defs() {
    let tool = Tool::new(
        "search/repos",
        "Search repositories.",
        Arc::new(Map::from_iter([
            ("$defs".into(), json!({"Query":{"type":"string"}})),
            (
                "properties".into(),
                json!({"query":{"$ref":"#/$defs/Query"}}),
            ),
        ])),
    );
    let converted = convert_tool("git hub", "git_hub", tool).expect("tool conversion");
    assert_eq!(converted.spec.name, "git_hub_search_repos");
    assert_eq!(converted.spec.permission_name, "mcp");
    assert_eq!(converted.spec.parameters["type"], "object");
    assert_eq!(converted.spec.parameters["additionalProperties"], false);
    assert!(converted.spec.parameters.get("$defs").is_some());
    assert!(converted.spec.description.contains("untrusted MCP output"));
}

#[tokio::test]
async fn stdio_lists_calls_and_refreshes_tools() {
    let registry = registry(McpServerSource::UserFile);
    let server = registry.server("fixture").expect("fixture server");
    server.connect().await.expect("connect fixture");
    let specs = registry
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .expect("MCP tools");
    assert_eq!(specs.len(), 2);
    assert!(specs.iter().any(|tool| tool.name == "fixture_echo_text"));

    let result = execute(&registry, "fixture_echo_text", json!({"text":"refresh"})).await;
    assert_eq!(result.output, "refresh");
    assert_eq!(result.metadata, json!({"mcp": {"is_error": false}}));

    tokio::time::timeout(WAIT, async {
        loop {
            let names = registry
                .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
                .expect("refreshed tools")
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>();
            if names.contains(&"fixture_new_tool".to_owned()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("tools/list_changed refresh");

    let error = execute(&registry, "fixture_fail", json!({})).await;
    assert_eq!(error.output, "fixture failure");
    assert_eq!(error.metadata, json!({"mcp": {"is_error": true}}));
    registry.shutdown().await;
}

#[test]
fn mcp_registry_has_no_system_prompt_contribution() {
    let registry = registry(McpServerSource::UserFile);
    assert!(
        registry
            .prompt_sections(&SessionToolContext::new(SessionId::new_v7()))
            .expect("default MCP prompt sections")
            .is_empty()
    );
}

#[tokio::test]
async fn tool_list_notification_burst_makes_one_refresh_round_trip() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = notification_fixture_config(&directory, 10, "new");
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    server.connect().await.expect("connect fixture");

    tokio::time::timeout(WAIT, async {
        while list_count(&directory) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("debounced tools/list refresh");
    tokio::time::sleep(TOOL_LIST_DEBOUNCE + Duration::from_millis(250)).await;

    assert_eq!(list_count(&directory), 2);
    assert_eq!(server.tools.lock().expect("tools")[0].raw_name, "new");
    registry.shutdown().await;
}

#[tokio::test]
async fn tool_list_debounce_resets_from_each_notification() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = notification_fixture_config(&directory, 0, "new");
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    server.connect().await.expect("connect fixture");
    let peer = server
        .service
        .lock()
        .await
        .as_ref()
        .expect("service")
        .peer()
        .clone();
    let armed = Arc::clone(&server.tool_refresh.lock().expect("tool refresh").armed);
    tokio::time::pause();

    server.schedule_tool_refresh(peer.clone());
    wait_until_debounce_armed(&armed, 1).await;
    tokio::time::advance(Duration::from_millis(500)).await;
    server.schedule_tool_refresh(peer);
    wait_until_debounce_armed(&armed, 2).await;
    tokio::time::advance(TOOL_LIST_DEBOUNCE - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(list_count(&directory), 1);

    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::time::resume();
    wait_for_list_count(&directory, 2).await;
    assert_eq!(list_count(&directory), 2);
    registry.shutdown().await;
}

#[tokio::test]
async fn reset_pending_tool_refresh_starts_an_isolated_worker() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = notification_fixture_config(&directory, 0, "new");
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    server.connect().await.expect("connect fixture");
    let peer = server
        .service
        .lock()
        .await
        .as_ref()
        .expect("service")
        .peer()
        .clone();
    tokio::time::pause();

    server.schedule_tool_refresh(peer.clone());
    let (old_notify, old_worker, old_armed) = {
        let refresh = server.tool_refresh.lock().expect("tool refresh");
        (
            Arc::clone(&refresh.notify),
            refresh.worker.as_ref().expect("worker").abort_handle(),
            Arc::clone(&refresh.armed),
        )
    };
    wait_until_debounce_armed(&old_armed, 1).await;
    server.reset_tool_refresh();
    assert!(!Arc::ptr_eq(
        &old_notify,
        &server.tool_refresh.lock().expect("tool refresh").notify
    ));
    let new_armed = Arc::clone(&server.tool_refresh.lock().expect("tool refresh").armed);

    server.schedule_tool_refresh(peer);
    wait_until_debounce_armed(&new_armed, 1).await;
    tokio::time::advance(TOOL_LIST_DEBOUNCE).await;
    tokio::time::resume();
    wait_for_list_count(&directory, 2).await;

    assert!(old_worker.is_finished());
    assert_eq!(list_count(&directory), 2);
    registry.shutdown().await;
}

#[tokio::test]
async fn dropping_registry_aborts_idle_tool_refresh_worker() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = notification_fixture_config(&directory, 0, "new");
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    server.connect().await.expect("connect fixture");
    let peer = server
        .service
        .lock()
        .await
        .as_ref()
        .expect("service")
        .peer()
        .clone();
    server.schedule_tool_refresh(peer);
    wait_for_list_count(&directory, 2).await;
    let worker = server
        .tool_refresh
        .lock()
        .expect("tool refresh")
        .worker
        .as_ref()
        .expect("worker")
        .abort_handle();

    drop(server);
    drop(registry);
    tokio::time::timeout(WAIT, async {
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tool refresh worker stopped");
}

#[test]
fn stale_generation_created_during_publish_cannot_overwrite_newer_tools() {
    let registry = registry(McpServerSource::UserFile);
    let server = registry.server("fixture").expect("server");
    server.list_generation.store(1, Ordering::Release);
    server.publish_tools_before_commit(
        1,
        vec![Tool::new("old", "old", Arc::new(Map::new()))],
        || assert_eq!(server.next_list_generation(), 2),
    );
    assert!(server.tools.lock().expect("tools").is_empty());
    server.publish_tools(2, vec![Tool::new("new", "new", Arc::new(Map::new()))]);
    assert_eq!(
        server.tools.lock().expect("tools")[0].spec.name,
        "fixture_new"
    );
}

#[test]
fn identical_relist_keeps_tools_and_claims_untouched() {
    let registry = registry(McpServerSource::UserFile);
    let server = registry.server("fixture").expect("server");
    let tool = || Tool::new("same", "same", Arc::new(Map::new()));
    server.list_generation.store(1, Ordering::Release);
    server.publish_tools(1, vec![tool()]);
    let tools_pointer = server.tools.lock().expect("tools").as_ptr();
    registry
        .inner
        .claimed_names
        .lock()
        .expect("claimed names")
        .insert("fixture_same".into(), "sentinel".into());

    server.list_generation.store(2, Ordering::Release);
    server.publish_tools(2, vec![tool()]);

    assert_eq!(server.tools.lock().expect("tools").as_ptr(), tools_pointer);
    assert_eq!(
        registry
            .inner
            .claimed_names
            .lock()
            .expect("claimed names")
            .get("fixture_same")
            .map(String::as_str),
        Some("sentinel")
    );
}

#[tokio::test]
async fn removing_server_cancels_pending_tool_list_refresh() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = notification_fixture_config(&directory, 1, "new");
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    server.connect().await.expect("connect fixture");
    tokio::time::timeout(WAIT, async {
        loop {
            if server
                .tool_refresh
                .lock()
                .expect("tool refresh")
                .worker
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending tool refresh");

    registry
        .remove_server("fixture")
        .await
        .expect("remove fixture");
    tokio::time::sleep(TOOL_LIST_DEBOUNCE + Duration::from_millis(250)).await;

    assert_eq!(list_count(&directory), 1);
    assert_eq!(server.tools.lock().expect("tools")[0].raw_name, "old");
    registry.shutdown().await;
}

#[test]
fn stale_refresh_failure_cannot_invalidate_newer_publication() {
    let registry = registry(McpServerSource::UserFile);
    let server = registry.server("fixture").expect("server");
    server.set_status(McpServerState::Connected, None);
    server.list_generation.store(1, Ordering::Release);
    server.publish_refresh_failure_before_commit(1, "old refresh failed".into(), || {
        let newer = server.next_list_generation();
        assert_eq!(newer, 2);
        server.publish_tools(newer, vec![Tool::new("new", "new", Arc::new(Map::new()))]);
    });
    assert_eq!(
        server.tools.lock().expect("tools")[0].spec.name,
        "fixture_new"
    );
    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::Connected);
    assert_eq!(status.message, None);
    assert_eq!(status.tools, ["fixture_new"]);
}

#[test]
fn collisions_fail_the_named_server() {
    let registry = registry(McpServerSource::UserFile);
    let server = registry.server("fixture").expect("server");
    registry
        .inner
        .reserved_names
        .lock()
        .expect("reserved names")
        .insert("fixture_read".into());
    server.list_generation.store(1, Ordering::Release);
    server.publish_tools(1, vec![Tool::new("read", "read", Arc::new(Map::new()))]);
    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::Failed);
    assert!(
        status
            .message
            .expect("collision message")
            .contains("fixture")
    );
}

#[tokio::test]
async fn eager_readiness_waits_for_initial_tool_listing() {
    let mut config = fixture_config();
    config
        .env
        .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "100".into());
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    registry.start_eager(&tokio::runtime::Handle::current());
    assert!(
        registry
            .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
            .expect("tools before readiness")
            .is_empty()
    );
    registry.await_eager_ready().await;
    let tools = registry
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .expect("tools after readiness");
    assert_eq!(tools.len(), 2);
    registry.shutdown().await;
}

#[tokio::test]
async fn shutdown_aborts_inflight_connect_and_blocks_late_installation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut config = fixture_config();
    config
        .env
        .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "10000".into());
    let pid_file = directory.path().join("fixture.pid");
    config.env.insert(
        "MCP_FIXTURE_PID_FILE".into(),
        pid_file.to_string_lossy().into_owned(),
    );
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let server = registry.server("fixture").expect("server");
    registry.start_eager(&tokio::runtime::Handle::current());
    tokio::time::timeout(WAIT, async {
        while server.current_state() != McpServerState::Connecting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection started");
    tokio::time::timeout(WAIT, async {
        while std::fs::read_to_string(&pid_file)
            .map(|pid| pid.trim().is_empty())
            .unwrap_or(true)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture process started");
    let fixture_pid = std::fs::read_to_string(&pid_file)
        .expect("fixture PID")
        .trim()
        .to_owned();
    registry.shutdown().await;
    assert!(server.service.lock().await.is_none());
    assert_ne!(server.current_state(), McpServerState::Connected);
    assert_eq!(registry.inner.active_connects.load(Ordering::Acquire), 0);
    #[cfg(target_os = "linux")]
    let reaped = tokio::time::timeout(WAIT, async {
        let process = format!("/proc/{fixture_pid}");
        while std::path::Path::new(&process).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    #[cfg(target_os = "linux")]
    assert!(
        reaped.is_ok(),
        "fixture process was not reaped: {}",
        std::fs::read_to_string(format!("/proc/{fixture_pid}/stat"))
            .unwrap_or_else(|error| error.to_string())
    );
    #[cfg(not(target_os = "linux"))]
    let _ = fixture_pid;
}

#[tokio::test]
async fn replacing_an_inflight_connect_cannot_publish_stale_tools() {
    let mut delayed = fixture_config();
    delayed
        .env
        .insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "10000".into());
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: delayed,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let old = registry.server("fixture").expect("old server");
    registry.start_eager(&tokio::runtime::Handle::current());
    tokio::time::timeout(WAIT, async {
        while old.current_state() != McpServerState::Connecting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old connection started");

    registry
        .upsert_server(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::Runtime,
                config: fixture_config(),
            },
        )
        .await
        .expect("replace server");
    let replacement = registry.server("fixture").expect("replacement server");
    replacement.connect().await.expect("connect replacement");
    tokio::time::timeout(WAIT, async {
        while registry.inner.active_connects.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old connection cancelled");

    assert!(old.tools.lock().expect("old tools").is_empty());
    assert_eq!(replacement.current_state(), McpServerState::Connected);
    assert_eq!(registry.statuses()[0].tools.len(), 2);
    registry.shutdown().await;
}

#[tokio::test]
async fn reconnect_failure_transitions_server_to_failed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut config = fixture_config();
    config.command = Some(
        directory
            .path()
            .join("missing-server")
            .display()
            .to_string(),
    );
    config.args.clear();
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");

    registry
        .reconnect_server("fixture")
        .await
        .expect_err("reconnect must fail");

    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::Failed);
    let message = status.message.as_ref().expect("MCP startup diagnostic");
    assert!(message.contains("fixture"));
    assert!(message.contains("missing-server"));
    assert!(
        status
            .message
            .is_some_and(|message| message.contains("failed"))
    );
    registry.shutdown().await;
}

#[test]
fn sanitized_server_name_collisions_are_rejected_at_startup() {
    let servers = BTreeMap::from([
        (
            "git hub".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: fixture_config(),
            },
        ),
        (
            "git/hub".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: fixture_config(),
            },
        ),
    ]);
    let error = McpRegistry::new(servers, McpRegistryOptions::default())
        .expect_err("sanitized server collision");
    assert!(error.to_string().contains("git/hub"));
    assert!(error.to_string().contains("git hub"));
}

fn tool_names(registry: &McpRegistry) -> Vec<String> {
    registry
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .expect("MCP tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

async fn wait_for_state(registry: &McpRegistry, expected: McpServerState) {
    tokio::time::timeout(WAIT, async {
        while registry.statuses()[0].state != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "server did not reach {expected:?}: {:?}",
            registry.statuses()[0]
        )
    });
}

/// A connected fixture whose restarts fail while `refuse` exists.
async fn droppable_registry(
    refuse: &std::path::Path,
    policy: cookie_agent_runtime_api::reconnect::ReconnectPolicy,
) -> McpRegistry {
    let mut config = fixture_config();
    config.env.insert(
        "MCP_FIXTURE_REFUSE_FILE".into(),
        refuse.to_string_lossy().into_owned(),
    );
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    registry.set_reconnect_policy(policy);
    registry
        .server("fixture")
        .expect("fixture")
        .connect()
        .await
        .expect("connect fixture");
    registry
}

#[tokio::test]
async fn a_dropped_server_keeps_its_tools_while_reconnecting_and_restores_them() {
    let directory = tempfile::tempdir().expect("tempdir");
    let refuse = directory.path().join("refuse");
    let registry = droppable_registry(
        &refuse,
        cookie_agent_runtime_api::reconnect::ReconnectPolicy::with_delays(
            vec![Duration::from_millis(5); 10_000],
        ),
    )
    .await;
    let published = tool_names(&registry);
    assert_eq!(published.len(), 2);

    std::fs::write(&refuse, "").expect("refuse restarts");
    let error = try_execute(&registry, "fixture_echo_text", json!({"text":"exit"}))
        .await
        .expect_err("the server exits during the call");
    assert!(
        matches!(error, crate::ToolError::RuntimeUnavailable(_)),
        "{error:?}"
    );
    wait_for_state(&registry, McpServerState::Reconnecting).await;
    assert_eq!(tool_names(&registry), published);
    let error = try_execute(
        &registry,
        "fixture_echo_text",
        json!({"text":"while reconnecting"}),
    )
    .await
    .expect_err("calls fail fast while reconnecting");
    assert!(
        matches!(error, crate::ToolError::RuntimeUnavailable(_)),
        "{error:?}"
    );

    std::fs::remove_file(&refuse).expect("allow restarts");
    wait_for_state(&registry, McpServerState::Connected).await;
    assert_eq!(tool_names(&registry), published);
    let result = execute(&registry, "fixture_echo_text", json!({"text":"back"})).await;
    assert_eq!(result.output, "back");
    registry.shutdown().await;
}

#[tokio::test]
async fn a_server_that_cannot_reconnect_drops_its_tools_and_reports_why() {
    let directory = tempfile::tempdir().expect("tempdir");
    let refuse = directory.path().join("refuse");
    let registry = droppable_registry(
        &refuse,
        cookie_agent_runtime_api::reconnect::ReconnectPolicy::with_delays(
            vec![Duration::from_millis(1); 2],
        ),
    )
    .await;
    let mut failures = registry.subscribe_failures();
    std::fs::write(&refuse, "").expect("refuse restarts");
    try_execute(&registry, "fixture_echo_text", json!({"text":"exit"}))
        .await
        .expect_err("the server exits during the call");

    let McpServerFailure { server, reason } = tokio::time::timeout(WAIT, failures.recv())
        .await
        .expect("failure reported")
        .expect("failure channel open");
    assert_eq!(server, "fixture");
    assert!(
        reason.contains("did not reconnect after 2 attempts"),
        "{reason}"
    );
    let status = registry.statuses().remove(0);
    assert_eq!(status.state, McpServerState::Failed);
    assert_eq!(status.message.as_deref(), Some(reason.as_str()));
    assert!(tool_names(&registry).is_empty());
    registry.shutdown().await;
}

#[tokio::test]
async fn a_failed_server_is_retried_when_asked_and_reports_the_failure() {
    let directory = tempfile::tempdir().expect("tempdir");
    let refuse = directory.path().join("refuse");
    std::fs::write(&refuse, "").expect("refuse starts");
    let mut config = fixture_config();
    config.env.insert(
        "MCP_FIXTURE_REFUSE_FILE".into(),
        refuse.to_string_lossy().into_owned(),
    );
    let registry = McpRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config,
            },
        )]),
        McpRegistryOptions::default(),
    )
    .expect("registry");
    let mut failures = registry.subscribe_failures();
    registry.start_eager(&tokio::runtime::Handle::current());
    registry.await_eager_ready().await;
    assert_eq!(registry.statuses()[0].state, McpServerState::Failed);
    let failure = failures.try_recv().expect("startup failure reported");
    assert_eq!(failure.server, "fixture");
    assert!(tool_names(&registry).is_empty());

    std::fs::remove_file(&refuse).expect("allow starts");
    registry.retry_failed(&tokio::runtime::Handle::current());
    assert_eq!(registry.statuses()[0].state, McpServerState::Connecting);
    registry.await_eager_ready().await;
    assert_eq!(registry.statuses()[0].state, McpServerState::Connected);
    assert_eq!(tool_names(&registry).len(), 2);
    registry.shutdown().await;
}
