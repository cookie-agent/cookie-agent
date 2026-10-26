//! Client behaviour around a session that every client shares: the
//! approval queue a list refresh replaces, the permission-mode cycle, and
//! the complete content of a cut display block.

use cookie_agent_protocol::{
    ApprovalListResult, ArtifactReadResult, ArtifactRef, DisplayBlock, Open, PermissionMode,
    SemVer, SessionId, ToolCallId,
};

use crate::{
    FullContent, FullContentState, MAX_FULL_CONTENT_BYTES, SessionState, StateStore,
    approval_state_from_record,
};

/// The first client protocol whose daemons let any client take back a
/// waiting (pending or queued) input, not only the client that sent it.
pub const RECALL_ANY_SINCE: SemVer = SemVer::new(1, 31, 0);

/// Whether the daemon, speaking `version`, lets this client take back
/// another client's waiting input. An older daemon answers `not_owner`.
#[must_use]
pub fn daemon_recalls_any_input(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= RECALL_ANY_SINCE)
}

/// How clients name a permission mode.
pub const fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AutoApprove => "auto-approve",
        PermissionMode::AutoApproveN => "auto-n",
        PermissionMode::AutoApproveY => "auto-y",
        PermissionMode::Ask => "ask",
        PermissionMode::Yolo => "yolo",
    }
}

/// The mode after `mode` when a client cycles through them.
pub const fn next_permission_mode(mode: PermissionMode) -> PermissionMode {
    match mode {
        PermissionMode::AutoApprove => PermissionMode::AutoApproveN,
        PermissionMode::AutoApproveN => PermissionMode::AutoApproveY,
        PermissionMode::AutoApproveY => PermissionMode::Ask,
        PermissionMode::Ask => PermissionMode::Yolo,
        PermissionMode::Yolo => PermissionMode::AutoApprove,
    }
}

impl StateStore {
    /// Installs an `approval.list` of escalated approvals for a tree whose
    /// sessions are `tree_sessions`: the list replaces only their
    /// user-visible queues, keeping event-projected internal requests so a
    /// later, strictly ordered `approval_escalated` can still reveal them.
    pub fn apply_approval_list(&mut self, tree_sessions: &[SessionId], result: ApprovalListResult) {
        for session_id in tree_sessions {
            if let Some(state) = self.sessions.get_mut(session_id) {
                state.approvals.retain(|approval| !approval.escalated);
            }
        }
        for record in result.approvals {
            if let Some(approval) = approval_state_from_record(record) {
                let state = self.sessions.entry(approval.session_id).or_default();
                state
                    .approvals
                    .retain(|candidate| candidate.approval_id != approval.approval_id);
                state.approvals.push(approval);
            }
        }
    }
}

/// The `full` artifact of display block `index` in `blocks`: where a cut
/// block's complete content lives.
pub fn display_full(blocks: &[Open<DisplayBlock>], index: usize) -> Option<&ArtifactRef> {
    blocks.get(index).and_then(Open::known).and_then(block_full)
}

/// The `full` artifact of a cut block.
pub fn block_full(block: &DisplayBlock) -> Option<&ArtifactRef> {
    match block {
        DisplayBlock::FileExcerpt { full, .. } | DisplayBlock::Terminal { full, .. } => {
            full.as_ref()
        }
        DisplayBlock::Diff(diff) => diff.full.as_ref(),
        _ => None,
    }
}

/// What a client must read for the complete content of a cut block: the
/// artifact and how many bytes of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullContentRead {
    pub artifact: ArtifactRef,
    pub length: u64,
}

impl SessionState {
    /// Toggles the complete content of display block `block` of `call_id`:
    /// loaded content is put away, a read in flight stays, and otherwise
    /// the block is marked loading and the read to make is returned.
    pub fn toggle_full_content(
        &mut self,
        call_id: ToolCallId,
        block: usize,
    ) -> Option<FullContentRead> {
        let tool = self.tools.get(&call_id)?;
        match tool.full_content(block) {
            Some(FullContentState::Loading) => return None,
            Some(FullContentState::Loaded { .. }) => {
                self.update_tool(call_id, |tool| tool.full.retain(|full| full.block != block));
                return None;
            }
            Some(FullContentState::Failed(_)) | None => {}
        }
        let artifact = display_full(tool.shown_display(), block)?.clone();
        self.update_tool(call_id, |tool| {
            tool.full.retain(|full| full.block != block);
            tool.full.push(FullContent {
                block,
                state: FullContentState::Loading,
            });
        });
        Some(FullContentRead {
            length: artifact.byte_length.clamp(1, MAX_FULL_CONTENT_BYTES),
            artifact,
        })
    }

    /// The read for block `block` of `call_id` ended. A call that ended or
    /// was put away since keeps what it shows now.
    pub fn finish_full_content(
        &mut self,
        call_id: ToolCallId,
        block: usize,
        state: FullContentState,
    ) {
        let pending = self
            .tools
            .get(&call_id)
            .is_some_and(|tool| tool.full_content(block) == Some(&FullContentState::Loading));
        if pending {
            self.update_tool(call_id, |tool| {
                if let Some(full) = tool.full.iter_mut().find(|full| full.block == block) {
                    full.state = state;
                }
            });
        }
    }
}

impl FullContentState {
    /// The state an `artifact.read` of a block's complete content leaves.
    pub fn of_read(read: Result<ArtifactReadResult, String>) -> Self {
        read.and_then(|read| {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(read.data)
                .map_err(|error| format!("the daemon sent malformed data: {error}"))?;
            Ok(Self::Loaded {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                truncated: read.total_bytes > bytes.len() as u64,
            })
        })
        .unwrap_or_else(Self::Failed)
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::PermissionMode;

    use super::{next_permission_mode, permission_mode_label};

    #[test]
    fn the_mode_cycle_visits_every_mode() {
        let mut mode = PermissionMode::AutoApprove;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(permission_mode_label(mode));
            mode = next_permission_mode(mode);
        }
        assert_eq!(mode, PermissionMode::AutoApprove);
        assert_eq!(seen, ["auto-approve", "auto-n", "auto-y", "ask", "yolo"]);
    }
}
