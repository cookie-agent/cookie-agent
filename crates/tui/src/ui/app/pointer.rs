//! Pointer targets: the one resolver that turns a cell into the element a
//! click acts on and a hover highlights, the click dispatch over it, and the
//! hover paint pass.

use super::*;

impl App {
    /// Resolve the element under a point. Overlay *ownership* comes from
    /// current state, never from hit-map geometry: an overlay that opened
    /// since the last frame still owns the pointer (its geometry may not
    /// exist yet), and one that just closed leaves content geometry
    /// underneath intact. Each overlay branch therefore always returns —
    /// geometry only picks the element within the panel, never whether the
    /// panel owns the point. Content follows in click priority order.
    pub(in crate::ui) fn pointer_target_at(&self, column: u16, row: u16) -> Option<PointerTarget> {
        let over = |rect: Rect| contains(rect, column, row);
        let hits = &self.hit_map;
        if self.command_palette_visible() {
            return hits
                .palette_rows
                .iter()
                .find(|hit| over(hit.rect))
                .map(|hit| PointerTarget::PaletteRow(hit.index));
        }
        if self.modal != Modal::None {
            return self.modal_target_at(over);
        }
        if self.current_approval().is_some() {
            if let Some(hit) = hits.approval_actions.iter().find(|hit| over(hit.rect)) {
                return Some(PointerTarget::ApprovalAction(hit.decision));
            }
            return hits
                .approval_details
                .filter(|rect| over(*rect))
                .map(|_| PointerTarget::ApprovalDetails);
        }
        // A goal that went away since the last frame leaves no target
        // behind on its stale bar geometry.
        if self.goal_bar_visible()
            && let Some((_, action)) = hits.goal_actions.iter().find(|(rect, _)| over(*rect))
        {
            return Some(PointerTarget::GoalAction(*action));
        }
        if hits.restart_banner.is_some_and(over) {
            return Some(PointerTarget::RestartBanner);
        }
        if hits.background_bar.is_some_and(over) {
            return Some(PointerTarget::BackgroundBar);
        }
        if let Some((_, action)) = hits.onboarding.notice.iter().find(|(rect, _)| over(*rect)) {
            return Some(PointerTarget::OnboardingNotice(*action));
        }
        if hits.permission_mode.is_some_and(over) {
            return Some(PointerTarget::PermissionMode);
        }
        if hits.session_cost.is_some_and(over) {
            return Some(PointerTarget::SessionCost);
        }
        if hits.project_label.is_some_and(over) {
            return Some(PointerTarget::ProjectLabel);
        }
        if hits.event_level_filter.is_some_and(over) {
            return Some(PointerTarget::EventLevelFilter);
        }
        if let Some(hit) = hits.title_segments.iter().find(|hit| over(hit.rect)) {
            return Some(PointerTarget::TitleSegment(hit.segment));
        }
        if let Some(hit) = hits.input.filter(|hit| over(hit.rect)) {
            // The composer's reserved scrollbar column mirrors the
            // conversation's; every other composer cell places the caret.
            if hit.scrollbar.is_some_and(|geometry| over(geometry.track)) {
                return Some(PointerTarget::Scrollbar(ScrollbarTarget::Input));
            }
            return Some(PointerTarget::ComposerText);
        }
        if let Some(hit) = hits.queue_entries.iter().find(|hit| over(hit.rect)) {
            return Some(if hit.recall_seq.is_some() {
                PointerTarget::QueueEntry(hit.index)
            } else {
                PointerTarget::PassiveQueueEntry(hit.index)
            });
        }
        // The scrollbar column is reserved from content and block hit
        // regions; it is a target only while there is a thumb to move.
        if hits.scrollbar.is_some_and(over) && self.scrollbar_geometry.is_some() {
            return Some(PointerTarget::Scrollbar(ScrollbarTarget::Conversation));
        }
        if hits.conversation.is_some_and(over)
            && let Some(target) = self.transcript_target_at(over)
        {
            return Some(target);
        }
        hits.tree_rows.iter().find(|hit| over(hit.rect)).map(|hit| {
            match hit.expand_rect.filter(|rect| over(*rect)) {
                Some(_) => PointerTarget::TreeExpand(hit.session_id),
                None => PointerTarget::TreeRow(hit.session_id),
            }
        })
    }

