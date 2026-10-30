//! Delegating to runtimes (`docs/site/specs/runtime-delegation.md`) and the
//! registry remembering where runtimes ran (`docs/site/specs/memory.md`
//! section 16.8).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, MachineAssignment, MachineName, PermissionAction,
    PermissionEffect, RunSelection, RunStartParams, RuntimeId, RuntimeLifetime, SessionId,
    SessionStatus,
};

use super::support::*;
use crate::{
    SessionRuntimes,
    runtime::runtime_delegation::CapabilitiesParam,
    runtimes::{RuntimeListingState, StartError, StartedRuntime},
};

const PRIMARY: &str = "---\ndescription: Orchestrator\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    sub-*: allow\n---\nOrchestrator prompt.\n";
const WORKER: &str = "---\ndescription: Remote worker\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nWorker prompt.\n";

/// A fixture with the real base layer, so `delegate_subagent` is the one the
/// daemon publishes.
fn fixture_with(endpoint: &str, primary: &str) -> (Fixture, RunSelection) {
    let (fixture, selection) = custom_fixture_with_capabilities_and_worker_name(
        endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        4_096,
        Some(WORKER),
        "openai-chat",
        None,
        None,
        None,
        "sub-worker",
    );
    fixture.engine.register_base_layer().expect("base layer");
    (fixture, selection)
}

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

/// A root session with only `global` attached.
async fn global_only(fixture: &Fixture, selection: &RunSelection) -> SessionId {
    fixture
        .engine
        .create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            SessionRuntimes {
                runtimes: Some(Vec::new()),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect("global-only session")
        .session_id
}

async fn run(fixture: &Fixture, session: SessionId, selection: RunSelection, content: &str) {
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(format!("run-{session}")).unwrap(),
                selection,
                content: content.into(),
            },
            origin(),
        )
        .await
        .expect("run");
    await_projection(
        &fixture.engine,
        session,
        "root run completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
}

fn send(
    responses: &tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>,
    script: Vec<MatchedScriptedResponse>,
) {
    for response in script {
        responses.send(response).expect("scripted response");
    }
}

/// The tool named `name` in a captured chat request.
fn request_tool(request: &str, name: &str) -> Option<serde_json::Value> {
    request_body(request)["tools"]
        .as_array()?
        .iter()
        .find(|tool| tool["function"]["name"] == name)
        .map(|tool| tool["function"].clone())
}

