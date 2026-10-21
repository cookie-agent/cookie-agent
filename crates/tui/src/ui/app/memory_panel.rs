//! The `/memory` panel (`docs/site/specs/memory.md` section 10): the
//! daemon's memories by layer, grouped by machine and project; editing one
//! in `$VISUAL`/`$EDITOR` and saving it with `memory.write` at the revision
//! read; creating and deleting memories; and the invalid files and other
//! entries of the memory folder with the reason. A memory that changed
//! while it was open is never overwritten silently: the panel offers to
//! reload it or to overwrite it.

pub(in crate::ui) use cookie_agent_client_state::memory::{
    MemoryRow, MemoryWriteOutcome, NEW_MEMORY,
};
use cookie_agent_client_state::memory::{
    delete_refusal, layer_title, memory_name, memory_rows, message_of, new_memory_refusal,
    saved_status,
};
#[cfg(test)]
use cookie_agent_protocol::MemoryIgnoredEntry;
use cookie_agent_protocol::{MemoryInfo, MemoryLayer, MemoryListParams, MemoryListResult, errors};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, keys::edit_plain_input, paint_panel};
use crate::{theme::Theme, ui::input::InputState};

/// A memory to open in the editor; the event loop runs the editor and
/// hands the result to [`App::finish_memory_edit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct MemoryEditRequest {
    pub(in crate::ui) url: String,
    pub(in crate::ui) text: String,
    /// The text as read; the template for a new memory.
    pub(in crate::ui) original: String,
    /// The revision read; `None` for a new memory.
    pub(in crate::ui) revision: Option<String>,
}

/// Why a write was not saved, as the panel offers to go on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) enum PendingEdit {
    /// The text is not a valid memory: edit it again, or drop it.
    Invalid {
        request: MemoryEditRequest,
        message: String,
    },
    /// The memory changed, appeared, or went away since it was read: reload
    /// it, overwrite it with this text, or drop the text.
    Changed { request: MemoryEditRequest },
}

/// What to do with a memory once `memory.read` answers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) enum ReadPurpose {
    /// Open it in the editor; a missing memory opens the template.
    Edit,
    /// Write this text over whatever revision it has now.
    Overwrite(MemoryEditRequest),
}

/// An action the panel offers, by key or by button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum MemoryAction {
    Edit,
    New,
    Delete,
    ConfirmDelete,
    Cancel,
    EditAgain,
    Reload,
    Overwrite,
}

impl MemoryAction {
    const fn key_label(self) -> &'static str {
        match self {
            Self::Edit => "enter",
            Self::New => "n",
            Self::Delete => "d",
            Self::ConfirmDelete => "y",
            Self::Cancel => "esc",
            Self::EditAgain => "e",
            Self::Reload => "r",
            Self::Overwrite => "o",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::New => "new memory",
            Self::Delete => "delete",
            Self::ConfirmDelete => "delete",
            Self::Cancel => "cancel",
            Self::EditAgain => "edit again",
            Self::Reload => "reload current",
            Self::Overwrite => "overwrite",
        }
    }
}

/// Where the panel's clickable parts landed this frame.
#[derive(Clone, Debug, Default)]
pub(in crate::ui) struct MemoryHits {
    /// (row rect, row index).
    pub(in crate::ui) rows: Vec<(Rect, usize)>,
    pub(in crate::ui) actions: Vec<(Rect, MemoryAction)>,
}

#[derive(Default)]
pub(in crate::ui) struct MemoryPanelState {
    /// `memory.list`, or why it failed; `None` until it answers.
    pub(in crate::ui) listing: Option<Result<MemoryListResult, String>>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) refresh_in_flight: bool,
    /// The URL of a new memory being typed.
    pub(in crate::ui) new_url: Option<InputState>,
    /// The memory a delete waits to be confirmed for: URL and revision.
    pub(in crate::ui) confirm_delete: Option<(String, String)>,
    /// A write that was not saved, waiting for the user's choice.
    pub(in crate::ui) pending: Option<PendingEdit>,
}

