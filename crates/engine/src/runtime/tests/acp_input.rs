//! ACP agents' input and pickers (`docs/site/specs/acp-agents.md`
//! section 15, input and pickers): the options a draft offers before its
//! session exists and the starting options a session is created with.

use cookie_agent_protocol::{AcpOptionChoice, EventPayload};

use super::acp::*;
use crate::SessionRuntimes;

pub(super) fn choice(option_id: &str, value: &str) -> AcpOptionChoice {
    AcpOptionChoice {
        option_id: option_id.to_owned(),
        value: value.to_owned(),
    }
}

/// The options the agent's descriptor offers a draft on `local`.
pub(super) fn draft_options(acp: &AcpFixture) -> Vec<cookie_agent_protocol::AcpConfigOption> {
    acp.engine()
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id == agent_id())
        .and_then(|agent| agent.runtimes.first())
        .map(|runtime| runtime.options.clone())
        .unwrap_or_default()
}

pub(super) fn current(
    options: &[cookie_agent_protocol::AcpConfigOption],
    id: &str,
) -> Option<String> {
    options
        .iter()
        .find(|option| option.id == id)
        .map(|option| option.current_value.clone())
}

/// The `session/set_config_option` requests the agent received, as
/// `(option, value)`.
pub(super) fn options_set(acp: &AcpFixture) -> Vec<(String, String)> {
    acp.received()
        .into_iter()
        .filter(|message| message["method"] == "session/set_config_option")
        .map(|message| {
            (
                message["params"]["configId"].as_str().unwrap().to_owned(),
                message["params"]["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_offers_the_options_the_agent_last_started_a_session_with() {
    let document = "---\ndescription: Fixture\nmode: all\nacp:\n  config: { model: large }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    // Nothing is known before the agent first starts a session.
    assert!(draft_options(&acp).is_empty());
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    let options = draft_options(&acp);
    // The agent's own starting values with the document's mode rule and
    // `acp.config` applied: what a new session would start with.
    assert_eq!(current(&options, "mode").as_deref(), Some("auto"));
    assert_eq!(current(&options, "model").as_deref(), Some("large"));
    assert_eq!(
        current(&options, "reasoning_effort").as_deref(),
        Some("low")
    );
    let effort = options
        .iter()
        .find(|option| option.id == "reasoning_effort")
        .unwrap();
    assert_eq!(effort.category.as_deref(), Some("thought_level"));
    // Setting an option in the session does not change what drafts start
    // with.
    engine
        .set_session_agent_option(cookie_agent_protocol::SessionAgentOptionSetParams {
            session_id: session,
            option_id: "reasoning_effort".to_owned(),
            value: "high".to_owned(),
        })
        .await
        .expect("option set");
    assert_eq!(
        current(&draft_options(&acp), "reasoning_effort").as_deref(),
        Some("low")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_options_apply_after_the_documents_config_and_invalid_ones_are_skipped() {
    let document = "---\ndescription: Fixture\nmode: all\nacp:\n  config: { model: small }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine
        .create_session_with_agent_options(
            selection(),
            None,
            Vec::new(),
            SessionRuntimes::default(),
            None,
            vec![
                choice("model", "large"),
                choice("reasoning_effort", "high"),
                choice("reasoning_effort_x", "high"),
                choice("mode", "nonsense"),
            ],
            None,
        )
        .await
        .expect("session created")
        .session_id;
    let recorded = events(engine, session)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => update.starting_options,
            _ => None,
        })
        .expect("the starting options are recorded");
    assert_eq!(recorded.len(), 4);
    turn(engine, session, "first", "say hello").await;
    // The session's choices go after the document's `acp.config`.
    assert_eq!(
        options_set(&acp),
        [
            ("mode".to_owned(), "auto".to_owned()),
            ("model".to_owned(), "small".to_owned()),
            ("model".to_owned(), "large".to_owned()),
            ("reasoning_effort".to_owned(), "high".to_owned()),
        ]
    );
    let events = events(engine, session);
    let options = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::AcpSessionUpdated { update } => update.options.clone(),
            _ => None,
        })
        .expect("the options");
    assert_eq!(current(&options, "model").as_deref(), Some("large"));
    assert_eq!(
        current(&options, "reasoning_effort").as_deref(),
        Some("high")
    );
    let skipped = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Diagnostic { message, .. }
                if message.starts_with("The session's starting agent option was skipped") =>
            {
                Some(message.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped[0].contains("no option `reasoning_effort_x`"));
    assert!(skipped[1].contains("option `mode` takes no value `nonsense`"));
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_starting_options_create_no_session() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let before = engine.list_sessions().len();
    let refused = engine
        .create_session_with_agent_options(
            selection(),
            None,
            Vec::new(),
            SessionRuntimes::default(),
            None,
            vec![choice("model", "large"), choice("model", "small")],
            None,
        )
        .await
        .expect_err("an option chosen twice is refused");
    assert!(refused.to_string().contains("more than once"), "{refused}");
    assert_eq!(engine.list_sessions().len(), before);
}

async fn upload(
    engine: &crate::Engine,
    session: cookie_agent_protocol::SessionId,
    bytes: &[u8],
    filename: &str,
) -> cookie_agent_protocol::ArtifactRef {
    use sha2::Digest as _;
    let id = engine
        .begin_upload(1, session, bytes.len() as u64, Some(filename.into()))
        .await
        .expect("begin upload");
    engine
        .upload_chunk(1, &id, 0, bytes.to_vec())
        .await
        .expect("upload chunk");
    engine
        .finish_upload(1, &id, &format!("{:x}", sha2::Sha256::digest(bytes)))
        .await
        .expect("finish upload")
        .artifact
}

/// A WAV file's header and a few samples.
fn wav() -> Vec<u8> {
    let mut bytes = b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x04\0\0\0".to_vec();
    bytes.extend_from_slice(&[0, 0, 1, 0]);
    bytes
}

/// Runs one prompt of `text` with files `artifacts` and returns the
/// non-text blocks of the prompt the agent received.
async fn prompt_with_files(
    acp: &AcpFixture,
    session: cookie_agent_protocol::SessionId,
    text: &str,
    artifacts: Vec<cookie_agent_protocol::ArtifactRef>,
) -> Vec<serde_json::Value> {
    use cookie_agent_protocol::{Open, UserContent, UserContentPart};
    let engine = acp.engine();
    let mut parts = vec![Open::Known(UserContentPart::Text { text: text.into() })];
    parts.extend(
        artifacts
            .into_iter()
            .map(|artifact| Open::Known(UserContentPart::File { artifact })),
    );
    let run = engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("files").unwrap(),
                selection: selection(),
                content: UserContent::new(parts).unwrap(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    final_text(&finished(engine, session, run).await);
    acp.received()
        .into_iter()
        .rev()
        .find(|message| message["method"] == "session/prompt")
        .expect("a prompt")["params"]["prompt"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| {
            block["type"] != "text" || block["text"].as_str().unwrap().starts_with("[attached")
        })
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn text_and_audio_files_reach_an_agent_that_takes_them() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let notes = upload(engine, session, b"line one\n", "notes.md").await;
    assert_eq!(notes.media_type, "text/plain");
    let sound = upload(engine, session, &wav(), "hello.wav").await;
    assert_eq!(sound.media_type, "audio/wav");
    let blocks = prompt_with_files(&acp, session, "say ok", vec![notes, sound]).await;
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "resource", "resource": {
            "uri": "attachment:///notes.md", "mimeType": "text/plain", "text": "line one\n",
        }})
    );
    assert_eq!(blocks[1]["type"], "audio");
    assert_eq!(blocks[1]["mimeType"], "audio/wav");
    // The clients are told what the agent takes, once its process started.
    let kinds = engine
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id == agent_id())
        .and_then(|agent| agent.runtimes.first())
        .map(|runtime| {
            runtime
                .input_media
                .iter()
                .map(|media| media.kind.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(kinds, ["image", "audio", "text"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn files_an_agent_does_not_take_become_a_note() {
    let acp = acp_fixture(&[("ACP_FIXTURE_IMAGE_ONLY", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let notes = upload(engine, session, b"line one\n", "notes.md").await;
    let sound = upload(engine, session, &wav(), "hello.wav").await;
    let blocks = prompt_with_files(&acp, session, "say ok", vec![notes, sound]).await;
    let notes = blocks
        .iter()
        .map(|block| block["text"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        notes,
        [
            "[attached file notes.md (text/plain), which this agent cannot read]",
            "[attached file hello.wav (audio/wav), which this agent cannot read]",
        ]
    );
}

/// A session of the agent that attaches `local` at the daemon's workspace
/// and at a second folder in it, whose canonical path it returns.
async fn two_workspace_session(acp: &AcpFixture) -> (cookie_agent_protocol::SessionId, String) {
    let second = acp.workspace().join("second");
    std::fs::create_dir_all(&second).unwrap();
    let request = |workspace: std::path::PathBuf| crate::SessionRuntimeRequest {
        runtime_id: "local".to_owned(),
        capability: Some("coding".to_owned()),
        workspace: Some(workspace),
    };
    let session = acp
        .engine()
        .create_session_with_permissions(
            selection(),
            None,
            Vec::new(),
            SessionRuntimes {
                runtimes: Some(vec![request(acp.workspace()), request(second.clone())]),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect("a session with two workspaces")
        .session_id;
    let second = cookie_agent_protocol::paths::canonicalize(&second).unwrap();
    (session, second.to_string_lossy().into_owned())
}

/// The `session/new` requests the agent received.
fn new_sessions(acp: &AcpFixture) -> Vec<serde_json::Value> {
    acp.received()
        .into_iter()
        .filter(|message| message["method"] == "session/new")
        .map(|message| message["params"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sessions_other_workspaces_are_the_agents_additional_directories() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let (session, second) = two_workspace_session(&acp).await;
    turn(engine, session, "first", "say hi").await;
    let new = new_sessions(&acp).pop().expect("session/new");
    let cwd = cookie_agent_protocol::paths::canonicalize(&acp.workspace()).unwrap();
    assert_eq!(new["cwd"], cwd.to_string_lossy().as_ref());
    assert_eq!(new["additionalDirectories"], serde_json::json!([second]));
    // A resume names them too.
    super::acp::stop_process(engine).await;
    turn(engine, session, "second", "say again").await;
    let resume = acp
        .received()
        .into_iter()
        .rev()
        .find(|message| message["method"] == "session/resume")
        .expect("session/resume");
    assert_eq!(
        resume["params"]["additionalDirectories"],
        serde_json::json!([second])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_without_additional_directories_gets_only_its_cwd() {
    let acp = acp_fixture(&[("ACP_FIXTURE_IMAGE_ONLY", "1")], &[]).await;
    let engine = acp.engine();
    let (session, _) = two_workspace_session(&acp).await;
    turn(engine, session, "first", "say hi").await;
    let new = new_sessions(&acp).pop().expect("session/new");
    assert!(new.get("additionalDirectories").is_none(), "{new}");
}

#[tokio::test(flavor = "multi_thread")]
async fn several_attachments_say_where_the_agents_builtin_tools_run() {
    // Every tool hidden: the attachments are listed all the same.
    let document =
        "---\ndescription: Fixture\nmode: all\nacp: {}\ntool_groups: { \"*\": off }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let (session, second) = two_workspace_session(&acp).await;
    turn(engine, session, "first", "say hi").await;
    let cwd = cookie_agent_protocol::paths::canonicalize(&acp.workspace()).unwrap();
    let cwd = cwd.to_string_lossy();
    let names = engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .filter(|attachment| attachment.runtime_id.as_str() == "local")
        .map(|attachment| attachment.name.as_str().to_owned())
        .collect::<Vec<_>>();
    let [first, other] = &names[..] else {
        panic!("two attachments on `local`: {names:?}");
    };
    let machine = engine
        .runtime_machine("local")
        .expect("the daemon's machine");
    let received = prompts(&acp);
    // One machine holds both workspaces: it is marked, and the note names
    // the agent's working directory; nothing names attachments any more.
    let block = &received[0][0];
    let _ = (first, other, second);
    assert!(
        block.contains(&format!("<machine name=\"{machine}\""))
            && block.contains("builtin_tools=\"true\""),
        "{block}"
    );
    let note = block.rsplit("</machines>\n").next().unwrap();
    assert!(note.contains(&*cwd), "{note}");
    assert!(!block.contains("<attachment>_"), "{block}");
    assert_eq!(
        received[0][1],
        crate::runtime::prompt_blocks::attachments_block(true)
    );
    assert!(
        received[0][2].starts_with("<system-reminder>\n# Memory\n"),
        "{received:?}"
    );
    assert_eq!(received[0][3..], ["say hi"]);
    // A resumed ACP session already knows.
    super::acp::stop_process(engine).await;
    turn(engine, session, "second", "say again").await;
    assert_eq!(
        session_starts(engine, session).pop().unwrap().how,
        cookie_agent_protocol::AcpSessionStart::Resumed
    );
    assert_eq!(prompts(&acp)[1], ["say again"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompt_names_the_sessions_project() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let project = engine
        .create_project(cookie_agent_protocol::ProjectCreateParams {
            name: cookie_agent_protocol::ProjectName::new("web").unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap();
    engine
        .set_session_project(session, Some(project.id))
        .await
        .unwrap();
    turn(engine, session, "first", "say hi").await;
    let cwd = cookie_agent_protocol::paths::canonicalize(&acp.workspace()).unwrap();
    let machine = engine
        .runtime_machine("local")
        .expect("the daemon's machine");
    let first = &prompts(&acp)[0];
    let _ = (cwd, machine);
    assert!(
        first
            .iter()
            .any(|part| part.contains("<machines project=\"web\">")),
        "{first:#?}"
    );
}

/// A session of the agent whose `local` workspace holds the project skill
/// `review`.
async fn session_with_a_skill(acp: &AcpFixture) -> cookie_agent_protocol::SessionId {
    let project = acp.workspace().join("project");
    let skill = project.join(".cookie-agent/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review a change\n---\nRead the diff.\n",
    )
    .unwrap();
    acp.engine()
        .create_session_with_permissions(
            selection(),
            None,
            Vec::new(),
            SessionRuntimes {
                runtimes: Some(vec![crate::SessionRuntimeRequest {
                    runtime_id: "local".to_owned(),
                    capability: Some("coding".to_owned()),
                    workspace: Some(project),
                }]),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect("a session with a skill")
        .session_id
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompt_lists_the_skills_when_the_skill_tool_is_offered() {
    let document = "---\ndescription: Fixture\nmode: all\nacp: {}\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = session_with_a_skill(&acp).await;
    turn(engine, session, "first", "say hi").await;
    turn(engine, session, "second", "say again").await;
    let prompts = prompts(&acp);
    let listing = prompts[0]
        .iter()
        .find(|part| part.starts_with("<available_skills>"))
        .expect("the skill listing");
    assert!(listing.contains("<name>review</name>"), "{listing}");
    assert!(listing.contains("Review a change"), "{listing}");
    // It follows the tool instructions, and only the first prompt has it.
    let tools = prompts[0]
        .iter()
        .position(|part| part.starts_with("<tool_instructions"));
    let skills = prompts[0]
        .iter()
        .position(|part| part.starts_with("<available_skills>"));
    assert!(
        tools.is_none_or(|tools| Some(tools) < skills),
        "{prompts:?}"
    );
    // The attachments come before both, the memory after.
    let attachments = prompts[0]
        .iter()
        .position(|part| part.starts_with("This session is attached to the following machines."));
    assert!(
        attachments < tools.or(skills) && attachments.is_some(),
        "{prompts:?}"
    );
    let memory = prompts[0]
        .iter()
        .position(|part| part.starts_with("<system-reminder>\n# Memory"));
    assert!(
        memory.is_none_or(|memory| skills < Some(memory)),
        "{prompts:?}"
    );
    assert!(
        !prompts[1]
            .iter()
            .any(|part| part.starts_with("<available_skills>"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_skill_tool_lists_no_skills() {
    let document = "---\ndescription: Fixture\nmode: all\nacp: {}\ntool_groups: { coding: off }\nskills: { \"*\": off }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = session_with_a_skill(&acp).await;
    turn(engine, session, "first", "say hi").await;
    assert!(
        !prompts(&acp)[0]
            .iter()
            .any(|part| part.contains("<available_skills>"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_agent_hears_of_machines_that_came_between_its_turns_once() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hi").await;
    // Nothing changed: the next prompt is the message alone.
    turn(engine, session, "second", "say again").await;
    // A workspace attached between turns is announced ahead of the next
    // prompt, with the machines as they stand, and shown in the session.
    let other = acp.workspace().join("other");
    std::fs::create_dir_all(&other).unwrap();
    engine
        .attach_runtime(
            session,
            "local",
            Some("coding"),
            Some(other),
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    turn(engine, session, "third", "say three").await;
    turn(engine, session, "fourth", "say four").await;
    let received = prompts(&acp);
    assert_eq!(received[1], ["say again"]);
    assert_eq!(received[2].len(), 2, "{:?}", received[2]);
    assert!(
        received[2][0].starts_with("<system-reminder>\n<machines_changed>\n"),
        "{:?}",
        received[2]
    );
    assert!(
        received[2][0].contains("builtin_tools=\"true\""),
        "{:?}",
        received[2]
    );
    assert_eq!(received[2][1], "say three");
    assert_eq!(received[3], ["say four"], "told once");
    assert!(events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::Diagnostic { message, .. } if message.contains("<machines_changed>")
    )));
}