/// The text of every tool message of a captured chat request.
fn tool_results(request: &str) -> Vec<String> {
    request_body(request)["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].to_string())
        .collect()
}

fn attachment_names(fixture: &Fixture, session: SessionId) -> Vec<String> {
    fixture
        .engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .map(|attachment| attachment.name.into_string())
        .collect()
}

#[tokio::test]
async fn a_global_only_root_delegates_to_the_runtimes_it_names() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    let (fixture, selection) = fixture_with(&endpoint, PRIMARY);
    let api = fixture._directory.path().join("api");
    create_private_test_dir(&api);
    let web = fixture._directory.path().join("web");
    create_private_test_dir(&web);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            Arc::new(|_| Vec::new()),
        ))
        .expect("register labpc");
    let capabilities = serde_json::json!([
        {"capability": "labpc/coding", "workspace": api},
        {"capability": "labpc/coding", "workspace": web},
    ]);
    send(
        &responses,
        vec![
            MatchedScriptedResponse::last_message_contains(
                "work on labpc",
                scripted_tool_batch_body(&[
                    (
                        "first-call",
                        "delegate_subagent",
                        serde_json::json!({
                            "agent_type": "sub-worker",
                            "description": "First remote child",
                            "prompt": "first remote task",
                            "capabilities": capabilities,
                        }),
                    ),
                    (
                        "second-call",
                        "delegate_subagent",
                        serde_json::json!({
                            "agent_type": "sub-worker",
                            "description": "Second remote child",
                            "prompt": "second remote task",
                            "capabilities": [{"capability": "labpc/coding", "workspace": api}],
                        }),
                    ),
                ]),
            ),
            MatchedScriptedResponse::last_message_contains(
                "first remote task",
                scripted_text_body("first report"),
            ),
            MatchedScriptedResponse::last_message_contains(
                "second remote task",
                scripted_text_body("second report"),
            ),
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("root done")),
        ],
    );
    let root = global_only(&fixture, &selection).await;
    assert_eq!(
        fixture.engine.delegate_capabilities_param(root).unwrap(),
        CapabilitiesParam::Required
    );
    run(&fixture, root, selection, "work on labpc").await;
    let requests = with_watchdog("captured requests", captured).await.unwrap();

    // The root saw `capabilities` as required, `list_runtimes`, and the
    // paragraph for a root with no workspace.
    let root_request = requests
        .iter()
        .find(|request| request.contains("work on labpc") && !request.contains("remote task"))
        .expect("root request");
    let delegate = request_tool(root_request, "delegate_subagent").expect("delegate tool");
    assert!(
        delegate["parameters"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("capabilities")),
        "{delegate}"
    );
    assert!(request_tool(root_request, "list_runtimes").is_some());
    assert!(
        root_request
            .contains("No machine is attached: you have no files or shell of your own yet.")
    );

    // Each child got `global`, then the listed capabilities in order, under
    // the usual names, and no `capabilities` of its own.
    let children = fixture.engine.children(root).unwrap();
    assert_eq!(children.len(), 2);
    let mut child_names = children
        .iter()
        .map(|child| attachment_names(&fixture, child.session_id))
        .collect::<Vec<_>>();
    child_names.sort();
    assert_eq!(
        child_names,
        [
            vec!["global", "labpc_api"],
            vec!["global", "labpc_api", "labpc_web"]
        ]
    );
    for child in &children {
        assert_eq!(
            fixture
                .engine
                .delegate_capabilities_param(child.session_id)
                .unwrap(),
            CapabilitiesParam::Absent
        );
    }
    // The root keeps only `global`, and records where its children work,
    // once per place.
    assert_eq!(attachment_names(&fixture, root), ["global"]);
    let projection = fixture.engine.inner.store.get(root).unwrap();
    let mut delegated = projection
        .meta
        .delegated_attachments
        .iter()
        .map(|attachment| attachment.workspace.clone().unwrap())
        .collect::<Vec<_>>();
    delegated.sort();
    // The runtime records each workspace in its canonical spelling, which on
    // Windows is a drive path with backslashes.
    let mut expected = [&api, &web].map(|workspace| {
        cookie_agent_protocol::paths::canonicalize(workspace)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    });
    expected.sort();
    assert_eq!(delegated, expected);
    let recorded = projection
        .log
        .event_snapshot()
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::DelegatedAttachmentsAdded { .. }
            )
        })
        .count();
    assert!((1..=2).contains(&recorded), "{recorded}");
    // The metadata the listing reads carries them too.
    let listed = fixture.engine.get_session(root).unwrap();
    assert_eq!(listed.delegated_attachments.len(), 2);

    // A grandchild would inherit its parent's attachments, with `global`
    // as the root has it now.
    let child = children
        .iter()
        .find(|child| attachment_names(&fixture, child.session_id).len() == 3)
        .unwrap()
        .session_id;
    let child_projection = fixture.engine.inner.store.get(child).unwrap();
    let names = |attachments: Vec<cookie_agent_protocol::RuntimeAttachment>| {
        attachments
            .into_iter()
            .map(|attachment| attachment.name.into_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(
            fixture
                .engine
                .inherited_attachments(&child_projection)
                .unwrap()
        ),
        ["global", "labpc_api", "labpc_web"]
    );
    fixture
        .engine
        .detach_runtime(
            root,
            cookie_agent_protocol::AttachmentName::new("global").unwrap(),
            origin(),
        )
        .await
        .expect("detach global");
    assert_eq!(
        names(
            fixture
                .engine
                .inherited_attachments(&child_projection)
                .unwrap()
        ),
        ["labpc_api", "labpc_web"]
    );
    // The root's detach leaves what its children brought.
    assert_eq!(
        fixture
            .engine
            .get_session(root)
            .unwrap()
            .delegated_attachments
            .len(),
        2
    );
    fixture.engine.shutdown().await;
}

/// A spawned runtime that never connects.
struct Unreachable;

#[async_trait]
impl crate::RuntimeTemplate for Unreachable {
    fn runtime_id(&self) -> &str {
        "offline"
    }