impl MemoryPanelState {
    /// The rows: each layer's heading, its memories grouped by machine or
    /// project, then what is not a memory.
    pub(in crate::ui) fn rows(&self) -> Vec<MemoryRow> {
        match &self.listing {
            Some(Ok(listing)) => memory_rows(listing),
            _ => Vec::new(),
        }
    }

    pub(in crate::ui) fn selected_row(&self) -> Option<MemoryRow> {
        let index = self.selection.selected()?;
        self.rows().into_iter().nth(index)
    }

    fn selected_memory(&self) -> Option<MemoryInfo> {
        match self.selected_row()? {
            MemoryRow::Memory(memory) => Some(memory),
            _ => None,
        }
    }

    /// The actions the panel offers now.
    pub(in crate::ui) fn actions(&self) -> Vec<MemoryAction> {
        if self.new_url.is_some() {
            return Vec::new();
        }
        match &self.pending {
            Some(PendingEdit::Invalid { .. }) => {
                return vec![MemoryAction::EditAgain, MemoryAction::Cancel];
            }
            Some(PendingEdit::Changed { .. }) => {
                return vec![
                    MemoryAction::Reload,
                    MemoryAction::Overwrite,
                    MemoryAction::Cancel,
                ];
            }
            None => {}
        }
        if self.confirm_delete.is_some() {
            return vec![MemoryAction::ConfirmDelete, MemoryAction::Cancel];
        }
        if !matches!(self.listing, Some(Ok(_))) {
            return Vec::new();
        }
        if self.selected_memory().is_some() {
            vec![MemoryAction::Edit, MemoryAction::New, MemoryAction::Delete]
        } else {
            vec![MemoryAction::New]
        }
    }

    fn install(&mut self, listing: Result<MemoryListResult, String>) {
        self.listing = Some(listing);
        self.refresh_in_flight = false;
        let count = self.rows().len();
        self.selection
            .select((count > 0).then(|| self.selection.selected().unwrap_or(0).min(count - 1)));
    }
}

impl App {
    pub(in crate::ui) fn open_memory_panel(&mut self) {
        self.memory_panel = MemoryPanelState::default();
        self.modal = Modal::Memory;
        self.poll_memory();
    }

