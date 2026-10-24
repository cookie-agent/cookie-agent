//! The question panel: an agent's question to the user (ACP elicitation,
//! `docs/site/specs/acp-agents.md` section 15).
//!
//! It docks where the approval panel does, above the status bar in place
//! of the composer, whenever the viewed session has a question waiting and
//! no approval is on top. A form shows one block per field, in the form's
//! order; Tab moves between the fields and the buttons, ↑↓ between a
//! field's choices, Space picks a choice or flips a box (Enter picks and
//! moves on), and Enter on a button answers. A URL question shows the whole URL and its host,
//! which `c` copies. Esc cancels the question and Ctrl-C the run. Like an
//! approval's hotkeys, the answers wait out a short grace after the
//! question appears.

use super::*;

use cookie_agent_client_state::elicitation::{
    DraftValue, FieldProblem, FormDraft, QuestionKind, QuestionState, field_hint, url_host,
    url_warnings,
};
use cookie_agent_protocol::{
    ElicitationAction, ElicitationForm, ElicitationId, ElicitationInput, ElicitationRespondParams,
};

/// Rows of the conversation the docked panel always leaves visible.
const QUESTION_MIN_CONVERSATION_ROWS: u16 = 4;

/// A button of the question panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum QuestionButton {
    /// Send the form, or agree to open the URL (`accept`).
    Submit,
    /// Copy a URL question's URL.
    CopyLink,
    Decline,
    Cancel,
}

impl QuestionButton {
    fn label(self, kind: QuestionKind<'_>) -> &'static str {
        match (self, kind) {
            (Self::Submit, QuestionKind::Url(_)) => "✓ Done",
            (Self::Submit, _) => "✓ Submit",
            (Self::CopyLink, _) => "⧉ Copy link (c)",
            (Self::Decline, _) => "✗ Decline",
            (Self::Cancel, _) => "Cancel (esc)",
        }
    }

    fn tone(self) -> crate::theme::DecisionTone {
        match self {
            Self::Submit => crate::theme::DecisionTone::Allow,
            Self::Decline => crate::theme::DecisionTone::Deny,
            Self::CopyLink | Self::Cancel => crate::theme::DecisionTone::Neutral,
        }
    }
}

/// The buttons a question offers, left to right.
pub(in crate::ui) fn question_buttons(kind: QuestionKind<'_>) -> Vec<QuestionButton> {
    match kind {
        QuestionKind::Form(_) => vec![
            QuestionButton::Submit,
            QuestionButton::Decline,
            QuestionButton::Cancel,
        ],
        QuestionKind::Url(_) => vec![
            QuestionButton::CopyLink,
            QuestionButton::Submit,
            QuestionButton::Decline,
            QuestionButton::Cancel,
        ],
        QuestionKind::Unsupported => vec![QuestionButton::Decline, QuestionButton::Cancel],
    }
}

/// What has the keyboard in the panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum QuestionFocus {
    /// A form field, by index.
    Field(usize),
    /// A button, by its index in [`question_buttons`].
    Button(usize),
}

/// The panel's interaction state; it starts fresh for each question.
#[derive(Default)]
pub(in crate::ui) struct QuestionPanel {
    question: Option<ElicitationId>,
    /// When the question was first drawn on top.
    shown_at: Option<Instant>,
    pub(in crate::ui) draft: Option<FormDraft>,
    /// The text of each text-like field, index-aligned with the form.
    pub(in crate::ui) inputs: Vec<InputState>,
    /// The highlighted choice of each choice field.
    pub(in crate::ui) cursors: Vec<usize>,
    pub(in crate::ui) focus: Option<QuestionFocus>,
    /// The field that did not fit, from the last submit.
    pub(in crate::ui) problem: Option<FieldProblem>,
    /// A message beside the buttons.
    pub(in crate::ui) notice: Option<String>,
    /// An answer is on its way.
    pub(in crate::ui) sending: bool,
    pub(in crate::ui) scroll: u16,
    max_scroll: u16,
}