    /// The transcript element under a point inside the conversation
    /// viewport, which bounds every transcript target.
    fn transcript_target_at(&self, over: impl Fn(Rect) -> bool) -> Option<PointerTarget> {
        let hits = &self.hit_map;
        // Past user-message rows open the copy/revert/fork menu — never
        // assistant/tool rows, which keep their expand/collapse toggle.
        if let Some(hit) = hits.user_messages.iter().find(|hit| over(hit.rect)) {
            return Some(PointerTarget::UserMessage(hit.seq));
        }
        // A running call's background button sits on its title row, which
        // otherwise toggles the call.
        if let Some(call_id) = hits
            .blocks
            .iter()
            .rev()
            .find(|hit| hit.action_rect.is_some_and(&over))
            .and_then(|hit| match hit.id {
                BlockId::Tool(call_id) => Some(call_id),
                _ => None,
            })
        {
            return Some(PointerTarget::BackgroundButton(call_id));
        }
        hits.blocks
            .iter()
            .rev()
            .find(|hit| hit.toggle_rect.is_some_and(&over))
            .map(|hit| PointerTarget::TranscriptBlock(hit.id))
    }

    /// The element under the pointer inside the open modal, which owns
    /// every point while it is up.
    fn modal_target_at(&self, over: impl Fn(Rect) -> bool) -> Option<PointerTarget> {
        let hits = &self.hit_map;
        match self.modal {
            Modal::Config => {
                return hits
                    .config_actions
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, action)| PointerTarget::ConfigAction(*action));
            }
            Modal::RestartConfirm => {
                return hits
                    .restart_choices
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, choice)| PointerTarget::RestartChoice(*choice));
            }
            Modal::GoalDetail => {
                return hits
                    .goal_close
                    .filter(|rect| over(*rect))
                    .map(|_| PointerTarget::GoalClose);
            }
            Modal::Permissions => {
                let permissions = &hits.permissions;
                if permissions.mode.is_some_and(&over) {
                    return Some(PointerTarget::PermissionPanelMode);
                }
                // The rule form owns the panel: only its own effects act.
                let effects: &[(Rect, PermissionEffect)] = if self.permission_panel.form.is_some() {
                    &permissions.form_effects
                } else if self.permission_panel.selected().is_some() {
                    &permissions.effects
                } else {
                    // A row effect sets the selected rule's effect; with no
                    // rule selected the click selects the row instead.
                    &[]
                };
                if let Some((_, effect)) = effects.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::PermissionEffect(*effect));
                }
                if self.permission_panel.form.is_some() {
                    return None;
                }
                return permissions
                    .rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::PermissionRow(*index));
            }
            Modal::Runtimes => {
                let runtimes = &hits.runtimes;
                if let Some((_, view)) = runtimes.tabs.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::RuntimeTab(*view));
                }
                if let Some((_, action)) = runtimes
                    .machine_actions
                    .iter()
                    .find(|(rect, _)| over(*rect))
                {
                    return Some(PointerTarget::MachineAction(*action));
                }
                return runtimes
                    .machine_rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::MachineRow(*index));
            }
            Modal::Onboarding => {
                let onboarding = &hits.onboarding;
                if let Some((_, step)) = onboarding.tabs.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::OnboardingTab(*step));
                }
                if let Some((_, action)) = onboarding.actions.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::OnboardingAction(*action));
                }
                return onboarding
                    .rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::OnboardingRow(*index));
            }
            Modal::Cron => {
                let cron = &hits.cron;
                if let Some((_, action)) = cron.actions.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::CronAction(*action));
                }
                return cron
                    .rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::CronRow(*index));
            }
            Modal::Memory => {
                let memory = &hits.memory;
                if let Some((_, action)) = memory.actions.iter().find(|(rect, _)| over(*rect)) {
                    return Some(PointerTarget::MemoryAction(*action));
                }
                return memory
                    .rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::MemoryRow(*index));
            }
            // A waiting delete owns the picker: only its answers act.
            Modal::Sessions if self.session_delete.is_some() => {
                return hits
                    .confirm_buttons
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, button)| PointerTarget::ConfirmButton(*button));
            }
            // The title field owns the picker; it is a text field, so
            // nothing in it is a target.
            Modal::Sessions if self.session_rename.is_some() => return None,
            // So does the new-project field.
            Modal::Projects if self.project_create.is_some() => return None,
            Modal::Sessions => {
                if let Some((_, action)) = hits
                    .session_picker_actions
                    .iter()
                    .find(|(rect, _)| over(*rect))
                {
                    return Some(PointerTarget::SessionPickerAction(*action));
                }
            }
            Modal::Background => {
                if let Some((_, index)) = hits.background_stops.iter().find(|(rect, _)| over(*rect))
                {
                    return Some(PointerTarget::BackgroundStop(*index));
                }
                return hits
                    .background_rows
                    .iter()
                    .find(|(rect, _)| over(*rect))
                    .map(|(_, index)| PointerTarget::BackgroundRow(*index));
            }
            _ => {}
        }
        if let Some((_, button)) = hits.confirm_buttons.iter().find(|(rect, _)| over(*rect)) {
            return Some(PointerTarget::ConfirmButton(*button));
        }
        if hits.picker_input.is_some_and(|hit| over(hit.rect)) {
            return Some(PointerTarget::PickerInput);
        }
        if let Some(hit) = hits.picker_rows.iter().find(|hit| over(hit.rect)) {
            return Some(PointerTarget::PickerRow(hit.index));
        }
        if hits.provider_submit.is_some_and(&over) {
            return Some(PointerTarget::ProviderSubmit);
        }
        if hits.provider_cancel.is_some_and(&over) {
            return Some(PointerTarget::ProviderCancel);
        }
        hits.provider_fields
            .iter()
            .find(|hit| over(hit.rect))
            .map(|hit| PointerTarget::ProviderField(hit.focus))
    }

    /// The hover target under a point: the pointer target, unless it is
    /// one of the hover-quiet exemptions.
    pub(in crate::ui) fn hover_target_at(&self, column: u16, row: u16) -> Option<PointerTarget> {
        self.pointer_target_at(column, row)
            .filter(|target| target.is_hoverable())
    }

    /// Recompute the hovered element from the current hit map. Returns true
    /// when the hover target changed (the only case needing a redraw). A
    /// captured scrollbar drag freezes hover until the press is released.
    pub(super) fn update_hover(&mut self, column: u16, row: u16) -> bool {
        if self.scrollbar_drag.is_some() {
            return false;
        }
        let next = self.hover_target_at(column, row);
        if next == self.hover {
            return false;
        }
        self.hover = next;
        match next {
            Some(PointerTarget::GoalAction(action)) => {
                self.status = format!("Goal: {}", goal::goal_action_label(action));
            }
            Some(PointerTarget::GoalClose) => self.status = "Close goal details".into(),
            _ => {}
        }
        true
    }

    pub(in crate::ui) async fn handle_click(&mut self, column: u16, row: u16) {
        self.handle_click_inner(column, row).await;
        self.resume_onboarding();
    }

    async fn handle_click_inner(&mut self, column: u16, row: u16) {
        let target = self.pointer_target_at(column, row);
        if self.command_palette_visible() {
            if let Some(target) = target {
                self.activate_pointer_target(target, column, row).await;
            }
            return;
        }
        if self.modal != Modal::None {
            if self.modal == Modal::Permissions
                && self.permission_panel.form.is_none()
                && target != Some(PointerTarget::PermissionPanelMode)
            {
                self.permission_panel.notice = None;
            }
            match target {
                Some(target) => self.activate_pointer_target(target, column, row).await,
                // A click beside the background panel's rows closes it.
                None if self.modal == Modal::Background => self.modal = Modal::None,
                None => {}
            }
            return;
        }
        // Any content click past the goal and background bars leaves the
        // goal bar's keyboard focus.
        if self.current_approval().is_none()
            && !matches!(
                target,
                Some(PointerTarget::GoalAction(_) | PointerTarget::BackgroundBar)
            )
        {
            self.goal_focus = None;
        }
        if let Some(target) = target {
            self.activate_pointer_target(target, column, row).await;
        }
    }

    /// Perform a resolved target's click action. `column`/`row` place a
    /// caret or a scrollbar grab within the target.
    async fn activate_pointer_target(&mut self, target: PointerTarget, column: u16, row: u16) {
        match target {
            PointerTarget::PaletteRow(index) => self.activate_palette_row(index).await,
            PointerTarget::GoalClose => {
                self.handle_goal_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            PointerTarget::PermissionPanelMode | PointerTarget::PermissionMode => {
                self.cycle_permission_mode();
            }
            PointerTarget::PermissionEffect(effect) => {
                if let Some(form) = &mut self.permission_panel.form {
                    form.effect = effect;
                    form.focus = PermissionFormFocus::Effect;
                } else if let Some(selected) = self.permission_panel.selected() {
                    self.set_permission_effect(&selected, effect);
                }
            }
            PointerTarget::PermissionRow(index) => {
                self.permission_panel.selection.select(Some(index));
                if self.permission_panel.add_row_selected() {
                    self.open_permission_form();
                }
            }
            PointerTarget::BackgroundRow(index) => self.background_panel.selected = index,
            PointerTarget::RuntimeTab(view) => self.show_runtime_view(view),
            PointerTarget::MachineRow(index) => {
                self.runtime_panel.machine_selection.select(Some(index));
            }
            PointerTarget::MachineAction(action) => self.begin_machine_action(action),
            PointerTarget::MemoryRow(index) => {
                self.memory_panel.selection.select(Some(index));
            }
            PointerTarget::MemoryAction(action) => self.activate_memory_action(action),
            PointerTarget::CronRow(index) => self.cron_panel.selection.select(Some(index)),
            PointerTarget::CronAction(action) => self.activate_cron_action(action),
            PointerTarget::OnboardingNotice(action) | PointerTarget::OnboardingAction(action) => {
                self.activate_onboarding_action(action);
            }
            PointerTarget::OnboardingTab(step) => self.show_onboarding_step(step),
            PointerTarget::OnboardingRow(index) => self.select_onboarding_file(index),
            PointerTarget::BackgroundStop(index) => self.stop_background_call(index),
            PointerTarget::ConfirmButton(button) => {
                if self.modal == Modal::RevertConfirm {
                    self.answer_revert_confirm(button);
                } else if self.modal == Modal::Sessions {
                    self.answer_session_delete(button);
                } else if self.modal == Modal::AttachConfirm {
                    self.answer_attach_offer(button).await;
                }
            }
            PointerTarget::PickerInput => {
                let Some(hit) = self.hit_map.picker_input else {
                    return;
                };
                let search = match self.modal {
                    Modal::Sessions => &mut self.session_search,
                    Modal::Agents => &mut self.agent_search,
                    Modal::Models => &mut self.model_search,
                    Modal::ConnectProviders => &mut self.provider_search,
                    _ => return,
                };
                search.focus_input();
                search.input_mut().set_cursor_from_display_position(
                    row.saturating_sub(hit.text_rect.y)
                        .min(hit.text_rect.height.saturating_sub(1)),
                    column
                        .saturating_sub(hit.text_rect.x)
                        .min(hit.text_rect.width),
                );
            }
            PointerTarget::PickerRow(index) => self.choose_picker_entry(index).await,
            PointerTarget::SessionPickerAction(action) => {
                self.activate_session_picker_action(action).await;
            }
            PointerTarget::ProviderSubmit => self.dispatch_provider_connect(),
            PointerTarget::ProviderCancel => self.cancel_connect_form(),
            PointerTarget::ProviderField(focus) => {
                if focus == ProviderFormFocus::AuthMethod {
                    // Clicking the selector mirrors pressing Enter on it:
                    // cycle to the next method, wiping its stale secrets.
                    if let Some(form) = self.provider_form.as_mut() {
                        form.cycle_auth_method(false);
                    }
                } else if let Some(hit) = self
                    .hit_map
                    .provider_fields
                    .iter()
                    .find(|hit| hit.focus == focus)
                    .copied()
                {
                    self.focus_provider_field(hit, column, row);
                }
            }
            PointerTarget::ApprovalAction(decision) => self.answer_approval(decision).await,
            PointerTarget::ApprovalDetails => self.toggle_approval_details(),
            PointerTarget::GoalAction(action) => self.activate_goal_action(action),
            PointerTarget::BackgroundBar => self.open_background_panel(),
            PointerTarget::RestartBanner => self.open_restart_confirm(),
            PointerTarget::ConfigAction(action) => self.activate_config_action(action),
            PointerTarget::RestartChoice(choice) => self.answer_restart_confirm(choice),
            PointerTarget::SessionCost => {
                self.modal = Modal::Usage;
                self.load_usage();
            }
            PointerTarget::ProjectLabel => self.open_project_picker(),
            PointerTarget::EventLevelFilter => self.cycle_event_level_filter(),
            // Agent and model title segments open selectors. The bracketed
            // variant suffix cycles in place and never opens a separate panel.
            PointerTarget::TitleSegment(segment) => match segment {
                TitleSegment::Agent => self.open_selection_modal(Modal::Agents),
                TitleSegment::Model => self.open_selection_modal(Modal::Models),
                TitleSegment::Variant => self.cycle_draft_variant(),
            },
            PointerTarget::ComposerText => {
                let Some(hit) = self.hit_map.input else {
                    return;
                };
                self.input.set_cursor_from_display_position(
                    row.saturating_sub(hit.text_rect.y)
                        .min(hit.text_rect.height.saturating_sub(1)),
                    column
                        .saturating_sub(hit.text_rect.x)
                        .min(hit.text_rect.width),
                );
            }
            PointerTarget::QueueEntry(index) | PointerTarget::PassiveQueueEntry(index) => {
                let Some((kind, recall_seq)) = self
                    .hit_map
                    .queue_entries
                    .iter()
                    .find(|hit| hit.index == index)
                    .map(|hit| (hit.kind, hit.recall_seq))
                else {
                    return;
                };
                if let Some(seq) = recall_seq {
                    self.recall_input(seq);
                } else if matches!(kind, QueueEntryKind::User | QueueEntryKind::Queued) {
                    self.status = "only the client that sent a message can recall it".into();
                }
            }
            // Both scrollbars behave alike: a press on the thumb captures a
            // drag, a press on the bare track pages to the matching offset —
            // all without moving the text cursor.
            PointerTarget::Scrollbar(target) => {
                let geometry = match target {
                    ScrollbarTarget::Conversation => self.scrollbar_geometry,
                    ScrollbarTarget::Input => self.hit_map.input.and_then(|hit| hit.scrollbar),
                };
                let Some(geometry) = geometry else {
                    return;
                };
                if contains(geometry.thumb, column, row) {
                    self.scrollbar_drag = Some(ScrollbarDrag {
                        grab_row: row.saturating_sub(geometry.thumb.y),
                        target,
                    });
                } else {
                    let offset = geometry.clamp_offset(geometry.offset_for_track_row(row));
                    match target {
                        ScrollbarTarget::Conversation => self.conversation_scroll.scroll_to(offset),
                        ScrollbarTarget::Input => self.input.scroll_to(offset),
                    }
                }
            }
            PointerTarget::UserMessage(seq) => {
                if let Some(hit) = self
                    .hit_map
                    .user_messages
                    .iter()
                    .find(|hit| hit.seq == seq)
                    .copied()
                {
                    self.open_user_menu(hit);
                }
            }
            PointerTarget::BackgroundButton(call_id) => self.request_background(call_id),
            PointerTarget::TranscriptBlock(id) => self.toggle_block(id),
            PointerTarget::TreeExpand(session_id) => self.toggle_tree_session(session_id),
            PointerTarget::TreeRow(session_id) => {
                self.tree_cursor = Some(session_id);
                // Watching a descendant changes the conversation/highlight
                // only; the tree root snapshot is retained.
                self.watch_session(session_id);
            }
        }
    }

    /// Patch the hover affordance onto the resolved target's cells. Text
    /// targets get the glaze text style; buttons get the fill-only variant
    /// so their glyphs stay put; transcript blocks and messages get the
    /// block fill; scrollbars take their own accent.
    pub(super) fn apply_hover(&self, frame: &mut ratatui::Frame) {
        let Some(hover) = self.hover else {
            return;
        };
        let text_style = self.theme.hover();
        let fill_style = self.theme.hover_fill();
        let patch = |frame: &mut ratatui::Frame, rect: Rect, style: Style| {
            for y in rect.y..rect.y.saturating_add(rect.height) {
                for x in rect.x..rect.x.saturating_add(rect.width) {
                    if frame.area().contains(Position::new(x, y)) {
                        let cell = &mut frame.buffer_mut()[(x, y)];
                        cell.set_style(style);
                    }
                }
            }
        };
        let hits = &self.hit_map;
        let one = |rect: Option<Rect>| rect.into_iter().collect::<Vec<_>>();
        let (rects, style) = match hover {
            // The block's own header content cells, minus a background
            // button, are the paint target; the wider hit rect stays the
            // click/toggle target.
            PointerTarget::TranscriptBlock(id) => (
                hits.blocks
                    .iter()
                    .find(|hit| hit.id == id)
                    .map(|hit| hit.hover_cells(hits.conversation))
                    .unwrap_or_default(),
                self.theme.block_hover(),
            ),
            PointerTarget::BackgroundButton(call_id) => (
                one(hits
                    .blocks
                    .iter()
                    .find(|hit| hit.id == BlockId::Tool(call_id))
                    .and_then(|hit| hit.action_rect)
                    .map(|rect| match hits.conversation {
                        Some(viewport) => rect.intersection(viewport),
                        None => rect,
                    })),
                fill_style,
            ),
            PointerTarget::UserMessage(seq) => (
                one(hits
                    .user_messages
                    .iter()
                    .find(|hit| hit.seq == seq)
                    .map(|hit| match hits.conversation {
                        Some(viewport) => hit.hover_rect.intersection(viewport),
                        None => hit.hover_rect,
                    })),
                self.theme.block_hover(),
            ),
            PointerTarget::GoalAction(action) => (
                one(hits
                    .goal_actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                if action == GoalBarAction::Details {
                    self.theme.block_hover()
                } else {
                    text_style
                },
            ),
            PointerTarget::GoalClose => (one(hits.goal_close), text_style),
            PointerTarget::PaletteRow(index) => (
                one(hits
                    .palette_rows
                    .iter()
                    .find(|hit| hit.index == index)
                    .map(|hit| hit.rect)),
                text_style,
            ),
            PointerTarget::PermissionPanelMode => (one(hits.permissions.mode), text_style),
            PointerTarget::PermissionEffect(effect) => (
                one(hits
                    .permissions
                    .form_effects
                    .iter()
                    .chain(&hits.permissions.effects)
                    .find(|(_, candidate)| *candidate == effect)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::PermissionRow(index) => {
                (one(indexed_rect(&hits.permissions.rows, index)), text_style)
            }
            PointerTarget::BackgroundRow(index) => {
                (one(indexed_rect(&hits.background_rows, index)), text_style)
            }
            PointerTarget::BackgroundStop(index) => {
                (one(indexed_rect(&hits.background_stops, index)), fill_style)
            }
            PointerTarget::RuntimeTab(view) => (
                one(hits
                    .runtimes
                    .tabs
                    .iter()
                    .find(|(_, candidate)| *candidate == view)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::MachineRow(index) => (
                one(indexed_rect(&hits.runtimes.machine_rows, index)),
                text_style,
            ),
            PointerTarget::MachineAction(action) => (
                one(hits
                    .runtimes
                    .machine_actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::MemoryRow(index) => {
                (one(indexed_rect(&hits.memory.rows, index)), text_style)
            }
            PointerTarget::MemoryAction(action) => (
                one(hits
                    .memory
                    .actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::CronRow(index) => {
                (one(indexed_rect(&hits.cron.rows, index)), text_style)
            }
            PointerTarget::CronAction(action) => (
                one(hits
                    .cron
                    .actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::OnboardingNotice(action) => (
                one(hits
                    .onboarding
                    .notice
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::OnboardingAction(action) => (
                one(hits
                    .onboarding
                    .actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::OnboardingTab(step) => (
                one(hits
                    .onboarding
                    .tabs
                    .iter()
                    .find(|(_, candidate)| *candidate == step)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::OnboardingRow(index) => {
                (one(indexed_rect(&hits.onboarding.rows, index)), text_style)
            }
            PointerTarget::PickerRow(index) => (
                one(hits
                    .picker_rows
                    .iter()
                    .find(|hit| hit.index == index)
                    .map(|hit| hit.rect)),
                text_style,
            ),
            PointerTarget::SessionPickerAction(action) => (
                one(hits
                    .session_picker_actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::ProviderField(focus) => (
                one(hits
                    .provider_fields
                    .iter()
                    .find(|hit| hit.focus == focus)
                    .map(|hit| hit.text_rect)),
                fill_style,
            ),
            PointerTarget::ProviderSubmit => (one(hits.provider_submit), fill_style),
            PointerTarget::ProviderCancel => (one(hits.provider_cancel), fill_style),
            PointerTarget::ConfirmButton(button) => (
                one(hits
                    .confirm_buttons
                    .iter()
                    .find(|(_, candidate)| *candidate == button)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::ApprovalDetails => (one(hits.approval_details), text_style),
            PointerTarget::ApprovalAction(decision) => (
                one(hits
                    .approval_actions
                    .iter()
                    .find(|hit| hit.decision == decision)
                    .map(|hit| hit.rect)),
                fill_style,
            ),
            PointerTarget::BackgroundBar => (one(hits.background_bar), text_style),
            PointerTarget::RestartBanner => (one(hits.restart_banner), fill_style),
            PointerTarget::ConfigAction(action) => (
                one(hits
                    .config_actions
                    .iter()
                    .find(|(_, candidate)| *candidate == action)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::RestartChoice(choice) => (
                one(hits
                    .restart_choices
                    .iter()
                    .find(|(_, candidate)| *candidate == choice)
                    .map(|(rect, _)| *rect)),
                fill_style,
            ),
            PointerTarget::TitleSegment(segment) => (
                one(hits
                    .title_segments
                    .iter()
                    .find(|hit| hit.segment == segment)
                    .map(|hit| hit.rect)),
                text_style,
            ),
            PointerTarget::PermissionMode => (one(hits.permission_mode), text_style),
            PointerTarget::SessionCost => (one(hits.session_cost), text_style),
            PointerTarget::ProjectLabel => (one(hits.project_label), text_style),
            PointerTarget::EventLevelFilter => (one(hits.event_level_filter), text_style),
            PointerTarget::QueueEntry(index) => (
                one(hits
                    .queue_entries
                    .iter()
                    .find(|hit| hit.index == index && hit.recall_seq.is_some())
                    .map(|hit| hit.rect)),
                text_style,
            ),
            // The whole column lights up, rail and thumb alike, wherever on
            // it the pointer rests.
            PointerTarget::Scrollbar(target) => (
                one(match target {
                    ScrollbarTarget::Conversation => self.scrollbar_geometry,
                    ScrollbarTarget::Input => hits.input.and_then(|hit| hit.scrollbar),
                }
                .map(|geometry| geometry.track)),
                self.theme.scrollbar_hover(),
            ),
            PointerTarget::TreeExpand(session_id) => (
                one(hits
                    .tree_rows
                    .iter()
                    .find(|hit| hit.session_id == session_id)
                    .and_then(|hit| hit.expand_rect)),
                fill_style,
            ),
            PointerTarget::TreeRow(session_id) => (
                one(hits
                    .tree_rows
                    .iter()
                    .find(|hit| hit.session_id == session_id)
                    .map(TreeRowHit::hover_rect)),
                text_style,
            ),
            PointerTarget::PickerInput
            | PointerTarget::ComposerText
            | PointerTarget::PassiveQueueEntry(_) => return,
        };
        for rect in rects {
            patch(frame, rect, style);
        }
    }
}

/// The rect of the `(rect, index)` hit for `index`, if it rendered.
fn indexed_rect(hits: &[(Rect, usize)], index: usize) -> Option<Rect> {
    hits.iter()
        .find(|(_, candidate)| *candidate == index)
        .map(|(rect, _)| *rect)
}
