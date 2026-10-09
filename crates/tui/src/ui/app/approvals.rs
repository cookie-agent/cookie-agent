//! Approval panel state, decisions, and rendering.
//!
//! The panel docks above the status bar in place of the composer, leaving
//! the conversation visible above it. It leads with what the user is
//! judging: the call itself (its command or diff), why it asks, and what
//! the focused decision would grant. The full prepared-operation identity
//! stays one `d` away.

use super::*;

use ratatui::style::Color;

use crate::ui::transcript::{ApprovalPreviewRow, approval_operation_preview, approval_tool_icon};

/// Rows of the conversation the docked panel always leaves visible above it.
const APPROVAL_MIN_CONVERSATION_ROWS: u16 = 4;

/// Columns of the `why` / decision label beside the panel's explanations.
const APPROVAL_LABEL_COLUMNS: usize = 10;

/// Interaction state of the approval panel. It starts fresh whenever a
/// different request (or revision) is on top.
#[derive(Default)]
pub(in crate::ui) struct ApprovalPanel {
    request: Option<(cookie_agent_protocol::ApprovalId, u64)>,
    pub(in crate::ui) scroll: u16,
    pub(in crate::ui) max_scroll: u16,
    /// The keyboard-focused decision: Enter answers with it.
    pub(in crate::ui) focus: Option<ApprovalUserDecision>,
    /// Whether the full prepared-operation identity is expanded.
    pub(in crate::ui) details: bool,
    /// The rejection note being written, while its field is open.
    pub(in crate::ui) note: Option<InputState>,
    /// A message about this request shown beside the buttons, where the
    /// panel cannot hide it the way it hides the status row.
    pub(in crate::ui) notice: Option<String>,
}

impl ApprovalPanel {
    /// Reset for a request that was not on top before. Focus starts on the
    /// first offered decision, matching the button order.
    pub(super) fn sync(&mut self, approval: &ApprovalState) {
        let request = (approval.approval_id, approval.request_revision);
        if self.request != Some(request) {
            *self = Self {
                request: Some(request),
                focus: offered_decisions(approval).first().copied(),
                ..Self::default()
            };
        }
    }
}

/// The decisions a request offers, in button order. Reject is always
/// offered; the others follow the request's constraints.
pub(super) fn offered_decisions(approval: &ApprovalState) -> Vec<ApprovalUserDecision> {
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
    decisions
}

/// Whole seconds before the request expires, if it ever does.
pub(super) fn seconds_left(approval: &ApprovalState) -> Option<i64> {
    approval.constraints.expires_at.map(|expires_at| {
        expires_at
            .duration_since(jiff::Timestamp::now())
            .as_secs()
            .max(0)
    })
}

fn countdown_label(seconds: i64) -> String {
    if seconds >= 86_400 {
        format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600)
    } else if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

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

/// Visual hierarchy for the expanded details: section headers are
/// headings, identity digests recede, and the remaining evidence is body
/// text. The content itself is produced by `approval_content` unchanged.
pub(super) fn approval_line_style(line: &str, theme: &Theme) -> Style {
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

/// The key that answers an approval with `decision`: letters for the
/// grants and the reject, Esc for cancel.
pub(super) fn approval_hotkey(decision: ApprovalUserDecision) -> &'static str {
    match decision {
        ApprovalUserDecision::ApproveOnce => "y",
        ApprovalUserDecision::ApproveTree => "a",
        ApprovalUserDecision::Reject => "n",
        ApprovalUserDecision::Cancel => "esc",
    }
}

/// The decision a letter hotkey answers with, if this approval offers it.
/// Case-insensitive so Caps Lock does not silently disable the keys.
pub(super) fn approval_hotkey_decision(
    character: char,
    approval: &ApprovalState,
) -> Option<ApprovalUserDecision> {
    match character.to_ascii_lowercase() {
        'y' if approval.constraints.allow_once => Some(ApprovalUserDecision::ApproveOnce),
        'a' if approval.constraints.allow_tree_grant => Some(ApprovalUserDecision::ApproveTree),
        'n' => Some(ApprovalUserDecision::Reject),
        _ => None,
    }
}