    async fn capabilities(
        &self,
    ) -> Result<Vec<cookie_agent_protocol::RuntimeCapabilityInfo>, String> {
        Err("ssh: connect to host offline: Connection refused".into())
    }

    fn listing(&self) -> RuntimeListingState {
        RuntimeListingState::default()
    }

    async fn start(
        &self,
        _capability: &cookie_agent_protocol::CapabilityName,
        _workspace: Option<&std::path::Path>,
    ) -> Result<StartedRuntime, StartError> {
        Err(StartError::Connect(
            "ssh: connect to host offline: Connection refused".into(),
        ))
    }
}

#[tokio::test]
async fn capabilities_that_cannot_be_given_fail_the_call_and_create_nothing() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = fixture_with(&endpoint, PRIMARY);
    let api = fixture._directory.path().join("api");
    create_private_test_dir(&api);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            Arc::new(|_| Vec::new()),
        ))
        .expect("register labpc");
    fixture
        .engine
        .register_runtime_template(Arc::new(Unreachable))
        .expect("register offline");
    let call = |id: &str, extra: serde_json::Value| {
        let mut arguments = serde_json::json!({
            "agent_type": "sub-worker",
            "description": id,
            "prompt": format!("task {id}"),
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        (id.to_owned(), arguments)
    };
    let calls = [
        call("missing", serde_json::json!({})),
        call(
            "unknown-runtime",
            serde_json::json!({"capabilities": [{"capability": "nowhere/coding", "workspace": "/x"}]}),
        ),
        call(
            "unknown-capability",
            serde_json::json!({"capabilities": [{"capability": "labpc/shell"}]}),
        ),
        call(
            "global",
            serde_json::json!({"capabilities": [{"capability": "global/global"}]}),
        ),
        call(
            "no-workspace",
            serde_json::json!({"capabilities": [{"capability": "labpc/coding"}]}),
        ),
        call(
            "resumed",
            serde_json::json!({
                "capabilities": [{"capability": "labpc/coding", "workspace": api}],
                "resume_session_id": "sub-worker_1a2b3c4d",
            }),
        ),
        call(
            "unreachable",
            serde_json::json!({"capabilities": [
                {"capability": "labpc/coding", "workspace": api},
                {"capability": "offline/coding", "workspace": "/srv"},
            ]}),
        ),
    ];
    let batch = calls
        .iter()
        .map(|(id, arguments)| (id.as_str(), "delegate_subagent", arguments.clone()))
        .collect::<Vec<_>>();
    send(
        &responses,
        vec![
            MatchedScriptedResponse::last_message_contains(
                "try everything",
                scripted_tool_batch_body(&batch),
            ),
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("root done")),
        ],
    );
    let root = global_only(&fixture, &selection).await;
    run(&fixture, root, selection, "try everything").await;
    let requests = with_watchdog("captured requests", captured).await.unwrap();
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), calls.len(), "{results:?}");
    for expected in [
        "capabilities is required",
        "there is no runtime `nowhere`",
        "runtime `labpc` has no capability `shell`; it offers `labpc/coding`",
        "`global` follows this session's root",
        "`labpc/coding` takes a workspace",
        "cannot be combined with resume_session_id",
        "`offline/coding` could not be opened at `/srv`",
    ] {
        assert!(
            results.iter().any(|result| result.contains(expected)),
            "{expected}: {results:?}"
        );
    }
    assert!(fixture.engine.children(root).unwrap().is_empty());
    let projection = fixture.engine.inner.store.get(root).unwrap();
    assert!(projection.meta.delegated_attachments.is_empty());
    assert!(
        !projection
            .log
            .event_snapshot()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::DelegationReserved { .. }))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn capabilities_are_optional_beside_a_workspace_and_absent_without_global() {
    let (fixture, selection) = fixture_with("http://127.0.0.1:9/v1", PRIMARY);
    let default = fixture.engine.create_session(selection.clone()).unwrap();
    assert_eq!(
        fixture
            .engine
            .delegate_capabilities_param(default.session_id)
            .unwrap(),
        CapabilitiesParam::Optional
    );
    let without_global = fixture
        .engine
        .create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            SessionRuntimes {
                runtimes: None,
                global: false,
                cwd: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .engine
            .delegate_capabilities_param(without_global.session_id)
            .unwrap(),
        CapabilitiesParam::Absent
    );
    let root = global_only(&fixture, &selection).await;
    assert_eq!(
        fixture.engine.delegate_capabilities_param(root).unwrap(),
        CapabilitiesParam::Required
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn delegate_at_runtime_rules_check_each_runtime_a_call_names() {
    let primary = "---\ndescription: Orchestrator\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate: allow\n  delegate@labpc: deny\n---\nOrchestrator prompt.\n";
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = fixture_with(&endpoint, primary);
    let api = fixture._directory.path().join("api");
    create_private_test_dir(&api);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            Arc::new(|_| Vec::new()),
        ))
        .expect("register labpc");
    send(
        &responses,
        vec![
            MatchedScriptedResponse::last_message_contains(
                "delegate twice",
                scripted_tool_batch_body(&[
                    (
                        "denied",
                        "delegate_subagent",
                        serde_json::json!({
                            "agent_type": "sub-worker",
                            "description": "Denied child",
                            "prompt": "denied task",
                            "capabilities": [
                                {"capability": "local/coding", "workspace": api},
                                {"capability": "labpc/coding", "workspace": api},
                            ],
                        }),
                    ),
                    (
                        "allowed",
                        "delegate_subagent",
                        serde_json::json!({
                            "agent_type": "sub-worker",
                            "description": "Allowed child",
                            "prompt": "allowed task",
                            "capabilities": [{"capability": "local/coding", "workspace": api}],
                        }),
                    ),
                ]),
            ),
            MatchedScriptedResponse::last_message_contains(
                "allowed task",
                scripted_text_body("allowed report"),
            ),
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("root done")),
        ],
    );
    let root = global_only(&fixture, &selection).await;
    run(&fixture, root, selection, "delegate twice").await;
    let requests = with_watchdog("captured requests", captured).await.unwrap();
    let results = tool_results(requests.last().unwrap());
    assert!(
        results
            .iter()
            .any(|result| result.contains("permission denied")),
        "{results:?}"
    );
    let children = fixture.engine.children(root).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(
        attachment_names(&fixture, children[0].session_id),
        ["global", "local_api"]
    );
    fixture.engine.shutdown().await;
}

