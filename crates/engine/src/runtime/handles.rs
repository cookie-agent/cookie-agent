//! Short, tree-unique model-facing subagent handles.
//!
//! A handle is `<agent_type slug (max 16)>_<8 lowercase hex>`, e.g.
//! `explore_1a2b3c4d`. It is generated at `SessionCreated` time, recorded on the
//! session's first event, and resolved only within the caller's own delegation
//! tree. See `docs/site/specs/subagent-handles.md`.

use std::collections::HashSet;

use cookie_agent_protocol::{SessionId, SessionMeta, SessionOrigin, SessionTree};

use super::{Engine, EngineError, delegation::session_status_name};

pub(crate) const HANDLE_HEX_LEN: usize = 8;
pub(crate) const HANDLE_SLUG_MAX: usize = 16;
pub(crate) const HANDLE_ATTEMPTS: usize = 16;

/// The addressable set a reference resolves against. `send_message` reaches
/// every peer in the tree; the subagent-result/cancel/resume tools only reach
/// the caller's own direct children, matching their ownership checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentScope {
    DirectChildren,
    TreePeers,
}

/// The slug portion of a handle: the agent type truncated to
/// [`HANDLE_SLUG_MAX`] characters with any trailing `-` trimmed so the result
/// still satisfies the handle grammar.
#[must_use]
pub(crate) fn handle_slug(agent_type: &str) -> String {
    let mut slug = agent_type.chars().take(HANDLE_SLUG_MAX).collect::<String>();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push('a');
    }
    slug
}

/// Grammar check for a complete handle: `^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9])){0,15}_[0-9a-f]{8}$`.
#[must_use]
pub(crate) fn is_valid_handle(value: &str) -> bool {
    let Some((slug, hex)) = value.rsplit_once('_') else {
        return false;
    };
    if hex.len() != HANDLE_HEX_LEN
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    if slug.is_empty() || slug.len() > HANDLE_SLUG_MAX {
        return false;
    }
    let bytes = slug.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || {
            *byte == b'-' && bytes.get(index + 1).is_some_and(u8::is_ascii_alphanumeric)
        }
    })
}

/// Generates a handle for `agent_type`, regenerating on collision against the
/// existing tree handles. Caps the loop at [`HANDLE_ATTEMPTS`] and fails loudly:
/// reaching the cap means the uniqueness check itself is broken.
pub(crate) fn generate_handle(
    agent_type: &str,
    existing: &HashSet<String>,
    mut next_hex: impl FnMut() -> u32,
) -> Result<String, EngineError> {
    let slug = handle_slug(agent_type);
    for _ in 0..HANDLE_ATTEMPTS {
        let candidate = format!("{slug}_{:08x}", next_hex());
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(EngineError::ToolFailed(format!(
        "subagent handle generation exhausted {HANDLE_ATTEMPTS} attempts; the tree uniqueness check is broken"
    )))
}

/// Production entropy source: four random bytes rendered as eight hex digits.
/// `RandomState` draws its keys from the OS, so no extra dependency or feature
/// flag is needed for the non-cryptographic 32-bit collision space.
pub(crate) fn random_hex() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0);
    (hasher.finish() & 0xffff_ffff) as u32
}

fn push_tree(tree: &SessionTree, out: &mut Vec<SessionMeta>) {
    out.push(tree.session.clone());
    for child in &tree.children {
        push_tree(child, out);
    }
}

fn session_handles(sessions: &[SessionMeta]) -> HashSet<String> {
    sessions
        .iter()
        .filter_map(|meta| meta.short_id.clone())
        .collect()
}

fn describe(meta: &SessionMeta) -> String {
    meta.title.as_ref().map_or_else(
        || meta.creation_selection.agent.to_string(),
        ToString::to_string,
    )
}