    /// Fetches `memory.list` for the open `/memory` panel.
    pub(super) fn poll_memory(&mut self) {
        if self.modal != Modal::Memory || self.memory_panel.refresh_in_flight {
            return;
        }
        self.memory_panel.refresh_in_flight = true;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_memory(MemoryListParams::default())
                .await
                .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::MemoryListed { result });
        });
    }

    pub(super) fn install_memory_listing(&mut self, result: Result<MemoryListResult, String>) {
        self.memory_panel.install(result);
    }

    /// Reads `url` for `purpose`.
    fn dispatch_memory_read(&mut self, url: String, purpose: ReadPurpose) {
        self.status = format!("reading {url}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = match client.read_memory(url.clone()).await {
                Ok(read) => Ok(Some(read)),
                Err(error) if error.is(&errors::MEMORY_NOT_FOUND) => Ok(None),
                Err(error) => Err(message_of(&error)),
            };
            let _ = updates.send(RpcUpdate::MemoryRead {
                url,
                purpose,
                result,
            });
        });
    }

    pub(super) fn install_memory_read(
        &mut self,
        url: String,
        purpose: ReadPurpose,
        result: Result<Option<cookie_agent_protocol::MemoryReadResult>, String>,
    ) {
        let read = match result {
            Ok(read) => read,
            Err(error) => {
                self.status = format!("{url} could not be read: {error}");
                return;
            }
        };
        match purpose {
            ReadPurpose::Edit => {
                self.memory_panel.pending = None;
                self.status = match read.as_ref().and_then(|read| read.invalid.as_deref()) {
                    Some(reason) => format!("{url} is not a valid memory: {reason}"),
                    None => format!("editing {url}"),
                };
                self.pending_memory_editor = Some(match read {
                    Some(read) => MemoryEditRequest {
                        url: read.url,
                        text: read.text.clone(),
                        original: read.text,
                        revision: Some(read.revision),
                    },
                    None => MemoryEditRequest {
                        url,
                        text: NEW_MEMORY.to_owned(),
                        original: NEW_MEMORY.to_owned(),
                        revision: None,
                    },
                });
            }
            ReadPurpose::Overwrite(request) => {
                let request = MemoryEditRequest {
                    revision: read.map(|read| read.revision),
                    ..request
                };
                self.dispatch_memory_write(request);
            }
        }
    }

    /// What the editor left for `request`: saved unless unchanged.
    pub(in crate::ui) fn finish_memory_edit(
        &mut self,
        request: MemoryEditRequest,
        edited: Result<String, String>,
    ) {
        let text = match edited {
            Ok(text) => text,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if text == request.original {
            self.memory_panel.pending = None;
            self.status = if request.revision.is_some() {
                format!("No changes to {}.", request.url)
            } else {
                format!("No changes; {} was not created.", request.url)
            };
            return;
        }
        self.dispatch_memory_write(MemoryEditRequest { text, ..request });
    }

    fn dispatch_memory_write(&mut self, request: MemoryEditRequest) {
        self.status = format!("saving {}…", request.url);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .write_memory(
                    request.url.clone(),
                    request.text.clone(),
                    request.revision.clone(),
                )
                .await;
            let outcome = MemoryWriteOutcome::from_result(result);
            let _ = updates.send(RpcUpdate::MemoryWritten { request, outcome });
        });
    }

    pub(super) fn install_memory_write(
        &mut self,
        request: MemoryEditRequest,
        outcome: MemoryWriteOutcome,
    ) {
        match outcome {
            MemoryWriteOutcome::Saved { notice } => {
                self.memory_panel.pending = None;
                self.status =
                    saved_status(&request.url, request.revision.is_none(), notice.as_deref());
            }
            MemoryWriteOutcome::Invalid(message) => {
                self.status = format!("Not saved: {message}");
                self.memory_panel.pending = Some(PendingEdit::Invalid { request, message });
            }
            MemoryWriteOutcome::Changed => {
                self.status = format!("Not saved: {} changed since it was opened.", request.url);
                self.memory_panel.pending = Some(PendingEdit::Changed { request });
            }
            MemoryWriteOutcome::Failed(error) => {
                self.status = format!("{} was not saved: {error}", request.url);
            }
        }
        self.poll_memory();
    }

    fn dispatch_memory_delete(&mut self, url: String, revision: String) {
        self.status = format!("deleting {url}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .delete_memory(url.clone(), revision)
                .await
                .map(|_| format!("Deleted {url}."))
                .map_err(|error| delete_refusal(&url, &error));
            let _ = updates.send(RpcUpdate::MemoryDeleted { result });
        });
    }

    pub(super) fn install_memory_delete(&mut self, result: Result<String, String>) {
        self.status = match result {
            Ok(done) | Err(done) => done,
        };
        self.poll_memory();
    }

    /// Acts on `action`, from its key or its button.
    pub(in crate::ui) fn activate_memory_action(&mut self, action: MemoryAction) {
        if !self.memory_panel.actions().contains(&action) {
            return;
        }
        let panel = &mut self.memory_panel;
        match action {
            MemoryAction::Edit => {
                if let Some(memory) = panel.selected_memory() {
                    self.dispatch_memory_read(memory.url, ReadPurpose::Edit);
                }
            }
            MemoryAction::New => {
                let prefix = panel
                    .selected_row()
                    .map_or_else(|| "project://".to_owned(), |row| row.new_prefix());
                let mut input = InputState::default();
                input.set_buffer(prefix);
                panel.new_url = Some(input);
            }
            MemoryAction::Delete => {
                if let Some(memory) = panel.selected_memory() {
                    panel.confirm_delete = Some((memory.url, memory.revision));
                }
            }
            MemoryAction::ConfirmDelete => {
                if let Some((url, revision)) = panel.confirm_delete.take() {
                    self.dispatch_memory_delete(url, revision);
                }
            }
            MemoryAction::Cancel => {
                if panel.confirm_delete.take().is_some() {
                    self.status = "Delete cancelled.".into();
                } else if let Some(pending) = panel.pending.take() {
                    let url = match pending {
                        PendingEdit::Invalid { request, .. } | PendingEdit::Changed { request } => {
                            request.url
                        }
                    };
                    self.status = format!("Your edit of {url} was dropped.");
                }
            }
            MemoryAction::EditAgain => {
                if let Some(PendingEdit::Invalid { request, .. }) = panel.pending.take() {
                    self.pending_memory_editor = Some(request);
                }
            }
            MemoryAction::Reload => {
                if let Some(PendingEdit::Changed { request }) = panel.pending.take() {
                    self.dispatch_memory_read(request.url, ReadPurpose::Edit);
                }
            }
            MemoryAction::Overwrite => {
                if let Some(PendingEdit::Changed { request }) = panel.pending.take() {
                    self.dispatch_memory_read(request.url.clone(), ReadPurpose::Overwrite(request));
                }
            }
        }
    }

    pub(super) fn handle_memory_key(&mut self, key: KeyEvent) {
        if let Some(input) = self.memory_panel.new_url.as_mut() {
            match key.code {
                KeyCode::Esc => self.memory_panel.new_url = None,
                KeyCode::Enter => {
                    let url = input.as_str().trim().to_owned();
                    self.memory_panel.new_url = None;
                    self.begin_new_memory(url);
                }
                _ => edit_plain_input(input, key),
            }
            return;
        }
        let actions = self.memory_panel.actions();
        let action = match key.code {
            KeyCode::Esc if actions.contains(&MemoryAction::Cancel) => Some(MemoryAction::Cancel),
            KeyCode::Esc => {
                self.modal = Modal::None;
                return;
            }
            KeyCode::Enter => Some(MemoryAction::Edit),
            KeyCode::Char(character) => actions
                .iter()
                .copied()
                .find(|action| action.key_label() == character.to_string()),
            KeyCode::Up | KeyCode::Down => {
                if self.memory_panel.confirm_delete.is_none() {
                    let count = self.memory_panel.rows().len();
                    super::move_picker_selection(
                        &mut self.memory_panel.selection,
                        count,
                        key.code == KeyCode::Up,
                    );
                }
                return;
            }
            _ => None,
        };
        if let Some(action) = action {
            self.activate_memory_action(action);
        }
    }

    /// Opens a new memory at `url` in the editor, unless it exists.
    fn begin_new_memory(&mut self, url: String) {
        if let Some(Ok(listing)) = &self.memory_panel.listing
            && let Some(refusal) = new_memory_refusal(listing, &url)
        {
            self.status = refusal;
            return;
        }
        // The daemon checks the URL when the memory is saved.
        self.pending_memory_editor = Some(MemoryEditRequest {
            url,
            text: NEW_MEMORY.to_owned(),
            original: NEW_MEMORY.to_owned(),
            revision: None,
        });
    }

    pub(super) fn render_memory_panel(&mut self, frame: &mut Frame) {
        let area = centered(frame.area(), 88, 76);
        paint_panel(frame, area, &self.theme);
        self.hit_map.memory = render_memory(frame, area, &mut self.memory_panel, &self.theme);
    }
}