impl QuestionPanel {
    /// Reset for `question` when it was not the one shown.
    pub(super) fn sync(&mut self, question: &QuestionState) {
        if self.question == Some(question.request.elicitation_id) {
            return;
        }
        let kind = question.kind();
        *self = Self {
            question: Some(question.request.elicitation_id),
            ..Self::default()
        };
        match kind {
            QuestionKind::Form(form) => {
                let draft = FormDraft::new(form);
                self.inputs = draft
                    .values
                    .iter()
                    .map(|value| {
                        let mut input = InputState::default();
                        if let DraftValue::Text(text) = value {
                            input.insert_text(text);
                        }
                        input
                    })
                    .collect();
                self.cursors = draft
                    .values
                    .iter()
                    .map(|value| match value {
                        DraftValue::Choice(Some(index)) => *index,
                        _ => 0,
                    })
                    .collect();
                self.focus = Some(if form.fields.is_empty() {
                    QuestionFocus::Button(0)
                } else {
                    QuestionFocus::Field(0)
                });
                self.draft = Some(draft);
            }
            // A URL question starts on Copy link, never on its answer.
            QuestionKind::Url(_) | QuestionKind::Unsupported => {
                self.focus = Some(QuestionFocus::Button(0));
            }
        }
    }

    /// The draft with the typed texts in it.
    fn current_draft(&self) -> Option<FormDraft> {
        let mut draft = self.draft.clone()?;
        for (value, input) in draft.values.iter_mut().zip(&self.inputs) {
            if let DraftValue::Text(text) = value {
                input.as_str().clone_into(text);
            }
        }
        Some(draft)
    }
}

/// The marker of a choice row: a radio for a single choice, a box for a
/// multiple one.
fn choice_marker(multiple: bool, chosen: bool) -> &'static str {
    match (multiple, chosen) {
        (false, true) => "(•)",
        (false, false) => "( )",
        (true, true) => "[x]",
        (true, false) => "[ ]",
    }
}