impl Engine {
    /// Every session in the caller's delegation tree, root first.
    fn tree_sessions(&self, session: SessionId) -> Result<Vec<SessionMeta>, EngineError> {
        let root = match self.inner.store.get(session)?.meta.origin {
            SessionOrigin::Root => session,
            SessionOrigin::Delegated {
                root_session_id, ..
            } => root_session_id,
        };
        let tree = self.tree(root)?;
        let mut sessions = Vec::new();
        push_tree(&tree, &mut sessions);
        Ok(sessions)
    }

    /// The handle recorded for one session, when it has one.
    pub(crate) fn session_short_id(&self, session: SessionId) -> Option<String> {
        self.inner
            .store
            .get(session)
            .ok()
            .and_then(|projection| projection.meta.short_id)
    }

    /// Existing handles across a whole tree, used as the generation collision
    /// set.
    pub(crate) fn existing_tree_handles(
        &self,
        root: SessionId,
    ) -> Result<HashSet<String>, EngineError> {
        let tree = self.tree(root)?;
        let mut sessions = Vec::new();
        push_tree(&tree, &mut sessions);
        Ok(session_handles(&sessions))
    }

    /// Resolves a full UUID or a complete handle within `caller`'s addressable
    /// set (its direct children, or every tree peer).
    ///
    /// There is deliberately no prefix matching: a partial handle is an error.
    /// An unresolvable reference never falls through to creating a subagent.
    pub fn resolve_subagent_target(
        &self,
        caller: SessionId,
        reference: impl std::fmt::Display,
        scope: SubagentScope,
    ) -> Result<SessionId, EngineError> {
        let reference = reference.to_string();
        let sessions = self.tree_sessions(caller)?;
        let candidates = scoped_candidates(&sessions, caller, scope);
        match_reference(&candidates, &reference)
            .ok_or_else(|| subagent_reference_error(&candidates, &reference))
    }
}

/// Narrows a caller's tree to the sessions an operation may address. The
/// caller itself is never a candidate.
pub(crate) fn scoped_candidates(
    sessions: &[SessionMeta],
    caller: SessionId,
    scope: SubagentScope,
) -> Vec<SessionMeta> {
    sessions
        .iter()
        .filter(|meta| meta.session_id != caller)
        .filter(|meta| match scope {
            SubagentScope::DirectChildren => matches!(
                &meta.origin,
                SessionOrigin::Delegated {
                    parent_session_id,
                    ..
                } if *parent_session_id == caller
            ),
            SubagentScope::TreePeers => true,
        })
        .cloned()
        .collect()
}

/// The two accepted reference forms, in order: a full UUID parse that names a
/// session in `sessions`, then a complete handle exact match. Partial handles
/// and fabricated values match nothing.
pub(crate) fn match_reference(sessions: &[SessionMeta], reference: &str) -> Option<SessionId> {
    if let Ok(id) = reference.parse::<SessionId>()
        && sessions.iter().any(|meta| meta.session_id == id)
    {
        return Some(id);
    }
    if is_valid_handle(reference)
        && let Some(found) = sessions
            .iter()
            .find(|meta| meta.short_id.as_deref() == Some(reference))
    {
        return Some(found.session_id);
    }
    None
}

