//! Approval panel state, decisions, and rendering helpers.

use super::*;

pub(super) fn is_approval_scroll_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    )
}

pub(in crate::ui) fn approval_content(approval: &ApprovalState) -> String {
    let mut content = String::new();
    writeln!(
        content,
        "PERMISSION REQUIRED{}",
        if approval.escalated {
            " · ESCALATED"
        } else {
            ""
        }
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "consent target: {}",
        approval.evaluations[0].trace.normalized_resource
    )
    .expect("writing to a String cannot fail");
    writeln!(content, "approval id: {}", approval.approval_id)
        .expect("writing to a String cannot fail");
    writeln!(content, "request revision: {}", approval.request_revision)
        .expect("writing to a String cannot fail");
    writeln!(content, "trigger: {:?}", approval.trigger).expect("writing to a String cannot fail");
    writeln!(
        content,
        "operation fingerprint: {}",
        approval.operation_fingerprint.digest()
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "normalized-arguments digest: {}",
        approval.normalized_arguments_digest
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "execution-context digest: {}",
        approval.execution_context_digest
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "prepared capability lifetime: {:?}",
        approval.capability_lifetime
    )
    .expect("writing to a String cannot fail");

    writeln!(content, "\nCAPABILITIES ({})", approval.capabilities.len())
        .expect("writing to a String cannot fail");
    for (index, capability) in approval.capabilities.iter().enumerate() {
        writeln!(
            content,
            "{}. action: {:?}\n   operation: {}\n   lifetime: {:?}",
            index + 1,
            capability.action,
            capability.operation.as_str(),
            approval.capability_lifetime
        )
        .expect("writing to a String cannot fail");
    }

    writeln!(content, "\nRESOURCES ({})", approval.resources.len())
        .expect("writing to a String cannot fail");
    for (index, resource) in approval.resources.iter().enumerate() {
        let normalized = approval
            .evaluations
            .iter()
            .find(|evaluation| evaluation.resource_digest == resource.binding_digest)
            .expect("validated approval evaluations cover every resource")
            .trace
            .normalized_resource
            .as_str();
        writeln!(
            content,
            "{}. action: {:?}\n   normalized identity: {}\n   canonical identity: {}\n   binding digest: {}\n   boundary: {}\n   binding lifetime: {:?}\n   source: {:?}",
            index + 1,
            resource.capability,
            normalized,
            resource.canonical.as_str(),
            resource.binding_digest.digest(),
            approval_boundary(&resource.boundary),
            resource.binding_lifetime,
            resource.source
        )
        .expect("writing to a String cannot fail");
    }

    writeln!(content, "\nEVALUATIONS ({})", approval.evaluations.len())
        .expect("writing to a String cannot fail");
    for (index, evaluation) in approval.evaluations.iter().enumerate() {
        writeln!(
            content,
            "{}. resource binding digest: {}\n   result effect: {:?}\n   trace action: {:?}\n   trace normalized resource: {}\n   trace effect: {:?}\n   precedence reason: {}\n   candidate rules ({}):",
            index + 1,
            evaluation.resource_digest.digest(),
            evaluation.effect,
            evaluation.trace.action,
            evaluation.trace.normalized_resource,
            evaluation.trace.effect,
            evaluation.trace.precedence_reason,
            evaluation.trace.candidates.len()
        )
        .expect("writing to a String cannot fail");
        if evaluation.trace.candidates.is_empty() {
            writeln!(content, "      (none)").expect("writing to a String cannot fail");
        } else {
            for (candidate_index, candidate) in evaluation.trace.candidates.iter().enumerate() {
                writeln!(
                    content,
                    "      {}. action: {:?} · resource: {} · source layer: {} · effect: {:?}",
                    candidate_index + 1,
                    candidate.action,
                    candidate.resource,
                    candidate.source_layer,
                    candidate.effect
                )
                .expect("writing to a String cannot fail");
            }
        }
    }

    writeln!(content, "\nRESPONSE CONSTRAINTS").expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow approve once: {}",
        approval.constraints.allow_once
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow delegation-tree grant: {}",
        approval.constraints.allow_tree_grant
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "allow cancel: {}",
        approval.constraints.cancellable
    )
    .expect("writing to a String cannot fail");
    writeln!(
        content,
        "expires at: {}",
        approval
            .constraints
            .expires_at
            .map_or_else(|| "never".into(), |timestamp| timestamp.to_string())
    )
    .expect("writing to a String cannot fail");
    content
}