#[test]
fn a_call_on_several_runtimes_takes_the_strictest_decision() {
    use crate::permissions::{PermissionPipeline, PermissionScope};

    let mut agent = crate::test_support::agent_snapshot(
        "orchestrator",
        cookie_agent_protocol::AgentMode::Primary,
    );
    agent.permissions = vec![
        cookie_agent_protocol::PermissionRule::new(
            PermissionAction::DELEGATE,
            cookie_agent_protocol::WildcardPattern::new("*").unwrap(),
            PermissionEffect::Allow,
        ),
        cookie_agent_protocol::PermissionRule {
            runtime: Some(RuntimeId::new("labpc").unwrap()),
            ..cookie_agent_protocol::PermissionRule::new(
                PermissionAction::DELEGATE,
                cookie_agent_protocol::WildcardPattern::new("*").unwrap(),
                PermissionEffect::Ask,
            )
        },
    ];
    let resource = cookie_agent_runtime_api::authoring::prepared_resource(
        PermissionAction::DELEGATE,
        "agent",
        b"worker",
        b"worker",
        cookie_agent_protocol::PreparedBindingLifetime::RestartStable,
        cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
    )
    .unwrap();
    let operation = cookie_agent_runtime_api::authoring::prepared_operation(
        "delegate_subagent",
        &serde_json::json!({}),
        vec![(PermissionAction::DELEGATE, "spawn")],
        vec![resource],
        b"",
    )
    .unwrap();
    let labels = [Some("worker".to_owned())];
    let pipeline = PermissionPipeline::default();
    let decide = |runtime: &str| {
        pipeline.decide_operation(
            &agent,
            &operation,
            &labels,
            PermissionScope::runtime(runtime, None),
        )
    };
    assert_eq!(decide("local").effect, PermissionEffect::Allow);
    assert_eq!(decide("labpc").effect, PermissionEffect::Ask);
    let combined =
        crate::runtime::tool_execution::strictest_decision([decide("local"), decide("labpc")]);
    assert_eq!(combined.effect, PermissionEffect::Ask);
    assert_eq!(combined.evaluations.len(), 1);
    // A plain `delegate` check, with no runtime, sees only the plain rule.
    assert_eq!(
        pipeline
            .decide_operation(&agent, &operation, &labels, PermissionScope::BASE)
            .effect,
        PermissionEffect::Allow
    );
}