fn subagent_reference_error(sessions: &[SessionMeta], reference: &str) -> EngineError {
    let mut candidates = sessions
        .iter()
        .map(|meta| match &meta.short_id {
            Some(handle) => format!(
                "{handle} — {} — {} (session_id {})",
                describe(meta),
                session_status_name(meta.status),
                meta.session_id
            ),
            // Pre-handle sessions have no second reference to show: print the
            // UUID once instead of repeating it in a `session_id` suffix.
            None => format!(
                "{} — {} — {}",
                meta.session_id,
                describe(meta),
                session_status_name(meta.status),
            ),
        })
        .collect::<Vec<_>>();
    candidates.sort();
    let list = if candidates.is_empty() {
        "(none)".to_owned()
    } else {
        candidates.join("\n")
    };
    EngineError::ToolFailed(format!(
        "unknown subagent reference {reference:?}; live subagents in your tree:\n{list}"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_protocol::{
        AgentRevision, CatalogRevision, CwdIdentity, ModelRevision, ModelSelection,
        ModelSnapshotRevision, ProviderStateRevision, RecipeRegistryRevision, RunSelection,
        RuntimeRevision, SessionStatus,
    };

    use super::*;

    fn revision(label: char) -> String {
        format!("sha256:{}", label.to_string().repeat(64))
    }

    fn meta(id: SessionId, short_id: Option<&str>) -> SessionMeta {
        SessionMeta {
            session_id: id,
            origin: SessionOrigin::Root,
            short_id: short_id.map(str::to_owned),
            cwd_identity: CwdIdentity::new("workspace:test").expect("cwd"),
            creation_selection: RunSelection {
                agent: cookie_agent_protocol::AgentId::new("explore").expect("agent"),
                model: ModelSelection {
                    model: "provider/model".parse().expect("model key"),
                    variant: None,
                },
                preset: None,
            },
            runtime_revision: RuntimeRevision::new(revision('1')).expect("runtime"),
            catalog_revision: CatalogRevision::new(revision('2')).expect("catalog"),
            provider_state_revision: ProviderStateRevision::new(revision('3')).expect("provider"),
            model_revision: ModelRevision::new(revision('4')).expect("model"),
            agent_revision: AgentRevision::new(revision('5')).expect("agent"),
            recipe_registry_revision: RecipeRegistryRevision::new(revision('6')).expect("recipes"),
            manifest_revision: ModelSnapshotRevision::new(revision('7')).expect("manifest"),
            title: None,
            title_updated_seq: 0,
            last_event_seq: 1,
            last_activity: "2026-08-06T12:00:00Z".parse().expect("timestamp"),
            status: SessionStatus::Completed,
            skipped_events: Vec::new(),
        }
    }

    #[test]
    fn handle_slug_caps_at_sixteen_and_trims_trailing_hyphens() {
        assert_eq!(handle_slug("explore"), "explore");
        assert_eq!(handle_slug("sub-debugger"), "sub-debugger");
        assert_eq!(handle_slug("abcdefghijklmnopq-rst"), "abcdefghijklmnop");
        assert_eq!(handle_slug("abc-"), "abc");
    }

    #[test]
    fn valid_handle_grammar_accepts_complete_handles_only() {
        assert!(is_valid_handle("explore_1a2b3c4d"));
        assert!(is_valid_handle("a_00000000"));
        assert!(!is_valid_handle("explore_1a2b3c4"));
        assert!(!is_valid_handle("explore_1A2B3C4D"));
        assert!(!is_valid_handle("explore"));
        assert!(!is_valid_handle("-explore_1a2b3c4d"));
        assert!(!is_valid_handle("explore-_1a2b3c4d"));
        assert!(!is_valid_handle("abcdefghijklmnopq_1a2b3c4d"));
    }

    #[test]
    fn generation_regenerates_on_collision_then_succeeds() {
        let mut existing = HashSet::new();
        existing.insert("explore_00000001".to_owned());
        let mut values = [1_u32, 2_u32].into_iter();
        let handle = generate_handle("explore", &existing, || values.next().unwrap())
            .expect("second candidate is free");
        assert_eq!(handle, "explore_00000002");
    }

    #[test]
    fn generation_fails_loudly_after_sixteen_attempts() {
        let existing = HashSet::from(["explore_00000001".to_owned()]);
        let error = generate_handle("explore", &existing, || 1).expect_err("cap reached");
        assert!(error.to_string().contains("16 attempts"));
    }

    #[test]
    fn reference_resolution_prefers_uuid_then_exact_handle() {
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        let sessions = vec![
            meta(first, Some("explore_1a2b3c4d")),
            meta(second, Some("coder_9f8e7d6b")),
        ];
        assert_eq!(match_reference(&sessions, &first.to_string()), Some(first));
        assert_eq!(match_reference(&sessions, "coder_9f8e7d6b"), Some(second));
    }

    #[test]
    fn reference_resolution_rejects_prefixes_and_fabrications() {
        let id = SessionId::new_v7();
        let sessions = vec![meta(id, Some("explore_1a2b3c4d"))];
        assert_eq!(match_reference(&sessions, "explore_1a2b3c4"), None);
        assert_eq!(match_reference(&sessions, "explore"), None);
        assert_eq!(match_reference(&sessions, "explore_00000000"), None);
        assert_eq!(match_reference(&sessions, "not-a-handle"), None);
        assert_eq!(
            match_reference(&sessions, &SessionId::new_v7().to_string()),
            None
        );
    }

    fn delegated_meta(id: SessionId, parent: SessionId, short_id: Option<&str>) -> SessionMeta {
        let mut meta = meta(id, short_id);
        meta.origin = SessionOrigin::Delegated {
            root_session_id: parent,
            parent_session_id: parent,
            parent_run_id: cookie_agent_protocol::RunId::new_v7(),
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
            depth: 1,
        };
        meta
    }

    #[test]
    fn candidate_scope_separates_direct_children_from_tree_peers() {
        // root -> child -> grandchild, plus root -> sibling.
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let grandchild = SessionId::new_v7();
        let sibling = SessionId::new_v7();
        let sessions = vec![
            meta(root, Some("root_00000000")),
            delegated_meta(child, root, Some("explore_1a2b3c4d")),
            delegated_meta(grandchild, child, Some("coder_9f8e7d6b")),
            delegated_meta(sibling, root, Some("writer_12345678")),
        ];
        let direct = scoped_candidates(&sessions, root, SubagentScope::DirectChildren);
        assert_eq!(
            direct
                .iter()
                .map(|meta| meta.session_id)
                .collect::<Vec<_>>(),
            vec![child, sibling]
        );
        // A grandchild handle is not addressable by the direct-child tools.
        assert_eq!(match_reference(&direct, "coder_9f8e7d6b"), None);
        let peers = scoped_candidates(&sessions, root, SubagentScope::TreePeers);
        assert_eq!(peers.len(), 3);
        assert_eq!(match_reference(&peers, "coder_9f8e7d6b"), Some(grandchild));
    }

    #[test]
    fn old_sessions_without_handles_resolve_by_uuid_only() {
        let id = SessionId::new_v7();
        let sessions = vec![meta(id, None)];
        assert_eq!(match_reference(&sessions, &id.to_string()), Some(id));
        assert_eq!(match_reference(&sessions, "explore_1a2b3c4d"), None);
    }

    #[test]
    fn reference_error_lists_live_candidates_with_handle_description_and_status() {
        let child = SessionId::new_v7();
        let mut child_meta = meta(child, Some("explore_1a2b3c4d"));
        child_meta.title =
            Some(cookie_agent_protocol::SessionTitle::new("Review API").expect("title"));
        child_meta.status = SessionStatus::Running;
        let error = subagent_reference_error(&[child_meta], "bogus");
        let message = error.to_string();
        assert!(message.contains("unknown subagent reference \"bogus\""));
        assert!(message.contains("explore_1a2b3c4d — Review API — running"));
        assert!(message.contains(&child.to_string()));
    }

    #[test]
    fn reference_error_does_not_repeat_the_uuid_for_pre_handle_sessions() {
        let child = SessionId::new_v7();
        let mut child_meta = meta(child, None);
        child_meta.status = SessionStatus::Running;
        let message = subagent_reference_error(&[child_meta], "bogus").to_string();
        let row = format!("{child} — explore — running");
        assert!(message.contains(&row));
        assert_eq!(message.matches(&child.to_string()).count(), 1);
    }
}