/// Glyph, full label, and short label of a decision. The glyph carries the
/// decision where color does not.
fn decision_labels(decision: ApprovalUserDecision) -> (&'static str, &'static str, &'static str) {
    match decision {
        ApprovalUserDecision::ApproveOnce => ("✓", "Allow once", "Once"),
        ApprovalUserDecision::ApproveTree => ("✓", "Allow all", "All"),
        ApprovalUserDecision::Reject => ("✗", "Reject", "No"),
        ApprovalUserDecision::Cancel => ("⎋", "Cancel", "Esc"),
    }
}

/// What answering with `decision` does, phrased for the label row that
/// follows keyboard focus.
fn decision_effect(decision: ApprovalUserDecision, approval: &ApprovalState) -> String {
    match decision {
        ApprovalUserDecision::ApproveOnce => {
            "runs this exact call now; you will be asked again next time".into()
        }
        ApprovalUserDecision::ApproveTree => {
            let mut scopes = approval
                .resources
                .iter()
                .map(|resource| match &resource.boundary {
                    cookie_agent_protocol::ApprovalBoundary::CommandPrefix { prefix } => {
                        format!("commands starting with `{prefix}`")
                    }
                    cookie_agent_protocol::ApprovalBoundary::Exact => format!(
                        "this exact {} target",
                        crate::ui::management::action_label(resource.capability)
                    ),
                    cookie_agent_protocol::ApprovalBoundary::DelegationTree { .. } => {
                        "this delegation tree".into()
                    }
                })
                .collect::<Vec<_>>();
            scopes.dedup();
            format!(
                "also allows {} for this session and its subagents",
                scopes.join(", ")
            )
        }
        ApprovalUserDecision::Reject => {
            "refuses this call and tells the agent so · e adds a note".into()
        }
        ApprovalUserDecision::Cancel => "withdraws the request; the call does not run".into(),
    }
}

/// Why the request reached the user, in one plain sentence.
fn approval_reason(approval: &ApprovalState) -> String {
    use cookie_agent_protocol::ApprovalTrigger;
    let asking = approval
        .evaluations
        .iter()
        .filter(|evaluation| evaluation.effect == PermissionEffect::Ask)
        .collect::<Vec<_>>();
    let first = asking.first().copied().unwrap_or(&approval.evaluations[0]);
    match approval.trigger {
        ApprovalTrigger::ModelToolApproval => {
            format!("the model asked first: {}", first.trace.precedence_reason)
        }
        ApprovalTrigger::DoomLoop => "the agent keeps repeating this exact call".into(),
        ApprovalTrigger::InternalAgent => "an internal agent asked for your decision".into(),
        ApprovalTrigger::PermissionPolicy => {
            let action = crate::ui::management::action_label(first.trace.action);
            let rule = first
                .trace
                .candidates
                .iter()
                .find(|candidate| candidate.effect == first.trace.effect);
            let mut reason = match rule {
                Some(rule) => format!(
                    "your {action} rule `{}` asks first ({})",
                    rule.resource,
                    rule.source_layer.as_str().replace('_', " ")
                ),
                None => format!("no rule allows {action} here, so it asks first"),
            };
            if asking.len() > 1 {
                write!(reason, " · {} resources ask", asking.len())
                    .expect("writing to a String cannot fail");
            }
            reason
        }
    }
}

/// A fixed-width label beside wrapped text, continuation rows hung under
/// the text column.
fn labeled_rows(label: Span<'static>, text: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let label_width = UnicodeWidthStr::width(label.content.as_ref());
    let pad = APPROVAL_LABEL_COLUMNS.saturating_sub(label_width);
    let room = u16::try_from(usize::from(width).saturating_sub(APPROVAL_LABEL_COLUMNS))
        .unwrap_or(u16::MAX)
        .max(1);
    let mut label = Some(label);
    wrapped_line(text, room)
        .into_iter()
        .map(|mut line| {
            let lead = match label.take() {
                Some(label) => vec![label, Span::raw(" ".repeat(pad))],
                None => vec![Span::raw(" ".repeat(APPROVAL_LABEL_COLUMNS))],
            };
            line.spans.splice(0..0, lead);
            line
        })
        .collect()
}

