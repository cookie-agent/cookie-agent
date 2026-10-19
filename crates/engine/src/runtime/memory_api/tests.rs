use cookie_agent_protocol::{AgentId, MachineName, MemoryLayer, MemoryListParams};

use super::{MemoryRequestError, list_scope, parse_revision, parse_url};
use crate::memory::{
    store::ListScope,
    url::{self, MemoryName},
};

fn field_of(error: MemoryRequestError) -> &'static str {
    match error {
        MemoryRequestError::InvalidParams { field, .. } => field,
        other => panic!("not invalid params: {other:?}"),
    }
}

#[test]
fn list_filters_pick_one_scope() {
    let params = |layer, agent: Option<&str>, machine: Option<&str>, project: Option<&str>| {
        MemoryListParams {
            layer,
            agent: agent.map(|agent| AgentId::new(agent).unwrap()),
            machine: machine.map(|machine| MachineName::new(machine).unwrap()),
            project: project.map(str::to_owned),
        }
    };
    assert_eq!(
        list_scope(params(None, None, None, None)).unwrap(),
        ListScope::All
    );
    assert_eq!(
        list_scope(params(Some(MemoryLayer::User), None, None, None)).unwrap(),
        ListScope::Layer(url::MemoryLayer::User)
    );
    assert_eq!(
        list_scope(params(None, Some("reviewer"), None, None)).unwrap(),
        ListScope::Agent(AgentId::new("reviewer").unwrap())
    );
    // A layer that agrees with the filter is allowed.
    assert_eq!(
        list_scope(params(
            Some(MemoryLayer::Machine),
            None,
            Some("labpc"),
            None
        ))
        .unwrap(),
        ListScope::Machine(MachineName::new("labpc").unwrap())
    );
    assert_eq!(
        list_scope(params(None, None, None, Some("cookie-agent"))).unwrap(),
        ListScope::Project(MemoryName::new("cookie-agent").unwrap())
    );

    // Two filters, a layer that disagrees, a bad project name, and an
    // unknown layer are refused, naming the member.
    assert_eq!(
        field_of(list_scope(params(None, Some("default"), Some("labpc"), None)).unwrap_err()),
        "machine"
    );
    assert_eq!(
        field_of(list_scope(params(Some(MemoryLayer::User), None, None, Some("web"))).unwrap_err()),
        "project"
    );
    assert_eq!(
        field_of(list_scope(params(None, None, None, Some("Web"))).unwrap_err()),
        "project"
    );
    assert_eq!(
        field_of(list_scope(params(Some(MemoryLayer::Unknown), None, None, None)).unwrap_err()),
        "layer"
    );
}

#[test]
fn urls_and_revisions_are_checked_before_the_store() {
    assert!(parse_url("project://web/build").is_ok());
    for url in [
        "project://web",
        "Project://web/build",
        "project://web/build.md",
        "agent://self",
        "file:///etc/passwd",
    ] {
        assert!(
            matches!(parse_url(url), Err(MemoryRequestError::InvalidUrl { .. })),
            "{url}"
        );
    }
    assert!(parse_revision("0123456789abcdef").is_ok());
    for revision in ["0123456789ABCDEF", "0123456789abcde", "0123456789abcdef0"] {
        assert_eq!(field_of(parse_revision(revision).unwrap_err()), "revision");
    }
}