/// A spawned or dial-in runtime that is connected while `connected` says.
/// While connected it reports a machine of its own named after it; while
/// not, it reports nothing, as after a daemon restart.
struct Switchable {
    id: &'static str,
    lifetime: RuntimeLifetime,
    connected: AtomicBool,
    message: Option<&'static str>,
}

impl Switchable {
    fn new(id: &'static str, lifetime: RuntimeLifetime) -> Arc<Self> {
        Arc::new(Self {
            id,
            lifetime,
            connected: AtomicBool::new(true),
            message: None,
        })
    }
}

#[async_trait]
impl crate::RuntimeTemplate for Switchable {
    fn runtime_id(&self) -> &str {
        self.id
    }

    fn lifetime(&self) -> RuntimeLifetime {
        self.lifetime
    }

    async fn capabilities(
        &self,
    ) -> Result<Vec<cookie_agent_protocol::RuntimeCapabilityInfo>, String> {
        Ok(self.listing().capabilities)
    }

    fn listing(&self) -> RuntimeListingState {
        if !self.connected.load(Ordering::SeqCst) {
            return RuntimeListingState {
                message: self.message.map(str::to_owned),
                ..RuntimeListingState::default()
            };
        }
        RuntimeListingState {
            connected: true,
            capabilities: vec![crate::runtimes::coding_capability()],
            platform: Some("linux".into()),
            machine: Some(cookie_agent_protocol::runtime_protocol::RuntimeMachine {
                id: format!("machine-of-{}", self.id),
                host_name: Some(self.id.to_owned()),
            }),
            home: Some(format!("/home/{}", self.id)),
            message: None,
        }
    }

    async fn start(
        &self,
        _capability: &cookie_agent_protocol::CapabilityName,
        _workspace: Option<&std::path::Path>,
    ) -> Result<StartedRuntime, StartError> {
        Err(StartError::Connect("not in this test".into()))
    }
}