/// The panel body's rows, and the row each field starts at.
fn question_body(
    question: &QuestionState,
    panel: &QuestionPanel,
    width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut rows = Vec::new();
    let wrap = |rows: &mut Vec<Line<'static>>, line: Line<'static>, indent: usize| {
        let room = width
            .saturating_sub(u16::try_from(indent).unwrap_or(0))
            .max(1);
        for mut wrapped in wrapped_line(line, room) {
            wrapped.spans.insert(0, Span::raw(" ".repeat(indent)));
            rows.push(wrapped);
        }
    };
    for line in question.request.message.lines() {
        wrap(
            &mut rows,
            Line::from(Span::styled(line.to_owned(), theme.body())),
            0,
        );
    }
    let mut starts = Vec::new();
    match question.kind() {
        QuestionKind::Url(url) => {
            rows.push(Line::default());
            let host = url_host(url).unwrap_or("(no host)").to_owned();
            rows.push(Line::from(vec![
                Span::styled("Opens  ", theme.muted()),
                Span::styled(host, theme.heading()),
            ]));
            wrap(
                &mut rows,
                Line::from(Span::styled(url.to_owned(), theme.link())),
                7,
            );
            for warning in url_warnings(url) {
                wrap(
                    &mut rows,
                    Line::from(Span::styled(format!("⚠ {warning}"), theme.warning())),
                    7,
                );
            }
            rows.push(Line::from(Span::styled(
                "Open it in your browser, then choose Done when you have finished there.",
                theme.muted(),
            )));
        }
        QuestionKind::Unsupported => {
            rows.push(Line::from(Span::styled(
                "This client cannot show this kind of question: decline or cancel it.",
                theme.warning(),
            )));
        }
        QuestionKind::Form(form) => {
            for line in form.title.iter().chain(&form.description) {
                wrap(
                    &mut rows,
                    Line::from(Span::styled(line.clone(), theme.muted())),
                    0,
                );
            }
            let draft = panel.current_draft();
            for (index, field) in form.fields.iter().enumerate() {
                rows.push(Line::default());
                starts.push(rows.len());
                let focused = panel.focus == Some(QuestionFocus::Field(index));
                let pointer = if focused { "› " } else { "  " };
                let mut heading = vec![
                    Span::styled(pointer, theme.warning()),
                    Span::styled(field.label().to_owned(), theme.heading()),
                ];
                let hint = field_hint(field);
                if !hint.is_empty() {
                    heading.push(Span::styled(format!("  {hint}"), theme.muted()));
                }
                wrap(&mut rows, Line::from(heading), 0);
                if let Some(description) = &field.description {
                    wrap(
                        &mut rows,
                        Line::from(Span::styled(description.clone(), theme.muted())),
                        4,
                    );
                }
                let value = draft
                    .as_ref()
                    .and_then(|draft| draft.values.get(index))
                    .cloned()
                    .unwrap_or(DraftValue::Unsupported);
                let cursor = panel.cursors.get(index).copied().unwrap_or(0);
                match (field.input.known(), value) {
                    (
                        Some(
                            ElicitationInput::Choice { choices, .. }
                            | ElicitationInput::MultiChoice { choices, .. },
                        ),
                        value,
                    ) => {
                        let multiple = matches!(value, DraftValue::MultiChoice(_));
                        for (choice_index, choice) in choices.iter().enumerate() {
                            let chosen = match &value {
                                DraftValue::Choice(chosen) => *chosen == Some(choice_index),
                                DraftValue::MultiChoice(chosen) => {
                                    chosen.get(choice_index).copied().unwrap_or(false)
                                }
                                _ => false,
                            };
                            let highlighted = focused && cursor == choice_index;
                            let style = if highlighted {
                                theme.selected()
                            } else {
                                theme.body()
                            };
                            let mut spans = vec![Span::styled(
                                format!("{} {}", choice_marker(multiple, chosen), choice.title),
                                style,
                            )];
                            if let Some(description) = &choice.description {
                                spans
                                    .push(Span::styled(format!(" — {description}"), theme.muted()));
                            }
                            wrap(&mut rows, Line::from(spans), 4);
                        }
                    }
                    (Some(ElicitationInput::Boolean { .. }), DraftValue::Boolean(on)) => {
                        let style = if focused {
                            theme.selected()
                        } else {
                            theme.body()
                        };
                        rows.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(
                                format!(
                                    "{} {}",
                                    choice_marker(true, on),
                                    if on { "yes" } else { "no" }
                                ),
                                style,
                            ),
                        ]));
                    }
                    (Some(_), DraftValue::Text(_)) => {
                        let text = panel
                            .inputs
                            .get(index)
                            .map(|input| input.as_str().to_owned())
                            .unwrap_or_default();
                        let caret = if focused { "▏" } else { "" };
                        let shown = if text.is_empty() && !focused {
                            Span::styled("(empty)", theme.internal())
                        } else {
                            Span::styled(format!("{text}{caret}"), theme.body())
                        };
                        rows.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled("[ ", theme.muted()),
                            shown,
                            Span::styled(" ]", theme.muted()),
                        ]));
                    }
                    _ => rows.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("this client cannot fill it in", theme.internal()),
                    ])),
                }
                if let Some(problem) = panel
                    .problem
                    .as_ref()
                    .filter(|problem| problem.index == index)
                {
                    wrap(
                        &mut rows,
                        Line::from(Span::styled(
                            format!("✗ {}", problem.message),
                            theme.error(),
                        )),
                        4,
                    );
                }
            }
        }
    }
    (rows, starts)
}

/// The footer hint for the bottom border, the longest that fits.
fn question_hints(kind: QuestionKind<'_>, width: u16) -> String {
    let variants: &[&str] = match kind {
        QuestionKind::Form(_) => &[
            "tab next · ↑↓ choose · space pick · ⏎ next/press · esc cancel",
            "tab · ↑↓ · space · ⏎ · esc",
        ],
        _ => &["←→ choose · ⏎ press · esc cancel", "←→ ⏎ · esc"],
    };
    let room = usize::from(width.saturating_sub(4));
    variants
        .iter()
        .find(|hint| UnicodeWidthStr::width(**hint) <= room)
        .map_or_else(String::new, |hint| (*hint).to_owned())
}