pub(super) fn approval_boundary(boundary: &cookie_agent_protocol::ApprovalBoundary) -> String {
    match boundary {
        cookie_agent_protocol::ApprovalBoundary::Exact => "exact".into(),
        cookie_agent_protocol::ApprovalBoundary::CommandPrefix { prefix } => {
            format!("command prefix: {prefix}")
        }
        cookie_agent_protocol::ApprovalBoundary::DelegationTree { root_session_id } => {
            format!("delegation tree rooted at session {root_session_id}")
        }
    }
}

/// Visual hierarchy for the approval body: the banner is a warning, the
/// consent target is the prominent identity, section headers are headings,
/// identity digests recede, and the remaining evidence is body text. The
/// content itself is produced by `approval_content` unchanged.
pub(super) fn approval_line_style(line: &str, theme: &Theme) -> ratatui::style::Style {
    if line.starts_with("PERMISSION REQUIRED") {
        return theme.warning();
    }
    if line.starts_with("consent target:") {
        return theme.user();
    }
    if [
        "CAPABILITIES (",
        "RESOURCES (",
        "EVALUATIONS (",
        "RESPONSE CONSTRAINTS",
    ]
    .iter()
    .any(|header| line.starts_with(header))
    {
        return theme.heading();
    }
    if [
        "approval id:",
        "request revision:",
        "trigger:",
        "operation fingerprint:",
        "normalized-arguments digest:",
        "execution-context digest:",
        "prepared capability lifetime:",
    ]
    .iter()
    .any(|key| line.starts_with(key))
    {
        return theme.internal();
    }
    theme.body()
}

pub(super) fn decision_tone(decision: ApprovalUserDecision) -> crate::theme::DecisionTone {
    match decision {
        ApprovalUserDecision::ApproveOnce | ApprovalUserDecision::ApproveTree => {
            crate::theme::DecisionTone::Allow
        }
        ApprovalUserDecision::Reject => crate::theme::DecisionTone::Deny,
        ApprovalUserDecision::Cancel => crate::theme::DecisionTone::Neutral,
    }
}

pub(super) fn approval_action_hits(area: Rect, approval: &ApprovalState) -> Vec<ApprovalHit> {
    let inner = inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return Vec::new();
    }
    // Roomy panels get three-row rounded buttons; cramped ones keep the
    // single action row. Heights here and in `render_approval` must agree.
    let height = if inner.width >= 44 && inner.height >= 10 {
        3
    } else {
        1
    };
    let row = Rect::new(
        inner.x,
        inner.y + inner.height - height,
        inner.width,
        height,
    );
    let mut decisions = Vec::new();
    if approval.constraints.allow_once {
        decisions.push(ApprovalUserDecision::ApproveOnce);
    }
    if approval.constraints.allow_tree_grant {
        decisions.push(ApprovalUserDecision::ApproveTree);
    }
    decisions.push(ApprovalUserDecision::Reject);
    if approval.constraints.cancellable {
        decisions.push(ApprovalUserDecision::Cancel);
    }
    if decisions.is_empty() {
        return Vec::new();
    }
    let width = 100 / u16::try_from(decisions.len()).unwrap_or(1);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(decisions.iter().map(|_| Constraint::Percentage(width)))
        .split(row)
        .iter()
        .zip(decisions)
        .map(|(rect, decision)| ApprovalHit {
            rect: *rect,
            decision,
        })
        .collect()
}

/// Render each decision as a distinct button: a rounded frame in the
/// decision's tone with a glyph-bearing label (never color alone). Buttons
/// leave a one-column visual gap between frames while their hit regions
/// stay contiguous; single-row areas fall back to flat labels.
pub(super) fn render_approval_actions(
    frame: &mut ratatui::Frame,
    actions: &[ApprovalHit],
    theme: &Theme,
) {
    for action in actions {
        let tone = theme.decision(decision_tone(action.decision), false);
        if action.rect.height == 1 {
            let label = approval_action_label(action.decision, action.rect.width);
            frame.render_widget(
                Paragraph::new(Span::styled(label, tone))
                    .alignment(ratatui::layout::Alignment::Center),
                action.rect,
            );
            continue;
        }
        // Visual frame shrinks one column off the hit region for the gap.
        let visual = Rect::new(
            action.rect.x,
            action.rect.y,
            action.rect.width.saturating_sub(1).max(1),
            action.rect.height,
        );
        let inner = inner_rect(visual);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(tone)
                .style(theme.panel()),
            visual,
        );
        let label = approval_action_label(action.decision, visual.width);
        frame.render_widget(
            Paragraph::new(Span::styled(label, tone)).alignment(ratatui::layout::Alignment::Center),
            inner,
        );
    }
}