fn plain_rows(lines: Vec<Line<'static>>) -> Vec<ApprovalPreviewRow> {
    lines
        .into_iter()
        .map(|line| ApprovalPreviewRow { line, band: None })
        .collect()
}

/// The scrolling body of the panel and the index of its `details` toggle
/// row. `width` is the text width inside the panel's padding.
fn approval_body(
    approval: &ApprovalState,
    tool: Option<&crate::state::ToolCallState>,
    panel: &ApprovalPanel,
    width: u16,
    theme: &Theme,
) -> (Vec<ApprovalPreviewRow>, usize) {
    let mut rows = Vec::new();
    let first_action = approval.resources[0].capability;
    let title = tool.map_or_else(
        || crate::ui::management::action_label(first_action).to_owned(),
        |tool| tool.presentation.title.as_str().to_owned(),
    );
    let mut hero = vec![
        Span::raw(approval_tool_icon(&title)),
        Span::raw(" "),
        Span::styled(title.clone(), theme.heading()),
    ];
    // Bash's argument is its command, which the preview shows in full.
    if title != "bash"
        && let Some(argument) = tool.and_then(|tool| tool.presentation.primary_argument.as_ref())
    {
        hero.push(Span::raw("  "));
        hero.push(Span::styled(argument.as_str().to_owned(), theme.user()));
    }
    rows.extend(plain_rows(wrapped_line(Line::from(hero), width)));
    rows.push(ApprovalPreviewRow {
        line: Line::default(),
        band: None,
    });

    let preview = tool.and_then(|tool| approval_operation_preview(tool, width, theme));
    let show_resources = preview.is_none() || approval.resources.len() > 1;
    if let Some(preview) = preview {
        rows.extend(preview);
    }
    if show_resources {
        let band = theme.code_background();
        for resource in &approval.resources {
            let normalized = approval
                .evaluations
                .iter()
                .find(|evaluation| evaluation.resource_digest == resource.binding_digest)
                .map_or_else(
                    || resource.canonical.as_str(),
                    |evaluation| evaluation.trace.normalized_resource.as_str(),
                );
            let label = Span::styled(
                format!(
                    " {}",
                    crate::ui::management::action_label(resource.capability)
                ),
                theme.code_gutter(),
            );
            rows.extend(
                labeled_rows(
                    label,
                    Line::styled(normalized.to_owned(), theme.body()),
                    width,
                )
                .into_iter()
                .map(|line| ApprovalPreviewRow { line, band }),
            );
        }
    }
    rows.push(ApprovalPreviewRow {
        line: Line::default(),
        band: None,
    });

    rows.extend(plain_rows(labeled_rows(
        Span::styled("why", theme.muted()),
        Line::styled(approval_reason(approval), theme.muted_text()),
        width,
    )));
    if let Some(focus) = panel.focus {
        let (glyph, _, short) = decision_labels(focus);
        let tone = theme.decision(decision_tone(focus), false);
        rows.extend(plain_rows(labeled_rows(
            Span::styled(format!("{glyph} {}", short.to_lowercase()), tone),
            Line::styled(decision_effect(focus, approval), theme.body()),
            width,
        )));
    }
    rows.push(ApprovalPreviewRow {
        line: Line::default(),
        band: None,
    });

    let details_row = rows.len();
    rows.push(ApprovalPreviewRow {
        line: Line::from(vec![
            Span::styled(
                if panel.details {
                    "▾ details"
                } else {
                    "▸ details"
                },
                theme.muted(),
            ),
            Span::styled(
                "  ids, fingerprints, and the policy trace · d",
                theme.internal(),
            ),
        ]),
        band: None,
    });
    if panel.details {
        // The banner and consent target repeat what the panel already leads
        // with; the rest is the full prepared identity.
        let content = approval_content(approval);
        for line in content.lines().skip(2) {
            rows.extend(plain_rows(wrapped_line(
                Line::styled(line.to_owned(), approval_line_style(line, theme)),
                width,
            )));
        }
    }
    (rows, details_row)
}