#[tokio::test]
async fn an_offline_runtime_keeps_the_machine_it_was_last_seen_on() {
    let (fixture, _selection) = fixture_with("http://127.0.0.1:9/v1", PRIMARY);
    let labpc = Switchable::new("labpc", RuntimeLifetime::Spawned);
    let roamer = Switchable::new("roamer", RuntimeLifetime::DialIn);
    for template in [&labpc, &roamer] {
        fixture
            .engine
            .register_runtime_template(template.clone())
            .unwrap();
    }
    fixture.engine.assign_connected_runtimes();
    for template in [&labpc, &roamer] {
        template.connected.store(false, Ordering::SeqCst);
    }
    fixture.engine.assign_connected_runtimes();
    let labpc_machine = MachineName::new("labpc").unwrap();
    assert_eq!(
        fixture.engine.runtime_machine("labpc"),
        Some(labpc_machine.clone())
    );
    let remembered = fixture.engine.remembered_runtime("labpc").unwrap();
    assert_eq!(remembered.home.as_deref(), Some("/home/labpc"));
    assert_eq!(remembered.platform.as_deref(), Some("linux"));
    let machine = fixture
        .engine
        .machines()
        .into_iter()
        .find(|machine| machine.name == labpc_machine)
        .unwrap();
    assert!(machine.connected.is_empty());
    assert_eq!(machine.remembered.len(), 1);
    assert_eq!(
        machine.remembered[0].assigned_by,
        MachineAssignment::LastSeen
    );
    assert_eq!(machine.remembered[0].home.as_deref(), Some("/home/labpc"));

    // Forgetting it while it is connected is refused.
    labpc.connected.store(true, Ordering::SeqCst);
    assert!(
        fixture
            .engine
            .forget_runtime_record(RuntimeId::new("labpc").unwrap())
            .await
            .is_err()
    );
    labpc.connected.store(false, Ordering::SeqCst);
    fixture
        .engine
        .forget_runtime_record(RuntimeId::new("labpc").unwrap())
        .await
        .expect("forget labpc");
    assert_eq!(fixture.engine.remembered_runtime("labpc"), None);
    assert!(
        fixture
            .engine
            .machines()
            .iter()
            .all(|machine| machine.remembered.is_empty()
                || machine.remembered[0].runtime_id.as_str() == "roamer")
    );

    // A configuration without a template drops a spawned runtime's record
    // and keeps a dial-in's.
    labpc.connected.store(true, Ordering::SeqCst);
    fixture.engine.assign_connected_runtimes();
    labpc.connected.store(false, Ordering::SeqCst);
    fixture.engine.assign_connected_runtimes();
    assert!(fixture.engine.remembered_runtime("labpc").is_some());
    let config = fixture.engine.configuration();
    fixture
        .engine
        .drop_unconfigured_runtime_records(&config)
        .await;
    assert_eq!(fixture.engine.remembered_runtime("labpc"), None);
    assert!(fixture.engine.remembered_runtime("roamer").is_some());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn list_runtimes_prints_machines_runtimes_and_the_session_s_own_attachments() {
    let (fixture, selection) = fixture_with("http://127.0.0.1:9/v1", PRIMARY);
    let labpc = Switchable::new("labpc", RuntimeLifetime::Spawned);
    let roamer = Arc::new(Switchable {
        id: "roamer",
        lifetime: RuntimeLifetime::DialIn,
        connected: AtomicBool::new(false),
        message: Some("runtime `roamer` disconnected: the other end closed the <connection>"),
    });
    fixture
        .engine
        .register_runtime_template(labpc.clone())
        .unwrap();
    fixture.engine.register_runtime_template(roamer).unwrap();
    // `labpc` was seen once and is offline now.
    fixture.engine.assign_connected_runtimes();
    labpc.connected.store(false, Ordering::SeqCst);
    fixture.engine.assign_connected_runtimes();
    // Name the daemon's machine so the output does not depend on the host.
    let daemon = fixture.engine.runtime_machine("local").unwrap();
    fixture
        .engine
        .rename_machine(daemon, MachineName::new("daemon").unwrap())
        .await
        .unwrap();
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    let local = fixture
        .engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .unwrap();
    let home = crate::runtimes::daemon_home()
        .map(|home| format!(" home=\"{home}\""))
        .unwrap_or_default();
    let expected = format!(
        "<runtimes>\n  \
         <!-- global is attached to this session's root, so every subagent in this tree has it, whether or not its capabilities list it. -->\n  \
         <global/>\n  \
         <machine name=\"daemon\" platform=\"{os}\">\n    \
         <runtime id=\"local\" connected=\"true\"{home}>\n      \
         <capability name=\"coding\" takes_workspace=\"true\" description=\"Read, write, edit, and run commands in a workspace\"/>\n      \
         <attachment name=\"{name}\" capability=\"coding\" workspace=\"{workspace}\" status=\"ready\"/>\n    \
         </runtime>\n  \
         </machine>\n  \
         <machine name=\"labpc\" platform=\"linux\">\n    \
         <runtime id=\"labpc\" connected=\"false\" starts_on_use=\"true\" home=\"/home/labpc\"/>\n  \
         </machine>\n  \
         <!-- The daemon does not know these runtimes' machines yet: they are not connected. One is placed on a machine when it connects, as attaching or delegating to it makes it do. -->\n  \
         <not_connected_yet>\n    \
         <runtime id=\"roamer\" connected=\"false\" platform=\"\" message=\"runtime `roamer` disconnected: the other end closed the &lt;connection&gt;\"/>\n  \
         </not_connected_yet>\n\
         </runtimes>",
        os = std::env::consts::OS,
        name = local.name,
        workspace = local.workspace.clone().unwrap(),
    )
    .replace(" platform=\"\"", "");
    assert_eq!(
        fixture.engine.list_runtimes_text(session).unwrap(),
        expected
    );

    // Without `global` the comment and `<global/>` go; the runtimes stay.
    let local_name = local.name.clone();
    fixture
        .engine
        .detach_runtime(
            session,
            cookie_agent_protocol::AttachmentName::new("global").unwrap(),
            origin(),
        )
        .await
        .unwrap();
    let text = fixture.engine.list_runtimes_text(session).unwrap();
    assert!(
        !text.contains("<global/>") && !text.contains("<!-- global"),
        "{text}"
    );
    assert!(text.contains(&format!("<attachment name=\"{local_name}\"")));
    fixture.engine.shutdown().await;
}