pub(super) fn approval_action_label(decision: ApprovalUserDecision, width: u16) -> &'static str {
    let full = match decision {
        ApprovalUserDecision::ApproveOnce => "✓ Allow once",
        ApprovalUserDecision::ApproveTree => "✓ Allow all",
        ApprovalUserDecision::Reject => "✗ Reject",
        ApprovalUserDecision::Cancel => "⎋ Cancel",
    };
    if usize::from(width) >= full.len() + 2 {
        return full;
    }
    let short = match decision {
        ApprovalUserDecision::ApproveOnce => "✓ Once",
        ApprovalUserDecision::ApproveTree => "✓ Tree",
        ApprovalUserDecision::Reject => "✗ No",
        ApprovalUserDecision::Cancel => "⎋ Esc",
    };
    if usize::from(width) >= short.len() + 2 {
        return short;
    }
    match decision {
        ApprovalUserDecision::ApproveOnce => "✓",
        ApprovalUserDecision::ApproveTree => "✓T",
        ApprovalUserDecision::Reject => "✗",
        ApprovalUserDecision::Cancel => "⎋",
    }
}

impl App {
    pub(super) fn handle_approval_scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.scroll_approval(true, 1),
            KeyCode::Down => self.scroll_approval(false, 1),
            KeyCode::PageUp => self.scroll_approval(true, 10),
            KeyCode::PageDown => self.scroll_approval(false, 10),
            KeyCode::Home => self.approval_scroll = 0,
            KeyCode::End => self.approval_scroll = self.approval_max_scroll,
            _ => {}
        }
    }

    pub(super) fn scroll_approval(&mut self, up: bool, lines: u16) {
        self.approval_scroll = if up {
            self.approval_scroll.saturating_sub(lines)
        } else {
            self.approval_scroll
                .saturating_add(lines)
                .min(self.approval_max_scroll)
        };
    }

    /// Optimistic approval response: the modal is dismissed immediately and
    /// the exact (id, revision, fingerprint, decision) tuple is sent
    /// asynchronously. Nothing executes locally; failures restore the modal
    /// only when the request is still durably escalated and unexpired.
    pub(in crate::ui) async fn answer_approval(&mut self, decision: ApprovalUserDecision) {
        if self.pending_approval.is_some() {
            return;
        }
        let Some(approval) = self.current_approval().cloned() else {
            return;
        };
        self.next_approval_request_id = self.next_approval_request_id.wrapping_add(1);
        let request_id = self.next_approval_request_id;
        let decision_label = match decision {
            ApprovalUserDecision::ApproveOnce => "approve once",
            ApprovalUserDecision::ApproveTree => "approve all",
            ApprovalUserDecision::Reject => "reject",
            ApprovalUserDecision::Cancel => "cancel",
        };
        self.pending_approval = Some(PendingApprovalSubmission {
            request_id,
            approval: approval.clone(),
            decision,
        });
        self.status = format!("Approval submitted ({decision_label})…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .respond_approval(ApprovalRespondParams {
                    session_id: approval.session_id,
                    approval_id: approval.approval_id,
                    request_revision: approval.request_revision,
                    operation_fingerprint: approval.operation_fingerprint,
                    client_response_id: client_response_id(),
                    decision,
                    feedback: None,
                })
                .await
                .map(|_| ())
                .map_err(ApprovalSubmissionError::from_client);
            let _ = updates.send(RpcUpdate::ApprovalResponse {
                request_id,
                approval_id: approval.approval_id,
                result,
            });
        });
    }

    /// Resolve an in-flight approval response. Success clears the pending
    /// marker; durable resolution arrives through the normal event stream.
    /// Failure restores the modal only when the request is still escalated
    /// and unexpired. Revision/fingerprint conflicts trigger an approval.list
    /// refresh and are never silently resubmitted.
    pub(super) fn finish_approval_submission(
        &mut self,
        request_id: u64,
        approval_id: cookie_agent_protocol::ApprovalId,
        result: Result<(), ApprovalSubmissionError>,
    ) {
        let Some(pending) = self.pending_approval.take_if(|pending| {
            pending.request_id == request_id && pending.approval.approval_id == approval_id
        }) else {
            return;
        };
        match result {
            Ok(()) => {
                self.remove_exact_approval(&pending.approval);
                let decision_label = match pending.decision {
                    ApprovalUserDecision::ApproveOnce => "approve once",
                    ApprovalUserDecision::ApproveTree => "approve all",
                    ApprovalUserDecision::Reject => "reject",
                    ApprovalUserDecision::Cancel => "cancel",
                };
                self.status = format!("approval response accepted ({decision_label})");
            }
            Err(error) => {
                let approval = pending.approval;
                if error.stale_projection() {
                    self.remove_exact_approval(&approval);
                    self.status = format!(
                        "Approval {approval_id} changed before the response landed; refreshing the approval list."
                    );
                    self.refresh_approvals(approval.session_id);
                    return;
                }
                if self.approval_is_exact_pending(&approval) {
                    self.status = format!("approval response failed: {}", error.message);
                } else {
                    self.status = format!(
                        "approval {approval_id} is no longer pending; refreshing the approval list."
                    );
                    self.refresh_approvals(approval.session_id);
                }
            }
        }
    }

    /// Refresh the durable approval queue after a conflict or expiry.
    pub(super) fn refresh_approvals(&mut self, session_id: SessionId) {
        let root_session_id = self.tree_root.unwrap_or(session_id);
        self.next_approval_refresh_id = self.next_approval_refresh_id.wrapping_add(1);
        let request_id = self.next_approval_refresh_id;
        let generation = self.selection_generation;
        self.approval_refresh_in_flight = Some((root_session_id, generation, request_id));
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_approvals(ApprovalListParams {
                    root_session_id,
                    status: Some(ApprovalStatus::Escalated),
                })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::ApprovalList {
                root_session_id,
                generation,
                request_id,
                result,
            });
        });
    }

    pub(in crate::ui) fn current_approval(&self) -> Option<&ApprovalState> {
        if self.pending_approval.is_some() {
            return None;
        }
        self.selected
            .and_then(|id| self.store.sessions.get(&id))
            .and_then(|state| {
                state
                    .approvals
                    .iter()
                    .find(|approval| approval.is_visible_user_escalation())
            })
    }

    pub(super) fn approval_is_exact_pending(&self, approval: &ApprovalState) -> bool {
        if approval
            .constraints
            .expires_at
            .is_some_and(|expires_at| expires_at <= jiff::Timestamp::now())
        {
            return false;
        }
        let Some(state) = self.store.sessions.get(&approval.session_id) else {
            return false;
        };
        let mut same_id = state
            .approvals
            .iter()
            .filter(|candidate| candidate.approval_id == approval.approval_id);
        same_id.next().is_some_and(|candidate| {
            candidate.is_visible_user_escalation()
                && candidate.request_revision == approval.request_revision
                && candidate.operation_fingerprint == approval.operation_fingerprint
        }) && same_id.next().is_none()
    }

    pub(super) fn remove_exact_approval(&mut self, approval: &ApprovalState) {
        if let Some(state) = self.store.sessions.get_mut(&approval.session_id) {
            state.approvals.retain(|candidate| {
                candidate.approval_id != approval.approval_id
                    || candidate.request_revision != approval.request_revision
                    || candidate.operation_fingerprint != approval.operation_fingerprint
            });
        }
    }

    pub(in crate::ui) fn reconcile_pending_approval(&mut self) {
        let stale = self
            .pending_approval
            .as_ref()
            .is_some_and(|pending| !self.approval_is_exact_pending(&pending.approval));
        if stale {
            let pending = self
                .pending_approval
                .take()
                .expect("stale pending approval exists");
            self.remove_exact_approval(&pending.approval);
            self.status = format!(
                "approval {} is no longer pending; showing the next valid approval",
                pending.approval.approval_id
            );
        }
    }

    pub(in crate::ui) fn apply_approval_list(
        &mut self,
        root_session_id: SessionId,
        result: ApprovalListResult,
    ) {
        let mut session_ids = vec![root_session_id];
        if self.tree_root == Some(root_session_id)
            && let Some(tree) = &self.tree
        {
            collect_tree_session_ids(tree, &mut session_ids);
        }
        session_ids.sort_unstable_by_key(ToString::to_string);
        session_ids.dedup();
        for session_id in session_ids {
            if let Some(state) = self.store.sessions.get_mut(&session_id) {
                // The list refresh replaces only the user-visible queue.
                // Preserve event-projected internal requests so a later,
                // strictly ordered ApprovalEscalated can still reveal them.
                state.approvals.retain(|approval| !approval.escalated);
            }
        }
        for record in result.approvals {
            if let Some(approval) = approval_state_from_record(record) {
                let state = self.store.sessions.entry(approval.session_id).or_default();
                state
                    .approvals
                    .retain(|candidate| candidate.approval_id != approval.approval_id);
                state.approvals.push(approval);
            }
        }
    }

    pub(in crate::ui) fn selected_running_tool(
        &mut self,
    ) -> Option<(
        cookie_agent_protocol::RunId,
        cookie_agent_protocol::ToolCallId,
    )> {
        let session_id = self.selected?;
        let run_id = self.store.sessions.get(&session_id)?.active_run?;
        let running = self.running_tool_ids();
        if !self
            .stdin_target
            .is_some_and(|call_id| running.contains(&call_id))
        {
            self.stdin_target = running.first().copied();
        }
        let call_id = self.stdin_target?;
        let state = self.store.sessions.get(&session_id)?;
        (state.tools.get(&call_id)?.status == ToolStatus::Running).then_some((run_id, call_id))
    }

    pub(in crate::ui) fn running_tool_ids(&self) -> Vec<cookie_agent_protocol::ToolCallId> {
        let Some(session_id) = self.selected else {
            return Vec::new();
        };
        let Some(state) = self.store.sessions.get(&session_id) else {
            return Vec::new();
        };
        let mut ids = state
            .tools
            .values()
            .filter(|tool| tool.status == ToolStatus::Running)
            .map(|tool| tool.id)
            .collect::<Vec<_>>();
        ids.sort_by_key(ToString::to_string);
        ids
    }

    pub(in crate::ui) fn cancel_active_run(&mut self) {
        let Some(session_id) = self.selected else {
            self.status = "no active run to cancel".into();
            return;
        };
        let Some(run_id) = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.active_run)
        else {
            self.status = "no active run to cancel".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.cancel_run(RunCancelParams { run_id }).await {
                Ok(result) if result.cancelled => {
                    RpcUpdate::Notice("run cancellation requested".into())
                }
                Ok(_) => RpcUpdate::Notice("run was already complete".into()),
                Err(error) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }

    pub(in crate::ui) fn render_approval(
        &mut self,
        frame: &mut ratatui::Frame,
        approval: &ApprovalState,
        area: Rect,
    ) -> Vec<ApprovalHit> {
        paint_panel(frame, area, &self.theme);
        let request = (approval.approval_id, approval.request_revision);
        if self.approval_scroll_request != Some(request) {
            self.approval_scroll_request = Some(request);
            self.approval_scroll = 0;
        }
        let actions = approval_action_hits(area, approval);
        let actions_height = actions.first().map_or(0, |hit| hit.rect.height);
        let spacer = u16::from(actions_height > 1);
        let inner = inner_rect(area);
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner
                .height
                .saturating_sub(actions_height)
                .saturating_sub(spacer),
        );
        let content = approval_content(approval);
        let lines = content
            .lines()
            .flat_map(|line| {
                wrapped_line(
                    Line::from(Span::styled(
                        line.to_owned(),
                        approval_line_style(line, &self.theme),
                    )),
                    body.width,
                )
            })
            .collect::<Vec<_>>();
        let line_count = lines.len().min(usize::from(u16::MAX)) as u16;
        let paragraph = Paragraph::new(lines);
        self.approval_max_scroll = line_count.saturating_sub(body.height);
        self.approval_scroll = self.approval_scroll.min(self.approval_max_scroll);
        let visible_end = self
            .approval_scroll
            .saturating_add(body.height)
            .min(line_count);
        let visible_start = self
            .approval_scroll
            .saturating_add(1)
            .min(line_count.max(1));
        let title = if area.width < 48 {
            format!("Approval {visible_start}–{visible_end}/{line_count} ↑↓")
        } else {
            format!(
                "Approval · lines {visible_start}–{visible_end}/{line_count} · ↑/↓ PgUp/PgDn Home/End"
            )
        };
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(self.theme.warning())
                .title(Span::styled(title, self.theme.heading()))
                .style(self.theme.panel()),
            area,
        );
        frame.render_widget(paragraph.scroll((self.approval_scroll, 0)), body);
        render_approval_actions(frame, &actions, &self.theme);
        actions
    }
}