/// The panel: the rows, the selected row's details or what waits for the
/// user, the actions, and the new-memory form.
fn render_memory(
    frame: &mut Frame,
    area: Rect,
    panel: &mut MemoryPanelState,
    theme: &Theme,
) -> MemoryHits {
    let mut hits = MemoryHits::default();
    let rows = panel.rows();
    let count = rows.len();
    panel
        .selection
        .select((count > 0).then(|| panel.selection.selected().unwrap_or(0).min(count - 1)));
    let form_height = if panel.new_url.is_some() { 3 } else { 0 };
    let [list_area, detail_area, actions_area, form_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(8),
            Constraint::Length(1),
            Constraint::Length(form_height),
        ])
        .areas(area);
    let items = rows
        .iter()
        .map(|row| ListItem::new(row_line(row, theme)))
        .collect::<Vec<_>>();
    let block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title("Memory"))
        .title_bottom(crate::ui::panel_title(
            Line::from(Span::styled(
                "enter edit | n new | d delete | esc close",
                theme.internal(),
            ))
            .right_aligned(),
        ));
    let inner = block.inner(list_area);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(block),
        list_area,
        &mut panel.selection,
    );
    let quiet = panel.new_url.is_some() || panel.confirm_delete.is_some();
    if !quiet {
        let offset = panel.selection.offset();
        for (line, index) in (offset..count).take(usize::from(inner.height)).enumerate() {
            let y = inner.y + u16::try_from(line).unwrap_or(u16::MAX);
            hits.rows
                .push((Rect::new(inner.x, y, inner.width, 1), index));
        }
    }
    frame.render_widget(
        Paragraph::new(detail_text(panel))
            .wrap(Wrap { trim: false })
            .block(
                crate::ui::panel_block()
                    .border_style(theme.panel_border())
                    .title(crate::ui::panel_title("Details")),
            ),
        detail_area,
    );
    let mut spans = Vec::new();
    let mut x = actions_area.x;
    for action in panel.actions() {
        let label = format!(" {} ({}) ", action.label(), action.key_label());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if x.saturating_add(width) <= actions_area.right() {
            hits.actions
                .push((Rect::new(x, actions_area.y, width, 1), action));
        }
        spans.push(Span::styled(label, theme.link()));
        spans.push(Span::raw(" "));
        x = x.saturating_add(width + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), actions_area);
    if let Some(input) = panel.new_url.as_mut() {
        crate::ui::input::render(
            frame,
            form_area,
            input,
            true,
            "New memory URL",
            Some("enter opens the editor | esc cancels"),
            theme,
        );
    }
    hits
}