/// One decision button: glyph and label in the decision's tone, then its
/// key, all on one pill. The focused button is filled with its tone.
fn button_spans(
    decision: ApprovalUserDecision,
    detail: usize,
    focused: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let (glyph, full, short) = decision_labels(decision);
    let key = approval_hotkey(decision);
    let label = match detail {
        0 => format!(" {glyph} {full} "),
        1 => format!(" {glyph} {short} "),
        _ => format!(" {glyph} "),
    };
    let tone = theme.decision(decision_tone(decision), focused);
    let (label_style, key_style) = if focused {
        (tone, tone.remove_modifier(Modifier::BOLD))
    } else {
        let pill = theme
            .code_background()
            .map_or_else(Style::default, |background| Style::default().bg(background));
        (pill.patch(tone), pill.patch(theme.muted()))
    };
    vec![
        Span::styled(label, label_style),
        Span::styled(format!("{key} "), key_style),
    ]
}

/// The decision buttons laid out left to right with a two-column gap, at
/// the most detailed label size that fits `area`. Hit rects cover exactly
/// each painted pill.
fn layout_buttons(
    decisions: &[ApprovalUserDecision],
    focus: Option<ApprovalUserDecision>,
    area: Rect,
    theme: &Theme,
) -> Vec<(ApprovalHit, Vec<Span<'static>>)> {
    let span_width = |spans: &[Span<'_>]| {
        spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>()
    };
    let gap = 2;
    let detail = (0..3)
        .find(|detail| {
            let total = decisions
                .iter()
                .map(|decision| span_width(&button_spans(*decision, *detail, false, theme)))
                .sum::<usize>()
                + gap * decisions.len().saturating_sub(1);
            total <= usize::from(area.width)
        })
        .unwrap_or(2);
    let mut x = area.x;
    let right = area.x.saturating_add(area.width);
    decisions
        .iter()
        .filter_map(|decision| {
            let spans = button_spans(*decision, detail, focus == Some(*decision), theme);
            let width = u16::try_from(span_width(&spans)).unwrap_or(u16::MAX);
            let visible = width.min(right.saturating_sub(x));
            if visible == 0 {
                return None;
            }
            let hit = ApprovalHit {
                rect: Rect::new(x, area.y, visible, 1),
                decision: *decision,
            };
            x = x.saturating_add(width).saturating_add(gap as u16);
            Some((hit, spans))
        })
        .collect()
}

/// The footer hint for the bottom border, the longest that fits.
fn approval_hints(note_open: bool, scrollable: bool, width: u16) -> String {
    let variants: &[&str] = match (note_open, scrollable) {
        (true, _) => &["⏎ reject with this note · esc back", "⏎ send · esc back"],
        (false, true) => &[
            "←→ choose · ⏎ confirm · e note · d details · ↑↓ scroll",
            "←→ choose · ⏎ confirm · e note · d details",
            "←→ ⏎ · e note · d details",
        ],
        (false, false) => &[
            "←→ choose · ⏎ confirm · e note · d details",
            "←→ ⏎ · e note · d details",
        ],
    };
    let room = usize::from(width.saturating_sub(4));
    variants
        .iter()
        .find(|hint| UnicodeWidthStr::width(**hint) <= room)
        .map_or_else(String::new, |hint| (*hint).to_owned())
}

/// The note sent with a rejection: trimmed, control-sanitized, and bounded.
/// An empty note sends a plain rejection.
fn approval_feedback(note: &str) -> Option<cookie_agent_protocol::ApprovalFeedback> {
    let note = note.trim();
    (!note.is_empty()).then(|| cookie_agent_protocol::ApprovalFeedback {
        message: cookie_agent_protocol::diagnostics::headline(note),
    })
}