/// Whether a field of `form` takes typed text.
fn is_text_field(form: &ElicitationForm, index: usize) -> bool {
    matches!(
        form.fields.get(index).and_then(|field| field.input.known()),
        Some(
            ElicitationInput::Text { .. }
                | ElicitationInput::Number { .. }
                | ElicitationInput::Integer { .. }
        )
    )
}

/// How many choices field `index` of `form` has, for a choice field.
fn choice_count(form: &ElicitationForm, index: usize) -> Option<usize> {
    match form.fields.get(index)?.input.known()? {
        ElicitationInput::Choice { choices, .. }
        | ElicitationInput::MultiChoice { choices, .. } => Some(choices.len()),
        _ => None,
    }
}

impl App {
    /// The question on top: the viewed session's oldest waiting one, while
    /// no approval is.
    pub(in crate::ui) fn current_question(&self) -> Option<&QuestionState> {
        if self.current_approval().is_some() || self.pending_approval.is_some() {
            return None;
        }
        self.selected
            .and_then(|id| self.store.sessions.get(&id))
            .and_then(|state| state.questions.first())
    }

    /// Whether the question on top has been shown for the full grace, so
    /// keys aimed at the composer cannot answer it.
    fn question_armed(&mut self, now: Instant) -> bool {
        let shown = *self.question_panel.shown_at.get_or_insert(now);
        now.duration_since(shown) >= APPROVAL_HOTKEY_GRACE
    }

    /// Backdate the question's appearance past the grace.
    #[cfg(test)]
    pub(crate) fn arm_question_for_test(&mut self) {
        if let Some(question) = self.current_question().cloned() {
            self.question_panel.sync(&question);
        }
        self.question_panel.shown_at = Instant::now().checked_sub(APPROVAL_HOTKEY_GRACE * 2);
    }