fn row_line(row: &MemoryRow, theme: &Theme) -> Line<'static> {
    match row {
        MemoryRow::Layer(layer) => Line::from(Span::styled(layer_title(*layer), theme.body())),
        MemoryRow::Group { layer, name } => Line::from(vec![
            Span::styled(format!("  {name}"), theme.body()),
            Span::styled(
                if *layer == MemoryLayer::Machine {
                    "  machine"
                } else {
                    "  project"
                },
                theme.muted(),
            ),
        ]),
        MemoryRow::Memory(memory) => {
            let indent = if memory.group.is_some() { "    " } else { "  " };
            let mut spans = vec![Span::styled(
                format!("{indent}{}", memory_name(memory)),
                theme.body(),
            )];
            match (&memory.invalid, &memory.description) {
                (Some(_), _) => spans.push(Span::styled("  invalid", theme.warning())),
                (None, Some(description)) => {
                    spans.push(Span::styled(format!("  {description}"), theme.muted()));
                }
                (None, None) => {}
            }
            if memory.orphaned {
                spans.push(Span::styled("  (no current agent)", theme.warning()));
            }
            Line::from(spans)
        }
        MemoryRow::IgnoredHeading => {
            Line::from(Span::styled("Not memories (ignored)", theme.body()))
        }
        MemoryRow::Ignored(ignored) => Line::from(vec![
            Span::styled(format!("  {}", ignored.path), theme.muted()),
            Span::styled(format!("  {}", ignored.reason), theme.warning()),
        ]),
    }
}