/// Paint `band` across `row` of `area` (the panel interior), so tinted rows
/// read as one band behind the padded text.
fn paint_band(frame: &mut ratatui::Frame, area: Rect, row: u16, band: Option<Color>) {
    let Some(band) = band else {
        return;
    };
    let clip = Rect::new(area.x, row, area.width, 1).intersection(frame.area());
    let buffer = frame.buffer_mut();
    for x in clip.left()..clip.right() {
        buffer[(x, row)].set_bg(band);
    }
}

/// The decision as the status line names it.
fn decision_name(decision: ApprovalUserDecision) -> &'static str {
    match decision {
        ApprovalUserDecision::ApproveOnce => "approve once",
        ApprovalUserDecision::ApproveTree => "approve all",
        ApprovalUserDecision::Reject => "reject",
        ApprovalUserDecision::Cancel => "cancel",
    }
}

impl App {
    /// Keys while an approval is the topmost panel. The panel owns the
    /// keyboard: arrows and Tab move focus between the decisions, Enter
    /// answers with the focused one, letter hotkeys answer directly, `d`
    /// toggles the details, `e` opens a rejection note, Esc cancels (when
    /// allowed), and Ctrl-C cancels the run. Anything else is dropped
    /// rather than typed into the composer hidden behind the panel.
    pub(super) async fn handle_approval_key(&mut self, key: KeyEvent) {
        let Some(approval) = self.current_approval().cloned() else {
            return;
        };
        self.approval_panel.sync(&approval);
        self.approval_panel.notice = None;
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.cancel_active_run();
            return;
        }
        if self.approval_panel.note.is_some() {
            self.handle_approval_note_key(key, &approval).await;
            return;
        }
        if is_approval_scroll_key(key.code) {
            self.handle_approval_scroll_key(key.code);
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => self.move_approval_focus(&approval, true),
            KeyCode::Right | KeyCode::Tab => self.move_approval_focus(&approval, false),
            KeyCode::Enter => {
                if let Some(decision) = self.approval_panel.focus {
                    self.answer_approval_when_armed(&approval, decision, None)
                        .await;
                }
            }
            KeyCode::Esc => {
                if approval.constraints.cancellable {
                    self.answer_approval(ApprovalUserDecision::Cancel).await;
                }
            }
            KeyCode::Char(character) if is_printable_key(key) => {
                match character.to_ascii_lowercase() {
                    'd' => self.toggle_approval_details(),
                    // The note swallows typing, so it waits out the grace
                    // like an answer: words already on their way to the
                    // composer must not land in it.
                    'e' if !self.approval_hotkeys_armed(&approval, Instant::now()) => {
                        self.approval_panel.notice = Some("just appeared · press again".into());
                    }
                    'e' => {
                        self.approval_panel.focus = Some(ApprovalUserDecision::Reject);
                        self.approval_panel.note = Some(InputState::default());
                    }
                    _ => {
                        if let Some(decision) = approval_hotkey_decision(character, &approval) {
                            self.answer_approval_when_armed(&approval, decision, None)
                                .await;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Keys while the rejection note is open: it takes all typing, Enter
    /// rejects with it, and Esc closes it without answering.
    async fn handle_approval_note_key(&mut self, key: KeyEvent, approval: &ApprovalState) {
        match key.code {
            KeyCode::Esc => self.approval_panel.note = None,
            KeyCode::Enter if key.modifiers.is_empty() => {
                let feedback = self
                    .approval_panel
                    .note
                    .as_ref()
                    .and_then(|note| approval_feedback(note.as_str()));
                self.answer_approval_when_armed(approval, ApprovalUserDecision::Reject, feedback)
                    .await;
            }
            KeyCode::PageUp | KeyCode::PageDown => self.handle_approval_scroll_key(key.code),
            _ if is_newline_key(key) => {}
            _ => {
                if let Some(note) = &mut self.approval_panel.note {
                    super::keys::edit_plain_input(note, key);
                }
            }
        }
    }

    fn move_approval_focus(&mut self, approval: &ApprovalState, backward: bool) {
        let decisions = offered_decisions(approval);
        let current = self
            .approval_panel
            .focus
            .and_then(|focus| decisions.iter().position(|decision| *decision == focus))
            .unwrap_or(0);
        let next = if backward {
            (current + decisions.len() - 1) % decisions.len()
        } else {
            (current + 1) % decisions.len()
        };
        self.approval_panel.focus = Some(decisions[next]);
    }

    /// Answer from the keyboard only once the request has been on top for
    /// the full grace, so keystrokes aimed at the composer cannot answer it.
    async fn answer_approval_when_armed(
        &mut self,
        approval: &ApprovalState,
        decision: ApprovalUserDecision,
        feedback: Option<cookie_agent_protocol::ApprovalFeedback>,
    ) {
        if self.approval_hotkeys_armed(approval, Instant::now()) {
            self.answer_approval_with(decision, feedback).await;
        } else {
            self.status = "Approval just appeared; press the key again to answer.".into();
            self.approval_panel.notice = Some("just appeared · press again".into());
        }
    }

    /// Record which approval is on top now, restarting the hotkey grace
    /// whenever a different request (or revision) appears. `None` means no
    /// approval is on top, so the next one starts a fresh grace.
    pub(super) fn note_approval_on_top(
        &mut self,
        on_top: Option<(cookie_agent_protocol::ApprovalId, u64)>,
        now: Instant,
    ) {
        self.approval_shown =
            on_top.map(
                |(approval_id, request_revision)| match self.approval_shown {
                    Some(shown)
                        if shown.approval_id == approval_id
                            && shown.request_revision == request_revision =>
                    {
                        shown
                    }
                    _ => ApprovalShown {
                        approval_id,
                        request_revision,
                        at: now,
                    },
                },
            );
    }

    /// Whether `approval` has been on top for the full grace. A request
    /// that was never drawn starts its grace now.
    fn approval_hotkeys_armed(&mut self, approval: &ApprovalState, now: Instant) -> bool {
        self.note_approval_on_top(Some((approval.approval_id, approval.request_revision)), now);
        self.approval_shown
            .is_some_and(|shown| now.duration_since(shown.at) >= APPROVAL_HOTKEY_GRACE)
    }

    pub(super) fn handle_approval_scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.scroll_approval(true, 1),
            KeyCode::Down => self.scroll_approval(false, 1),
            KeyCode::PageUp => self.scroll_approval(true, 10),
            KeyCode::PageDown => self.scroll_approval(false, 10),
            KeyCode::Home => self.approval_panel.scroll = 0,
            KeyCode::End => self.approval_panel.scroll = self.approval_panel.max_scroll,
            _ => {}
        }
    }

    pub(super) fn scroll_approval(&mut self, up: bool, lines: u16) {
        let panel = &mut self.approval_panel;
        panel.scroll = if up {
            panel.scroll.saturating_sub(lines)
        } else {
            panel.scroll.saturating_add(lines).min(panel.max_scroll)
        };
    }

    pub(super) fn toggle_approval_details(&mut self) {
        self.approval_panel.details = !self.approval_panel.details;
    }

    /// Optimistic approval response: the modal is dismissed immediately and
    /// the exact (id, revision, fingerprint, decision) tuple is sent
    /// asynchronously. Nothing executes locally; failures restore the modal
    /// only when the request is still durably escalated and unexpired.
    pub(in crate::ui) async fn answer_approval(&mut self, decision: ApprovalUserDecision) {
        self.answer_approval_with(decision, None).await;
    }

    /// [`Self::answer_approval`] with an optional note, which the protocol
    /// accepts only with a rejection.
    async fn answer_approval_with(
        &mut self,
        decision: ApprovalUserDecision,
        feedback: Option<cookie_agent_protocol::ApprovalFeedback>,
    ) {
        if self.pending_approval.is_some() {
            return;
        }
        let Some(approval) = self.current_approval().cloned() else {
            return;
        };
        let feedback = feedback.filter(|_| decision == ApprovalUserDecision::Reject);
        self.next_approval_request_id = self.next_approval_request_id.wrapping_add(1);
        let request_id = self.next_approval_request_id;
        let noted = if feedback.is_some() {
            " with a note"
        } else {
            ""
        };
        self.pending_approval = Some(PendingApprovalSubmission {
            request_id,
            approval: approval.clone(),
            decision,
        });
        self.status = format!("Approval submitted ({}{noted})…", decision_name(decision));
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
                    feedback,
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
                self.status = format!(
                    "approval response accepted ({})",
                    decision_name(pending.decision)
                );
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
                    self.approval_panel.notice = Some(format!("not sent: {}", error.message));
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

    /// Drop a pending approval submission that is no longer exactly
    /// pending. Returns whether one was dropped.
    pub(in crate::ui) fn reconcile_pending_approval(&mut self) -> bool {
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
        stale
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

    /// The selected session's running run, if any: what Ctrl-C interrupts.
    pub(in crate::ui) fn selected_active_run(&self) -> Option<cookie_agent_protocol::RunId> {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .and_then(|state| state.active_run)
    }

    pub(in crate::ui) fn cancel_active_run(&mut self) {
        let Some(run_id) = self.selected_active_run() else {
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

    /// The tool call an approval gates: the call whose prepared operation
    /// carries the same fingerprint, preferring one still running when an
    /// identical call repeated.
    fn approval_tool(&self, approval: &ApprovalState) -> Option<crate::state::ToolCallState> {
        let tools = &self.store.sessions.get(&approval.session_id)?.tools;
        let mut matching = tools
            .values()
            .filter(|tool| tool.operation_fingerprint == approval.operation_fingerprint);
        let first = matching.next()?;
        if first.status == ToolStatus::Running {
            return Some(first.clone());
        }
        Some(
            matching
                .find(|tool| tool.status == ToolStatus::Running)
                .unwrap_or(first)
                .clone(),
        )
    }

    /// Draw the approval panel docked with its bottom edge at `dock_bottom`
    /// (the composer's bottom row), sized to its content and capped so a
    /// few conversation rows stay visible. Returns the decision buttons'
    /// hit rects; the panel and details-toggle rects go to the hit map.
    pub(in crate::ui) fn render_approval(
        &mut self,
        frame: &mut ratatui::Frame,
        approval: &ApprovalState,
        screen: Rect,
        dock_bottom: u16,
    ) -> Vec<ApprovalHit> {
        self.approval_panel.sync(approval);
        let tool = self.approval_tool(approval);
        let theme = &self.theme;
        let text_width = screen.width.saturating_sub(4).max(1);
        let (rows, details_row) = approval_body(
            approval,
            tool.as_ref(),
            &self.approval_panel,
            text_width,
            theme,
        );
        // The note field is three rows under a one-row gap.
        let note_height = if self.approval_panel.note.is_some() {
            4
        } else {
            0
        };
        // Borders, a blank top row, the body, a gap, the note, the buttons.
        let footer_height = 2 + note_height;
        let desired = u16::try_from(rows.len())
            .unwrap_or(u16::MAX)
            .saturating_add(3 + footer_height);
        let available = dock_bottom.saturating_sub(screen.y);
        let cap = available
            .saturating_sub(APPROVAL_MIN_CONVERSATION_ROWS)
            .max(available.min(8 + footer_height));
        let height = desired.min(cap);
        let area = Rect::new(screen.x, dock_bottom - height, screen.width, height);
        self.hit_map.approval = Some(area);
        paint_panel(frame, area, theme);

        let inner = inner_rect(area);
        let content = Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        let body = Rect::new(
            content.x,
            content.y.saturating_add(1).min(content.bottom()),
            content.width,
            content.height.saturating_sub(1 + footer_height),
        );
        let line_count = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        self.approval_panel.max_scroll = line_count.saturating_sub(body.height);
        self.approval_panel.scroll = self
            .approval_panel
            .scroll
            .min(self.approval_panel.max_scroll);
        let scroll = self.approval_panel.scroll;

        let mut top_right = Vec::new();
        let queued = self
            .store
            .sessions
            .get(&approval.session_id)
            .map_or(0, |state| {
                state
                    .approvals
                    .iter()
                    .filter(|candidate| candidate.is_visible_user_escalation())
                    .count()
            });
        if queued > 1 {
            top_right.push(Span::styled(format!("1 of {queued}"), theme.muted()));
        }
        if let Some(seconds) = seconds_left(approval) {
            if !top_right.is_empty() {
                top_right.push(Span::styled(" · ", theme.muted()));
            }
            let style = if seconds <= 10 {
                theme.error()
            } else {
                theme.muted()
            };
            top_right.push(Span::styled(
                format!("expires in {}", countdown_label(seconds)),
                style,
            ));
        }
        let mut bottom_left = Vec::new();
        if self.approval_panel.max_scroll > 0 {
            bottom_left.push(Span::styled(
                format!(
                    "{}–{}/{line_count}",
                    scroll.saturating_add(1),
                    scroll.saturating_add(body.height).min(line_count)
                ),
                theme.internal(),
            ));
        }
        let hints = approval_hints(
            self.approval_panel.note.is_some(),
            self.approval_panel.max_scroll > 0,
            area.width,
        );
        let mut block = crate::ui::panel_block()
            .border_style(theme.warning())
            .title(crate::ui::fitted_panel_title(
                Span::styled("⚠ Permission needed", theme.warning()),
                area.width,
            ))
            .style(theme.panel());
        if !top_right.is_empty() {
            block = block.title(crate::ui::panel_title(
                Line::from(top_right).right_aligned(),
            ));
        }
        if !bottom_left.is_empty() {
            block = block.title_bottom(crate::ui::panel_title(Line::from(bottom_left)));
        }
        if !hints.is_empty() {
            block = block.title_bottom(crate::ui::panel_title(
                Line::from(Span::styled(hints, theme.internal())).right_aligned(),
            ));
        }
        frame.render_widget(block, area);

        let mut details_hit = None;
        for (offset, row) in rows
            .into_iter()
            .skip(usize::from(scroll))
            .take(usize::from(body.height))
            .enumerate()
        {
            let index = usize::from(scroll) + offset;
            let y = body.y + offset as u16;
            paint_band(frame, inner, y, row.band);
            let rect = Rect::new(body.x, y, body.width, 1);
            if index == details_row {
                details_hit = Some(rect);
            }
            frame.render_widget(Paragraph::new(row.line), rect);
        }
        self.hit_map.approval_details = details_hit;

        let buttons_row = Rect::new(
            content.x,
            content.bottom().saturating_sub(1),
            content.width,
            1,
        );
        if let Some(note) = &mut self.approval_panel.note {
            let note_area = Rect::new(
                inner.x,
                buttons_row.y.saturating_sub(note_height),
                inner.width,
                note_height - 1,
            );
            crate::ui::input::render(
                frame,
                note_area,
                note,
                true,
                Span::styled("Note for the agent", self.theme.heading()),
                Some("Why not, or what to do instead…"),
                &self.theme,
            );
        }
        let buttons = layout_buttons(
            &offered_decisions(approval),
            self.approval_panel.focus,
            buttons_row,
            &self.theme,
        );
        let buttons_end = buttons
            .last()
            .map_or(buttons_row.x, |(hit, _)| hit.rect.right());
        if let Some(notice) = &self.approval_panel.notice {
            let room = buttons_row
                .right()
                .saturating_sub(buttons_end.saturating_add(2));
            let notice = truncate_with_ellipsis(notice, usize::from(room));
            let width = u16::try_from(UnicodeWidthStr::width(notice.as_str())).unwrap_or(room);
            frame.render_widget(
                Paragraph::new(Span::styled(notice, self.theme.warning())),
                Rect::new(buttons_row.right() - width, buttons_row.y, width, 1),
            );
        }
        buttons
            .into_iter()
            .map(|(hit, spans)| {
                frame.render_widget(Paragraph::new(Line::from(spans)), hit.rect);
                hit
            })
            .collect()
    }
}