    /// Keys while a question is on top: the panel owns the keyboard.
    pub(super) async fn handle_question_key(&mut self, key: KeyEvent) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        self.question_panel.sync(&question);
        self.question_panel.notice = None;
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.cancel_active_run();
            return;
        }
        let kind = question.kind();
        let buttons = question_buttons(kind);
        let fields = match kind {
            QuestionKind::Form(form) => form.fields.len(),
            _ => 0,
        };
        let focus = self
            .question_panel
            .focus
            .unwrap_or(QuestionFocus::Button(0));
        let stops = fields + buttons.len();
        let position = match focus {
            QuestionFocus::Field(index) => index,
            QuestionFocus::Button(index) => fields + index,
        };
        let at = |position: usize| {
            if position < fields {
                QuestionFocus::Field(position)
            } else {
                QuestionFocus::Button(position - fields)
            }
        };
        let step = |forward: bool| {
            if forward {
                at((position + 1) % stops)
            } else {
                at((position + stops - 1) % stops)
            }
        };
        match (key.code, focus) {
            (KeyCode::Esc, _) => {
                self.answer_question(&question, QuestionButton::Cancel)
                    .await
            }
            (KeyCode::Tab, _) => self.question_panel.focus = Some(step(true)),
            (KeyCode::BackTab, _) => self.question_panel.focus = Some(step(false)),
            (KeyCode::Left, QuestionFocus::Button(_)) => {
                self.question_panel.focus = Some(step(false));
            }
            (KeyCode::Right, QuestionFocus::Button(_)) => {
                self.question_panel.focus = Some(step(true));
            }
            (KeyCode::Enter, QuestionFocus::Button(index)) => {
                if let Some(button) = buttons.get(index) {
                    self.answer_question(&question, *button).await;
                }
            }
            (KeyCode::Char('c'), QuestionFocus::Button(_))
                if matches!(kind, QuestionKind::Url(_)) =>
            {
                self.answer_question(&question, QuestionButton::CopyLink)
                    .await;
            }
            (KeyCode::Up | KeyCode::Down, QuestionFocus::Button(_)) => {
                self.question_panel.focus = Some(step(key.code == KeyCode::Down));
            }
            (_, QuestionFocus::Field(index)) => {
                let QuestionKind::Form(form) = kind else {
                    return;
                };
                self.question_field_key(form, index, key, step(true), step(false));
            }
            _ => {}
        }
    }

    /// A key on form field `index`: `next` and `previous` are the focus
    /// stops around it.
    fn question_field_key(
        &mut self,
        form: &ElicitationForm,
        index: usize,
        key: KeyEvent,
        next: QuestionFocus,
        previous: QuestionFocus,
    ) {
        let panel = &mut self.question_panel;
        panel.problem = None;
        if is_text_field(form, index) {
            match key.code {
                KeyCode::Enter | KeyCode::Down => panel.focus = Some(next),
                KeyCode::Up => panel.focus = Some(previous),
                _ => {
                    if let Some(input) = panel.inputs.get_mut(index) {
                        super::keys::edit_plain_input(input, key);
                    }
                }
            }
            return;
        }
        if let Some(count) = choice_count(form, index) {
            let cursor = panel.cursors.get(index).copied().unwrap_or(0);
            match key.code {
                KeyCode::Up if cursor == 0 => panel.focus = Some(previous),
                KeyCode::Up => panel.cursors[index] = cursor - 1,
                KeyCode::Down if cursor + 1 >= count => panel.focus = Some(next),
                KeyCode::Down => panel.cursors[index] = cursor + 1,
                KeyCode::Char(' ') | KeyCode::Enter => {
                    let Some(value) = panel
                        .draft
                        .as_mut()
                        .and_then(|draft| draft.values.get_mut(index))
                    else {
                        return;
                    };
                    match value {
                        DraftValue::Choice(chosen) => {
                            *chosen = Some(cursor);
                            if key.code == KeyCode::Enter {
                                panel.focus = Some(next);
                            }
                        }
                        DraftValue::MultiChoice(chosen) => {
                            if let Some(chosen) = chosen.get_mut(cursor) {
                                *chosen = !*chosen;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            return;
        }
        // A yes-or-no box, or a field this client cannot fill in.
        match key.code {
            KeyCode::Down | KeyCode::Enter => panel.focus = Some(next),
            KeyCode::Up => panel.focus = Some(previous),
            KeyCode::Char(' ') => {
                if let Some(DraftValue::Boolean(on)) = panel
                    .draft
                    .as_mut()
                    .and_then(|draft| draft.values.get_mut(index))
                {
                    *on = !*on;
                }
            }
            _ => {}
        }
    }

    /// Presses `button` of `question`: answers it, or copies its URL.
    async fn answer_question(&mut self, question: &QuestionState, button: QuestionButton) {
        if button == QuestionButton::CopyLink {
            if let QuestionKind::Url(url) = question.kind() {
                self.copy_to_clipboard(url.to_owned());
                self.question_panel.notice = Some("link copied".into());
            }
            return;
        }
        if self.question_panel.sending {
            return;
        }
        if !self.question_armed(Instant::now()) {
            self.question_panel.notice = Some("just appeared · press again".into());
            return;
        }
        let (action, content) = match button {
            QuestionButton::Submit => {
                let content = match question.kind() {
                    QuestionKind::Form(form) => {
                        let draft = self
                            .question_panel
                            .current_draft()
                            .unwrap_or_else(|| FormDraft::new(form));
                        match draft.content(form) {
                            Ok(content) => Some(content),
                            Err(problem) => {
                                self.question_panel.notice = Some(problem.message.clone());
                                self.question_panel.focus =
                                    Some(QuestionFocus::Field(problem.index));
                                self.question_panel.problem = Some(problem);
                                return;
                            }
                        }
                    }
                    QuestionKind::Url(_) => None,
                    QuestionKind::Unsupported => return,
                };
                (ElicitationAction::Accept, content)
            }
            QuestionButton::Decline => (ElicitationAction::Decline, None),
            QuestionButton::Cancel | QuestionButton::CopyLink => (ElicitationAction::Cancel, None),
        };
        self.question_panel.sending = true;
        self.status = format!("answering {}…", question.heading());
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let params = ElicitationRespondParams {
            session_id: question.session_id,
            elicitation_id: question.request.elicitation_id,
            action,
            content,
        };
        self.spawn_rpc(async move {
            let session_id = params.session_id;
            let elicitation_id = params.elicitation_id;
            let result = client
                .respond_elicitation(params)
                .await
                .map(|_| ())
                .map_err(|error| (error.error_code().map(str::to_owned), error.to_string()));
            let _ = updates.send(RpcUpdate::QuestionAnswered {
                session_id,
                elicitation_id,
                result,
            });
        });
    }

    /// `elicitation.respond` answered. The question leaves when its
    /// `elicitation_resolved` arrives; one no longer waiting leaves now.
    pub(super) fn finish_question_answer(
        &mut self,
        session_id: SessionId,
        elicitation_id: ElicitationId,
        result: Result<(), (Option<String>, String)>,
    ) {
        if self.question_panel.question == Some(elicitation_id) {
            self.question_panel.sending = false;
        }
        match result {
            Ok(()) => self.status = "answer sent".into(),
            Err((code, message)) => {
                if code.as_deref() == Some("elicitation_not_pending") {
                    if let Some(state) = self.store.sessions.get_mut(&session_id) {
                        state
                            .questions
                            .retain(|question| question.request.elicitation_id != elicitation_id);
                    }
                    self.status = "the question is no longer waiting".into();
                } else {
                    self.status = format!("answer not sent: {message}");
                    if self.question_panel.question == Some(elicitation_id) {
                        self.question_panel.notice = Some(format!("not sent: {message}"));
                    }
                }
            }
        }
    }

    /// Pastes `text`, on one line, into the question's focused text field.
    pub(super) fn paste_into_question(&mut self, text: &str) {
        let Some(question) = self.current_question().cloned() else {
            return;
        };
        self.question_panel.sync(&question);
        let QuestionKind::Form(form) = question.kind() else {
            return;
        };
        if let Some(QuestionFocus::Field(index)) = self.question_panel.focus
            && is_text_field(form, index)
            && let Some(input) = self.question_panel.inputs.get_mut(index)
        {
            input.insert_text(&text.replace(['\r', '\n'], " "));
        }
    }

    pub(super) fn scroll_question(&mut self, up: bool, lines: u16) {
        let panel = &mut self.question_panel;
        panel.scroll = if up {
            panel.scroll.saturating_sub(lines)
        } else {
            panel.scroll.saturating_add(lines).min(panel.max_scroll)
        };
    }

    /// Draws the question panel docked with its bottom edge at
    /// `dock_bottom`, sized to its content and capped so a few
    /// conversation rows stay visible.
    pub(in crate::ui) fn render_question(
        &mut self,
        frame: &mut ratatui::Frame,
        question: &QuestionState,
        screen: Rect,
        dock_bottom: u16,
    ) {
        self.question_panel.sync(question);
        self.question_panel
            .shown_at
            .get_or_insert_with(Instant::now);
        let theme = &self.theme;
        let kind = question.kind();
        let text_width = screen.width.saturating_sub(4).max(1);
        let (rows, starts) = question_body(question, &self.question_panel, text_width, theme);
        let footer_height = 2;
        let desired = u16::try_from(rows.len())
            .unwrap_or(u16::MAX)
            .saturating_add(3 + footer_height);
        let available = dock_bottom.saturating_sub(screen.y);
        let cap = available
            .saturating_sub(QUESTION_MIN_CONVERSATION_ROWS)
            .max(available.min(8 + footer_height));
        let height = desired.min(cap);
        let area = Rect::new(screen.x, dock_bottom - height, screen.width, height);
        self.hit_map.question = Some(area);
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
        let panel = &mut self.question_panel;
        panel.max_scroll = line_count.saturating_sub(body.height);
        // Keep the focused field in view.
        if let Some(QuestionFocus::Field(index)) = panel.focus
            && let Some(start) = starts.get(index)
        {
            let start = u16::try_from(*start).unwrap_or(u16::MAX);
            let end = starts
                .get(index + 1)
                .map_or(line_count, |next| u16::try_from(*next).unwrap_or(u16::MAX));
            if start < panel.scroll {
                panel.scroll = start;
            } else if end > panel.scroll.saturating_add(body.height) {
                panel.scroll = end.saturating_sub(body.height).min(start);
            }
        }
        panel.scroll = panel.scroll.min(panel.max_scroll);
        let scroll = panel.scroll;
        let queued = self
            .selected
            .and_then(|id| self.store.sessions.get(&id))
            .map_or(0, |state| state.questions.len());
        let mut block = crate::ui::panel_block()
            .border_style(theme.warning())
            .title(crate::ui::fitted_panel_title(
                Span::styled(
                    format!("? {} · waiting on you", question.heading()),
                    theme.warning(),
                ),
                area.width,
            ))
            .style(theme.panel());
        if queued > 1 {
            block = block.title(crate::ui::panel_title(
                Line::from(Span::styled(format!("1 of {queued}"), theme.muted())).right_aligned(),
            ));
        }
        if panel.max_scroll > 0 {
            block = block.title_bottom(crate::ui::panel_title(Line::from(Span::styled(
                format!(
                    "{}–{}/{line_count}",
                    scroll.saturating_add(1),
                    scroll.saturating_add(body.height).min(line_count)
                ),
                theme.internal(),
            ))));
        }
        let hints = question_hints(kind, area.width);
        if !hints.is_empty() {
            block = block.title_bottom(crate::ui::panel_title(
                Line::from(Span::styled(hints, theme.internal())).right_aligned(),
            ));
        }
        frame.render_widget(block, area);
        for (offset, row) in rows
            .into_iter()
            .skip(usize::from(scroll))
            .take(usize::from(body.height))
            .enumerate()
        {
            let y = body.y + offset as u16;
            frame.render_widget(Paragraph::new(row), Rect::new(body.x, y, body.width, 1));
        }
        let buttons_row = Rect::new(
            content.x,
            content.bottom().saturating_sub(1),
            content.width,
            1,
        );
        let mut x = buttons_row.x;
        for (index, button) in question_buttons(kind).into_iter().enumerate() {
            let focused = self.question_panel.focus == Some(QuestionFocus::Button(index));
            let tone = theme.decision(button.tone(), focused);
            let style = if focused {
                tone
            } else {
                theme
                    .code_background()
                    .map_or_else(Style::default, |background| Style::default().bg(background))
                    .patch(tone)
            };
            let label = format!(" {} ", button.label(kind));
            let width = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX);
            let visible = width.min(buttons_row.right().saturating_sub(x));
            if visible == 0 {
                break;
            }
            frame.render_widget(
                Paragraph::new(Span::styled(label, style)),
                Rect::new(x, buttons_row.y, visible, 1),
            );
            x = x.saturating_add(width).saturating_add(2);
        }
        let notice = if self.question_panel.sending {
            Some("sending…".to_owned())
        } else {
            self.question_panel.notice.clone()
        };
        if let Some(notice) = notice {
            let room = buttons_row.right().saturating_sub(x);
            let notice = truncate_with_ellipsis(&notice, usize::from(room));
            let width = u16::try_from(UnicodeWidthStr::width(notice.as_str())).unwrap_or(room);
            frame.render_widget(
                Paragraph::new(Span::styled(notice, theme.warning())),
                Rect::new(buttons_row.right() - width, buttons_row.y, width, 1),
            );
        }
    }
}