fn detail_text(panel: &MemoryPanelState) -> String {
    if panel.new_url.is_some() {
        return "Type the new memory's URL: user://<category>, agent://<agent>, \
                machine://<machine>/<name>, or project://<project>/<name>. Enter opens it \
                in $VISUAL or $EDITOR; saving creates it."
            .into();
    }
    if let Some((url, _)) = &panel.confirm_delete {
        return format!("Delete {url}? y deletes; Esc cancels.");
    }
    match &panel.pending {
        Some(PendingEdit::Invalid { request, message }) => {
            return format!(
                "{} was not saved: {message}\n\ne edits your text again; Esc drops it.",
                request.url
            );
        }
        Some(PendingEdit::Changed { request }) => {
            return format!(
                "{} changed since you opened it, so your edit was not saved.\n\n\
                 r reloads the current version into the editor (your edit is dropped); \
                 o overwrites the current version with your edit; Esc drops your edit.",
                request.url
            );
        }
        None => {}
    }
    match &panel.listing {
        None => return "Loading memory...".into(),
        Some(Err(error)) => return format!("Memory is unavailable: {error}"),
        Some(Ok(_)) => {}
    }
    match panel.selected_row() {
        None => "No memory yet. n creates one.".into(),
        Some(MemoryRow::Memory(memory)) => {
            let mut lines = vec![memory.url.clone()];
            if let Some(description) = &memory.description {
                lines.push(description.clone());
            }
            if !memory.keywords.is_empty() {
                lines.push(format!("keywords: {}", memory.keywords.join(", ")));
            }
            if let Some(reason) = &memory.invalid {
                lines.push(format!(
                    "invalid: {reason}; search and the snapshot skip it until it is fixed"
                ));
            }
            if memory.orphaned {
                lines.push(
                    "No current agent has this ID; the file stays until you delete it.".into(),
                );
            }
            lines.push(format!(
                "{} bytes, revision {}",
                memory.size, memory.revision
            ));
            lines.join("\n")
        }
        Some(MemoryRow::Ignored(ignored)) => format!(
            "{}: {}\nSearch and the snapshot never read it.",
            ignored.path, ignored.reason
        ),
        Some(MemoryRow::IgnoredHeading) => {
            "Entries of the memory folder that are not memory files of its layout.".into()
        }
        Some(row @ (MemoryRow::Layer(_) | MemoryRow::Group { .. })) => {
            format!("n creates a memory here ({}…).", row.new_prefix())
        }
    }
}

/// A listing with every kind of row: two categories, an agent file of no
/// current agent, a machine memory, an invalid project memory, and an
/// ignored file.
#[cfg(test)]
pub(in crate::ui) fn sample_memory() -> MemoryListResult {
    let memory = |url: &str, layer, group: Option<&str>, description: Option<&str>| MemoryInfo {
        url: url.into(),
        layer,
        group: group.map(str::to_owned),
        revision: "0123456789abcdef".into(),
        size: 64,
        description: description.map(str::to_owned),
        keywords: Vec::new(),
        invalid: description.is_none().then(|| "no frontmatter".to_owned()),
        orphaned: false,
    };
    let mut ghost = memory(
        "agent://ghost",
        MemoryLayer::Agent,
        None,
        Some("Old lessons"),
    );
    ghost.orphaned = true;
    MemoryListResult {
        memories: vec![
            memory(
                "user://feedback",
                MemoryLayer::User,
                None,
                Some("Corrections"),
            ),
            memory(
                "user://preferences",
                MemoryLayer::User,
                None,
                Some("How the user wants work done"),
            ),
            ghost,
            memory(
                "machine://labpc/gpu",
                MemoryLayer::Machine,
                Some("labpc"),
                Some("The GPU"),
            ),
            memory(
                "project://web/build",
                MemoryLayer::Project,
                Some("web"),
                None,
            ),
        ],
        ignored: vec![MemoryIgnoredEntry {
            path: "projects/web/notes.txt".into(),
            reason: "not a `.md` file".into(),
        }],
    }
}
